//! `minictr run`の実QEMU end-to-end検証。
//!
//! pin留めしたminiOS source revisionからguest kernelをbuildし、決定的なhello
//! MiniBundleを一時storeへimportして`hello` tagを作り、`minictr run`の標準出力、
//! 標準エラー出力、終了code、QEMU回収、一時directory cleanupを確認する。
//!
//! timeoutとmalformed-frameの経路では、非0終了と一時領域の非残留に加え、
//! happy-pathの成功内容が混入していないことも検査する。malformed-frame経路
//! はminiOSを経由せず、不正control headerをUARTへ直接書く最小supervisor
//! kernelをQEMU `-kernel` で起動し、`SessionError::Protocol` になることを
//! 検査する (guestはcontrol headerを直接書けないため)。
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
    process::Command,
    process::ExitStatus,
    thread,
    time::{Duration, Instant},
};

use crate::tools;

/// E2Eがbuildして起動するminiOS sourceの公開URL。
pub const MINIOS_REPO_URL: &str = "https://github.com/kunihiko-t/minios.git";
/// E2EがbuildするminiOS kernelのpin留めrevision (M1統合のmerge commit)。
pub const MINIOS_KERNEL_REV: &str = "9be99255a59d58d19db25b835af0e28a8d2a4036";
/// E2E kernelが実装するGuest ABI tag。変更時は別承認のABI更新が必要である。
pub const MINIOS_ABI_TAG: &str = "minios-abi-v0.1.1";
/// E2Eが解決するimage tag。
pub const E2E_IMAGE: &str = "hello";
/// hello guestが報告する終了code。
pub const E2E_EXIT_CODE: i32 = 42;
/// hello guestの標準出力。
pub const E2E_STDOUT: &[u8] = b"hello stdout\n";
/// hello guestの標準エラー出力。
pub const E2E_STDERR: &[u8] = b"hello stderr\n";

const RISCV_TARGET: &str = "riscv64gc-unknown-none-elf";
const KERNEL_BUILD_TIMEOUT: Duration = Duration::from_secs(600);
const QEMU_RUN_TIMEOUT: Duration = Duration::from_secs(180);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
const CHECKOUT_TIMEOUT: Duration = Duration::from_secs(120);
const HAPPY_PATH_TIMEOUT_MS: &str = "30000";
const SPIN_TIMEOUT_MS: &str = "800";
const POLL_INTERVAL: Duration = Duration::from_millis(50);

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
    let kernel = ensure_kernel(workspace)?;
    log(&format!(
        "e2e: kernel ready at {} (rev {MINIOS_KERNEL_REV}, abi {MINIOS_ABI_TAG})",
        kernel.display()
    ));

    let minictr = workspace.join("target/debug/minictr");
    if !minictr.is_file() {
        return Err(E2EError::MissingMinictr(minictr));
    }

    log("e2e: happy path (`minictr run hello` returns stdout, stderr, exit 42)");
    let happy = run_happy_path(&minictr, &kernel)?;
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

    Ok(transcript)
}

/// 決定的なhello MiniBundle bytesを作る (quick-start example向けに公開)。
pub fn hello_bundle() -> Result<Vec<u8>, E2EError> {
    hello_bundle_bytes(&hello_elf_bytes())
}

/// hello bundleを`store_root`へimportして`hello` tagを作る (quick-start向け)。
pub fn import_hello_store(store_root: &Path) -> Result<[u8; 32], E2EError> {
    let store = minicontainer_bundle::Store::new(store_root)
        .map_err(|error| E2EError::Store(error.to_string()))?;
    let digest = store
        .import(&hello_bundle()?)
        .map_err(|error| E2EError::Store(error.to_string()))?;
    store
        .tag(E2E_IMAGE, digest)
        .map_err(|error| E2EError::Store(error.to_string()))?;
    Ok(digest)
}

