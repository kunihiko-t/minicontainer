//! opt-in監視の永続結果。CLIとinstance state v1を変更しない内部基盤。

use crate::instance::{Probe, probe_identity};
use crate::{
    OutputSink, ProcessBackend, RunOutcome, RunRequest, Runtime, RuntimeError, SessionEvent,
};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

const MAGIC: &[u8] = b"minicontainer-result-v1\n";
const MAX_RECORD: usize = 2 * 1024 * 1024;
const MAX_LOG: usize = 1024 * 1024;
const MAX_DIAGNOSTIC: usize = 4096;
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// cleanupを含めて確定した結果。guest codeをhost失敗と混同しない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordedExit {
    /// PID順の最初の非zero codeと、PID順の結果。
    Guest {
        code: u32,
        process_exits: Vec<(u32, u32)>,
    },
    /// runtime、回収、記録streamの失敗。診断は上限付きで保存する。
    HostFailure { diagnostic: String },
}

/// 未確定の記録を成功とみなさない観測結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordedObservation {
    /// 未確定で、記録した監視processのidentityが現在も一致する。
    Running,
    /// 放棄、監視役喪失、identity不一致などで結果が不明。
    Unknown,
    /// QEMU終了の検査、reap、cleanupを終えた確定結果。
    Finished(RecordedExit),
}

/// wireにPIDのない共有出力。task別I/Oは提供しない。
#[derive(Debug, Clone, Copy)]
pub enum RecordStream {
    Stdout,
    Stderr,
}
impl RecordStream {
    fn name(self) -> &'static str {
        match self {
            Self::Stdout => "stdout.log",
            Self::Stderr => "stderr.log",
        }
    }
}

/// 最終保存失敗でも、既に完了したruntimeの主結果を保持する。
#[derive(Debug)]
pub struct RecordingFailure {
    /// 永続保存の失敗理由。
    pub error: io::Error,
    /// 実行前の失敗はNone、実行後ならcleanupを含む主結果。
    pub run_result: Option<Result<RunOutcome, RuntimeError>>,
}
impl std::fmt::Display for RecordingFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "run recording failed: {}", self.error)
    }
}
impl std::error::Error for RecordingFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// 明示store/resultsのowner専用記録。既存run/から独立している。
pub struct RunRecords {
    root: PathBuf,
}
/// 一つの監視役に所有させるhandle。別processへの移譲は受け付けない。
pub struct RecordedRun {
    root: PathBuf,
    id: String,
    snapshot: Snapshot,
    stdout: File,
    stderr: File,
    log_bytes: usize,
    finalized: bool,
}

#[derive(Clone)]
struct Snapshot {
    pid: u32,
    token: u64,
    comm: String,
    boot: String,
    sequence: u64,
    abandoned: bool,
    result: Option<RecordedExit>,
    stdout_len: u64,
    stderr_len: u64,
}

