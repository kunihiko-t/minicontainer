//! `minictr run`の実QEMU end-to-end検証。
//!
//! pin留めしたminiOS source revisionからguest kernelをbuildし、決定的なhello
//! MiniBundleを一時storeへimportして`hello` tagを作り、`minictr run`の標準出力、
//! 標準エラー出力、終了code、QEMU回収、一時directory cleanupを確認する。
//!
//! timeout、malformed-frame、出力上限の経路では、非0終了と一時領域の非残留
//! に加え、失敗時にguest出力を転送しないことも検査する。malformed-frame経路
//! はminiOSを経由せず、不正control headerをUARTへ直接書く最小supervisor
//! kernelをQEMU `-kernel` で起動し、`SessionError::Protocol` になることを
//! 検査する (guestはcontrol headerを直接書けないため)。出力上限経路は1 MiB
//! を超える連打guestをminiOS経由で走らせ、`SessionError::GuestOutputTooLarge`
//! になることを検査する。割り込み経路は回転中のguestへSIGINTを`minictr`の
//! group宛に送り、125終了とQEMU・一時領域の非残留を検査する。SIGTERM経路は
//! `minictr`のPIDだけへ送り、別groupのQEMUへhost転送が届くことと同じ後始末
//! を検査する。
//!
//! 実kernelのwire契約 (pin `9be99255a59d58d19db25b835af0e28a8d2a4036` の
//! `run_boot_payload`、`console::enter_control_mode`、`user::run` から確認):
//! Ready frameの後にcontrol modeへ入り、以降のUARTはcontrol frameだけになる。
//! `write`はStdout/Stderr frame、`exit`はExit frame (u32 LE) として届く。Exit
//! の後、kernelはresource回収を検証し、成功すれば`println!`がDiagnostic frame
//! (`MiniOS payload: ok code=N`) になってhostへ届いてからshutdownする。回収
//! 失敗やfatal trapでは`emergency_print`がGuestError frameになる。guestは
//! control headerを直接書けないため、不正headerのprotocol破損はguest経由で
//! 再現できず、malformed fixtureのsupervisor kernelが直接書いた不正headerで
//! frame層の失敗を検査する。

use std::{
    ffi::{OsStr, OsString},
    fmt, io,
    path::{Path, PathBuf},
    process::ExitStatus,
    process::{Child, Command},
    thread,
    time::{Duration, Instant},
};

use crate::tools;

/// E2Eがbuildして起動するminiOS sourceの公開URL。
pub const MINIOS_REPO_URL: &str = "https://github.com/kunihiko-t/minios.git";
/// E2EがbuildするminiOS kernelのpin留めrevision (M1統合のmerge commit)。
pub const MINIOS_KERNEL_REV: &str = "4865f9be97a6cdcd77c71e36b1ba426b49bd73d7";
/// E2E kernelが実装するGuest ABI tag。変更時は別承認のABI更新が必要である。
pub const MINIOS_ABI_TAG: &str = "minios-abi-v0.2.0";
/// E2Eが解決するimage tag。
pub const E2E_IMAGE: &str = "hello";
/// hello guestが報告する終了code。
pub const E2E_EXIT_CODE: i32 = 42;
/// hello guestの標準出力。
pub const E2E_STDOUT: &[u8] = b"hello stdout\n";
/// hello guestの標準エラー出力。
pub const E2E_STDERR: &[u8] = b"hello stderr\n";
/// 同梱guest例のrelease ELF。GuestExampleBuild位相の成果物でroot基準。
const GUEST_HELLO_ELF: &str = "target/guest-hello/riscv64gc-unknown-none-elf/release/guest-hello";
/// 同梱guest例の標準出力。
const GUEST_HELLO_STDOUT: &[u8] = b"hello from guest\n";
/// 同梱guest例の標準エラー出力。
const GUEST_HELLO_STDERR: &[u8] = b"guest stderr\n";
/// stdin echoの同梱guest例のrelease ELF。GuestEchoBuild位相の成果物。
const GUEST_ECHO_ELF: &str = "target/guest-echo/riscv64gc-unknown-none-elf/release/guest-echo";
/// echo guestを登録するE2Eのimage tag。
const E2E_ECHO_IMAGE: &str = "echo";

const RISCV_TARGET: &str = "riscv64gc-unknown-none-elf";
const KERNEL_BUILD_TIMEOUT: Duration = Duration::from_secs(600);
const QEMU_RUN_TIMEOUT: Duration = Duration::from_secs(180);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
const CHECKOUT_TIMEOUT: Duration = Duration::from_secs(120);
const HAPPY_PATH_TIMEOUT_MS: &str = "30000";
/// output-cap経路は1 MiB超を実UART速度 (~77 KiB/s) で流すため、happy path
/// より長い期限が要る。
const OUTPUT_CAP_TIMEOUT_MS: &str = "90000";
const SPIN_TIMEOUT_MS: &str = "800";
const POLL_INTERVAL: Duration = Duration::from_millis(50);
/// 割り込み経路でQEMU出現を待つ上限。`minictr`自体の30秒期限より短くし、
/// signal前に自己timeoutで終わる空振りを防ぐ。
const INTERRUPT_BOOT_TIMEOUT: Duration = Duration::from_secs(20);
/// QEMU出現後にguestを回してからSIGINTを送るまでの待ち時間。
const INTERRUPT_SETTLE: Duration = Duration::from_secs(2);
/// SIGINT後の`minictr`終了を待つ上限。
const INTERRUPT_EXIT_TIMEOUT: Duration = Duration::from_secs(30);
/// stress節の反復回数。全scenario合計21回、E2E全体に数十秒を足す量に抑える。
const STRESS_RUN_ITERS: usize = 5;
const STRESS_TIMEOUT_ITERS: usize = 5;
const STRESS_SIGNAL_ITERS: usize = 3;
const STRESS_DETACH_ITERS: usize = 3;
const STRESS_CRASH_ITERS: usize = 2;
/// stress節で回転するguestのimage tag。`hello`は正常終了するguestへ使う。
const E2E_SPIN_IMAGE: &str = "spin";

/// 実QEMU E2Eが失敗した理由。
#[derive(Debug)]
pub enum E2EError {
    /// 前提program (QEMU、Git) が使えない。
    Tool(crate::tools::ToolError),
    /// 外部commandの起動または実行が失敗した。
    Command { command: String, message: String },
    /// 外部commandが非0終了した。
    CommandFailed {
        command: String,
        status: Option<i32>,
        stdout: String,
        stderr: String,
    },
    /// miniOS checkoutのHEADがpin留めrevisionと異なる。
    UnexpectedKernelRev { expected: String, actual: String },
    /// miniOS checkoutに未commitの変更がある。
    DirtyCheckout { directory: PathBuf, status: String },
    /// `minictr` binaryがworkspace build成果物に存在しない。
    MissingMinictr(PathBuf),
    /// 同梱guest例のELFがguest build成果物に存在しない。
    MissingGuestElf(PathBuf),
    /// guestの実行結果が期待と異なる。
    UnexpectedRun {
        case: &'static str,
        expected: String,
        actual: String,
    },
    /// run後に一時payload directoryが残留した。
    PayloadLeftover(Vec<PathBuf>),
    /// run後にQEMU processが残留した。
    QemuLeftover(Vec<u32>),
    /// stress節の一iterationが失敗した。scenarioとiteration番号を残す。
    StressIteration {
        /// 失敗したscenario名。
        scenario: &'static str,
        /// 失敗したiteration番号(0始まり)。
        iteration: usize,
        /// 内側の失敗。
        source: Box<E2EError>,
    },
    /// 一時storeの準備または後始末が失敗した。
    Store(String),
    /// 制限時間内に操作が終わらなかった。
    TimedOut { command: String },
    /// outputの書き出しが失敗した。
    Output(io::Error),
}

impl fmt::Display for E2EError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tool(error) => error.fmt(formatter),
            Self::Command { command, message } => {
                write!(formatter, "could not run {command}: {message}")
            }
            Self::CommandFailed {
                command,
                status,
                stdout,
                stderr,
            } => {
                write!(
                    formatter,
                    "{command} failed with status {}",
                    status
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "unknown".to_owned())
                )?;
                if !stdout.is_empty() {
                    write!(formatter, "\nstdout:\n{}", stdout.trim_end())?;
                }
                if !stderr.is_empty() {
                    write!(formatter, "\nstderr:\n{}", stderr.trim_end())?;
                }
                Ok(())
            }
            Self::UnexpectedKernelRev { expected, actual } => write!(
                formatter,
                "miniOS checkout revision {actual} does not match the pinned E2E revision {expected}"
            ),
            Self::DirtyCheckout { directory, status } => write!(
                formatter,
                "miniOS checkout at {} is not clean; refusing to build from a dirty tree:\n{}",
                directory.display(),
                status.trim_end()
            ),
            Self::MissingMinictr(path) => write!(
                formatter,
                "minictr binary is missing at {}; run the workspace build phase first",
                path.display()
            ),
            Self::MissingGuestElf(path) => write!(
                formatter,
                "guest example ELF is missing at {}; run the guest example build phase first",
                path.display()
            ),
            Self::UnexpectedRun {
                case,
                expected,
                actual,
            } => write!(
                formatter,
                "E2E case {case} diverged: expected {expected}, got {actual}"
            ),
            Self::PayloadLeftover(paths) => {
                write!(formatter, "temporary payload directories remain: ")?;
                for (index, path) in paths.iter().enumerate() {
                    if index > 0 {
                        write!(formatter, ", ")?;
                    }
                    write!(formatter, "{}", path.display())?;
                }
                Ok(())
            }
            Self::QemuLeftover(pids) => {
                write!(formatter, "QEMU processes remain after the run: ")?;
                for (index, pid) in pids.iter().enumerate() {
                    if index > 0 {
                        write!(formatter, ", ")?;
                    }
                    write!(formatter, "{pid}")?;
                }
                Ok(())
            }
            Self::StressIteration {
                scenario,
                iteration,
                source,
            } => write!(
                formatter,
                "stress {scenario} iteration {iteration} failed: {source}"
            ),
            Self::Store(message) => write!(formatter, "E2E store failure: {message}"),
            Self::TimedOut { command } => {
                write!(formatter, "{command} did not finish within the E2E limit")
            }
            Self::Output(error) => write!(formatter, "could not write E2E transcript: {error}"),
        }
    }
}

impl std::error::Error for E2EError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Tool(error) => Some(error),
            Self::StressIteration { source, .. } => Some(source.as_ref()),
            Self::Output(error) => Some(error),
            _ => None,
        }
    }
}

/// release gateの最終phaseとして実QEMU E2Eを実行し、transcriptを返す。
pub fn run_e2e(workspace: &Path) -> Result<String, E2EError> {
    let mut transcript = String::new();
    let mut log = |line: &str| {
        transcript.push_str(line);
        transcript.push('\n');
    };

    log("e2e: checking QEMU and Git availability");
    let qemu_version =
        tools::require_program("qemu-system-riscv64", &["--version"]).map_err(E2EError::Tool)?;
    let qemu_version = tools::parse_qemu_version(&qemu_version).map_err(E2EError::Tool)?;
    let git_version = tools::require_program("git", &["--version"]).map_err(E2EError::Tool)?;
    tools::parse_git_version(&git_version).map_err(E2EError::Tool)?;
    log(&format!(
        "e2e: QEMU {qemu_version}; {}",
        git_version.lines().next().unwrap_or("git version unknown")
    ));

    log("e2e: ensuring the pinned miniOS kernel");
    let kernel = ensure_kernel(workspace, &mut log)?;
    log(&format!(
        "e2e: kernel ready at {} (rev {MINIOS_KERNEL_REV}, abi {MINIOS_ABI_TAG})",
        kernel.display()
    ));

    let minictr = workspace.join("target/debug/minictr");
    if !minictr.is_file() {
        return Err(E2EError::MissingMinictr(minictr));
    }

    log("e2e: happy path (`minictr run hello` returns stdout, stderr, exit 42)");
    let happy = run_happy_path(&minictr, &kernel, workspace)?;
    log(&format!(
        "e2e: happy path passed (minictr pid={}, qemu before={:?} after={:?}, elapsed={:.1}s)",
        happy.minictr_pid,
        happy.qemu_before,
        happy.qemu_after,
        happy.elapsed.as_secs_f64()
    ));

    log("e2e: timeout path (a spinning guest exits 125 without leftovers)");
    let timeout = run_timeout_path(&minictr, &kernel)?;
    log(&format!(
        "e2e: timeout path passed (minictr pid={}, qemu before={:?} after={:?}, elapsed={:.1}s)",
        timeout.minictr_pid,
        timeout.qemu_before,
        timeout.qemu_after,
        timeout.elapsed.as_secs_f64()
    ));

    log(
        "e2e: malformed-frame path (a raw kernel writes a bad header, exits 125 without leftovers)",
    );
    let malformed = run_malformed_frame_path(&minictr)?;
    log(&format!(
        "e2e: malformed-frame path passed (minictr pid={}, qemu before={:?} after={:?}, elapsed={:.1}s)",
        malformed.minictr_pid,
        malformed.qemu_before,
        malformed.qemu_after,
        malformed.elapsed.as_secs_f64()
    ));

    log("e2e: output-cap path (a chatty guest exceeds 1 MiB, exits 125 without leftovers)");
    let capped = run_output_cap_path(&minictr, &kernel)?;
    log(&format!(
        "e2e: output-cap path passed (minictr pid={}, qemu before={:?} after={:?}, elapsed={:.1}s)",
        capped.minictr_pid,
        capped.qemu_before,
        capped.qemu_after,
        capped.elapsed.as_secs_f64()
    ));

    log("e2e: resources path (`--memory 256 --cpus 2` keeps stdout, stderr, exit 42)");
    let resourced = run_resources_path(&minictr, &kernel)?;
    log(&format!(
        "e2e: resources path passed (minictr pid={}, qemu before={:?} after={:?}, elapsed={:.1}s)",
        resourced.minictr_pid,
        resourced.qemu_before,
        resourced.qemu_after,
        resourced.elapsed.as_secs_f64()
    ));

    log("e2e: interrupt path (SIGINT to a running group exits 125 without leftovers)");
    let interrupted = run_interrupt_path(&minictr, &kernel)?;
    log(&format!(
        "e2e: interrupt path passed (minictr pid={}, qemu before={:?} after={:?}, elapsed={:.1}s)",
        interrupted.minictr_pid,
        interrupted.qemu_before,
        interrupted.qemu_after,
        interrupted.elapsed.as_secs_f64()
    ));

    log("e2e: sigterm path (SIGTERM to the minictr pid exits 125 without leftovers)");
    let terminated = run_sigterm_path(&minictr, &kernel)?;
    log(&format!(
        "e2e: sigterm path passed (minictr pid={}, qemu before={:?} after={:?}, elapsed={:.1}s)",
        terminated.minictr_pid,
        terminated.qemu_before,
        terminated.qemu_after,
        terminated.elapsed.as_secs_f64()
    ));

    log("e2e: crash path (a SIGKILLed minictr leaves its instance; ps reports stale)");
    let crashed = run_orphan_path(&minictr, &kernel)?;
    log(&format!(
        "e2e: crash path passed (minictr pid={}, qemu before={:?} after={:?}, elapsed={:.1}s)",
        crashed.minictr_pid,
        crashed.qemu_before,
        crashed.qemu_after,
        crashed.elapsed.as_secs_f64()
    ));

    log("e2e: detached path (run -d prints an id, stop collects it)");
    let detached = run_detached_path(&minictr, &kernel)?;
    log(&format!(
        "e2e: detached path passed (minictr pid={}, qemu before={:?} after={:?}, elapsed={:.1}s)",
        detached.minictr_pid,
        detached.qemu_before,
        detached.qemu_after,
        detached.elapsed.as_secs_f64()
    ));

    log("e2e: stdin echo path (piped bytes echo back, EOF exits 42)");
    let echoed = run_stdin_echo_path(&minictr, &kernel, workspace)?;
    log(&format!(
        "e2e: stdin echo path passed (minictr pid={}, qemu before={:?} after={:?}, elapsed={:.1}s)",
        echoed.minictr_pid,
        echoed.qemu_before,
        echoed.qemu_after,
        echoed.elapsed.as_secs_f64()
    ));

    log("e2e: stress section (bounded loops over a shared store)");
    let stress = run_stress(&minictr, &kernel, workspace)?;
    log(&format!("e2e: stress passed ({stress})"));

    Ok(transcript)
}

