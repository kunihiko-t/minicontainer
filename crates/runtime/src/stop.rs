//! `minictr stop`のinstance停止と残骸回収。
//!
//! 記録されたidentity (pid+token+comm) を照合してからsignalを送るため、
//! pid再利用で無関係なprocessを止めることはない。停止後はpayload dirと
//! state fileを回収する。

use std::error::Error;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use crate::instance::{Probe, probe_identity, read_row};
use crate::{
    CleanupFailure, InstanceDir, InstanceError, InstanceRow, InstanceState, InstanceStatus,
};

/// identity照合つきの停止がprocessへ届くのを待つ間隔。
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// SIGKILL後にprocessが消えるのを待つ上限。カーネルがkillを処理すれば
/// zombie化を含めてほぼ即座に消えるため、この期限に届くのは異常である。
const KILL_BUDGET: Duration = Duration::from_secs(5);

/// payload dirのbasenameが持つべき接頭辞。これ以外のpathはstate fileが
/// 何を書いていても削除しない。
const PAYLOAD_PREFIX: &str = "minicontainer-run-";

/// `stop`一回分の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    /// liveなprocessへsignalを送り、消えたことを確認して残骸を回収した。
    Stopped,
    /// 記録されたprocessは既に居なかった。残骸だけを回収した。
    AlreadyGone,
}

/// 停止処理が成功した場合の報告。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopReport {
    /// 対象のinstance id。
    pub id: String,
    /// 停止の結果。
    pub outcome: StopOutcome,
}

/// `stop`が失敗した理由。
#[derive(Debug)]
pub enum StopError {
    /// instance idが`i-<pid>`のcanonicalな形ではない。
    InvalidId(String),
    /// state fileが存在しない。既に停止済みか、記録されていない。
    Unknown(String),
    /// state fileがcorruptで、process identityを信頼できない。誤った
    /// processを殺す危険があるため、fileにもprocessにも触れない。
    Corrupt(String),
    /// processへsignalを送れなかった。
    Signal(io::Error),
    /// SIGKILL後もprocessが消えなかった。state fileとpayloadは残るため
    /// 再試行できる。
    Survived {
        /// 消えなかったinstanceのid。
        id: String,
    },
    /// 停止自体は完了したが残骸の回収に失敗した。
    Cleanup {
        /// 完了していた停止の結果。
        outcome: StopOutcome,
        /// 回収中に起きた失敗。
        failures: Vec<CleanupFailure>,
    },
    /// state directoryの読み取りに失敗した。
    Instance(InstanceError),
}

impl fmt::Display for StopError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidId(id) => write!(formatter, "invalid instance id: {id}"),
            Self::Unknown(id) => write!(formatter, "instance {id} is not registered"),
            Self::Corrupt(id) => write!(
                formatter,
                "instance {id} state is corrupt; refusing to touch the process"
            ),
            Self::Signal(error) => write!(formatter, "could not signal the instance: {error}"),
            Self::Survived { id } => write!(
                formatter,
                "instance {id} did not die after SIGKILL; state left for retry"
            ),
            Self::Cleanup { outcome, failures } => {
                let outcome = match outcome {
                    StopOutcome::Stopped => "stopped",
                    StopOutcome::AlreadyGone => "already gone",
                };
                write!(formatter, "instance was {outcome} but cleanup failed")?;
                for failure in failures {
                    write!(formatter, ": {failure}")?;
                }
                Ok(())
            }
            Self::Instance(error) => write!(formatter, "instance state failed: {error}"),
        }
    }
}

impl Error for StopError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Signal(error) => Some(error),
            Self::Cleanup { failures, .. } => failures.first().map(|failure| failure as _),
            Self::Instance(error) => Some(error),
            _ => None,
        }
    }
}

impl From<InstanceError> for StopError {
    fn from(error: InstanceError) -> Self {
        Self::Instance(error)
    }
}