/// pin留めrevisionのminiOS kernelをbuildし、そのbinary pathを返す。
///
/// cache checkoutはworkspaceの`target/e2e/minios`に置き、存在すればpin留め
/// revisionへ更新して再利用する。fetchはrevisionが欠けているときだけ行い、
/// checkout後はrevisionとcleanさを毎回検証する。`MINICTR_E2E_MINIOS_DIR`が
/// 絶対pathで与えられた場合はそのcheckoutを読み取り専用として扱い、fetchや
/// checkoutやin-place buildで書き換えず、target directoryだけworkspace側へ
/// 隔離してbuildする。
fn ensure_kernel(workspace: &Path) -> Result<PathBuf, E2EError> {
    match std::env::var_os("MINICTR_E2E_MINIOS_DIR") {
        Some(directory) => {
            let directory = PathBuf::from(directory);
            verify_checkout_rev(&directory)?;
            verify_clean_checkout(&directory)?;
            let target_dir = workspace.join("target/e2e/minios-override-target");
            std::fs::create_dir_all(&target_dir)
                .map_err(|error| E2EError::Store(error.to_string()))?;
            build_kernel(&directory, Some(&target_dir))
        }
        None => {
            let directory = workspace.join("target/e2e/minios");
            prepare_checkout(&directory)?;
            build_kernel(&directory, None)
        }
    }
}