/// pin留めrevisionのminiOS kernelをbuildし、そのbinary pathを返す。
///
/// cache checkoutはworkspaceの`target/e2e/minios`に置き、存在すればpin留め
/// revisionへ更新して再利用する。fetchはrevisionが欠けているときだけ行い、
/// checkout後はrevisionとcleanさを毎回検証する。`MINICTR_E2E_MINIOS_DIR`が
/// 絶対pathで与えられた場合はそのcheckoutを読み取り専用として扱い、fetchや
/// checkoutやin-place buildで書き換えず、target directoryだけworkspace側へ
/// 隔離してbuildする。
fn ensure_kernel(workspace: &Path, log: &mut dyn FnMut(&str)) -> Result<PathBuf, E2EError> {
    match std::env::var_os("MINICTR_E2E_MINIOS_DIR") {
        Some(directory) => {
            let directory = PathBuf::from(directory);
            verify_checkout_rev(&directory, MINIOS_KERNEL_REV)?;
            verify_clean_checkout(&directory)?;
            let target_dir = workspace.join("target/e2e/minios-override-target");
            std::fs::create_dir_all(&target_dir)
                .map_err(|error| E2EError::Store(error.to_string()))?;
            build_kernel(&directory, Some(&target_dir))
        }
        None => {
            let directory = workspace.join("target/e2e/minios");
            prepare_checkout(&directory, log, MINIOS_REPO_URL, MINIOS_KERNEL_REV)?;
            build_kernel(&directory, None)
        }
    }
}

/// pin留めkernelのcheckoutを用意し、初回失敗時はdirectoryごと破棄して
/// fresh cloneから一度だけ再試行する。CI cacheから復元した壊れたcheckout
/// が残ると、再試行なしでは同じ失敗を繰り返してgateが赤信号のまま固まる。
/// 再試行も失敗したときは再試行のerrorを返す。初回errorの要約はtranscript
/// に残す。
fn prepare_checkout(
    directory: &Path,
    log: &mut dyn FnMut(&str),
    repo_url: &str,
    rev: &str,
) -> Result<(), E2EError> {
    match prepare_checkout_once(directory, repo_url, rev) {
        Ok(()) => Ok(()),
        Err(first) => {
            let summary = first.to_string();
            let summary = summary.lines().next().unwrap_or("unknown failure");
            log(&format!(
                "e2e: checkout unusable ({summary}); discarding and re-cloning once"
            ));
            let _ = std::fs::remove_dir_all(directory);
            prepare_checkout_once(directory, repo_url, rev)
        }
    }
}

fn prepare_checkout_once(directory: &Path, repo_url: &str, rev: &str) -> Result<(), E2EError> {
    if !directory.join(".git").exists() {
        if let Some(parent) = directory.parent() {
            std::fs::create_dir_all(parent).map_err(|error| E2EError::Store(error.to_string()))?;
        }
        run_checked(
            "git",
            &[
                OsString::from("clone"),
                OsString::from(repo_url),
                directory.as_os_str().to_owned(),
            ],
            None,
            &[],
            KERNEL_BUILD_TIMEOUT,
        )?;
    }
    if !rev_present(directory, rev) {
        run_checked(
            "git",
            &[
                OsString::from("-C"),
                directory.as_os_str().to_owned(),
                OsString::from("fetch"),
                OsString::from("origin"),
                OsString::from(rev),
            ],
            None,
            &[],
            KERNEL_BUILD_TIMEOUT,
        )?;
    }
    run_checked(
        "git",
        &[
            OsString::from("-C"),
            directory.as_os_str().to_owned(),
            OsString::from("checkout"),
            OsString::from(rev),
        ],
        None,
        &[],
        CHECKOUT_TIMEOUT,
    )?;
    verify_checkout_rev(directory, rev)?;
    verify_clean_checkout(directory)
}

fn rev_present(directory: &Path, rev: &str) -> bool {
    run_checked(
        "git",
        &[
            OsString::from("-C"),
            directory.as_os_str().to_owned(),
            OsString::from("cat-file"),
            OsString::from("-e"),
            OsString::from(rev),
        ],
        None,
        &[],
        COMMAND_TIMEOUT,
    )
    .is_ok()
}

fn verify_checkout_rev(directory: &Path, expected_rev: &str) -> Result<(), E2EError> {
    let stdout = run_checked(
        "git",
        &[
            OsString::from("-C"),
            directory.as_os_str().to_owned(),
            OsString::from("rev-parse"),
            OsString::from("HEAD"),
        ],
        None,
        &[],
        COMMAND_TIMEOUT,
    )?;
    let actual = String::from_utf8_lossy(&stdout).trim().to_owned();
    if actual != expected_rev {
        return Err(E2EError::UnexpectedKernelRev {
            expected: expected_rev.to_owned(),
            actual,
        });
    }
    Ok(())
}

/// checkoutに未commitの変更や未trackのfileが残っていないことを確認する。
/// build対象がpin留めsourceと同一であることの証拠になる。
fn verify_clean_checkout(directory: &Path) -> Result<(), E2EError> {
    let stdout = run_checked(
        "git",
        &[
            OsString::from("-C"),
            directory.as_os_str().to_owned(),
            OsString::from("status"),
            OsString::from("--porcelain"),
        ],
        None,
        &[],
        COMMAND_TIMEOUT,
    )?;
    let status = String::from_utf8_lossy(&stdout).into_owned();
    if status.trim().is_empty() {
        Ok(())
    } else {
        Err(E2EError::DirtyCheckout {
            directory: directory.to_path_buf(),
            status,
        })
    }
}

fn build_kernel(checkout: &Path, target_dir: Option<&Path>) -> Result<PathBuf, E2EError> {
    let args: Vec<OsString> = [
        "build",
        "-p",
        "minios-kernel",
        "--bin",
        "minios-kernel",
        "--target",
        RISCV_TARGET,
        "--locked",
    ]
    .iter()
    .map(OsString::from)
    .collect();
    let target_string;
    let mut extra_env: Vec<(&str, &str)> = Vec::new();
    if let Some(directory) = target_dir {
        target_string = directory.display().to_string();
        extra_env.push(("CARGO_TARGET_DIR", target_string.as_str()));
    }
    run_checked(
        "cargo",
        &args,
        Some(checkout),
        &extra_env,
        KERNEL_BUILD_TIMEOUT,
    )?;
    let default_target = checkout.join("target");
    let root = target_dir.unwrap_or(&default_target);
    Ok(root.join(format!("{RISCV_TARGET}/debug/minios-kernel")))
}

/// 決定的なhello MiniBundleを一時storeへimportして`hello` tagを作る。
fn prepare_store(elf: &[u8]) -> Result<TempStore, E2EError> {
    TempStore::with_bundle(&hello_bundle_bytes(elf)?)
}

/// 決定的なhello MiniBundle bytesを作る。
fn hello_bundle_bytes(elf: &[u8]) -> Result<Vec<u8>, E2EError> {
    bundle_bytes(E2E_IMAGE, elf)
}

/// manifest名`name`の決定的なMiniBundle bytesを作る。
fn bundle_bytes(name: &str, elf: &[u8]) -> Result<Vec<u8>, E2EError> {
    minicontainer_bundle::build(minicontainer_bundle::ImageSpec {
        name,
        args: &[],
        elf,
    })
    .map_err(|error| E2EError::Store(error.to_string()))
}

/// 一つのE2E caseの観測結果。PIDとQEMU snapshotがreapの証拠になる。
struct CaseReport {
    minictr_pid: u32,
    qemu_before: Vec<u32>,
    qemu_after: Vec<u32>,
    elapsed: Duration,
    status: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// 一時storeとQEMU snapshotを用意して`minictr run`を一回実行し、cleanupまで
/// 検証する。`minictr`の終了を待ってから出力を読むのではなく、reader thread
/// が両pipeを並行drainするため、大出力でもdeadlockしない。
fn run_case(
    minictr: &Path,
    elf: &[u8],
    kernel: &Path,
    timeout_ms: &str,
) -> Result<CaseReport, E2EError> {
    run_case_in_store(minictr, prepare_store(elf)?, kernel, timeout_ms)
}

/// 用意済みの一時storeで`minictr run`を一回実行し、cleanupまで検証する。
fn run_case_in_store(
    minictr: &Path,
    store: TempStore,
    kernel: &Path,
    timeout_ms: &str,
) -> Result<CaseReport, E2EError> {
    let store_path = store.path.clone();
    let qemu_before = qemu_pids()?;
    let payload_before = payload_temp_leftovers();
    let started = Instant::now();
    let outcome = run_minictr(minictr, &store.path, kernel, timeout_ms, E2E_IMAGE);
    let elapsed = started.elapsed();
    let qemu_after = check_case_leftovers(&qemu_before, &payload_before, store, &store_path)?;
    let completed = outcome?;
    Ok(CaseReport {
        minictr_pid: completed.pid,
        qemu_before,
        qemu_after,
        elapsed,
        status: completed.status.code(),
        stdout: completed.stdout,
        stderr: completed.stderr,
    })
}

/// `minictr`の成否にかかわらず、QEMU、payload、一時storeの全検査を実行し、
/// 終了後のsnapshotを返す。どれか一つでも`?`で即返すと、後続の残留物を
/// 見逃す。QEMU snapshotはkillせず観測だけ行い、新規残留があれば失敗に
/// する。報告は固定順 (QEMU残留、snapshot失敗、payload残留、store残留) の
/// 最初の失敗に決める。
fn check_case_leftovers(
    qemu_before: &[u32],
    payload_before: &[PathBuf],
    store: TempStore,
    store_path: &Path,
) -> Result<Vec<u32>, E2EError> {
    let mut failure: Option<E2EError> = None;
    let mut check = |result: Result<(), E2EError>| {
        if failure.is_none() {
            failure = result.err();
        }
    };
    let mut qemu_after = Vec::new();
    match qemu_pids() {
        Ok(pids) => {
            check(check_no_new_qemu(qemu_before, &pids));
            qemu_after = pids;
        }
        Err(error) => check(Err(error)),
    }
    check(check_no_leftovers(payload_before));
    drop(store);
    if store_path.exists() {
        check(Err(E2EError::Store(format!(
            "temporary store was not removed: {}",
            store_path.display()
        ))));
    }
    if let Some(error) = failure {
        return Err(error);
    }
    Ok(qemu_after)
}

fn run_happy_path(minictr: &Path, kernel: &Path, workspace: &Path) -> Result<CaseReport, E2EError> {
    let elf = workspace.join(GUEST_HELLO_ELF);
    let elf_len = std::fs::metadata(&elf)
        .map(|metadata| metadata.len())
        .map_err(|_| E2EError::MissingGuestElf(elf.clone()))?;
    let store = TempStore::empty();
    let digest = run_image_build(minictr, &store.path, &elf)?;
    run_image_inspect(minictr, &store.path, &digest, elf_len)?;
    let report = run_case_in_store(minictr, store, kernel, HAPPY_PATH_TIMEOUT_MS)?;

    if report.status != Some(E2E_EXIT_CODE) {
        return Err(E2EError::UnexpectedRun {
            case: "happy-path exit code",
            expected: format!("exit {}", E2E_EXIT_CODE),
            actual: format!("status {:?}", report.status),
        });
    }
    if report.stdout != GUEST_HELLO_STDOUT {
        return Err(E2EError::UnexpectedRun {
            case: "happy-path stdout",
            expected: format!("{:?}", GUEST_HELLO_STDOUT),
            actual: format!("{:?}", report.stdout),
        });
    }
    if report.stderr != GUEST_HELLO_STDERR {
        return Err(E2EError::UnexpectedRun {
            case: "happy-path stderr",
            expected: format!("{:?}", GUEST_HELLO_STDERR),
            actual: format!("{:?}", report.stderr),
        });
    }
    Ok(report)
}

/// 公開`image build`で同梱guestを一時storeへ登録し、digestを返す。
fn run_image_build(minictr: &Path, store: &Path, elf: &Path) -> Result<String, E2EError> {
    let args = minictr_build_args(store, E2E_IMAGE, elf);
    let stdout = run_checked(minictr, &args, None, &[], COMMAND_TIMEOUT)?;
    parse_build_output(&stdout, E2E_IMAGE)
}

/// 公開`image inspect`の安定した5行をbuild結果と突き合わせる。
fn run_image_inspect(
    minictr: &Path,
    store: &Path,
    digest: &str,
    elf_len: u64,
) -> Result<(), E2EError> {
    let args = minictr_inspect_args(store, E2E_IMAGE);
    let stdout = run_checked(minictr, &args, None, &[], COMMAND_TIMEOUT)?;
    check_inspect_output(&stdout, E2E_IMAGE, digest, elf_len)
}

fn minictr_build_args(store: &Path, image: &str, elf: &Path) -> Vec<OsString> {
    vec![
        OsString::from("image"),
        OsString::from("build"),
        OsString::from("--store"),
        store.as_os_str().to_owned(),
        OsString::from(image),
        elf.as_os_str().to_owned(),
    ]
}

fn minictr_inspect_args(store: &Path, image: &str) -> Vec<OsString> {
    vec![
        OsString::from("image"),
        OsString::from("inspect"),
        OsString::from("--store"),
        store.as_os_str().to_owned(),
        OsString::from(image),
    ]
}

/// `image build`の成功行から小文字16進64桁のdigestを抜き出す。
fn parse_build_output(output: &[u8], image: &str) -> Result<String, E2EError> {
    let unexpected = |actual: String| E2EError::UnexpectedRun {
        case: "happy-path image build output",
        expected: format!("{image} sha256:<64 lowercase hex digits>"),
        actual,
    };
    let text = String::from_utf8_lossy(output);
    let line = text
        .strip_suffix('\n')
        .ok_or_else(|| unexpected(format!("{text:?}")))?;
    let (tag, digest) = line
        .split_once(' ')
        .ok_or_else(|| unexpected(format!("{line:?}")))?;
    let digest = digest
        .strip_prefix("sha256:")
        .ok_or_else(|| unexpected(format!("{line:?}")))?;
    if tag != image
        || digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(unexpected(format!("{line:?}")));
    }
    Ok(digest.to_owned())
}

/// `image inspect`の5行がbuild結果と一致することを確認する。
fn check_inspect_output(
    output: &[u8],
    image: &str,
    digest: &str,
    elf_len: u64,
) -> Result<(), E2EError> {
    let expected = format!(
        "tag: {image}\nname: {image}\ndigest: sha256:{digest}\nargs: 0\nelf-bytes: {elf_len}\n"
    );
    if output != expected.as_bytes() {
        return Err(E2EError::UnexpectedRun {
            case: "happy-path image inspect output",
            expected: format!("{expected:?}"),
            actual: format!("{:?}", String::from_utf8_lossy(output)),
        });
    }
    Ok(())
}