impl InstanceDir {
    /// instanceを停止し、payload dirとstate fileを回収する。
    ///
    /// liveなprocessにはまずSIGTERMをprocessへ送り、`grace`の間だけ終了を
    /// 待つ。残っていればSIGKILLで畳み、さらに`KILL_BUDGET`まで消滅を待つ。
    /// signal対象は常に記録されたtoken+commを持つprocessであり、照合に
    /// 一致しなくなったpidへは何も送らない。
    ///
    /// staleなstateはprocessへ触れずに残骸だけを回収して`AlreadyGone`を
    /// 返す。corruptなstateはidentityを信頼できないため一切触れない。
    pub fn stop(&self, id: &str, grace: Duration) -> Result<StopReport, StopError> {
        let Some(path) = self.state_path(id) else {
            return Err(StopError::InvalidId(id.to_owned()));
        };
        let Some(row) = read_row(id.to_owned(), &path)? else {
            return Err(StopError::Unknown(id.to_owned()));
        };
        let InstanceRow::Known { state, status, .. } = row else {
            return Err(StopError::Corrupt(id.to_owned()));
        };

        let outcome = match status {
            InstanceStatus::Stale => StopOutcome::AlreadyGone,
            InstanceStatus::Live => {
                signal_process(state.pid, libc::SIGTERM)?;
                if wait_for_exit(&state, grace) == WaitOutcome::StillAlive {
                    signal_process(state.pid, libc::SIGKILL)?;
                    if wait_for_exit(&state, KILL_BUDGET) == WaitOutcome::StillAlive {
                        return Err(StopError::Survived { id: id.to_owned() });
                    }
                }
                StopOutcome::Stopped
            }
        };

        let mut failures = Vec::new();
        failures.extend(remove_payload_dir(&state.payload));
        if let Err(error) = self.remove_state(id) {
            failures.push(CleanupFailure::Instance(error));
        }
        if failures.is_empty() {
            Ok(StopReport {
                id: id.to_owned(),
                outcome,
            })
        } else {
            Err(StopError::Cleanup { outcome, failures })
        }
    }
}

/// 記録されたprocessが消えるのを待つ。pidが再利用されてidentityが
/// 変わった場合も「記録されたprocessは死んだ」とみなし、そのpidの今の
/// 住人へ追加のsignalを送らないための照合である。
#[derive(Debug, PartialEq, Eq)]
enum WaitOutcome {
    Gone,
    StillAlive,
}

fn wait_for_exit(state: &InstanceState, budget: Duration) -> WaitOutcome {
    let deadline = Instant::now() + budget;
    loop {
        match probe_identity(state.pid) {
            Probe::Gone => return WaitOutcome::Gone,
            Probe::Found(current) if current.token == state.token && current.comm == state.comm => {
                if Instant::now() >= deadline {
                    return WaitOutcome::StillAlive;
                }
            }
            // identityが変わったpidは再利用済みであり、記録したprocessは
            // 既に死んでいる。Transient (exec途中などでidentityが読めない)
            // は生存の可能性を残すため期限まで待ち続ける。
            Probe::Found(_) => return WaitOutcome::Gone,
            Probe::Transient => {
                if Instant::now() >= deadline {
                    return WaitOutcome::StillAlive;
                }
            }
        }
        thread::sleep(STOP_POLL_INTERVAL);
    }
}