impl RunRecords {
    /// symlink、非directory、別ownerや公開permissionの保存先は拒否する。
    /// 既存store自体のmodeを変更せず、results以下を0700/0600に限定する。
    pub fn open(store: &Path) -> io::Result<Self> {
        match fs::symlink_metadata(store) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => private_directory(store)?,
            Err(e) => return Err(e),
            Ok(meta) if !meta.is_dir() || meta.file_type().is_symlink() => return Err(invalid()),
            Ok(_) => {}
        }
        let store = fs::canonicalize(store)?;
        let root = store.join("results");
        match fs::symlink_metadata(&root) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => private_directory(&root)?,
            Err(e) => return Err(e),
            Ok(_) => {}
        }
        checked_directory(&root)?;
        Ok(Self { root })
    }

    /// QEMUを起動する前に、固有名のdirectoryと未確定snapshotをsyncする。
    pub fn begin(&self) -> io::Result<RecordedRun> {
        checked_directory(&self.root)?;
        let pid = std::process::id();
        let Probe::Found(identity) = probe_identity(pid) else {
            return Err(invalid());
        };
        if identity.token == 0 || identity.comm.len() > 64 {
            return Err(invalid());
        }
        let mut snapshot = Snapshot {
            pid,
            token: identity.token,
            comm: identity.comm,
            boot: boot_identity()?,
            sequence: 0,
            abandoned: false,
            result: None,
            stdout_len: 0,
            stderr_len: 0,
        };
        for _ in 0..64 {
            snapshot.sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let id = format!("r-{pid}-{}-{}", snapshot.token, snapshot.sequence);
            let root = self.root.join(&id);
            match private_directory(&root) {
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
                Ok(()) => {}
            }
            let create = || -> io::Result<_> {
                let stdout = private_file(&root.join("stdout.log"))?;
                let stderr = private_file(&root.join("stderr.log"))?;
                atomic_snapshot(&root, &snapshot)?;
                File::open(&self.root)?.sync_all()?;
                Ok(RecordedRun {
                    root: root.clone(),
                    id,
                    snapshot: snapshot.clone(),
                    stdout,
                    stderr,
                    log_bytes: 0,
                    finalized: false,
                })
            };
            return match create() {
                Ok(run) => Ok(run),
                Err(e) => {
                    let _ = fs::remove_dir_all(&root);
                    Err(e)
                }
            };
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "record name collisions",
        ))
    }

    fn directory(&self, id: &str) -> io::Result<PathBuf> {
        checked_directory(&self.root)?;
        let fields: Vec<_> = id.split('-').collect();
        if fields.len() != 4
            || fields[0] != "r"
            || fields[1..]
                .iter()
                .any(|s| s.parse::<u64>().ok().is_none_or(|n| n.to_string() != *s))
        {
            return Err(invalid());
        }
        let root = self.root.join(id);
        checked_directory(&root)?;
        Ok(root)
    }

    /// 旧state fileを解釈・変換せず、明示されたrun記録だけを読む。
    /// 壊れた記録、未知version、欠落fileはerrorであり成功ではない。
    pub fn observe(&self, id: &str) -> io::Result<RecordedObservation> {
        let root = self.directory(id)?;
        let snapshot = Snapshot::decode(&bounded_file(&root.join("snapshot"), MAX_RECORD)?)?;
        if id
            != format!(
                "r-{}-{}-{}",
                snapshot.pid, snapshot.token, snapshot.sequence
            )
        {
            return Err(invalid());
        }
        if let Some(result) = snapshot.result {
            for (name, length) in [
                ("stdout.log", snapshot.stdout_len),
                ("stderr.log", snapshot.stderr_len),
            ] {
                if bounded_file(&root.join(name), MAX_LOG)?.len() as u64 != length {
                    return Err(invalid());
                }
            }
            return Ok(RecordedObservation::Finished(result));
        }
        Ok(
            if !snapshot.abandoned
                && boot_identity().ok().as_ref() == Some(&snapshot.boot)
                && matches!(probe_identity(snapshot.pid), Probe::Found(current)
            if snapshot.token != 0 && current.token == snapshot.token && current.comm == snapshot.comm)
            {
                RecordedObservation::Running
            } else {
                RecordedObservation::Unknown
            },
        )
    }

    /// 共有出力を上限付きで読む。firmware診断やtask別streamは提供しない。
    pub fn read_log(&self, id: &str, stream: RecordStream) -> io::Result<Vec<u8>> {
        bounded_file(&self.directory(id)?.join(stream.name()), MAX_LOG)
    }
}

impl RecordedRun {
    /// PIDだけでは再利用されない固有の保存名。
    pub fn id(&self) -> &str {
        &self.id
    }

    fn owns_record(&self) -> bool {
        self.snapshot.pid == std::process::id()
            && boot_identity().ok().as_ref() == Some(&self.snapshot.boot)
            && matches!(probe_identity(self.snapshot.pid), Probe::Found(current)
                if current.token == self.snapshot.token && current.comm == self.snapshot.comm)
    }

    /// 既存runtimeがQEMUを所有しwait/reapとcleanupを終えるまで監視する。
    /// この関数自体はforkやdaemon登録をせず、呼び出しprocessが監視役になる。
    pub fn supervise<B: ProcessBackend>(
        mut self,
        runtime: &Runtime<B>,
        request: RunRequest<'_>,
    ) -> Result<Result<RunOutcome, RuntimeError>, RecordingFailure> {
        if !self.owns_record() {
            return Err(RecordingFailure {
                error: invalid(),
                run_result: None,
            });
        }
        let result = runtime.run_with_sink(request, &mut self);
        self.snapshot.result = Some(match &result {
            Ok(outcome) => RecordedExit::Guest {
                code: outcome.exit_code,
                process_exits: outcome
                    .process_exits
                    .iter()
                    .map(|p| (p.pid, p.code))
                    .collect(),
            },
            Err(error) => RecordedExit::HostFailure {
                diagnostic: bounded_diagnostic(&error.to_string()),
            },
        });
        let mut save = || -> io::Result<()> {
            self.snapshot.stdout_len = self.stdout.metadata()?.len();
            self.snapshot.stderr_len = self.stderr.metadata()?.len();
            if self
                .snapshot
                .stdout_len
                .checked_add(self.snapshot.stderr_len)
                .is_none_or(|n| n > MAX_LOG as u64)
            {
                return Err(invalid());
            }
            self.stdout.sync_all()?;
            self.stderr.sync_all()?;
            atomic_snapshot(&self.root, &self.snapshot)
        };
        if let Err(error) = save() {
            // Dropは未確定へ倒す。確定結果を永続保存できたとは返さない。
            self.snapshot.result = None;
            return Err(RecordingFailure {
                error,
                run_result: Some(result),
            });
        }
        self.finalized = true;
        Ok(result)
    }
}