fn run_timeout_path(minictr: &Path, kernel: &Path) -> Result<CaseReport, E2EError> {
    let report = run_case(minictr, &spin_elf_bytes(), kernel, SPIN_TIMEOUT_MS)?;

    if report.status != Some(125) {
        return Err(E2EError::UnexpectedRun {
            case: "timeout exit code",
            expected: "exit 125".to_owned(),
            actual: format!("status {:?}", report.status),
        });
    }
    if report.stdout == E2E_STDOUT {
        return Err(E2EError::UnexpectedRun {
            case: "timeout stdout",
            expected: "absence of the happy-path marker".to_owned(),
            actual: format!("{:?}", report.stdout),
        });
    }
    if !String::from_utf8_lossy(&report.stderr).contains("deadline elapsed") {
        return Err(E2EError::UnexpectedRun {
            case: "timeout diagnostic",
            expected: "a deadline-elapsed failure".to_owned(),
            actual: String::from_utf8_lossy(&report.stderr).into_owned(),
        });
    }
    Ok(report)
}

fn run_malformed_frame_path(minictr: &Path) -> Result<CaseReport, E2EError> {
    // miniOS kernelではなくfixture kernelを起動する。storeとpayloadは用意
    // するが (runtimeが要求するため)、fixtureはそれらを無視してUARTへ不正
    // headerだけを書く。
    let dir = TempDir::create("minictr-e2e-malformed-kernel-")?;
    let kernel = dir.path().join("kernel.elf");
    std::fs::write(&kernel, malformed_frame_kernel_bytes())
        .map_err(|error| E2EError::Store(error.to_string()))?;
    let report = run_case(minictr, &hello_elf_bytes(), &kernel, HAPPY_PATH_TIMEOUT_MS)?;

    if report.status != Some(125) {
        return Err(E2EError::UnexpectedRun {
            case: "malformed-frame exit code",
            expected: "exit 125".to_owned(),
            actual: format!("status {:?}", report.status),
        });
    }
    if report.stdout == E2E_STDOUT {
        return Err(E2EError::UnexpectedRun {
            case: "malformed-frame stdout",
            expected: "absence of the happy-path marker".to_owned(),
            actual: format!("{:?}", report.stdout),
        });
    }
    if !String::from_utf8_lossy(&report.stderr).contains("invalid UART control frame") {
        return Err(E2EError::UnexpectedRun {
            case: "malformed-frame diagnostic",
            expected: "an invalid-UART-frame protocol failure".to_owned(),
            actual: String::from_utf8_lossy(&report.stderr).into_owned(),
        });
    }
    Ok(report)
}

fn run_output_cap_path(minictr: &Path, kernel: &Path) -> Result<CaseReport, E2EError> {
    let report = run_case(minictr, &chatty_elf_bytes(), kernel, OUTPUT_CAP_TIMEOUT_MS)?;

    if report.status != Some(125) {
        return Err(E2EError::UnexpectedRun {
            case: "output-cap exit code",
            expected: "exit 125".to_owned(),
            actual: format!("status {:?}", report.status),
        });
    }
    // The cap counts displayed bytes: up to 1 MiB streams to the consumer
    // before the first byte past the cap refuses the run.
    if report.stdout.is_empty() || report.stdout.len() > 1024 * 1024 {
        return Err(E2EError::UnexpectedRun {
            case: "output-cap stdout",
            expected: "1 to 1048576 streamed bytes before the refusal".to_owned(),
            actual: format!("{} forwarded bytes", report.stdout.len()),
        });
    }
    if !String::from_utf8_lossy(&report.stderr).contains("guest output exceeds 1 MiB") {
        return Err(E2EError::UnexpectedRun {
            case: "output-cap diagnostic",
            expected: "an output-cap host failure".to_owned(),
            actual: String::from_utf8_lossy(&report.stderr).into_owned(),
        });
    }
    Ok(report)
}

fn run_resources_path(minictr: &Path, kernel: &Path) -> Result<CaseReport, E2EError> {
    let extra = [
        OsString::from("--memory"),
        OsString::from("256"),
        OsString::from("--cpus"),
        OsString::from("2"),
    ];
    let store = prepare_store(&hello_elf_bytes())?;
    let store_path = store.path.clone();
    let qemu_before = qemu_pids()?;
    let payload_before = payload_temp_leftovers();
    let started = Instant::now();
    let outcome = run_minictr_with_extra_args(
        minictr,
        &store.path,
        kernel,
        HAPPY_PATH_TIMEOUT_MS,
        E2E_IMAGE,
        &extra,
    );
    let elapsed = started.elapsed();
    let qemu_after = check_case_leftovers(&qemu_before, &payload_before, store, &store_path)?;
    let completed = outcome?;
    let report = CaseReport {
        minictr_pid: completed.pid,
        qemu_before,
        qemu_after,
        elapsed,
        status: completed.status.code(),
        stdout: completed.stdout,
        stderr: completed.stderr,
    };

    if report.status != Some(E2E_EXIT_CODE) {
        return Err(E2EError::UnexpectedRun {
            case: "resources exit code",
            expected: format!("exit {}", E2E_EXIT_CODE),
            actual: format!("status {:?}", report.status),
        });
    }
    if report.stdout != E2E_STDOUT {
        return Err(E2EError::UnexpectedRun {
            case: "resources stdout",
            expected: format!("{:?}", E2E_STDOUT),
            actual: format!("{:?}", report.stdout),
        });
    }
    if report.stderr != E2E_STDERR {
        return Err(E2EError::UnexpectedRun {
            case: "resources stderr",
            expected: format!("{:?}", E2E_STDERR),
            actual: format!("{:?}", report.stderr),
        });
    }
    Ok(report)
}