/// 記録されたprocessとそのprocess groupへsignalを送る。detachedなQEMUは
/// 自分のgroupを持つためgroupにも届け、groupが無いprocess (group leaderで
/// ない場合) にはpidへのsignalだけが効く。既に消えた対象へのESRCHは
/// 成功として扱う。group宛のEPERMも寛容する — leaderが死にかけてzombie
/// 化したgroupへは配送できないが、その時点で対象は既に消えている。
#[cfg(unix)]
fn signal_process(pid: u32, signal: libc::c_int) -> Result<(), StopError> {
    let pid = pid as libc::pid_t;
    // SAFETY: 単純なsignal配送。対象pidは直前にidentity照合済みである。
    if unsafe { libc::kill(pid, signal) } != 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(StopError::Signal(error));
        }
    }
    // SAFETY: 同上。-pidはpidをleaderとするgroupを指す。
    if unsafe { libc::kill(-pid, signal) } != 0 {
        let error = io::Error::last_os_error();
        if !matches!(error.raw_os_error(), Some(libc::ESRCH) | Some(libc::EPERM)) {
            return Err(StopError::Signal(error));
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn signal_process(_pid: u32, _signal: i32) -> Result<(), StopError> {
    Err(StopError::Signal(io::Error::new(
        io::ErrorKind::Unsupported,
        "instance stop is not supported on this platform",
    )))
}

/// state fileに記録されたpayload dirを、形状を検査してから消す。
///
/// state fileはuserが書き換えられるため、記録pathをそのまま`rm -rf`
/// するわけにはいかない。basenameが`minicontainer-run-`で始まり、
/// canonicalな親dirがcanonicalなTMPDIRそのもので、dir自身がsymlinkで
/// ない場合だけを削除対象にする。
fn remove_payload_dir(path: &Path) -> Vec<CleanupFailure> {
    match checked_payload_dir(path) {
        Ok(Some(canonical)) => match std::fs::remove_dir_all(&canonical) {
            Ok(()) => Vec::new(),
            Err(error) => vec![CleanupFailure::Payload(error)],
        },
        Ok(None) => Vec::new(),
        Err(error) => vec![CleanupFailure::Payload(error)],
    }
}

/// 削除してよいpayload dirのcanonical pathを返す。`None`はdirが既に
/// 存在しないこと、Errは形状が契約に合わないか検査自体が失敗したこと。
fn checked_payload_dir(path: &Path) -> io::Result<Option<PathBuf>> {
    let valid_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(PAYLOAD_PREFIX));
    if !valid_name {
        return Err(unsafe_path(
            "payload dir name does not match minicontainer-run-*",
        ));
    }
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(unsafe_path("payload dir is a symlink or not a directory"));
    }
    let canonical = std::fs::canonicalize(path)?;
    let parent = canonical
        .parent()
        .ok_or_else(|| unsafe_path("payload dir has no parent"))?;
    let temp = std::fs::canonicalize(std::env::temp_dir())?;
    if parent != temp {
        return Err(unsafe_path("payload dir does not live under TMPDIR"));
    }
    Ok(Some(canonical))
}

fn unsafe_path(reason: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, reason)
}

#[cfg(test)]
mod tests {
    use super::{StopError, StopOutcome};
    use crate::instance::{InstanceDir, InstanceState, STATE_SUFFIX};
    use std::{
        env, fs,
        path::PathBuf,
        process::{Child, Command, Stdio},
        sync::atomic::{AtomicU64, Ordering},
        time::Duration,
    };