fn prepare_checkout(directory: &Path) -> Result<(), E2EError> {
    if !directory.join(".git").exists() {
        if let Some(parent) = directory.parent() {
            std::fs::create_dir_all(parent).map_err(|error| E2EError::Store(error.to_string()))?;
        }
        run_checked(
            "git",
            &[
                OsString::from("clone"),
                OsString::from(MINIOS_REPO_URL),
                directory.as_os_str().to_owned(),
            ],
            None,
            &[],
            KERNEL_BUILD_TIMEOUT,
        )?;
    }
    if !rev_present(directory) {
        run_checked(
            "git",
            &[
                OsString::from("-C"),
                directory.as_os_str().to_owned(),
                OsString::from("fetch"),
                OsString::from("origin"),
                OsString::from(MINIOS_KERNEL_REV),
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
            OsString::from(MINIOS_KERNEL_REV),
        ],
        None,
        &[],
        CHECKOUT_TIMEOUT,
    )?;
    verify_checkout_rev(directory)?;
    verify_clean_checkout(directory)
}

fn rev_present(directory: &Path) -> bool {
    run_checked(
        "git",
        &[
            OsString::from("-C"),
            directory.as_os_str().to_owned(),
            OsString::from("cat-file"),
            OsString::from("-e"),
            OsString::from(MINIOS_KERNEL_REV),
        ],
        None,
        &[],
        COMMAND_TIMEOUT,
    )
    .is_ok()
}

fn verify_checkout_rev(directory: &Path) -> Result<(), E2EError> {
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
    if actual != MINIOS_KERNEL_REV {
        return Err(E2EError::UnexpectedKernelRev {
            expected: MINIOS_KERNEL_REV.to_owned(),
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
    minicontainer_bundle::build(minicontainer_bundle::ImageSpec {
        name: E2E_IMAGE,
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
    let store = prepare_store(elf)?;
    let store_path = store.path.clone();
    let qemu_before = qemu_pids()?;
    let payload_before = payload_temp_leftovers();
    let started = Instant::now();
    // `minictr`の成否にかかわらず、下の全検査を実行する。どれか一つでも
    // `?`で即返すと、後続の残留物を見逃す。QEMU snapshotはkillせず観測だけ
    // 行い、新規残留があれば失敗にする。報告は固定順 (QEMU残留、snapshot
    // 失敗、payload残留、store残留、command結果) の最初の失敗に決める。
    let outcome = run_minictr(minictr, &store.path, kernel, timeout_ms, E2E_IMAGE);
    let elapsed = started.elapsed();
    let mut failure: Option<E2EError> = None;
    let mut check = |result: Result<(), E2EError>| {
        if failure.is_none() {
            failure = result.err();
        }
    };
    let mut qemu_after = Vec::new();
    match qemu_pids() {
        Ok(pids) => {
            check(check_no_new_qemu(&qemu_before, &pids));
            qemu_after = pids;
        }
        Err(error) => check(Err(error)),
    }
    check(check_no_leftovers(&payload_before));
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

fn run_happy_path(minictr: &Path, kernel: &Path) -> Result<CaseReport, E2EError> {
    let report = run_case(minictr, &hello_elf_bytes(), kernel, HAPPY_PATH_TIMEOUT_MS)?;

    if report.status != Some(E2E_EXIT_CODE) {
        return Err(E2EError::UnexpectedRun {
            case: "happy-path exit code",
            expected: format!("exit {}", E2E_EXIT_CODE),
            actual: format!("status {:?}", report.status),
        });
    }
    if report.stdout != E2E_STDOUT {
        return Err(E2EError::UnexpectedRun {
            case: "happy-path stdout",
            expected: format!("{:?}", E2E_STDOUT),
            actual: format!("{:?}", report.stdout),
        });
    }
    if report.stderr != E2E_STDERR {
        return Err(E2EError::UnexpectedRun {
            case: "happy-path stderr",
            expected: format!("{:?}", E2E_STDERR),
            actual: format!("{:?}", report.stderr),
        });
    }
    Ok(report)
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

fn run_minictr(
    minictr: &Path,
    store: &Path,
    kernel: &Path,
    timeout_ms: &str,
    image: &str,
) -> Result<CompletedProcess, E2EError> {
    run_with_timeout(
        minictr,
        &[
            OsString::from("run"),
            OsString::from("--store"),
            store.as_os_str().to_owned(),
            OsString::from("--kernel"),
            kernel.as_os_str().to_owned(),
            OsString::from("--timeout-ms"),
            OsString::from(timeout_ms),
            OsString::from(image),
        ],
        None,
        &[],
        QEMU_RUN_TIMEOUT,
    )
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
fn run_with_timeout(
    program: impl AsRef<OsStr>,
    args: &[OsString],
    current_dir: Option<&Path>,
    extra_env: &[(&str, &str)],
    timeout: Duration,
) -> Result<CompletedProcess, E2EError> {
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
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| E2EError::Command {
            command: command_line.clone(),
            message: error.to_string(),
        })?;
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
                    kill_process_group(pid);
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
                kill_process_group(pid);
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

/// harnessの子のprocess group全体へSIGKILLを送る。子は `process_group(0)`
/// で自groupのleaderとして起動するため、PIDはPGIDと等しく、子孫 (timeout
/// した`minictr`が残したQEMUなど) まで届く。`kill` binaryの不在や既死PIDは
/// 無視する。呼び出し側は必ず直接のkillとwaitも行う。
fn kill_process_group(pid: u32) {
    let mut command = Command::new("kill");
    command.arg("-KILL");
    command.arg(format!("-{pid}"));
    command.stdout(std::process::Stdio::null());
    command.stderr(std::process::Stdio::null());
    let _ = command.status();
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
        assert_eq!(MINIOS_ABI_TAG, "minios-abi-v0.1.1");
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

    // Catches building the E2E kernel from a dirty checkout, where local
    // modifications would silently replace the pinned source.
    #[test]
    fn dirty_checkout_is_reported() {
        let directory = TempDir::create("minictr-e2e-git-").expect("a scratch directory");
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(directory.path())
                .status()
                .expect("git must run");
            assert!(status.success(), "git {args:?} must succeed");
        };
        git(&["init"]);
        git(&[
            "-c",
            "user.email=test@example.com",
            "-c",
            "user.name=test",
            "commit",
            "--allow-empty",
            "-m",
            "init",
        ]);

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

    // Catches a quick-start store that `minictr run` cannot resolve.
    #[test]
    fn hello_store_import_resolves_the_hello_tag() {
        let directory = TempDir::create("minictr-e2e-store-").expect("a scratch directory");

        import_hello_store(directory.path()).expect("the hello store must build");

        let resolved = minicontainer_bundle::Store::new(directory.path())
            .expect("the store must open")
            .resolve(E2E_IMAGE)
            .expect("the hello tag must resolve");
        assert_eq!(
            resolved,
            hello_bundle().expect("the hello bundle must build")
        );
    }
}