/// `minictr`のQEMUが現れるまで待ち、起動中のrunへsignalできる状態を
/// 確認する。待ち時間内に現れなければ起動したrunを残さずtimeout error、
/// 途中で`minictr`が終われば空振りとしてerrorにする。
fn wait_for_qemu_boot(
    mut spawned: SpawnedChild,
    qemu_before: &[u32],
) -> Result<SpawnedChild, E2EError> {
    let deadline = Instant::now() + INTERRUPT_BOOT_TIMEOUT;
    loop {
        match spawned.child.try_wait() {
            Ok(Some(_)) => {
                let error = E2EError::UnexpectedRun {
                    case: "interrupt pre-signal liveness",
                    expected: "a running minictr".to_owned(),
                    actual: "minictr exited before the interrupt".to_owned(),
                };
                terminate_spawned(spawned);
                return Err(error);
            }
            Ok(None) => {}
            Err(error) => {
                let failure = E2EError::Command {
                    command: spawned.command_line.clone(),
                    message: error.to_string(),
                };
                terminate_spawned(spawned);
                return Err(failure);
            }
        }
        let current_qemu = match qemu_pids() {
            Ok(pids) => pids,
            Err(error) => {
                terminate_spawned(spawned);
                return Err(error);
            }
        };
        let booted = current_qemu.iter().any(|pid| !qemu_before.contains(pid));
        if booted {
            return Ok(spawned);
        }
        if Instant::now() >= deadline {
            let error = E2EError::TimedOut {
                command: format!("{} (waiting for QEMU boot)", spawned.command_line),
            };
            terminate_spawned(spawned);
            return Err(error);
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// SIGINT直前に`minictr`がまだ走っていることを確認する。終わっていれば
/// groupを掃除して空振りとしてerrorにする。
fn assert_minictr_running(mut spawned: SpawnedChild) -> Result<SpawnedChild, E2EError> {
    match spawned.child.try_wait() {
        Ok(None) => Ok(spawned),
        Ok(Some(_)) => {
            let error = E2EError::UnexpectedRun {
                case: "interrupt pre-signal liveness",
                expected: "a running minictr".to_owned(),
                actual: "minictr exited before the interrupt".to_owned(),
            };
            terminate_spawned(spawned);
            Err(error)
        }
        Err(error) => {
            let failure = E2EError::Command {
                command: spawned.command_line.clone(),
                message: error.to_string(),
            };
            terminate_spawned(spawned);
            Err(failure)
        }
    }
}

/// SIGINTを`minictr`のprocess group宛に送る経路。QEMUは`minictr`と別の
/// groupにいるためgroup宛signalは`minictr`だけへ届き、QEMUへの到達は
/// host側の転送が担う。
fn run_interrupt_path(minictr: &Path, kernel: &Path) -> Result<CaseReport, E2EError> {
    run_signal_path(minictr, kernel, "SIGINT", |pid| {
        signal_process_group(pid, libc::SIGINT)
    })
}

/// SIGTERMを`minictr`のPIDだけへ送る経路。group宛ではないため、QEMUが
/// 止まるのはhostが転送した場合だけである。
fn run_sigterm_path(minictr: &Path, kernel: &Path) -> Result<CaseReport, E2EError> {
    run_signal_path(minictr, kernel, "SIGTERM", |pid| {
        signal_process(pid, libc::SIGTERM)
    })
}

/// 回転中のrunへ`send`でsignalを送り、125終了・signalを名指す診断・
/// QEMUと一時領域の非残留を検査する。
fn run_signal_path(
    minictr: &Path,
    kernel: &Path,
    signal_name: &'static str,
    send: impl FnOnce(u32) -> io::Result<()>,
) -> Result<CaseReport, E2EError> {
    let store = prepare_store(&spin_elf_bytes())?;
    let store_path = store.path.clone();
    let qemu_before = qemu_pids()?;
    let payload_before = payload_temp_leftovers();
    let started = Instant::now();
    let spawned = spawn_minictr(
        minictr,
        &store.path,
        kernel,
        HAPPY_PATH_TIMEOUT_MS,
        E2E_IMAGE,
    )?;
    let outcome = (|| {
        let spawned = wait_for_qemu_boot(spawned, &qemu_before)?;
        thread::sleep(INTERRUPT_SETTLE);
        let spawned = assert_minictr_running(spawned)?;
        if let Err(error) = send(spawned.pid) {
            let failure = E2EError::Command {
                command: format!("send {signal_name} to minictr {}", spawned.pid),
                message: error.to_string(),
            };
            terminate_spawned(spawned);
            return Err(failure);
        }
        wait_for_exit(spawned, INTERRUPT_EXIT_TIMEOUT)
    })();
    let elapsed = started.elapsed();
    let qemu_after = check_case_leftovers(&qemu_before, &payload_before, store, &store_path)?;
    let completed = outcome?;
    let report = CaseReport {
        minictr_pid: completed.pid,
        qemu_before,
        qemu_after,
        elapsed,
        status: completed.status.code(),
        stdout: completed.stdout,
        stderr: completed.stderr,
    };
    if report.status != Some(125) {
        return Err(E2EError::UnexpectedRun {
            case: "signal exit code",
            expected: "exit 125".to_owned(),
            actual: format!(
                "status {:?} after {signal_name} (stderr: {})",
                report.status,
                String::from_utf8_lossy(&report.stderr)
            ),
        });
    }
    let expected = format!("run interrupted by {signal_name}");
    if !String::from_utf8_lossy(&report.stderr).contains(&expected) {
        return Err(E2EError::UnexpectedRun {
            case: "signal diagnostic",
            expected: format!("an {signal_name} interrupt diagnostic on stderr"),
            actual: format!("stderr: {}", String::from_utf8_lossy(&report.stderr)),
        });
    }
    Ok(report)
}

/// host crashを真似る経路。`minictr`をSIGKILLしてcleanupを回避し、孤児に
/// なったQEMUを殺したあと`minictr ps`がそのinstanceをstaleとして表示する
/// ことを確認する。crashが残したpayload directoryはharnessが除去する。
fn run_orphan_path(minictr: &Path, kernel: &Path) -> Result<CaseReport, E2EError> {
    let store = prepare_store(&spin_elf_bytes())?;
    let store_path = store.path.clone();
    let qemu_before = qemu_pids()?;
    let payload_before = payload_temp_leftovers();
    let started = Instant::now();
    let spawned = spawn_minictr(
        minictr,
        &store.path,
        kernel,
        HAPPY_PATH_TIMEOUT_MS,
        E2E_IMAGE,
    )?;
    // QEMU pidを外へ返し、失敗経路でも孤児を放置しないようにする。
    let mut qemu_pid = None;
    let outcome = (|| {
        let mut spawned = wait_for_qemu_boot(spawned, &qemu_before)?;
        qemu_pid = match qemu_pids() {
            Ok(pids) => pids.into_iter().find(|pid| !qemu_before.contains(pid)),
            Err(error) => {
                terminate_spawned(spawned);
                return Err(error);
            }
        };
        let Some(qemu_pid) = qemu_pid else {
            terminate_spawned(spawned);
            return Err(E2EError::UnexpectedRun {
                case: "crash-path QEMU discovery",
                expected: "a freshly booted QEMU pid".to_owned(),
                actual: "no new QEMU process".to_owned(),
            });
        };
        // run中のinstanceはliveとして見えなければならない。登録はQEMU起動
        // 直後に行われるため、boot観測からの僅かな遅れを許す。
        if let Err(error) = wait_for_ps_row(minictr, &store_path, E2E_IMAGE, qemu_pid, "live") {
            // 行が出なかった理由をrun側の状態から絞る。minictrが既に終わって
            // いれば登録失敗、走っていれば表示かidentity照合の問題である。
            let _ = signal_process_group(spawned.pid, libc::SIGKILL);
            let _ = spawned.child.kill();
            let exited = spawned.child.wait().ok();
            let stdout = join_reader(spawned.stdout_reader, &spawned.command_line)
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .unwrap_or_default();
            let stderr = join_reader(spawned.stderr_reader, &spawned.command_line)
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .unwrap_or_default();
            let _ = signal_process(qemu_pid, libc::SIGKILL);
            return Err(E2EError::UnexpectedRun {
                case: "crash-path ps live row",
                expected: format!("a live i-{qemu_pid} row"),
                actual: format!(
                    "{error}; minictr exit {exited:?} (stdout: {stdout}, stderr: {stderr})"
                ),
            });
        }
        // hostを即死させてcleanupを回避する。QEMUは別groupで生き続ける。
        if let Err(error) = signal_process(spawned.pid, libc::SIGKILL) {
            let failure = E2EError::Command {
                command: format!("send SIGKILL to minictr {}", spawned.pid),
                message: error.to_string(),
            };
            terminate_spawned(spawned);
            let _ = signal_process(qemu_pid, libc::SIGKILL);
            return Err(failure);
        }
        let status = match spawned.child.wait() {
            Ok(status) => status,
            Err(error) => {
                let failure = E2EError::Command {
                    command: spawned.command_line.clone(),
                    message: error.to_string(),
                };
                terminate_spawned(spawned);
                let _ = signal_process(qemu_pid, libc::SIGKILL);
                return Err(failure);
            }
        };
        let stdout = join_reader(spawned.stdout_reader, &spawned.command_line);
        let stderr = join_reader(spawned.stderr_reader, &spawned.command_line);
        // 孤児になったQEMUを殺し、pid死亡をpsのstale表示へ繋ぐ。
        let _ = signal_process(qemu_pid, libc::SIGKILL);
        wait_for_ps_row(minictr, &store_path, E2E_IMAGE, qemu_pid, "stale")?;
        Ok(CompletedProcess {
            pid: spawned.pid,
            status,
            stdout: stdout?,
            stderr: stderr?,
        })
    })();
    let elapsed = started.elapsed();
    if let Some(pid) = qemu_pid {
        let _ = signal_process(pid, libc::SIGKILL);
    }
    // crashが残したpayload directoryはharnessが除去してから残留検査へ。
    remove_stray_payloads(&payload_before);
    let qemu_after = check_case_leftovers(&qemu_before, &payload_before, store, &store_path)?;
    let completed = outcome?;
    Ok(CaseReport {
        minictr_pid: completed.pid,
        qemu_before,
        qemu_after,
        elapsed,
        status: completed.status.code(),
        stdout: completed.stdout,
        stderr: completed.stderr,
    })
}

/// detached runの契約: `run --detach`はguest Readyを確認してからinstance
/// idだけを出力して終了し、QEMUはhostの後も生き続ける。`stop`はそのidを
/// echoし、QEMUとpayloadとstate fileを回収する。
fn run_detached_path(minictr: &Path, kernel: &Path) -> Result<CaseReport, E2EError> {
    let store = prepare_store(&spin_elf_bytes())?;
    let store_path = store.path.clone();
    let qemu_before = qemu_pids()?;
    let payload_before = payload_temp_leftovers();
    let started = Instant::now();
    let mut qemu_pid = None;
    let outcome = (|| {
        let completed = run_minictr_with_extra_args(
            minictr,
            &store.path,
            kernel,
            HAPPY_PATH_TIMEOUT_MS,
            E2E_IMAGE,
            &[OsString::from("--detach")],
        )?;
        if !completed.status.success() {
            return Err(E2EError::UnexpectedRun {
                case: "detached run exit",
                expected: "exit 0 with an instance id".to_owned(),
                actual: format!(
                    "status {:?} (stderr: {})",
                    completed.status.code(),
                    String::from_utf8_lossy(&completed.stderr)
                ),
            });
        }
        // stdoutはid一行だけである。`i-<pid>`の形と、psがliveを示すことを
        // 検査する。
        let id = String::from_utf8_lossy(&completed.stdout).trim().to_owned();
        let pid = id
            .strip_prefix("i-")
            .and_then(|digits| digits.parse::<u32>().ok())
            .filter(|pid| id == format!("i-{pid}"));
        let Some(pid) = pid else {
            return Err(E2EError::UnexpectedRun {
                case: "detached run output",
                expected: "a bare i-<pid> instance id".to_owned(),
                actual: format!(
                    "stdout: {:?}, stderr: {}",
                    String::from_utf8_lossy(&completed.stdout),
                    String::from_utf8_lossy(&completed.stderr)
                ),
            });
        };
        qemu_pid = Some(pid);
        wait_for_ps_row(minictr, &store_path, E2E_IMAGE, pid, "live")?;
        // `stop`はidをechoして0で終わる。QEMUはorphanとしてlaunchdに
        // reapされるため、stop内の消失待ちは実経路どおりに動く。
        let stop_args = [
            OsString::from("stop"),
            OsString::from("--store"),
            store_path.as_os_str().to_owned(),
            OsString::from(&id),
        ];
        let stopped = run_with_timeout(minictr, &stop_args, None, &[], COMMAND_TIMEOUT)?;
        if !stopped.status.success() || String::from_utf8_lossy(&stopped.stdout).trim() != id {
            return Err(E2EError::UnexpectedRun {
                case: "detached stop",
                expected: format!("exit 0 echoing {id}"),
                actual: format!(
                    "status {:?} (stdout: {:?}, stderr: {})",
                    stopped.status.code(),
                    String::from_utf8_lossy(&stopped.stdout),
                    String::from_utf8_lossy(&stopped.stderr)
                ),
            });
        }
        // state fileが消えた後はps行も消える。QEMU自体はstopがESRCHまで
        // 待った時点で既に死んでいる。
        let ps = run_with_timeout(
            minictr,
            &[
                OsString::from("ps"),
                OsString::from("--store"),
                store_path.as_os_str().to_owned(),
            ],
            None,
            &[],
            COMMAND_TIMEOUT,
        )?;
        if String::from_utf8_lossy(&ps.stdout)
            .lines()
            .any(|line| line.starts_with(&id))
        {
            return Err(E2EError::UnexpectedRun {
                case: "detached post-stop ps",
                expected: "no instance rows".to_owned(),
                actual: format!("ps output: {}", String::from_utf8_lossy(&ps.stdout)),
            });
        }
        Ok(completed)
    })();
    let elapsed = started.elapsed();
    if let Some(pid) = qemu_pid {
        let _ = signal_process(pid, libc::SIGKILL);
    }
    remove_stray_payloads(&payload_before);
    let qemu_after = check_case_leftovers(&qemu_before, &payload_before, store, &store_path)?;
    let completed = outcome?;
    Ok(CaseReport {
        minictr_pid: completed.pid,
        qemu_before,
        qemu_after,
        elapsed,
        status: completed.status.code(),
        stdout: completed.stdout,
        stderr: completed.stderr,
    })
}

/// stdin転送経路。`guest-echo`へpipeしたbinary入力がそのままstdoutへ戻り、
/// EOF後にguestが42で終わることを検証する。入力はkernel stagingの4 KiBを
/// 跨ぐ長さにして、複数`STDIN` frameへの分割を兼ねて検査する。二回目の
/// 実行では空入力だけを送り、即座のEOFでも同じ終了codeになることを見る。
fn run_stdin_echo_path(
    minictr: &Path,
    kernel: &Path,
    workspace: &Path,
) -> Result<CaseReport, E2EError> {
    let elf = workspace.join(GUEST_ECHO_ELF);
    if !elf.is_file() {
        return Err(E2EError::MissingGuestElf(elf));
    }
    let store = TempStore::empty();
    let args = minictr_build_args(&store.path, E2E_ECHO_IMAGE, &elf);
    run_checked(minictr, &args, None, &[], COMMAND_TIMEOUT)?;

    // 0x00-0xFAを一周するbinary byte列。UTF-8としては不正な並びを含み、
    // テキスト扱いされないことも同時に確認する。
    let mut input = vec![0_u8; 10 * 1024 + 7];
    for (index, byte) in input.iter_mut().enumerate() {
        *byte = (index % 251) as u8;
    }
    let report = run_case_stdin(minictr, store, kernel, HAPPY_PATH_TIMEOUT_MS, &input)?;

    if report.status != Some(E2E_EXIT_CODE) {
        return Err(E2EError::UnexpectedRun {
            case: "stdin echo exit code",
            expected: format!("exit {}", E2E_EXIT_CODE),
            actual: format!(
                "status {:?} (stderr: {})",
                report.status,
                String::from_utf8_lossy(&report.stderr)
            ),
        });
    }
    if report.stdout != input {
        return Err(E2EError::UnexpectedRun {
            case: "stdin echo stdout",
            expected: format!("{} echoed bytes", input.len()),
            actual: format!(
                "{} bytes {:?}",
                report.stdout.len(),
                &report.stdout[..report.stdout.len().min(64)]
            ),
        });
    }
    if !report.stderr.is_empty() {
        return Err(E2EError::UnexpectedRun {
            case: "stdin echo stderr",
            expected: "no stderr output".to_owned(),
            actual: format!("{:?}", report.stderr),
        });
    }

    // 空入力のEOFだけでも同じ終了codeへ辿り着くことを確認する。
    let store = TempStore::empty();
    let args = minictr_build_args(&store.path, E2E_ECHO_IMAGE, &elf);
    run_checked(minictr, &args, None, &[], COMMAND_TIMEOUT)?;
    let empty = run_case_stdin(minictr, store, kernel, HAPPY_PATH_TIMEOUT_MS, &[])?;
    if empty.status != Some(E2E_EXIT_CODE) || !empty.stdout.is_empty() {
        return Err(E2EError::UnexpectedRun {
            case: "stdin echo empty input",
            expected: format!("exit {} with no output", E2E_EXIT_CODE),
            actual: format!(
                "status {:?}, {} stdout bytes (stderr: {})",
                empty.status,
                empty.stdout.len(),
                String::from_utf8_lossy(&empty.stderr)
            ),
        });
    }
    Ok(report)
}

/// stdin bytesを`minictr run`へpipeし、cleanupまで検証する`run_case_in_store`
/// のstdin版。
fn run_case_stdin(
    minictr: &Path,
    store: TempStore,
    kernel: &Path,
    timeout_ms: &str,
    input: &[u8],
) -> Result<CaseReport, E2EError> {
    let store_path = store.path.clone();
    let qemu_before = qemu_pids()?;
    let payload_before = payload_temp_leftovers();
    let started = Instant::now();
    let args = minictr_run_args(&store.path, kernel, timeout_ms, E2E_ECHO_IMAGE);
    let outcome = run_with_timeout_stdin(
        minictr,
        &args,
        None,
        &[],
        Some(input.to_vec()),
        QEMU_RUN_TIMEOUT,
    );
    let elapsed = started.elapsed();
    let qemu_after = check_case_leftovers(&qemu_before, &payload_before, store, &store_path)?;
    let completed = outcome?;
    Ok(CaseReport {
        minictr_pid: completed.pid,
        qemu_before,
        qemu_after,
        elapsed,
        status: completed.status.code(),
        stdout: completed.stdout,
        stderr: completed.stderr,
    })
}

/// stress節の開始時に取るhost snapshot。各iteration後と節の終わりに、
/// QEMUとpayload directoryの差分が無いことを確認する。
struct StressBaseline {
    qemu: Vec<u32>,
    payload: Vec<PathBuf>,
}

fn stress_baseline() -> Result<StressBaseline, E2EError> {
    Ok(StressBaseline {
        qemu: qemu_pids()?,
        payload: payload_temp_leftovers(),
    })
}

/// snapshot以降に増えたQEMUとpayload directoryを両方報告する。
/// QEMUの掃除に失敗してもpayload側の残留も見るため、失敗はまとめて拾う。
fn check_stress_leftovers(baseline: &StressBaseline) -> Result<(), E2EError> {
    let mut failure: Option<E2EError> = None;
    let mut check = |result: Result<(), E2EError>| {
        if failure.is_none() {
            failure = result.err();
        }
    };
    match qemu_pids() {
        Ok(pids) => check(check_no_new_qemu(&baseline.qemu, &pids)),
        Err(error) => check(Err(error)),
    }
    check(check_no_leftovers(&baseline.payload));
    match failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// scenario名とiteration番号つきで失敗を包み、成功時は値をそのまま返す。
fn stress_iter<T>(
    scenario: &'static str,
    iteration: usize,
    result: Result<T, E2EError>,
) -> Result<T, E2EError> {
    result.map_err(|source| E2EError::StressIteration {
        scenario,
        iteration,
        source: Box::new(source),
    })
}

/// stress節用の共有store。正常終了する`hello`と回転する`spin`の二tagを
/// 登録し、全scenarioが同じstore (と同じ`run/` state directory) を使う。
fn stress_store(hello_elf: &[u8]) -> Result<TempStore, E2EError> {
    let store = TempStore::empty();
    let inner = minicontainer_bundle::Store::new(&store.path)
        .map_err(|error| E2EError::Store(error.to_string()))?;
    for (tag, bytes) in [
        (E2E_IMAGE, hello_bundle_bytes(hello_elf)?),
        (
            E2E_SPIN_IMAGE,
            bundle_bytes(E2E_SPIN_IMAGE, &spin_elf_bytes())?,
        ),
    ] {
        let digest = inner
            .import(&bytes)
            .map_err(|error| E2EError::Store(error.to_string()))?;
        inner
            .tag(tag, digest)
            .map_err(|error| E2EError::Store(error.to_string()))?;
    }
    Ok(store)
}

/// 正常終了するguestを1回runし、exit 42を確認する。
fn stress_foreground_once(minictr: &Path, store: &Path, kernel: &Path) -> Result<(), E2EError> {
    let completed = run_minictr(minictr, store, kernel, HAPPY_PATH_TIMEOUT_MS, E2E_IMAGE)?;
    if completed.status.code() != Some(E2E_EXIT_CODE) {
        return Err(E2EError::UnexpectedRun {
            case: "stress foreground exit code",
            expected: format!("exit {E2E_EXIT_CODE}"),
            actual: format!(
                "status {:?} (stderr: {})",
                completed.status.code(),
                String::from_utf8_lossy(&completed.stderr)
            ),
        });
    }
    Ok(())
}

/// timeoutで打ち切られるspin guestを1回runし、125を確認する。
fn stress_timeout_once(minictr: &Path, store: &Path, kernel: &Path) -> Result<(), E2EError> {
    let completed = run_minictr(minictr, store, kernel, SPIN_TIMEOUT_MS, E2E_SPIN_IMAGE)?;
    if completed.status.code() != Some(125) {
        return Err(E2EError::UnexpectedRun {
            case: "stress timeout exit code",
            expected: "exit 125".to_owned(),
            actual: format!(
                "status {:?} (stderr: {})",
                completed.status.code(),
                String::from_utf8_lossy(&completed.stderr)
            ),
        });
    }
    Ok(())
}

/// 回転中のrunへsignalを1回送り、125とsignal名の診断を確認する。
fn stress_signal_once(
    minictr: &Path,
    store: &Path,
    kernel: &Path,
    qemu_before: &[u32],
    signal_name: &'static str,
    send: impl FnOnce(u32) -> io::Result<()>,
) -> Result<(), E2EError> {
    let spawned = spawn_minictr(
        minictr,
        store,
        kernel,
        HAPPY_PATH_TIMEOUT_MS,
        E2E_SPIN_IMAGE,
    )?;
    let outcome = (|| {
        let spawned = wait_for_qemu_boot(spawned, qemu_before)?;
        thread::sleep(INTERRUPT_SETTLE);
        let spawned = assert_minictr_running(spawned)?;
        if let Err(error) = send(spawned.pid) {
            let failure = E2EError::Command {
                command: format!("send {signal_name} to minictr {}", spawned.pid),
                message: error.to_string(),
            };
            terminate_spawned(spawned);
            return Err(failure);
        }
        wait_for_exit(spawned, INTERRUPT_EXIT_TIMEOUT)
    })();
    let completed = outcome?;
    if completed.status.code() != Some(125)
        || !String::from_utf8_lossy(&completed.stderr)
            .contains(&format!("run interrupted by {signal_name}"))
    {
        return Err(E2EError::UnexpectedRun {
            case: "stress signal exit",
            expected: format!("exit 125 with a {signal_name} diagnostic"),
            actual: format!(
                "status {:?} (stderr: {})",
                completed.status.code(),
                String::from_utf8_lossy(&completed.stderr)
            ),
        });
    }
    Ok(())
}

/// detached runを1回行い、`stop`で回収する。失敗経路でもQEMUを孤児に
/// しないため、判明したpidは必ずSIGKILLで落とす。
fn stress_detach_once(minictr: &Path, store: &Path, kernel: &Path) -> Result<(), E2EError> {
    let mut qemu_pid = None;
    let result = (|| {
        let completed = run_minictr_with_extra_args(
            minictr,
            store,
            kernel,
            HAPPY_PATH_TIMEOUT_MS,
            E2E_SPIN_IMAGE,
            &[OsString::from("--detach")],
        )?;
        if !completed.status.success() {
            return Err(E2EError::UnexpectedRun {
                case: "stress detach run",
                expected: "exit 0 with an instance id".to_owned(),
                actual: format!(
                    "status {:?} (stderr: {})",
                    completed.status.code(),
                    String::from_utf8_lossy(&completed.stderr)
                ),
            });
        }
        let id = String::from_utf8_lossy(&completed.stdout).trim().to_owned();
        let pid = id
            .strip_prefix("i-")
            .and_then(|digits| digits.parse::<u32>().ok())
            .filter(|pid| id == format!("i-{pid}"));
        let Some(pid) = pid else {
            return Err(E2EError::UnexpectedRun {
                case: "stress detach output",
                expected: "a bare i-<pid> instance id".to_owned(),
                actual: format!(
                    "stdout: {:?}, stderr: {}",
                    String::from_utf8_lossy(&completed.stdout),
                    String::from_utf8_lossy(&completed.stderr)
                ),
            });
        };
        qemu_pid = Some(pid);
        wait_for_ps_row(minictr, store, E2E_SPIN_IMAGE, pid, "live")?;
        let stop_args = [
            OsString::from("stop"),
            OsString::from("--store"),
            store.as_os_str().to_owned(),
            OsString::from(&id),
        ];
        let stopped = run_with_timeout(minictr, &stop_args, None, &[], COMMAND_TIMEOUT)?;
        if !stopped.status.success() || String::from_utf8_lossy(&stopped.stdout).trim() != id {
            return Err(E2EError::UnexpectedRun {
                case: "stress detach stop",
                expected: format!("exit 0 echoing {id}"),
                actual: format!(
                    "status {:?} (stdout: {:?}, stderr: {})",
                    stopped.status.code(),
                    String::from_utf8_lossy(&stopped.stdout),
                    String::from_utf8_lossy(&stopped.stderr)
                ),
            });
        }
        Ok(())
    })();
    if let Some(pid) = qemu_pid {
        let _ = signal_process(pid, libc::SIGKILL);
    }
    result
}

/// `minictr`をSIGKILLしてhost crashを真似し、孤児QEMUも落としたあと
/// `stop`がstale instanceを回収することを1回確認する。
fn stress_crash_once(
    minictr: &Path,
    store: &Path,
    kernel: &Path,
    qemu_before: &[u32],
) -> Result<(), E2EError> {
    let spawned = spawn_minictr(
        minictr,
        store,
        kernel,
        HAPPY_PATH_TIMEOUT_MS,
        E2E_SPIN_IMAGE,
    )?;
    let mut qemu_pid = None;
    let result = (|| {
        let mut spawned = wait_for_qemu_boot(spawned, qemu_before)?;
        let pid = match qemu_pids() {
            Ok(pids) => pids.into_iter().find(|pid| !qemu_before.contains(pid)),
            Err(error) => {
                terminate_spawned(spawned);
                return Err(error);
            }
        };
        let Some(pid) = pid else {
            terminate_spawned(spawned);
            return Err(E2EError::UnexpectedRun {
                case: "stress crash QEMU discovery",
                expected: "a freshly booted QEMU pid".to_owned(),
                actual: "no new QEMU process".to_owned(),
            });
        };
        qemu_pid = Some(pid);
        if let Err(error) = wait_for_ps_row(minictr, store, E2E_SPIN_IMAGE, pid, "live") {
            terminate_spawned(spawned);
            let _ = signal_process(pid, libc::SIGKILL);
            return Err(error);
        }
        if let Err(error) = signal_process(spawned.pid, libc::SIGKILL) {
            let failure = E2EError::Command {
                command: format!("send SIGKILL to minictr {}", spawned.pid),
                message: error.to_string(),
            };
            terminate_spawned(spawned);
            let _ = signal_process(pid, libc::SIGKILL);
            return Err(failure);
        }
        if let Err(error) = spawned.child.wait() {
            let failure = E2EError::Command {
                command: spawned.command_line.clone(),
                message: error.to_string(),
            };
            let _ = signal_process(pid, libc::SIGKILL);
            return Err(failure);
        }
        let _ = join_reader(spawned.stdout_reader, &spawned.command_line);
        let _ = join_reader(spawned.stderr_reader, &spawned.command_line);
        // 孤児QEMUを落としてinstanceをstaleへ倒し、記録された回復経路
        // (`stop`)でpayloadとstateを回収する。
        let _ = signal_process(pid, libc::SIGKILL);
        wait_for_ps_row(minictr, store, E2E_SPIN_IMAGE, pid, "stale")?;
        let id = format!("i-{pid}");
        let stop_args = [
            OsString::from("stop"),
            OsString::from("--store"),
            store.as_os_str().to_owned(),
            OsString::from(&id),
        ];
        let stopped = run_with_timeout(minictr, &stop_args, None, &[], COMMAND_TIMEOUT)?;
        if !stopped.status.success() || String::from_utf8_lossy(&stopped.stdout).trim() != id {
            return Err(E2EError::UnexpectedRun {
                case: "stress crash stop",
                expected: format!("exit 0 echoing {id}"),
                actual: format!(
                    "status {:?} (stdout: {:?}, stderr: {})",
                    stopped.status.code(),
                    String::from_utf8_lossy(&stopped.stdout),
                    String::from_utf8_lossy(&stopped.stderr)
                ),
            });
        }
        Ok(())
    })();
    if let Some(pid) = qemu_pid {
        let _ = signal_process(pid, libc::SIGKILL);
    }
    result
}

/// 共有storeの`run/`にinstance state fileが残っていないことを確認する。
fn check_no_state_files(store: &Path) -> Result<(), E2EError> {
    let mut stray: Vec<PathBuf> = std::fs::read_dir(store.join("run"))
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name()
                        .map(|name| name.to_string_lossy().ends_with(".state"))
                        .unwrap_or(false)
                })
                .collect()
        })
        .unwrap_or_default();
    stray.sort();
    if stray.is_empty() {
        return Ok(());
    }
    let names: Vec<String> = stray
        .iter()
        .map(|path| path.display().to_string())
        .collect();
    Err(E2EError::Store(format!(
        "instance state files remain: {}",
        names.join(", ")
    )))
}