    const HELPER_ENV: &str = "MINICONTAINER_INSTANCE_HELPER";
    static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);

    // Runs in a fresh copy of this test binary as a long-lived sleeper.
    #[test]
    #[ignore]
    fn instance_helper() {
        match env::var(HELPER_ENV).as_deref() {
            Ok("sleep") => std::thread::sleep(Duration::from_secs(30)),
            mode => panic!("unknown instance helper mode: {mode:?}"),
        }
    }

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn create() -> Self {
            let sequence = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
            let root = env::temp_dir().join(format!(
                "minicontainer-stop-test-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&root).expect("a scratch root must be creatable");
            Self(root)
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn open_dir(root: &TempRoot) -> InstanceDir {
        InstanceDir::open(&root.0).expect("a scratch store root must open")
    }

    /// 記録対象のpayload dir。`stop`の安全検査を通るため、basenameと親dir
    /// は本物の契約どおりに作る。`PayloadTemp`と同じ名前空間を共有するため、
    /// 衝突は別の連番でやり直す。
    fn payload_root() -> PathBuf {
        for _ in 0..64 {
            let sequence = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
            let root = env::temp_dir().join(format!(
                "minicontainer-run-{}-{sequence}",
                std::process::id()
            ));
            match fs::create_dir(&root) {
                Ok(()) => return root,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("a scratch payload dir must be creatable: {error}"),
            }
        }
        panic!("a scratch payload dir must be creatable: name space exhausted")
    }

    fn helper_program() -> PathBuf {
        env::current_exe().expect("the test binary path must exist")
    }

    /// 自分のprocess groupを持つsleeper。`stop`のgroup signalが届く形を
    /// 再現するため、本物のQEMUと同じく`process_group(0)`で起きる。
    #[cfg(unix)]
    fn spawn_group_sleeper() -> Child {
        use std::os::unix::process::CommandExt;
        Command::new(env::current_exe().expect("the test binary path must exist"))
            .args([
                "--ignored",
                "--exact",
                "stop::tests::instance_helper",
                "--nocapture",
            ])
            .env(HELPER_ENV, "sleep")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("the sleeper helper must spawn")
    }

    // Catches a malformed id reaching the filesystem or a signal: anything
    // that is not the canonical `i-<pid>` shape is rejected up front.
    #[test]
    fn stop_rejects_noncanonical_ids() {
        let root = TempRoot::create();
        let dir = open_dir(&root);

        for id in ["x-1", "i-", "i-abc", "i-042", "i-99999999999999999", ""] {
            assert!(
                matches!(
                    dir.stop(id, Duration::from_secs(1)),
                    Err(StopError::InvalidId(_))
                ),
                "{id:?} must be rejected"
            );
        }
    }

    // Catches a second `stop` (or a stop of a never-registered id) looking
    // like success: without a state file there is nothing to verify, so the
    // command must report the instance as unknown.
    #[test]
    fn stop_reports_an_unknown_instance() {
        let root = TempRoot::create();
        let dir = open_dir(&root);

        assert!(matches!(
            dir.stop("i-4242", Duration::from_secs(1)),
            Err(StopError::Unknown(id)) if id == "i-4242"
        ));
    }

    // Catches a corrupt state file being used for a signal decision: with no
    // trustworthy identity the process and the file must both be left alone.
    #[test]
    fn stop_refuses_a_corrupt_state_file() {
        let root = TempRoot::create();
        let dir = open_dir(&root);
        let path = root.0.join("run").join(format!("i-1{STATE_SUFFIX}"));
        fs::write(&path, b"\x00\xffnot a state").unwrap();

        assert!(matches!(
            dir.stop("i-1", Duration::from_secs(1)),
            Err(StopError::Corrupt(id)) if id == "i-1"
        ));
        assert!(path.exists(), "a corrupt file must not be removed by stop");
    }

    // Catches stop sending a signal to a reused pid: a state file whose
    // recorded identity no longer matches the live process must be reaped as
    // debris only, and the unrelated live process must survive.
    #[test]
    #[cfg(unix)]
    fn a_reused_pid_is_stopped_as_debris_without_touching_the_process() {
        let root = TempRoot::create();
        let dir = open_dir(&root);
        let mut sleeper = spawn_group_sleeper();
        let payload = payload_root();
        let forged = InstanceState {
            pid: sleeper.id(),
            token: 1,
            comm: "not-the-real-comm".to_owned(),
            image: "hello".to_owned(),
            started: 0,
            payload: payload.clone(),
        };
        fs::write(
            root.0
                .join("run")
                .join(format!("i-{}{}", sleeper.id(), STATE_SUFFIX)),
            forged.encode(),
        )
        .unwrap();

        let report = dir
            .stop(&format!("i-{}", sleeper.id()), Duration::from_secs(1))
            .unwrap();

        assert_eq!(report.outcome, StopOutcome::AlreadyGone);
        assert!(
            crate::instance::process_exists(sleeper.id()),
            "a pid-reused live process must not be signaled"
        );
        assert!(!payload.exists(), "the stale payload dir must be removed");
        assert!(dir.list().unwrap().is_empty(), "the state file must go");

        sleeper.kill().unwrap();
        sleeper.wait().unwrap();
    }

    // Catches stop leaving a dead process's debris behind: a stale state must
    // report AlreadyGone and remove both the payload dir and the state file.
    #[test]
    #[cfg(unix)]
    fn stop_collects_a_stale_instance_without_signaling() {
        let root = TempRoot::create();
        let dir = open_dir(&root);
        let mut sleeper = spawn_group_sleeper();
        let payload = payload_root();
        let handle = dir
            .register(
                "hello",
                sleeper.id(),
                helper_program().as_os_str(),
                &payload,
            )
            .unwrap();
        sleeper.kill().unwrap();
        sleeper.wait().unwrap();
        let id = handle.id().to_owned();

        let report = dir.stop(&id, Duration::from_secs(1)).unwrap();

        assert_eq!(report.outcome, StopOutcome::AlreadyGone);
        assert!(!payload.exists());
        assert!(dir.list().unwrap().is_empty());
    }

    // Catches the live path doing half its job: SIGTERM must reach the
    // recorded process, the process must actually die, and the payload dir
    // plus the state file must be gone afterwards.
    #[test]
    #[cfg(unix)]
    fn stop_terminates_a_live_instance_and_collects_everything() {
        let root = TempRoot::create();
        let dir = open_dir(&root);
        let mut sleeper = spawn_group_sleeper();
        let payload = payload_root();
        let handle = dir
            .register(
                "hello",
                sleeper.id(),
                helper_program().as_os_str(),
                &payload,
            )
            .unwrap();
        let id = handle.id().to_owned();
        // stopが消滅を待つ間にsleeperがzombieとして残らないよう、reapを
        // 別threadへ委譲する。
        let reaper = std::thread::spawn(move || {
            let _ = sleeper.wait();
        });

        let report = dir.stop(&id, Duration::from_secs(2)).unwrap();

        assert_eq!(report.outcome, StopOutcome::Stopped);
        reaper.join().unwrap();
        assert!(!crate::instance::process_exists(
            report.id[2..].parse().unwrap()
        ));
        assert!(!payload.exists());
        assert!(dir.list().unwrap().is_empty());

        // 二度目のstopはstate fileが無いためunknownを報告する。
        assert!(matches!(
            dir.stop(&id, Duration::from_secs(1)),
            Err(StopError::Unknown(_))
        ));
    }

    // Catches a forged payload path being deleted: the recorded path must
    // pass the basename-and-TMPDIR shape check before removal, so a state
    // file pointing at /etc or another victim dir cannot erase it.
    #[test]
    #[cfg(unix)]
    fn stop_refuses_to_remove_a_payload_dir_outside_the_contract() {
        let root = TempRoot::create();
        let dir = open_dir(&root);
        let victim = TempRoot::create();
        let forged = InstanceState {
            pid: u32::MAX - 2,
            token: 1,
            comm: "qemu".to_owned(),
            image: "img".to_owned(),
            started: 0,
            payload: victim.0.join("minicontainer-run-evil"),
        };
        fs::create_dir(victim.0.join("minicontainer-run-evil")).unwrap();
        let id = format!("i-{}", u32::MAX - 2);
        fs::write(
            root.0.join("run").join(format!("{id}{STATE_SUFFIX}")),
            forged.encode(),
        )
        .unwrap();

        let outcome = dir.stop(&id, Duration::from_secs(1));

        match outcome {
            Err(StopError::Cleanup { outcome, failures }) => {
                assert_eq!(outcome, StopOutcome::AlreadyGone);
                assert!(!failures.is_empty());
            }
            other => panic!("a payload dir outside TMPDIR must fail cleanup, got {other:?}"),
        }
        assert!(
            victim.0.join("minicontainer-run-evil").exists(),
            "a dir outside TMPDIR must survive"
        );
        assert!(
            dir.lookup(&id).unwrap().is_none(),
            "the state file is still removed after the refused payload cleanup"
        );
    }
}