impl OutputSink for RecordedRun {
    fn push(&mut self, event: &SessionEvent) -> io::Result<()> {
        if !self.owns_record() {
            return Err(invalid());
        }
        let (file, length, bytes) = match event {
            SessionEvent::Stdout(bytes) => (&mut self.stdout, &mut self.snapshot.stdout_len, bytes),
            SessionEvent::Stderr(bytes) => (&mut self.stderr, &mut self.snapshot.stderr_len, bytes),
            _ => return Ok(()),
        };
        self.log_bytes = self
            .log_bytes
            .checked_add(bytes.len())
            .filter(|n| *n <= MAX_LOG)
            .ok_or_else(invalid)?;
        file.write_all(bytes)?;
        *length += bytes.len() as u64;
        Ok(())
    }
}

impl Drop for RecordedRun {
    fn drop(&mut self) {
        if !self.finalized && self.owns_record() {
            self.snapshot.result = None;
            self.snapshot.abandoned = true;
            let _ = atomic_snapshot(&self.root, &self.snapshot);
        }
    }
}

fn bounded_diagnostic(text: &str) -> String {
    let mut end = text.len().min(MAX_DIAGNOSTIC);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}
fn boot_identity() -> io::Result<String> {
    #[cfg(target_os = "linux")]
    {
        let id = fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
        Ok(id.trim().to_owned())
    }
    #[cfg(target_os = "macos")]
    {
        let mut value = libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        };
        let mut size = std::mem::size_of_val(&value);
        // SAFETY: NUL終端名とtimevalサイズの出力領域を渡し、変更値は渡さない。
        let result = unsafe {
            libc::sysctlbyname(
                c"kern.boottime".as_ptr(),
                (&mut value as *mut libc::timeval).cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if result != 0 || size != std::mem::size_of_val(&value) {
            return Err(io::Error::last_os_error());
        }
        Ok(format!("{}:{}", value.tv_sec, value.tv_usec))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err(invalid())
    }
}
fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid or unsafe run record")
}
fn private_directory(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder.create(path)?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    File::open(parent)?.sync_all()
}
fn checked_directory(path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err(invalid());
    }
    private_metadata(&meta)
}
fn private_metadata(meta: &fs::Metadata) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        // SAFETY: geteuidは引数も副作用もなく現在の実効uidを返す。
        if meta.mode() & 0o077 != 0 || meta.uid() != unsafe { libc::geteuid() } {
            return Err(invalid());
        }
    }
    Ok(())
}
fn private_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path)
}
fn bounded_file(path: &Path, maximum: usize) -> io::Result<Vec<u8>> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > maximum as u64 {
        return Err(invalid());
    }
    private_metadata(&meta)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    let actual = file.metadata()?;
    if !actual.is_file() {
        return Err(invalid());
    }
    private_metadata(&actual)?;
    let mut bytes = Vec::new();
    file.take(maximum as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > maximum {
        return Err(invalid());
    }
    Ok(bytes)
}
fn atomic_snapshot(root: &Path, snapshot: &Snapshot) -> io::Result<()> {
    checked_directory(root)?;
    let temporary = root.join(format!(
        ".snapshot-{}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = private_file(&temporary)?;
        file.write_all(&snapshot.encode())?;
        file.sync_all()?;
        fs::rename(&temporary, root.join("snapshot"))?;
        File::open(root)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

impl Snapshot {
    fn encode(&self) -> Vec<u8> {
        let mut bytes = MAGIC.to_vec();
        bytes.extend(self.pid.to_le_bytes());
        bytes.extend(self.token.to_le_bytes());
        push_blob(&mut bytes, self.comm.as_bytes());
        push_blob(&mut bytes, self.boot.as_bytes());
        bytes.extend(self.sequence.to_le_bytes());
        bytes.extend(self.stdout_len.to_le_bytes());
        bytes.extend(self.stderr_len.to_le_bytes());
        match &self.result {
            None => bytes.push(if self.abandoned { 3 } else { 0 }),
            Some(RecordedExit::Guest {
                code,
                process_exits,
            }) => {
                bytes.push(1);
                bytes.extend(code.to_le_bytes());
                bytes.extend((process_exits.len() as u32).to_le_bytes());
                for (pid, code) in process_exits {
                    bytes.extend(pid.to_le_bytes());
                    bytes.extend(code.to_le_bytes());
                }
            }
            Some(RecordedExit::HostFailure { diagnostic }) => {
                bytes.push(2);
                push_blob(&mut bytes, diagnostic.as_bytes());
            }
        }
        bytes
    }
    fn decode(mut bytes: &[u8]) -> io::Result<Self> {
        bytes = bytes.strip_prefix(MAGIC).ok_or_else(invalid)?;
        let pid = u32::from_le_bytes(take::<4>(&mut bytes)?);
        let token = u64::from_le_bytes(take::<8>(&mut bytes)?);
        let comm = read_text(&mut bytes, 64)?;
        let boot = read_text(&mut bytes, 128)?;
        let sequence = u64::from_le_bytes(take::<8>(&mut bytes)?);
        let stdout_len = u64::from_le_bytes(take::<8>(&mut bytes)?);
        let stderr_len = u64::from_le_bytes(take::<8>(&mut bytes)?);
        if pid == 0
            || token == 0
            || comm.is_empty()
            || boot.is_empty()
            || stdout_len
                .checked_add(stderr_len)
                .is_none_or(|n| n > MAX_LOG as u64)
        {
            return Err(invalid());
        }
        let state = take::<1>(&mut bytes)?[0];
        let result = match state {
            0 | 3 => None,
            1 => {
                let code = u32::from_le_bytes(take::<4>(&mut bytes)?);
                let count = u32::from_le_bytes(take::<4>(&mut bytes)?) as usize;
                if count > MAX_LOG / 8 || bytes.len() != count * 8 {
                    return Err(invalid());
                }
                let mut process_exits = Vec::new();
                for _ in 0..count {
                    let pid = u32::from_le_bytes(take::<4>(&mut bytes)?);
                    let result = u32::from_le_bytes(take::<4>(&mut bytes)?);
                    if process_exits
                        .last()
                        .is_some_and(|(previous, _)| *previous >= pid)
                    {
                        return Err(invalid());
                    }
                    process_exits.push((pid, result));
                }
                if !process_exits.is_empty()
                    && process_exits
                        .iter()
                        .map(|(_, code)| *code)
                        .find(|code| *code != 0)
                        .unwrap_or(0)
                        != code
                {
                    return Err(invalid());
                }
                Some(RecordedExit::Guest {
                    code,
                    process_exits,
                })
            }
            2 => Some(RecordedExit::HostFailure {
                diagnostic: read_text(&mut bytes, MAX_DIAGNOSTIC)?,
            }),
            _ => return Err(invalid()),
        };
        if !bytes.is_empty() {
            return Err(invalid());
        }
        Ok(Self {
            pid,
            token,
            comm,
            boot,
            sequence,
            abandoned: state == 3,
            result,
            stdout_len,
            stderr_len,
        })
    }
}
fn push_blob(bytes: &mut Vec<u8>, blob: &[u8]) {
    bytes.extend((blob.len() as u32).to_le_bytes());
    bytes.extend(blob);
}
fn take<const N: usize>(bytes: &mut &[u8]) -> io::Result<[u8; N]> {
    let (head, tail) = bytes.split_at_checked(N).ok_or_else(invalid)?;
    *bytes = tail;
    head.try_into().map_err(|_| invalid())
}
fn read_text(bytes: &mut &[u8], maximum: usize) -> io::Result<String> {
    let length = u32::from_le_bytes(take::<4>(bytes)?) as usize;
    if length > maximum {
        return Err(invalid());
    }
    let (head, tail) = bytes.split_at_checked(length).ok_or_else(invalid)?;
    *bytes = tail;
    String::from_utf8(head.to_vec()).map_err(|_| invalid())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        path::PathBuf,
        process::Command,
        sync::atomic::{AtomicU64, Ordering},
    };
    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "minicontainer-recording-{}-{}",
                std::process::id(),
                TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            Self(path)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn missing_and_malformed_records_are_not_success() {
        let temp = Temp::new();
        let records = RunRecords::open(&temp.0).unwrap();
        assert!(records.observe("../../outside").is_err());
        assert!(records.observe("r-1-1-0").is_err());
        let run = records.begin().unwrap();
        let path = records.root.join(run.id()).join("snapshot");
        fs::write(&path, b"minicontainer-result-v99\n").unwrap();
        assert!(records.observe(run.id()).is_err());
    }

    #[test]
    fn identity_mismatch_cannot_confirm_a_pending_result() {
        let temp = Temp::new();
        let records = RunRecords::open(&temp.0).unwrap();
        let run = records.begin().unwrap();
        let mut snapshot = run.snapshot.clone();
        snapshot.comm.push_str("-mismatch");
        atomic_snapshot(&run.root, &snapshot).unwrap();
        assert_eq!(
            records.observe(run.id()).unwrap(),
            RecordedObservation::Unknown
        );
    }

    #[test]
    fn abandoned_handle_is_unknown_even_if_process_is_alive() {
        let temp = Temp::new();
        let records = RunRecords::open(&temp.0).unwrap();
        let run = records.begin().unwrap();
        let id = run.id().to_owned();
        drop(run);
        assert_eq!(records.observe(&id).unwrap(), RecordedObservation::Unknown);
    }

    #[test]
    fn interrupted_publish_keeps_the_previous_unconfirmed_snapshot() {
        let temp = Temp::new();
        let records = RunRecords::open(&temp.0).unwrap();
        let run = records.begin().unwrap();
        fs::write(
            run.root.join(".snapshot-interrupted"),
            b"incomplete guest result",
        )
        .unwrap();
        assert_eq!(
            records.observe(run.id()).unwrap(),
            RecordedObservation::Running
        );
        let snapshot = fs::read(run.root.join("snapshot")).unwrap();
        for length in 0..snapshot.len() {
            assert!(Snapshot::decode(&snapshot[..length]).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn records_and_logs_are_private_and_symlinks_are_rejected() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
        let temp = Temp::new();
        let records = RunRecords::open(&temp.0).unwrap();
        let run = records.begin().unwrap();
        assert_eq!(fs::metadata(&records.root).unwrap().mode() & 0o777, 0o700);
        assert_eq!(fs::metadata(&run.root).unwrap().mode() & 0o777, 0o700);
        for name in ["snapshot", "stdout.log", "stderr.log"] {
            assert_eq!(
                fs::metadata(run.root.join(name)).unwrap().mode() & 0o777,
                0o600
            );
        }
        let stdout = run.root.join("stdout.log");
        fs::remove_file(&stdout).unwrap();
        symlink(run.root.join("snapshot"), &stdout).unwrap();
        assert!(records.read_log(run.id(), RecordStream::Stdout).is_err());
        fs::set_permissions(&records.root, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(RunRecords::open(&temp.0).is_err());
    }

    #[test]
    fn supervisor_process_death_leaves_unknown_on_reopen() {
        let temp = Temp::new();
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "recording::tests::record_child", "--nocapture"])
            .env("MINICTR_RECORD_TEST_STORE", &temp.0)
            .status()
            .unwrap();
        assert!(status.success());
        let id = fs::read_to_string(temp.0.join("test-id")).unwrap();
        assert_eq!(
            RunRecords::open(&temp.0).unwrap().observe(&id).unwrap(),
            RecordedObservation::Unknown
        );
    }

    #[test]
    fn inherited_handle_cannot_abandon_another_owner_record() {
        let temp = Temp::new();
        let records = RunRecords::open(&temp.0).unwrap();
        let mut run = records.begin().unwrap();
        let id = run.id().to_owned();
        run.snapshot.pid += 1;
        assert!(
            run.push(&SessionEvent::Stdout(b"foreign".to_vec()))
                .is_err()
        );
        drop(run);
        assert_eq!(records.observe(&id).unwrap(), RecordedObservation::Running);
        assert!(
            records
                .read_log(&id, RecordStream::Stdout)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_different_boot_cannot_be_reported_running() {
        let temp = Temp::new();
        let records = RunRecords::open(&temp.0).unwrap();
        let mut run = records.begin().unwrap();
        run.snapshot.boot.push_str("-previous-boot");
        atomic_snapshot(&run.root, &run.snapshot).unwrap();
        assert_eq!(
            records.observe(run.id()).unwrap(),
            RecordedObservation::Unknown
        );
    }

    #[test]
    fn record_child() {
        if let Some(path) = std::env::var_os("MINICTR_RECORD_TEST_STORE") {
            let store = PathBuf::from(path);
            let run = RunRecords::open(&store).unwrap().begin().unwrap();
            fs::write(store.join("test-id"), run.id()).unwrap();
            // destructorを通さず監視役だけが終了する経路を再現する。
            std::process::exit(0);
        }
    }
}