/// `minictr ps`がinstance行を一つも出さないことを確認する。
fn check_ps_rows_empty(minictr: &Path, store: &Path) -> Result<(), E2EError> {
    let args = [
        OsString::from("ps"),
        OsString::from("--store"),
        store.as_os_str().to_owned(),
    ];
    let ps = run_with_timeout(minictr, &args, None, &[], COMMAND_TIMEOUT)?;
    let text = String::from_utf8_lossy(&ps.stdout);
    if !ps.status.success() || text.lines().any(|line| line.starts_with("i-")) {
        return Err(E2EError::UnexpectedRun {
            case: "stress final ps",
            expected: "no instance rows".to_owned(),
            actual: format!(
                "status {:?}, ps output: {}",
                ps.status.code(),
                text.trim_end()
            ),
        });
    }
    Ok(())
}

/// 反復起動・timeout・signal・detach・crashの各scenarioを一つの共有storeへ
/// bounded回だけ実行する。累積漏れを見るため、instance stateとps行は節の
/// 終わりにまとめて検査し、各iterationではQEMUとpayloadの残留だけを
/// iteration番号つきで報告する。
fn run_stress(minictr: &Path, kernel: &Path, workspace: &Path) -> Result<String, E2EError> {
    let elf = workspace.join(GUEST_HELLO_ELF);
    let hello_elf = std::fs::read(&elf).map_err(|_| E2EError::MissingGuestElf(elf.clone()))?;
    let store = stress_store(&hello_elf)?;
    let store_path = store.path.clone();
    let baseline = stress_baseline()?;
    let started = Instant::now();

    for iteration in 0..STRESS_RUN_ITERS {
        let outcome = stress_foreground_once(minictr, &store_path, kernel)
            .and_then(|()| check_stress_leftovers(&baseline));
        stress_iter("foreground", iteration, outcome)?;
    }
    for iteration in 0..STRESS_TIMEOUT_ITERS {
        let outcome = stress_timeout_once(minictr, &store_path, kernel)
            .and_then(|()| check_stress_leftovers(&baseline));
        stress_iter("timeout", iteration, outcome)?;
    }
    for iteration in 0..STRESS_SIGNAL_ITERS {
        let outcome = stress_signal_once(
            minictr,
            &store_path,
            kernel,
            &baseline.qemu,
            "SIGINT",
            |pid| signal_process_group(pid, libc::SIGINT),
        )
        .and_then(|()| check_stress_leftovers(&baseline));
        stress_iter("SIGINT", iteration, outcome)?;
    }
    for iteration in 0..STRESS_SIGNAL_ITERS {
        let outcome = stress_signal_once(
            minictr,
            &store_path,
            kernel,
            &baseline.qemu,
            "SIGTERM",
            |pid| signal_process(pid, libc::SIGTERM),
        )
        .and_then(|()| check_stress_leftovers(&baseline));
        stress_iter("SIGTERM", iteration, outcome)?;
    }
    for iteration in 0..STRESS_DETACH_ITERS {
        let outcome = stress_detach_once(minictr, &store_path, kernel)
            .and_then(|()| check_stress_leftovers(&baseline));
        stress_iter("detach", iteration, outcome)?;
    }
    for iteration in 0..STRESS_CRASH_ITERS {
        let outcome = stress_crash_once(minictr, &store_path, kernel, &baseline.qemu)
            .and_then(|()| check_stress_leftovers(&baseline));
        stress_iter("crash", iteration, outcome)?;
    }

    check_no_state_files(&store_path)?;
    check_ps_rows_empty(minictr, &store_path)?;
    check_stress_leftovers(&baseline)?;
    drop(store);
    if store_path.exists() {
        return Err(E2EError::Store(format!(
            "stress store was not removed: {}",
            store_path.display()
        )));
    }
    let total = STRESS_RUN_ITERS
        + STRESS_TIMEOUT_ITERS
        + 2 * STRESS_SIGNAL_ITERS
        + STRESS_DETACH_ITERS
        + STRESS_CRASH_ITERS;
    Ok(format!(
        "{total} iterations in {:.1}s",
        started.elapsed().as_secs_f64()
    ))
}

/// `minictr ps`が`qemu_pid`の行を`status`で表示するまでpollする。QEMUの
/// 登録と死亡の観測には僅かな遅れがあるため、即時確認ではなく期限付きで
/// 待つ。
fn wait_for_ps_row(
    minictr: &Path,
    store: &Path,
    image: &str,
    qemu_pid: u32,
    status: &str,
) -> Result<(), E2EError> {
    let deadline = Instant::now() + INTERRUPT_BOOT_TIMEOUT;
    let expected = format!("i-{qemu_pid}\t{qemu_pid}\t{image}\t{status}\t");
    let args = [
        OsString::from("ps"),
        OsString::from("--store"),
        store.as_os_str().to_owned(),
    ];
    loop {
        // deadline到達時点では直前のpollが必ず成功しているため、最後のps
        // 出力をそのまま診断へ載せられる。
        let text = match run_with_timeout(minictr, &args, None, &[], COMMAND_TIMEOUT) {
            Ok(completed) if completed.status.success() => {
                String::from_utf8_lossy(&completed.stdout).into_owned()
            }
            Ok(completed) => {
                return Err(E2EError::UnexpectedRun {
                    case: "crash-path ps exit code",
                    expected: "a successful minictr ps".to_owned(),
                    actual: format!(
                        "status {:?} (stderr: {})",
                        completed.status.code(),
                        String::from_utf8_lossy(&completed.stderr)
                    ),
                });
            }
            Err(error) => return Err(error),
        };
        if text.lines().any(|line| line.starts_with(&expected)) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(E2EError::UnexpectedRun {
                case: "crash-path ps row",
                expected: format!("a {status} i-{qemu_pid} row within the E2E limit"),
                actual: format!("last ps output: {}", text.trim_end()),
            });
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// crash経路が残したpayload directoryを除去する。`before`に無かった
/// `minicontainer-run-*`だけを消し、検査前のbaselineを汚さない。
fn remove_stray_payloads(before: &[PathBuf]) {
    for path in payload_temp_leftovers() {
        if !before.contains(&path) {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

fn minictr_run_args(store: &Path, kernel: &Path, timeout_ms: &str, image: &str) -> Vec<OsString> {
    vec![
        OsString::from("run"),
        OsString::from("--store"),
        store.as_os_str().to_owned(),
        OsString::from("--kernel"),
        kernel.as_os_str().to_owned(),
        OsString::from("--timeout-ms"),
        OsString::from(timeout_ms),
        OsString::from(image),
    ]
}

fn run_minictr(
    minictr: &Path,
    store: &Path,
    kernel: &Path,
    timeout_ms: &str,
    image: &str,
) -> Result<CompletedProcess, E2EError> {
    run_minictr_with_extra_args(minictr, store, kernel, timeout_ms, image, &[])
}

fn run_minictr_with_extra_args(
    minictr: &Path,
    store: &Path,
    kernel: &Path,
    timeout_ms: &str,
    image: &str,
    extra: &[OsString],
) -> Result<CompletedProcess, E2EError> {
    let mut args = minictr_run_args(store, kernel, timeout_ms, image);
    let image_at = args.len() - 1;
    args.splice(image_at..image_at, extra.iter().cloned());
    run_with_timeout(minictr, &args, None, &[], QEMU_RUN_TIMEOUT)
}

/// `minictr run`を自groupのleaderとして起動し、待たずに返す。signalを
/// 送ってから終了を観測する経路が使う。
fn spawn_minictr(
    minictr: &Path,
    store: &Path,
    kernel: &Path,
    timeout_ms: &str,
    image: &str,
) -> Result<SpawnedChild, E2EError> {
    spawn_in_own_group(
        minictr,
        &minictr_run_args(store, kernel, timeout_ms, image),
        None,
        &[],
        None,
    )
}

/// 起動済みの子を期限まで待つ。期限切れではgroupへSIGKILLを送って回収し、
/// 残骸を残さずにtimeout errorを返す。
fn wait_for_exit(
    mut spawned: SpawnedChild,
    timeout: Duration,
) -> Result<CompletedProcess, E2EError> {
    let deadline = Instant::now() + timeout;
    loop {
        match spawned.child.try_wait() {
            Ok(Some(status)) => {
                let stdout = join_reader(spawned.stdout_reader, &spawned.command_line)?;
                let stderr = join_reader(spawned.stderr_reader, &spawned.command_line)?;
                return Ok(CompletedProcess {
                    pid: spawned.pid,
                    status,
                    stdout,
                    stderr,
                });
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = signal_process_group(spawned.pid, libc::SIGKILL);
                    let _ = spawned.child.kill();
                    let _ = spawned.child.wait();
                    join_reader(spawned.stdout_reader, &spawned.command_line).ok();
                    join_reader(spawned.stderr_reader, &spawned.command_line).ok();
                    return Err(E2EError::TimedOut {
                        command: spawned.command_line,
                    });
                }
                thread::sleep(POLL_INTERVAL);
            }
            Err(error) => {
                let _ = signal_process_group(spawned.pid, libc::SIGKILL);
                let _ = spawned.child.kill();
                let _ = spawned.child.wait();
                join_reader(spawned.stdout_reader, &spawned.command_line).ok();
                join_reader(spawned.stderr_reader, &spawned.command_line).ok();
                return Err(E2EError::Command {
                    command: spawned.command_line,
                    message: error.to_string(),
                });
            }
        }
    }
}

/// 終了した外部commandの出力。
#[derive(Debug)]
struct CompletedProcess {
    /// 観測した子process ID。transcriptへ記録する。
    pid: u32,
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// 外部commandを起動し、両pipeをreader threadで並行drainしながら終了を待つ。
///
/// 先に終了だけを待つ実装は、pipe buffer (通常64 KiB) が埋まると子がblock
/// してdeadlockする。子は自前のUnix process groupのleaderとして起動するた
/// め、timeout時とwait error時はgroup全体へsignalしてから直接の子をkill・
/// wait・joinする。`minictr`が残したQEMUのような孫processもorphan化しない。
/// 自groupのleaderとして起動した子と、その出力をdrainするreader。
struct SpawnedChild {
    child: Child,
    pid: u32,
    stdout_reader: thread::JoinHandle<io::Result<Vec<u8>>>,
    stderr_reader: thread::JoinHandle<io::Result<Vec<u8>>>,
    command_line: String,
}

/// commandを自前のprocess groupのleaderとして起動し、両pipeのdrainを
/// 始める。group宛のsignalはharnessへ届かず、子の子孫まで届く。
///
/// `stdin_data`があるときは子のstdinをpipeへ向け、writer threadがbytesを
/// 書き切ってから閉じる (guest側EOF)。`None`ではstdinを`/dev/null`へ向け、
/// 非TTY入力として即座にEOFが届く — xtaskのstdin継承に依存しない決定的な
/// 入力にするためである。
fn spawn_in_own_group(
    program: impl AsRef<OsStr>,
    args: &[OsString],
    current_dir: Option<&Path>,
    extra_env: &[(&str, &str)],
    stdin_data: Option<Vec<u8>>,
) -> Result<SpawnedChild, E2EError> {
    use std::process::Stdio;

    let command_line = command_line(&program, args);
    let mut command = Command::new(program);
    command.args(args);
    if let Some(directory) = current_dir {
        command.current_dir(directory);
    }
    for (key, value) in extra_env {
        command.env(key, value);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    let mut child = command
        .stdin(match &stdin_data {
            Some(_) => Stdio::piped(),
            None => Stdio::null(),
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| E2EError::Command {
            command: command_line.clone(),
            message: error.to_string(),
        })?;
    if let Some(bytes) = stdin_data {
        use std::io::Write as _;
        let mut pipe = child
            .stdin
            .take()
            .expect("piped stdin must be available after spawn");
        // 子の早期終了でpipeが閉じてもharnessは失敗にしない。書き込み側の
        // threadがdropでpipeを閉じるまでがEOFの区切りである。
        thread::spawn(move || {
            let _ = pipe.write_all(&bytes);
        });
    }
    let pid = child.id();
    let stdout_pipe = child
        .stdout
        .take()
        .expect("piped stdout must be available after spawn");
    let stderr_pipe = child
        .stderr
        .take()
        .expect("piped stderr must be available after spawn");
    let stdout_reader = thread::spawn(move || read_all(stdout_pipe));
    let stderr_reader = thread::spawn(move || read_all(stderr_pipe));
    Ok(SpawnedChild {
        child,
        pid,
        stdout_reader,
        stderr_reader,
        command_line,
    })
}

fn run_with_timeout(
    program: impl AsRef<OsStr>,
    args: &[OsString],
    current_dir: Option<&Path>,
    extra_env: &[(&str, &str)],
    timeout: Duration,
) -> Result<CompletedProcess, E2EError> {
    run_with_timeout_stdin(program, args, current_dir, extra_env, None, timeout)
}

/// `run_with_timeout`にstdin bytesを供給する版。`None`は`/dev/null`。
fn run_with_timeout_stdin(
    program: impl AsRef<OsStr>,
    args: &[OsString],
    current_dir: Option<&Path>,
    extra_env: &[(&str, &str)],
    stdin_data: Option<Vec<u8>>,
    timeout: Duration,
) -> Result<CompletedProcess, E2EError> {
    let SpawnedChild {
        mut child,
        pid,
        stdout_reader,
        stderr_reader,
        command_line,
    } = spawn_in_own_group(program, args, current_dir, extra_env, stdin_data)?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let stdout = join_reader(stdout_reader, &command_line)?;
                let stderr = join_reader(stderr_reader, &command_line)?;
                return Ok(CompletedProcess {
                    pid,
                    status,
                    stdout,
                    stderr,
                });
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = signal_process_group(pid, libc::SIGKILL);
                    let _ = child.kill();
                    let _ = child.wait();
                    join_reader(stdout_reader, &command_line).ok();
                    join_reader(stderr_reader, &command_line).ok();
                    return Err(E2EError::TimedOut {
                        command: command_line,
                    });
                }
                thread::sleep(POLL_INTERVAL);
            }
            Err(error) => {
                let _ = signal_process_group(pid, libc::SIGKILL);
                let _ = child.kill();
                let _ = child.wait();
                join_reader(stdout_reader, &command_line).ok();
                join_reader(stderr_reader, &command_line).ok();
                return Err(E2EError::Command {
                    command: command_line,
                    message: error.to_string(),
                });
            }
        }
    }
}

/// harnessの子のprocess group全体へsignalを送る。子は `process_group(0)`
/// で自groupのleaderとして起動するため、PIDはPGIDと等しく、子孫 (timeout
/// した`minictr`が残したQEMUなど) まで届く。既死groupのerrorは無視する。
/// SIGKILLで止める呼び出し側は、必ず直接のkillとwaitも行う。
///
/// 外部の`kill` binaryは使わない。procpsの`kill`は`-pgid`形式をexit 0の
/// まま黙って無視し、孫processを生かしたまま残す。`kill(2)`を直接呼ぶ。
fn signal_process_group(pid: u32, signal: libc::c_int) -> io::Result<()> {
    let target = -(pid as libc::pid_t);
    // SAFETY: `kill(2)`の第一引数が負のときはprocess groupを指定する。
    // `pid`はspawn直後の子のPIDで`pid_t`に収まる。
    let result = unsafe { libc::kill(target, signal) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// harnessの子のPIDだけへsignalを送る。group宛では届かない別groupの子孫
/// (host転送を待つQEMUなど) には届かない。
fn signal_process(pid: u32, signal: libc::c_int) -> io::Result<()> {
    // SAFETY: `pid`はspawn直後の子のPIDで`pid_t`に収まる。
    let result = unsafe { libc::kill(pid as libc::pid_t, signal) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// 起動途中または待機失敗の子groupをbest effortで停止し、直接の子と
/// reader threadを回収する。
fn terminate_spawned(mut spawned: SpawnedChild) {
    let _ = signal_process_group(spawned.pid, libc::SIGKILL);
    let _ = spawned.child.kill();
    let _ = spawned.child.wait();
    join_reader(spawned.stdout_reader, &spawned.command_line).ok();
    join_reader(spawned.stderr_reader, &spawned.command_line).ok();
}

fn read_all(mut stream: impl io::Read + Send + 'static) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).map(|_| bytes)
}

fn join_reader(
    reader: thread::JoinHandle<io::Result<Vec<u8>>>,
    command_line: &str,
) -> Result<Vec<u8>, E2EError> {
    reader
        .join()
        .map_err(|_| E2EError::Command {
            command: command_line.to_owned(),
            message: "output reader panicked".to_owned(),
        })?
        .map_err(|error| E2EError::Command {
            command: command_line.to_owned(),
            message: error.to_string(),
        })
}

fn command_line(program: impl AsRef<OsStr>, args: &[OsString]) -> String {
    let arguments = args
        .iter()
        .map(|argument| argument.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ");
    format!("{} {arguments}", program.as_ref().to_string_lossy())
}

/// 外部commandを実行し、非0終了を型付きerrorへ変える。
fn run_checked(
    program: impl AsRef<OsStr>,
    args: &[OsString],
    current_dir: Option<&Path>,
    extra_env: &[(&str, &str)],
    timeout: Duration,
) -> Result<Vec<u8>, E2EError> {
    let command = command_line(&program, args);
    let completed = run_with_timeout(program, args, current_dir, extra_env, timeout)?;
    if !completed.status.success() {
        return Err(E2EError::CommandFailed {
            command,
            status: completed.status.code(),
            stdout: String::from_utf8_lossy(&completed.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&completed.stderr).into_owned(),
        });
    }
    Ok(completed.stdout)
}

/// 一時storeを所有し、drop時にdirectoryごと削除する。
struct TempStore {
    path: PathBuf,
}

impl TempStore {
    /// 公開CLIがimportする空の一時store pathを予約する。directory自体は
    /// `Store::new`が作り、`Drop`が消す。
    fn empty() -> Self {
        let path = std::env::temp_dir().join(format!(
            "minictr-e2e-store-{}-{}",
            std::process::id(),
            next_temp_id()
        ));
        Self { path }
    }

    fn with_bundle(bundle: &[u8]) -> Result<Self, E2EError> {
        let path = std::env::temp_dir().join(format!(
            "minictr-e2e-store-{}-{}",
            std::process::id(),
            next_temp_id()
        ));
        let store = minicontainer_bundle::Store::new(&path)
            .map_err(|error| E2EError::Store(error.to_string()))?;
        let digest = store
            .import(bundle)
            .map_err(|error| E2EError::Store(error.to_string()))?;
        store
            .tag(E2E_IMAGE, digest)
            .map_err(|error| E2EError::Store(error.to_string()))?;
        Ok(Self { path })
    }
}

impl Drop for TempStore {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// 一時scratch directoryを所有し、drop時に再帰削除する。失敗経路でも
/// harnessの残骸を残さないためのguardである。
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn create(prefix: &str) -> Result<Self, E2EError> {
        for _ in 0..128 {
            let path = std::env::temp_dir().join(format!(
                "{prefix}{}-{}",
                std::process::id(),
                next_temp_id()
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(E2EError::Store(error.to_string())),
            }
        }
        Err(E2EError::Store(
            "could not allocate a unique temporary directory".to_owned(),
        ))
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

fn next_temp_id() -> u64 {
    NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
}

/// 実行中のrunが残した一時payload directoryを集める。
fn payload_temp_leftovers() -> Vec<PathBuf> {
    payload_temp_leftovers_in(&std::env::temp_dir())
}

fn payload_temp_leftovers_in(directory: &Path) -> Vec<PathBuf> {
    let mut leftovers = Vec::new();
    let Ok(entries) = std::fs::read_dir(directory) else {
        return leftovers;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with("minicontainer-run-") {
            leftovers.push(entry.path());
        }
    }
    leftovers.sort();
    leftovers
}

fn check_no_leftovers(before: &[PathBuf]) -> Result<(), E2EError> {
    let after = payload_temp_leftovers();
    let fresh: Vec<PathBuf> = after
        .into_iter()
        .filter(|path| !before.contains(path))
        .collect();
    if fresh.is_empty() {
        Ok(())
    } else {
        Err(E2EError::PayloadLeftover(fresh))
    }
}

/// 実行中の`qemu-system-riscv64` process IDを集める。
fn qemu_pids() -> Result<Vec<u32>, E2EError> {
    let stdout = run_checked(
        "ps",
        &[OsString::from("-eo"), OsString::from("pid,args")],
        None,
        &[],
        COMMAND_TIMEOUT,
    )?;
    Ok(parse_qemu_pids(&String::from_utf8_lossy(&stdout)))
}

fn parse_qemu_pids(text: &str) -> Vec<u32> {
    let mut pids: Vec<u32> = text
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid: u32 = fields.next()?.parse().ok()?;
            let executable = fields.next()?;
            (Path::new(executable).file_name() == Some(OsStr::new("qemu-system-riscv64")))
                .then_some(pid)
        })
        .collect();
    pids.sort_unstable();
    pids
}

/// 実行中の全process IDを集める (読み取り専用。killはしない)。
#[cfg(test)]
fn all_pids() -> Result<Vec<u32>, E2EError> {
    let stdout = run_checked(
        "ps",
        &[OsString::from("-eo"), OsString::from("pid")],
        None,
        &[],
        COMMAND_TIMEOUT,
    )?;
    Ok(String::from_utf8_lossy(&stdout)
        .split_whitespace()
        .filter_map(|field| field.parse().ok())
        .collect())
}

fn check_no_new_qemu(before: &[u32], after: &[u32]) -> Result<(), E2EError> {
    let fresh: Vec<u32> = after
        .iter()
        .copied()
        .filter(|pid| !before.contains(pid))
        .collect();
    if fresh.is_empty() {
        Ok(())
    } else {
        Err(E2EError::QemuLeftover(fresh))
    }
}

// 決定的なE2E guest ELF
//
// U-modeで`write`と`exit`だけを使う単一PT_LOADの静的RV64 ELFを手でencode
// する。loaderの検証対象となる入力であり、loader自体の複製ではない。
const ELF_ENTRY: u64 = 0x0010_0000;
const ELF_OFFSET: u64 = 0x1000;
const ELF_ALIGN: u64 = 0x1000;

/// QEMU virt `-kernel` がsupervisor binaryを配置するaddress。
const KERNEL_ENTRY: u64 = 0x8020_0000;
/// QEMU virt UART (16550互換) の送信register。
const UART_TX_OFFSET: i16 = 0;

const REG_X0: u32 = 0;
const REG_SP: u32 = 2;
const REG_T0: u32 = 5;
const REG_T1: u32 = 6;
const REG_S0: u32 = 8;
const REG_A0: u32 = 10;
const REG_A1: u32 = 11;
const REG_A2: u32 = 12;
const REG_A7: u32 = 17;

const fn lui(rd: u32, imm: u32) -> u32 {
    ((imm & 0xf_ffff) << 12) | (rd << 7) | 0x0037
}

const fn addi(rd: u32, rs1: u32, imm: i16) -> u32 {
    (((imm as u32) & 0xfff) << 20) | (rs1 << 15) | (rd << 7) | 0x0013
}

const fn sb(rs2: u32, rs1: u32, imm: i16) -> u32 {
    let imm = imm as u32;
    (((imm >> 5) & 0x7f) << 25) | (rs2 << 20) | (rs1 << 15) | ((imm & 0x1f) << 7) | 0x0023
}

const ECALL: u32 = 0x0000_0073;
const LOOP: u32 = 0x0000_006f;

/// 標準出力と標準エラー出力へ一行ずつ書き、`exit(42)`するguest。
fn hello_elf_bytes() -> Vec<u8> {
    let mut code: Vec<u32> = Vec::new();
    code.push(addi(REG_S0, REG_SP, -64));
    emit_string(&mut code, E2E_STDOUT, 0);
    emit_write(&mut code, 1, E2E_STDOUT.len());
    emit_string(&mut code, E2E_STDERR, 0);
    emit_write(&mut code, 2, E2E_STDERR.len());
    code.push(addi(REG_A0, REG_X0, E2E_EXIT_CODE as i16));
    code.push(addi(REG_A7, REG_X0, 2));
    code.push(ECALL);
    code.push(LOOP);
    build_elf(&code)
}

/// 終了せず回り続けるguest。host timeout経路の入力である。
fn spin_elf_bytes() -> Vec<u8> {
    build_elf(&[LOOP])
}

/// 1 MiBを大きく超える出力を連打してから、killされるまで回り続けるguest。
///
/// 1024 byteの`write`を1400回 (約1.37 MiB) 展開し、hostの出力合計上限に
/// 当ててからspinする。上限が効かなければhost timeoutで終わるため、E2Eの
/// 診断照合が上限の有無を判定する。分岐encodingを持ち込まず、展開した
/// `write`と末尾のspinだけで組む。
fn chatty_elf_bytes() -> Vec<u8> {
    const CHATTY_WRITES: usize = 1400;
    const CHATTY_LEN: usize = 1024;
    let mut code: Vec<u32> = Vec::new();
    code.push(addi(REG_S0, REG_SP, -(CHATTY_LEN as i16)));
    emit_string(&mut code, &[b'O'; CHATTY_LEN], 0);
    for _ in 0..CHATTY_WRITES {
        emit_write(&mut code, 1, CHATTY_LEN);
    }
    code.push(LOOP);
    build_elf(&code)
}

/// 不正control headerをUARTへ直接書く最小supervisor kernel。
///
/// miniOSを経由せずQEMU `-kernel` で起動する。virt UART MMIOへ`MCF1`と
/// 未知kind `0xFF` のheaderを書いた後、killされるまで回り続ける。guestは
/// control headerを直接書けないため、これが実QEMUでのprotocol破損経路の
/// 入力になる。16550の送信FIFO (16 byte) に収まる12 byteだけを連続store
/// するので、送信可能pollなしで欠落しない。
fn malformed_frame_kernel_bytes() -> Vec<u8> {
    const HEADER: [u8; 12] = [
        0x4D, 0x43, 0x46, 0x31, // MCF1
        0xFF, // 未知kind: decoderが即座にheader errorを返す
        0x00, 0x00, 0x00, // flags + reserved
        0x00, 0x00, 0x00, 0x00, // payload_len (検査に届く前にkindで失敗する)
    ];
    let mut code: Vec<u32> = Vec::new();
    code.push(lui(REG_T0, 0x10000));
    for byte in HEADER {
        code.push(addi(REG_T1, REG_X0, i16::from(byte)));
        code.push(sb(REG_T1, REG_T0, UART_TX_OFFSET));
    }
    code.push(LOOP);
    build_elf_at(KERNEL_ENTRY, &code)
}

fn emit_string(code: &mut Vec<u32>, bytes: &[u8], base: i16) {
    for (index, byte) in bytes.iter().enumerate() {
        code.push(addi(REG_T0, REG_X0, i16::from(*byte)));
        code.push(sb(
            REG_T0,
            REG_S0,
            base + i16::try_from(index).expect("E2E string must be short"),
        ));
    }
}

fn emit_write(code: &mut Vec<u32>, descriptor: i16, len: usize) {
    code.push(addi(REG_A0, REG_X0, descriptor));
    code.push(addi(REG_A1, REG_S0, 0));
    code.push(addi(
        REG_A2,
        REG_X0,
        i16::try_from(len).expect("E2E write must be short"),
    ));
    code.push(addi(REG_A7, REG_X0, 1));
    code.push(ECALL);
}

fn build_elf(code: &[u32]) -> Vec<u8> {
    build_elf_at(ELF_ENTRY, code)
}

fn build_elf_at(entry: u64, code: &[u32]) -> Vec<u8> {
    let code_bytes: Vec<u8> = code.iter().flat_map(|word| word.to_le_bytes()).collect();
    let mut bytes = vec![0u8; ELF_OFFSET as usize + code_bytes.len()];
    bytes[0..4].copy_from_slice(b"\x7fELF");
    bytes[4] = 2;
    bytes[5] = 1;
    bytes[6] = 1;
    bytes[16..18].copy_from_slice(&2u16.to_le_bytes());
    bytes[18..20].copy_from_slice(&243u16.to_le_bytes());
    bytes[20..24].copy_from_slice(&1u32.to_le_bytes());
    bytes[24..32].copy_from_slice(&entry.to_le_bytes());
    bytes[32..40].copy_from_slice(&64u64.to_le_bytes());
    bytes[52..54].copy_from_slice(&64u16.to_le_bytes());
    bytes[54..56].copy_from_slice(&56u16.to_le_bytes());
    bytes[56..58].copy_from_slice(&1u16.to_le_bytes());
    let header = 64;
    bytes[header..header + 4].copy_from_slice(&1u32.to_le_bytes());
    bytes[header + 4..header + 8].copy_from_slice(&5u32.to_le_bytes());
    bytes[header + 8..header + 16].copy_from_slice(&ELF_OFFSET.to_le_bytes());
    bytes[header + 16..header + 24].copy_from_slice(&entry.to_le_bytes());
    bytes[header + 24..header + 32].copy_from_slice(&entry.to_le_bytes());
    bytes[header + 32..header + 40].copy_from_slice(&(code_bytes.len() as u64).to_le_bytes());
    bytes[header + 40..header + 48].copy_from_slice(&(code_bytes.len() as u64).to_le_bytes());
    bytes[header + 48..header + 56].copy_from_slice(&ELF_ALIGN.to_le_bytes());
    bytes[ELF_OFFSET as usize..].copy_from_slice(&code_bytes);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn elf_header(bytes: &[u8]) -> (u16, u16, u32, u64, u16) {
        let kind = u16::from_le_bytes(bytes[16..18].try_into().unwrap());
        let machine = u16::from_le_bytes(bytes[18..20].try_into().unwrap());
        let version = u32::from_le_bytes(bytes[20..24].try_into().unwrap());
        let entry = u64::from_le_bytes(bytes[24..32].try_into().unwrap());
        let phnum = u16::from_le_bytes(bytes[56..58].try_into().unwrap());
        (kind, machine, version, entry, phnum)
    }

    // Catches emitting a guest ELF that the miniOS static loader would
    // reject before the guest ever runs (wrong class, type, machine, or
    // entry alignment).
    #[test]
    fn hello_elf_is_a_single_load_static_riscv_executable() {
        let bytes = hello_elf_bytes();

        assert_eq!(&bytes[0..4], b"\x7fELF");
        assert_eq!(bytes[4], 2, "must be a 64-bit object");
        assert_eq!(bytes[5], 1, "must be little-endian");
        let (kind, machine, version, entry, phnum) = elf_header(&bytes);
        assert_eq!(kind, 2, "must be ET_EXEC");
        assert_eq!(machine, 243, "must be EM_RISCV");
        assert_eq!(version, 1);
        assert_eq!(entry, ELF_ENTRY);
        assert_eq!(phnum, 1);

        let header = 64;
        assert_eq!(
            u32::from_le_bytes(bytes[header..header + 4].try_into().unwrap()),
            1
        );
        let flags = u32::from_le_bytes(bytes[header + 4..header + 8].try_into().unwrap());
        assert_eq!(
            flags & 0b101,
            0b101,
            "segment must be readable and executable"
        );
        assert_eq!(
            u64::from_le_bytes(bytes[header + 16..header + 24].try_into().unwrap()),
            ELF_ENTRY
        );
    }

    // Catches a hello guest that cannot spell its contract: the code must
    // contain the write/exit ecalls and spell both output lines.
    #[test]
    fn hello_elf_spells_both_outputs_and_exit_42() {
        let bytes = hello_elf_bytes();
        let code = &bytes[ELF_OFFSET as usize..];

        let ecalls = code
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|word| **word == ECALL.to_le_bytes())
            .count();
        assert_eq!(ecalls, 3, "write, write, exit");
        for needle in [E2E_STDOUT, E2E_STDERR] {
            for byte in needle {
                let encoded = addi(REG_T0, REG_X0, i16::from(*byte)).to_le_bytes();
                assert!(
                    code.windows(4).any(|word| word == encoded),
                    "guest must materialize byte {byte:#04x}"
                );
            }
        }
        let exit_42 = addi(REG_A0, REG_X0, 42).to_le_bytes();
        assert!(code.windows(4).any(|word| word == exit_42));
    }

    // Catches an output-cap fixture that emits less than the host
    // accepts: the guest must spell 1400 unrolled 1024-byte writes and
    // then spin, so only a missing host cap can let it survive.
    #[test]
    fn chatty_elf_spells_a_burst_over_1_mib_then_spins() {
        let bytes = chatty_elf_bytes();
        let code = &bytes[ELF_OFFSET as usize..];

        let ecalls = code
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|word| **word == ECALL.to_le_bytes())
            .count();
        assert_eq!(ecalls, 1400, "unrolled write burst");
        let materialized = addi(REG_T0, REG_X0, i16::from(b'O')).to_le_bytes();
        assert!(code.windows(4).any(|word| word == materialized));
        assert_eq!(&code[code.len() - 4..], LOOP.to_le_bytes());
        let (kind, machine, _, entry, phnum) = elf_header(&bytes);
        assert_eq!((kind, machine, entry, phnum), (2, 243, ELF_ENTRY, 1));
    }

    // Catches a timeout fixture that could exit on its own, which would turn
    // the timeout E2E into a flaky happy path.
    #[test]
    fn spin_elf_never_emits_an_ecall() {
        let bytes = spin_elf_bytes();
        let code = &bytes[ELF_OFFSET as usize..];

        assert_eq!(code, LOOP.to_le_bytes());
        let (kind, machine, _, entry, phnum) = elf_header(&bytes);
        assert_eq!((kind, machine, entry, phnum), (2, 243, ELF_ENTRY, 1));
    }

    // Catches a malformed-frame kernel that QEMU would not boot as a
    // supervisor binary: entry and load address must be the virt
    // `-kernel` address, not the miniOS payload entry.
    #[test]
    fn malformed_frame_kernel_is_a_supervisor_elf_at_the_kernel_address() {
        let bytes = malformed_frame_kernel_bytes();

        assert_eq!(&bytes[0..4], b"\x7fELF");
        assert_eq!(bytes[4], 2, "must be a 64-bit object");
        assert_eq!(bytes[5], 1, "must be little-endian");
        let (kind, machine, _, entry, phnum) = elf_header(&bytes);
        assert_eq!((kind, machine, entry, phnum), (2, 243, KERNEL_ENTRY, 1));

        let header = 64;
        let paddr = u64::from_le_bytes(bytes[header + 16..header + 24].try_into().unwrap());
        assert_eq!(paddr, KERNEL_ENTRY, "QEMU loads the kernel at its paddr");
        let ppaddr = u64::from_le_bytes(bytes[header + 24..header + 32].try_into().unwrap());
        assert_eq!(
            ppaddr, KERNEL_ENTRY,
            "QEMU riscv loads segments at p_paddr, which must equal the entry"
        );
    }

    // Catches a malformed-frame kernel that cannot corrupt the control
    // stream: it must write MCF1 plus an invalid header to the virt UART
    // and then spin without any supervisor call.
    #[test]
    fn malformed_frame_kernel_writes_a_bad_header_then_spins() {
        let bytes = malformed_frame_kernel_bytes();
        let code = &bytes[ELF_OFFSET as usize..];
        let words: Vec<u32> = code
            .as_chunks::<4>()
            .0
            .iter()
            .map(|word| u32::from_le_bytes(*word))
            .collect();

        assert_eq!(words[0], 0x1000_02b7, "lui t0,0x10000 maps the UART");
        assert_eq!(
            *words.last().unwrap(),
            LOOP,
            "the kernel spins until killed"
        );
        assert!(
            !words.contains(&ECALL),
            "no supervisor call may precede the malformed bytes"
        );
        for byte in [0x4Du8, 0x43, 0x46, 0x31, 0xFF, 0, 0, 0, 0, 0, 0, 0] {
            let materialize = addi(REG_T1, REG_X0, i16::from(byte)).to_le_bytes();
            let store = sb(REG_T1, REG_T0, 0).to_le_bytes();
            let position = code
                .windows(8)
                .position(|window| window[0..4] == materialize && window[4..8] == store);
            assert!(
                position.is_some(),
                "kernel must store byte {byte:#04x} to the UART"
            );
        }
        assert_eq!(
            words
                .iter()
                .filter(|word| **word == sb(REG_T1, REG_T0, 0))
                .count(),
            12,
            "exactly one 12-byte malformed header is written"
        );
    }

    // Catches a hello bundle that the MiniBundle validator rejects, which
    // would fail the E2E before QEMU ever boots.
    #[test]
    fn hello_bundle_round_trips_through_the_validator() {
        let bundle = hello_bundle_bytes(&hello_elf_bytes()).unwrap();
        let parsed = minicontainer_bundle::parse(&bundle).unwrap();

        assert_eq!(parsed.manifest.name(), E2E_IMAGE);
        assert_eq!(parsed.elf, hello_elf_bytes());
    }

    // Catches an empty leftover scan that misses names or reports an
    // unreadable directory as leftovers.
    #[test]
    fn leftover_scan_finds_only_payload_roots() {
        let directory = TempDir::create("minictr-e2e-scan-").expect("a scratch directory");
        std::fs::create_dir(directory.path().join("minicontainer-run-1-2")).unwrap();
        std::fs::write(directory.path().join("other"), b"x").unwrap();

        let found = payload_temp_leftovers_in(directory.path());

        assert_eq!(found, vec![directory.path().join("minicontainer-run-1-2")]);
    }

    // Catches losing the pinned kernel revision or ABI tag to an edit.
    #[test]
    fn pinned_inputs_stay_documented() {
        assert_eq!(MINIOS_REPO_URL, "https://github.com/kunihiko-t/minios.git");
        assert_eq!(MINIOS_KERNEL_REV.len(), 40);
        assert_eq!(MINIOS_ABI_TAG, "minios-abi-v0.2.0");
    }

    const HELPER_ENV: &str = "MINICTR_E2E_OUTPUT_HELPER";
    const PIDFILE_ENV: &str = "MINICTR_E2E_PIDFILE";

    // Runs in a fresh copy of this test binary. The parent exercises large
    // piped output and timeouts without requiring QEMU in unit tests.
    #[test]
    #[ignore]
    fn e2e_output_helper() {
        use std::io::Write as _;

        match std::env::var(HELPER_ENV).as_deref() {
            Ok("flood") => {
                let stdout = std::io::stdout();
                let mut out = stdout.lock();
                for _ in 0..64 {
                    out.write_all(&[0x41; 4096]).unwrap();
                }
                drop(out);
                let stderr = std::io::stderr();
                let mut err = stderr.lock();
                for _ in 0..64 {
                    err.write_all(&[0x45; 4096]).unwrap();
                }
            }
            Ok("sleep") => std::thread::sleep(Duration::from_secs(30)),
            Ok("family") => {
                // Spawn a grandchild that outlives this helper, record both
                // PIDs, then sleep so the harness must time out and reap the
                // whole descendant set. The leaked `Child` handle is
                // intentional: dropping it without waiting orphans the
                // grandchild, exactly like a killed `minictr` orphans QEMU.
                let exe = std::env::current_exe().unwrap();
                let grandchild = std::process::Command::new(&exe)
                    .args(helper_args())
                    .env(HELPER_ENV, "sleeper")
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .unwrap();
                let record = format!("{} {}", std::process::id(), grandchild.id());
                std::mem::forget(grandchild);
                if let Ok(path) = std::env::var(PIDFILE_ENV) {
                    std::fs::write(path, record).unwrap();
                }
                std::thread::sleep(Duration::from_secs(120));
            }
            Ok("sleeper") => std::thread::sleep(Duration::from_secs(120)),
            mode => panic!("unknown E2E helper mode: {mode:?}"),
        }
    }

    fn helper_args() -> Vec<OsString> {
        vec![
            OsString::from("--exact"),
            OsString::from("runtime::tests::e2e_output_helper"),
            OsString::from("--nocapture"),
            OsString::from("--ignored"),
        ]
    }

    // Catches waiting for the child before draining its pipes: once either
    // pipe fills, the child blocks forever and the harness hangs.
    #[test]
    fn drain_captures_large_output_without_deadlock() {
        let program = std::env::current_exe().expect("the test binary path must exist");
        let completed = run_with_timeout(
            &program,
            &helper_args(),
            None,
            &[(HELPER_ENV, "flood")],
            Duration::from_secs(60),
        )
        .expect("the flood helper must complete");

        assert!(completed.status.success());
        assert!(
            completed
                .stdout
                .iter()
                .filter(|byte| **byte == 0x41)
                .count()
                >= 262_144,
            "all flooded stdout must be captured, got {} bytes",
            completed.stdout.len(),
        );
        assert!(
            completed
                .stderr
                .iter()
                .filter(|byte| **byte == 0x45)
                .count()
                >= 262_144,
            "all flooded stderr must be captured, got {} bytes",
            completed.stderr.len()
        );
    }

    // Catches a broken interrupt harness: SIGINT to the spawned group
    // must stop the sleeper without touching the test runner itself.
    // The runner surviving this test proves the group isolation.
    #[test]
    fn spawned_group_receives_sigint_without_touching_the_runner() {
        let program = std::env::current_exe().expect("the test binary path must exist");
        let spawned = spawn_in_own_group(
            &program,
            &helper_args(),
            None,
            &[(HELPER_ENV, "sleep")],
            None,
        )
        .expect("the sleep helper must spawn");
        std::thread::sleep(Duration::from_millis(500));
        signal_process_group(spawned.pid, libc::SIGINT)
            .expect("SIGINT must reach the spawned process group");
        let completed =
            wait_for_exit(spawned, Duration::from_secs(10)).expect("SIGINT must stop the sleeper");

        assert_ne!(
            completed.status.code(),
            Some(0),
            "a SIGINT death must not look like success"
        );
    }

    // Catches leaking the child or its reader threads when the E2E limit
    // expires: the runner kills, reaps, and reports promptly.
    #[test]
    fn timeout_kills_and_reaps_promptly() {
        let program = std::env::current_exe().expect("the test binary path must exist");
        let started = Instant::now();
        let error = run_with_timeout(
            &program,
            &helper_args(),
            None,
            &[(HELPER_ENV, "sleep")],
            Duration::from_millis(200),
        )
        .expect_err("the sleep helper must exceed the limit");

        assert!(
            matches!(error, E2EError::TimedOut { .. }),
            "expected a timeout, got {error}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the timeout must reap promptly instead of waiting out the child"
        );
    }

    // Catches orphaning a grandchild when the harness times out: the
    // timeout handler itself must terminate the whole descendant set.
    // This test performs no cleanup kill of its own; it only polls and
    // asserts that both recorded PIDs are gone.
    #[test]
    fn timeout_reaps_an_orphaned_grandchild() {
        let dir = TempDir::create("minictr-e2e-family-").expect("a scratch directory");
        let pidfile = dir.path().join("pids");
        let pidfile_str = pidfile.to_str().expect("a UTF-8 scratch path").to_owned();
        let program = std::env::current_exe().expect("the test binary path must exist");
        let error = run_with_timeout(
            &program,
            &helper_args(),
            None,
            &[(HELPER_ENV, "family"), (PIDFILE_ENV, pidfile_str.as_str())],
            Duration::from_millis(500),
        )
        .expect_err("the family helper must exceed the limit");
        assert!(
            matches!(error, E2EError::TimedOut { .. }),
            "expected a timeout, got {error}"
        );

        let (helper_pid, grand_pid) = wait_pidfile(&pidfile);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let present = all_pids().expect("a process snapshot");
            if !present.contains(&helper_pid) && !present.contains(&grand_pid) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "pids {helper_pid} and {grand_pid} must not survive the timeout"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Reads two PIDs written by the family helper, waiting up to 10 seconds.
    fn wait_pidfile(path: &Path) -> (u32, u32) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(text) = std::fs::read_to_string(path) {
                let mut fields = text.split_whitespace();
                if let (Some(first), Some(second)) = (fields.next(), fields.next())
                    && let (Ok(helper), Ok(grand)) = (first.parse(), second.parse())
                {
                    return (helper, grand);
                }
            }
            assert!(
                Instant::now() < deadline,
                "the family helper must record its PIDs promptly"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    // Catches misreading the process snapshot used as QEMU reap evidence.
    #[test]
    fn qemu_pid_snapshot_parses_ps_output() {
        let text = "  PID ARGS\n    1 /sbin/launchd\n  432 qemu-system-riscv64 -machine virt -m 128M\n  433 /usr/bin/ps -eo pid,args\n  434 /opt/homebrew/bin/qemu-system-riscv64 -machine virt\n  435 muse review qemu-system-riscv64 output\n  436 /tmp/qemu-system-riscv64-wrapper\n";

        assert_eq!(parse_qemu_pids(text), vec![432, 434]);
        assert!(parse_qemu_pids("  PID ARGS\n").is_empty());
        assert!(parse_qemu_pids("").is_empty());
    }

    // Catches missing a fresh QEMU PID or blaming a pre-existing one: the
    // leftover check compares exact PID sets and never signals anything.
    #[test]
    fn new_qemu_check_reports_only_fresh_pids() {
        assert!(check_no_new_qemu(&[7, 9], &[7, 9]).is_ok());
        assert!(check_no_new_qemu(&[], &[]).is_ok());
        assert!(
            matches!(
                check_no_new_qemu(&[7], &[7, 9]),
                Err(E2EError::QemuLeftover(leftovers)) if leftovers == vec![9]
            ),
            "only the fresh PID may be reported"
        );
    }

    // Catches test fixtures inheriting a developer's init.defaultBranch,
    // which makes the harness behave differently across hosts.
    #[test]
    fn fixture_repository_uses_an_explicit_main_branch() {
        let directory = TempDir::create("minictr-e2e-git-").expect("a scratch directory");
        init_fixture_repository(directory.path());

        let output = git_output(directory.path(), &["branch", "--show-current"])
            .unwrap_or_else(|diagnostic| panic!("{diagnostic}"));
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "main");
    }

    // Catches fixture Git failures losing the command, status, or stderr
    // needed to diagnose a host-specific test failure.
    #[test]
    fn git_fixture_failure_preserves_its_diagnostics() {
        let directory = TempDir::create("minictr-e2e-git-").expect("a scratch directory");
        let diagnostic = git_output(directory.path(), &["rev-parse", "HEAD"])
            .expect_err("rev-parse outside a repository must fail");

        assert!(diagnostic.contains("git [\"rev-parse\", \"HEAD\"] failed with status"));
        assert!(diagnostic.contains("stdout:\n"));
        let (_, stderr) = diagnostic
            .split_once("stderr:\n")
            .expect("diagnostic must label stderr");
        assert!(!stderr.trim().is_empty(), "Git must explain the failure");
    }

    // Catches building the E2E kernel from a dirty checkout, where local
    // modifications would silently replace the pinned source.
    #[test]
    fn dirty_checkout_is_reported() {
        let directory = TempDir::create("minictr-e2e-git-").expect("a scratch directory");
        init_fixture_repository(directory.path());
        git(
            directory.path(),
            &[
                "-c",
                "user.email=test@example.com",
                "-c",
                "user.name=test",
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                "init",
            ],
        );

        verify_clean_checkout(directory.path()).expect("a clean checkout must pass");

        std::fs::write(directory.path().join("dirty.txt"), b"dirty").unwrap();
        assert!(
            matches!(
                verify_clean_checkout(directory.path()),
                Err(E2EError::DirtyCheckout { .. })
            ),
            "an untracked file must fail the cleanliness check"
        );
    }

    fn git(current_dir: &Path, args: &[&str]) {
        git_output(current_dir, args).unwrap_or_else(|diagnostic| panic!("{diagnostic}"));
    }

    fn git_output(current_dir: &Path, args: &[&str]) -> Result<std::process::Output, String> {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(current_dir)
            .output()
            .map_err(|error| format!("could not run git {args:?}: {error}"))?;
        if output.status.success() {
            return Ok(output);
        }
        Err(format!(
            "git {args:?} failed with status {}\nstdout:\n{}\nstderr:\n{}",
            output
                .status
                .code()
                .map_or_else(|| "unknown".to_owned(), |code| code.to_string()),
            String::from_utf8_lossy(&output.stdout).trim_end(),
            String::from_utf8_lossy(&output.stderr).trim_end()
        ))
    }

    fn init_fixture_repository(directory: &Path) {
        git(directory, &["init", "--quiet"]);
        git(directory, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    }

    fn commit_fixture(directory: &Path, message: &str) -> String {
        init_fixture_repository(directory);
        git(
            directory,
            &[
                "-c",
                "user.email=test@example.com",
                "-c",
                "user.name=test",
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                message,
            ],
        );
        git_head(directory)
    }

    fn git_head(directory: &Path) -> String {
        let output = git_output(directory, &["rev-parse", "HEAD"])
            .unwrap_or_else(|diagnostic| panic!("{diagnostic}"));
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    // Catches a poisoned checkout failing the gate permanently: when the
    // first attempt fails (e.g. a stale CI cache whose remote no longer
    // serves the rev), prepare_checkout discards the checkout and recovers
    // from a fresh clone instead of failing.
    #[test]
    fn prepare_checkout_recovers_from_a_poisoned_checkout() {
        let origin = TempDir::create("minictr-e2e-origin-").expect("a scratch directory");
        let rev = commit_fixture(origin.path(), "origin");
        let scratch = TempDir::create("minictr-e2e-poison-").expect("a scratch directory");
        let checkout = scratch.path().join("checkout");
        std::fs::create_dir(&checkout).expect("a poison directory");
        commit_fixture(&checkout, "poison");
        git(
            &checkout,
            &["remote", "add", "origin", "definitely-not-a-repository"],
        );

        let mut transcript = String::new();
        let mut log = |line: &str| {
            transcript.push_str(line);
            transcript.push('\n');
        };
        prepare_checkout(
            &checkout,
            &mut log,
            origin.path().to_str().expect("a UTF-8 scratch path"),
            &rev,
        )
        .expect("the retry must recover from a fresh clone");

        assert_eq!(
            git_head(&checkout),
            rev,
            "the recovered checkout must point at the rev"
        );
        assert!(
            transcript.contains("discarding and re-cloning once"),
            "the recovery must be visible in the transcript: {transcript}"
        );
    }

    // Catches retrying a checkout that already succeeded: the fresh-clone
    // retry must only run after a failure, never on the happy path.
    #[test]
    fn prepare_checkout_skips_retry_when_first_attempt_succeeds() {
        let origin = TempDir::create("minictr-e2e-origin-").expect("a scratch directory");
        let rev = commit_fixture(origin.path(), "origin");
        let scratch = TempDir::create("minictr-e2e-fresh-").expect("a scratch directory");
        let checkout = scratch.path().join("checkout");

        let mut transcript = String::new();
        let mut log = |line: &str| {
            transcript.push_str(line);
            transcript.push('\n');
        };
        prepare_checkout(
            &checkout,
            &mut log,
            origin.path().to_str().expect("a UTF-8 scratch path"),
            &rev,
        )
        .expect("a fresh clone must succeed");

        assert_eq!(
            git_head(&checkout),
            rev,
            "the fresh checkout must point at the rev"
        );
        assert!(
            !transcript.contains("discarding and re-cloning once"),
            "a successful first attempt must not retry: {transcript}"
        );
    }

    // Catches swallowing the retry outcome: when the fresh clone also
    // fails, prepare_checkout reports the retry failure after attempting
    // the recovery exactly once.
    #[test]
    fn prepare_checkout_reports_the_retry_failure() {
        let scratch = TempDir::create("minictr-e2e-broken-").expect("a scratch directory");
        let checkout = scratch.path().join("checkout");

        let mut transcript = String::new();
        let mut log = |line: &str| {
            transcript.push_str(line);
            transcript.push('\n');
        };
        let error = prepare_checkout(
            &checkout,
            &mut log,
            "definitely-not-a-repository",
            "missing",
        )
        .expect_err("an unusable origin must fail even after the retry");

        assert!(
            matches!(error, E2EError::CommandFailed { .. }),
            "the retry failure must surface, got {error}"
        );
        assert!(
            transcript.contains("discarding and re-cloning once"),
            "the attempted recovery must be visible in the transcript: {transcript}"
        );
    }

    // Catches leaking a harness scratch directory when a later step fails.
    #[test]
    fn temp_dir_guard_removes_its_root_on_drop() {
        let path = {
            let guard = TempDir::create("minictr-e2e-tmp-").expect("a scratch directory");
            std::fs::write(guard.path().join("file"), b"x").unwrap();
            assert!(guard.path().is_dir());
            guard.path().to_path_buf()
        };

        assert!(!path.exists(), "the scratch root must not survive the drop");
    }

    // Catches a happy path that prepares its store through the Store API
    // instead of the public `image build` command.
    #[test]
    fn happy_path_build_uses_public_image_build_command() {
        let args = minictr_build_args(
            Path::new("/tmp/store"),
            "hello",
            Path::new("/tmp/guest-hello"),
        );

        assert_eq!(
            args,
            vec![
                OsString::from("image"),
                OsString::from("build"),
                OsString::from("--store"),
                OsString::from("/tmp/store"),
                OsString::from("hello"),
                OsString::from("/tmp/guest-hello"),
            ]
        );
    }

    // Catches accepting a malformed `image build` success line as a digest.
    #[test]
    fn happy_path_build_parses_the_digest_line() {
        let digest = "ab".repeat(32);
        let line = format!("hello sha256:{digest}\n");

        assert_eq!(
            parse_build_output(line.as_bytes(), "hello").expect("a valid line must parse"),
            digest
        );
        assert!(
            parse_build_output(b"hello sha256:xyz\n", "hello").is_err(),
            "a short digest must not parse"
        );
        assert!(
            parse_build_output(b"other sha256:ab\n", "hello").is_err(),
            "another image tag must not parse"
        );
        let upper = format!("hello sha256:{}\n", "AB".repeat(32));
        assert!(
            parse_build_output(upper.as_bytes(), "hello").is_err(),
            "an uppercase digest must not parse"
        );
    }

    // Catches a happy path that skips the public `image inspect` command.
    #[test]
    fn happy_path_inspect_uses_public_image_inspect_command() {
        let args = minictr_inspect_args(Path::new("/tmp/store"), "hello");

        assert_eq!(
            args,
            vec![
                OsString::from("image"),
                OsString::from("inspect"),
                OsString::from("--store"),
                OsString::from("/tmp/store"),
                OsString::from("hello"),
            ]
        );
    }

    // Catches accepting a drifted `image inspect` output as the happy image.
    #[test]
    fn happy_path_inspect_checks_five_stable_lines() {
        let digest = "cd".repeat(32);
        let output = format!(
            "tag: hello\nname: hello\ndigest: sha256:{digest}\nargs: 0\nelf-bytes: 60456\n"
        );

        check_inspect_output(output.as_bytes(), "hello", &digest, 60456)
            .expect("matching inspect output must pass");
        assert!(
            check_inspect_output(output.as_bytes(), "hello", &digest, 7).is_err(),
            "a wrong ELF length must fail"
        );
        assert!(
            check_inspect_output(b"tag: hello\n", "hello", &digest, 60456).is_err(),
            "a truncated inspect output must fail"
        );
    }

    // Catches a stress wrapper that drops the scenario, iteration, or inner failure.
    #[test]
    fn stress_iter_reports_the_failed_iteration_and_its_cause() {
        let inner = E2EError::PayloadLeftover(vec![PathBuf::from("/tmp/minicontainer-run-9")]);
        let error = stress_iter("timeout", 2, Err::<(), E2EError>(inner)).unwrap_err();

        let text = error.to_string();
        assert!(text.contains("stress timeout iteration 2 failed"), "{text}");
        assert!(text.contains("/tmp/minicontainer-run-9"), "{text}");
        assert!(std::error::Error::source(&error).is_some());
    }

    // Catches a stress wrapper that also fails successful iterations.
    #[test]
    fn stress_iter_passes_a_successful_value_through() {
        assert_eq!(stress_iter("foreground", 0, Ok(7_u32)).unwrap(), 7);
    }

    // Catches a clean host reported as a leak, and a baseline that cannot be taken.
    #[test]
    fn stress_baseline_then_an_unchanged_host_reports_no_leftovers() {
        let baseline = stress_baseline().expect("baseline snapshot must be readable");
        check_stress_leftovers(&baseline).expect("an unchanged host must pass");
    }

    // Catches the final state check missing or inventing leftovers.
    #[test]
    fn check_no_state_files_reports_every_stray_state_file() {
        let scratch = TempDir::create("minictr-e2e-stress-state-").expect("scratch dir must exist");
        let run = scratch.path.join("run");
        std::fs::create_dir(&run).expect("run dir must exist");
        check_no_state_files(&scratch.path).expect("an empty run dir must pass");

        std::fs::write(run.join("i-11.state"), b"x").expect("state file must exist");
        std::fs::write(run.join("i-12.state"), b"x").expect("state file must exist");
        std::fs::write(run.join("other.bin"), b"x").expect("non-state file must exist");
        let error = check_no_state_files(&scratch.path).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("i-11.state"), "{text}");
        assert!(text.contains("i-12.state"), "{text}");
        assert!(!text.contains("other.bin"), "{text}");
    }

    // Catches the shared store losing one of the two tags.
    #[test]
    fn stress_store_registers_the_hello_and_spin_tags() {
        let store = stress_store(b"elf-bytes").expect("stress store must build");
        let inner = minicontainer_bundle::Store::new(&store.path).expect("store must reopen");
        for tag in [E2E_IMAGE, E2E_SPIN_IMAGE] {
            inner
                .resolve(tag)
                .unwrap_or_else(|_| panic!("tag {tag} must resolve"));
        }
    }
}
