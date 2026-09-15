//! `<store root>/run/` 以下のinstance state file。
//!
//! 一つのrunはQEMU起動直後に一つの`<id>.state` fileを作り、通常経路では
//! 終了時に削除する。host crashで残ったfileは`minictr ps`がlive/stale/
//! corruptとして表示する。pid単体では再利用後の別processと区別できない
//! ため、記録時のprocess開始tokenとcomm名の照合をlivenessに使う。

use std::{
    error::Error,
    fmt, fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

/// state fileのversion行。先頭が一致しないfileはcorruptとして扱う。
const STATE_VERSION: &[u8] = b"minicontainer-state-v1\n";
/// state fileの上限。version行と4行のfieldを超える正当な内容は存在しない。
const MAX_STATE_LEN: u64 = 1024;
/// instance file名の接頭辞。
const INSTANCE_PREFIX: &str = "i-";
/// instance file名の接尾辞。
const STATE_SUFFIX: &str = ".state";
/// image labelの上限byte数。
const MAX_IMAGE_LEN: usize = 128;
/// comm名の上限byte数。kernelの`comm`は15byteへ切り詰められるため、
/// これを超える記録は手作りのfileだけでありcorruptとして扱う。
const MAX_COMM_LEN: usize = 64;
/// atomic書き込みの一時file名が衝突したときの再試行回数。
const TEMP_CREATE_ATTEMPTS: usize = 8;

static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(0);

/// 起動時に確定するinstance identityの記録。
///
/// `pid`単体は再利用で別processを指し得るため、`token` (process開始時刻の
/// opaque値) と`comm`で同じprocessかを照合する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceState {
    /// QEMU childのPID。
    pub pid: u32,
    /// 記録時のprocess開始token。0は未対応platformで識別不能を表す。
    pub token: u64,
    /// 記録時に観測したcomm名。
    pub comm: String,
    /// `ps`のIMAGE列に出す呼び出し側label。
    pub image: String,
    /// state作成時点のunix epoch秒。`ps`のAGE列の起点。
    pub started: u64,
}

/// `minictr ps`が表示する一行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstanceRow {
    /// parseできたstate。`status`はpid+token+commの照合結果。
    Known {
        /// file名のstem。instanceの公開名。
        id: String,
        /// 記録されたstate。
        state: InstanceState,
        /// pid identity照合の結果。
        status: InstanceStatus,
    },
    /// 上限超過・不正bytes・version不一致のfile。`id`だけは表示できる。
    Corrupt {
        /// file名のstem。
        id: String,
    },
}

/// 記録されたprocessが今も同じprocessとして生きているか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstanceStatus {
    /// pidが生存し、tokenとcommが記録と一致した。
    Live,
    /// pidが死亡したか、再利用された別processだった。
    Stale,
}

impl InstanceStatus {
    /// `ps`のSTATE列の文字列。
    pub const fn name(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Stale => "stale",
        }
    }
}

/// instance state directory操作の失敗。
#[derive(Debug)]
pub enum InstanceError {
    /// state directoryまたはそのrootがsymlinkか不正な種類のfileだった。
    UnsafePath,
    /// image labelが上限を超えるかstate fileに書けない文字を含む。
    UnsafeImage,
    /// 記録対象のprocessを識別できなかった。死亡済みか、identityを取得
    /// できないplatformである。
    UnverifiableProcess,
    /// state fileやdirectoryの読み書きに失敗した。
    Io(io::Error),
}

impl fmt::Display for InstanceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsafePath => write!(formatter, "instance state path is unsafe"),
            Self::UnsafeImage => write!(formatter, "instance image label is unsafe"),
            Self::UnverifiableProcess => {
                write!(formatter, "instance process cannot be identified")
            }
            Self::Io(error) => write!(formatter, "instance state io failed: {error}"),
        }
    }
}

impl Error for InstanceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for InstanceError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// 一つのrunが登録したinstance fileのhandle。cleanupで消す対象を保持する。
#[derive(Debug)]
pub struct InstanceHandle {
    /// file名のstem。`ps`のINSTANCE列に出る公開名。
    id: String,
    path: PathBuf,
}

impl InstanceHandle {
    /// instanceの公開名。
    pub fn id(&self) -> &str {
        &self.id
    }
}

/// `<store root>/run/` に開いたinstance state directory。
///
/// 作成時にrootとdirectoryのsymlinkを拒否し、canonical pathがstore root内
/// に留まることを確認する。書き込みは一時file+renameでatomicに行い、list
/// はupdate途中のtorn readを観測しない。
#[derive(Debug)]
pub struct InstanceDir {
    root: PathBuf,
}

impl InstanceDir {
    /// store rootの`run/`を開く。無ければowner専用 (unix: 0700) で作る。
    ///
    /// rootと`run/`のsymlinkを拒否し、canonical pathがcanonical化した
    /// store rootの内側に留まることだけを受理する。
    pub fn open(store_root: &Path) -> Result<Self, InstanceError> {
        let root_meta = match fs::symlink_metadata(store_root) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir_all(store_root)?;
                fs::symlink_metadata(store_root)?
            }
            Err(error) => return Err(error.into()),
        };
        if root_meta.file_type().is_symlink() || !root_meta.is_dir() {
            return Err(InstanceError::UnsafePath);
        }
        let confined_root = fs::canonicalize(store_root)?;
        let root = store_root.join("run");
        match fs::symlink_metadata(&root) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(InstanceError::UnsafePath);
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                create_private_dir(&root)?;
            }
            Err(error) => return Err(error.into()),
        }
        let canonical = fs::canonicalize(&root)?;
        if !canonical.starts_with(&confined_root) {
            return Err(InstanceError::UnsafePath);
        }
        Ok(Self { root: canonical })
    }

    /// `pid`のprocessを記録し、公開名を持つhandleを返す。
    ///
    /// process identityを取得できないplatformや既に死亡したprocessは
    /// `UnverifiableProcess`で拒否する。fileはatomicに置き換わるため、
    /// 再利用されたpidのstale fileは新しい記録で上書きされる。
    pub fn register(&self, image: &str, pid: u32) -> Result<InstanceHandle, InstanceError> {
        validate_image(image)?;
        let identity = process_identity(pid).ok_or(InstanceError::UnverifiableProcess)?;
        if identity.token == 0 || identity.comm.is_empty() {
            return Err(InstanceError::UnverifiableProcess);
        }
        let state = InstanceState {
            pid,
            token: identity.token,
            comm: identity.comm,
            image: image.to_owned(),
            started: epoch_secs(),
        };
        let id = format!("{INSTANCE_PREFIX}{pid}");
        let path = self.root.join(format!("{id}{STATE_SUFFIX}"));
        atomic_write(&path, &state.encode())?;
        Ok(InstanceHandle { id, path })
    }

    /// 登録済みfileを消す。既に無いfileの削除は成功として扱う。
    pub fn unregister(&self, handle: &InstanceHandle) -> Result<(), InstanceError> {
        match fs::remove_file(&handle.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    /// state fileを一覧する。更新中のfileもrename済みの完全な内容だけを
    /// 読み、listの途中で消えたfileは結果から省く。
    pub fn list(&self) -> Result<Vec<InstanceRow>, InstanceError> {
        let mut rows = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if !name.starts_with(INSTANCE_PREFIX) || !name.ends_with(STATE_SUFFIX) {
                continue;
            }
            // 公開名は`i-<pid>`で、file名から`.state`を除いた部分である。
            let id = name[..name.len() - STATE_SUFFIX.len()].to_owned();
            let path = entry.path();
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            // symlinkのfileは追従せずcorruptとして見せる。
            if metadata.file_type().is_symlink() || metadata.len() > MAX_STATE_LEN {
                rows.push(InstanceRow::Corrupt { id });
                continue;
            }
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(_) => {
                    rows.push(InstanceRow::Corrupt { id });
                    continue;
                }
            };
            rows.push(match InstanceState::parse(&bytes) {
                Some(state) => InstanceRow::Known {
                    id,
                    status: status_of(&state),
                    state,
                },
                None => InstanceRow::Corrupt { id },
            });
        }
        rows.sort_by(|left, right| row_id(left).cmp(row_id(right)));
        Ok(rows)
    }
}

fn row_id(row: &InstanceRow) -> &str {
    match row {
        InstanceRow::Known { id, .. } | InstanceRow::Corrupt { id } => id,
    }
}

/// 記録と現在のprocess identityを照合する。pidが生存しtoken (開始時刻) と
/// commが記録と一致するprocessだけがliveであり、死亡も再利用もstaleである。
fn status_of(state: &InstanceState) -> InstanceStatus {
    match process_identity(state.pid) {
        Some(current) => {
            if state.token != 0 && current.token == state.token && current.comm == state.comm {
                InstanceStatus::Live
            } else {
                InstanceStatus::Stale
            }
        }
        None => InstanceStatus::Stale,
    }
}

/// `ps`のIMAGE列のlabelとして書ける文字列かを検査する。改行や`=`を含む
/// 値は行形式を壊すため拒否する。
fn validate_image(image: &str) -> Result<(), InstanceError> {
    let safe = !image.is_empty()
        && image.len() <= MAX_IMAGE_LEN
        && image
            .bytes()
            .all(|byte| matches!(byte, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b':' | b'/'));
    if safe {
        Ok(())
    } else {
        Err(InstanceError::UnsafeImage)
    }
}

fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

impl InstanceState {
    /// canonical text形式へ書き出す。行の順序はparse側が要求する順序と
    /// 常に一致する。
    fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(STATE_VERSION);
        bytes.extend_from_slice(format!("pid={}\n", self.pid).as_bytes());
        bytes.extend_from_slice(format!("token={}\n", self.token).as_bytes());
        bytes.extend_from_slice(format!("comm={}\n", self.comm).as_bytes());
        bytes.extend_from_slice(format!("image={}\n", self.image).as_bytes());
        bytes.extend_from_slice(format!("started={}\n", self.started).as_bytes());
        bytes
    }

    /// canonical text形式をparseする。version行とfieldの順序・必須性に
    /// 一致しない入力はすべて`None`であり、corruptとして扱われる。
    fn parse(bytes: &[u8]) -> Option<Self> {
        let rest = bytes.strip_prefix(STATE_VERSION)?;
        let mut lines = rest.split(|byte| *byte == b'\n');
        let pid = parse_field(lines.next(), b"pid=", |text| text.parse::<u32>().ok())?;
        let token = parse_field(lines.next(), b"token=", |text| text.parse::<u64>().ok())?;
        let comm = parse_field(lines.next(), b"comm=", |text| {
            (!text.is_empty() && text.len() <= MAX_COMM_LEN).then(|| text.to_owned())
        })?;
        let image = parse_field(lines.next(), b"image=", |text| {
            validate_image(text).ok().map(|()| text.to_owned())
        })?;
        let started = parse_field(lines.next(), b"started=", |text| text.parse::<u64>().ok())?;
        // 最後の改行の後に残るもの、または余分な行は受け付けない。
        if lines.next() != Some(b"") || lines.next().is_some() {
            return None;
        }
        Some(Self {
            pid,
            token,
            comm,
            image,
            started,
        })
    }
}

fn parse_field<'a, T>(
    line: Option<&'a [u8]>,
    prefix: &[u8],
    parse: impl FnOnce(&'a str) -> Option<T>,
) -> Option<T> {
    let value = line
        .and_then(|line| line.strip_prefix(prefix))
        .and_then(|value| std::str::from_utf8(value).ok())?;
    parse(value)
}

/// 同じpidのprocessを再利用と区別するための記録。
struct ProcessIdentity {
    /// process開始時刻のopaque値。単位はplatformごとに異なり、同じplatform
    /// の記録どうしの一致だけを見る。
    token: u64,
    /// 実行file名。kernelが15byteへ切り詰めた値。
    comm: String,
}

#[cfg(target_os = "macos")]
fn process_identity(pid: u32) -> Option<ProcessIdentity> {
    use std::mem::MaybeUninit;
    let mut info = MaybeUninit::<libc::proc_bsdinfo>::uninit();
    // SAFETY: `proc_pidinfo`は成功時に`info`をsize_of分だけ初期化する。
    // 戻り値が構造体サイズと一致しなければ初期化を仮定しない。
    let written = unsafe {
        libc::proc_pidinfo(
            pid as libc::pid_t,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast::<libc::c_void>(),
            std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int,
        )
    };
    if written as usize != std::mem::size_of::<libc::proc_bsdinfo>() {
        return None;
    }
    // SAFETY: 戻り値の検査で初期化済みを確認した。
    let info = unsafe { info.assume_init() };
    // `pbi_comm`はNUL終端の実行file名。ASCIIだけを仮定し、NULまでを取る。
    let comm: String = info
        .pbi_comm
        .iter()
        .take_while(|byte| **byte != 0)
        .map(|byte| (*byte as u8) as char)
        .collect();
    Some(ProcessIdentity {
        token: info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec,
        comm,
    })
}

#[cfg(target_os = "linux")]
fn process_identity(pid: u32) -> Option<ProcessIdentity> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm (field 2) は空白や括弧を含み得るため、最後の`)`までを名前として
    // 取り、その後の空白区切りfieldとして数える。starttimeはfield 22であり、
    // `)`の後ではindex 19 (field 3起点) に来る。
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let comm = stat.get(open + 1..close)?.to_owned();
    let starttime = stat
        .get(close + 2..)?
        .split_whitespace()
        .nth(19)?
        .parse::<u64>()
        .ok()?;
    Some(ProcessIdentity {
        token: starttime,
        comm,
    })
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
fn process_identity(pid: u32) -> Option<ProcessIdentity> {
    let _ = pid;
    // process開始時刻を取得できないplatformではpid再利用を識別できないため、
    // 記録もlive判定も行わない。
    None
}

#[cfg(not(unix))]
fn process_identity(_pid: u32) -> Option<ProcessIdentity> {
    None
}

/// `run/`をowner専用で作る。
fn create_private_dir(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder.create(path)
}

/// 一時fileへ書いてrenameし、listが部分書き込みを観測しないようにする。
fn atomic_write(destination: &Path, bytes: &[u8]) -> io::Result<()> {
    let directory = destination.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "atomic destination has no parent directory",
        )
    })?;

    for _ in 0..TEMP_CREATE_ATTEMPTS {
        let sequence = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
        let temporary = directory.join(format!(
            ".minicontainer-state-{}-{sequence}",
            std::process::id()
        ));
        let mut file = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };

        let write_result = file.write_all(bytes).and_then(|()| file.sync_all());
        drop(file);
        if let Err(error) = write_result {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        if let Err(error) = fs::rename(&temporary, destination) {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        return Ok(());
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique atomic temporary file",
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        InstanceDir, InstanceError, InstanceRow, InstanceState, InstanceStatus, STATE_SUFFIX,
    };
    use std::{
        env, fs,
        path::PathBuf,
        process::{Child, Command},
        sync::atomic::{AtomicU64, Ordering},
        time::{Duration, Instant},
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
                "minicontainer-instance-{}-{sequence}",
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

    fn spawn_sleeper() -> Child {
        Command::new(env::current_exe().expect("the test binary path must exist"))
            .args([
                "--ignored",
                "--exact",
                "instance::tests::instance_helper",
                "--nocapture",
            ])
            .env(HELPER_ENV, "sleep")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("the sleeper helper must spawn")
    }

    // Catches a canonical state file failing to round-trip: fields must come
    // back byte-identical through encode and parse.
    #[test]
    fn state_round_trips_through_canonical_text() {
        let state = InstanceState {
            pid: 4242,
            token: 9_999_999,
            comm: "qemu-system-ris".to_owned(),
            image: "hello".to_owned(),
            started: 1_726_531_200,
        };

        let text = String::from_utf8(state.encode()).unwrap();
        assert_eq!(
            text,
            "minicontainer-state-v1\npid=4242\ntoken=9999999\ncomm=qemu-system-ris\nimage=hello\nstarted=1726531200\n"
        );
        assert_eq!(InstanceState::parse(text.as_bytes()), Some(state));
    }

    // Catches the parser accepting hand-edited or torn files: any deviation
    // from the canonical shape is a rejection, not a guess.
    #[test]
    fn state_parse_rejects_any_deviation() {
        let good = InstanceState {
            pid: 1,
            token: 2,
            comm: "qemu".to_owned(),
            image: "img".to_owned(),
            started: 3,
        }
        .encode();

        for (name, bytes) in [
            (
                "wrong version",
                b"minicontainer-state-v2\npid=1\n".as_slice(),
            ),
            ("empty", b"".as_slice()),
            (
                "missing field",
                b"minicontainer-state-v1\npid=1\ntoken=2\n".as_slice(),
            ),
            (
                "extra line",
                b"minicontainer-state-v1\npid=1\ntoken=2\ncomm=q\nimage=i\nstarted=3\nx=y\n"
                    .as_slice(),
            ),
            (
                "bad pid",
                b"minicontainer-state-v1\npid=x\ntoken=2\ncomm=q\nimage=i\nstarted=3\n".as_slice(),
            ),
            (
                "swapped order",
                b"minicontainer-state-v1\ntoken=2\npid=1\ncomm=q\nimage=i\nstarted=3\n".as_slice(),
            ),
            (
                "empty image",
                b"minicontainer-state-v1\npid=1\ntoken=2\ncomm=q\nimage=\nstarted=3\n".as_slice(),
            ),
            (
                "no trailing newline",
                b"minicontainer-state-v1\npid=1\ntoken=2\ncomm=q\nimage=i\nstarted=3".as_slice(),
            ),
        ] {
            assert!(
                InstanceState::parse(bytes).is_none(),
                "{name} must be rejected"
            );
        }
        assert_eq!(InstanceState::parse(&good).unwrap().pid, 1);
    }

    // Catches following a symlinked root or run directory outside the store.
    #[test]
    #[cfg(unix)]
    fn open_rejects_symlinked_paths() {
        use std::os::unix::fs::symlink;
        let root = TempRoot::create();
        let target = TempRoot::create();

        symlink(&target.0, root.0.join("run")).unwrap();
        assert!(matches!(
            InstanceDir::open(&root.0),
            Err(InstanceError::UnsafePath)
        ));

        let symlinked = TempRoot::create();
        fs::remove_dir_all(&symlinked.0).unwrap();
        symlink(&target.0, &symlinked.0).unwrap();
        assert!(matches!(
            InstanceDir::open(&symlinked.0),
            Err(InstanceError::UnsafePath)
        ));
    }

    // Catches recording a process that cannot be re-identified: a dead pid
    // or an empty identity must never become a file.
    #[test]
    fn register_rejects_an_unverifiable_process() {
        let root = TempRoot::create();
        let dir = open_dir(&root);
        assert!(matches!(
            dir.register("img", u32::MAX - 1),
            Err(InstanceError::UnverifiableProcess)
        ));
        assert!(dir.list().unwrap().is_empty());
    }

    // Catches a live process being reported as anything else: register, then
    // list while the sleeper is alive, and the row must be live with the
    // recorded identity.
    #[test]
    fn a_registered_process_lists_as_live() {
        let root = TempRoot::create();
        let dir = open_dir(&root);
        let mut sleeper = spawn_sleeper();
        let handle = dir.register("hello", sleeper.id()).unwrap();

        let rows = dir.list().unwrap();
        assert_eq!(rows.len(), 1);
        match &rows[0] {
            InstanceRow::Known { id, state, status } => {
                assert_eq!(id, handle.id());
                assert_eq!(*status, InstanceStatus::Live);
                assert_eq!(state.pid, sleeper.id());
                assert_eq!(state.image, "hello");
            }
            InstanceRow::Corrupt { id } => panic!("expected a live row, got corrupt {id}"),
        }

        dir.unregister(&handle).unwrap();
        sleeper.kill().unwrap();
        sleeper.wait().unwrap();
    }

    // Catches a dead pid reading as live: once the recorded process is gone,
    // the leftover file must show stale.
    #[test]
    fn a_dead_process_lists_as_stale() {
        let root = TempRoot::create();
        let dir = open_dir(&root);
        let mut sleeper = spawn_sleeper();
        dir.register("hello", sleeper.id()).unwrap();
        sleeper.kill().unwrap();
        sleeper.wait().unwrap();

        let rows = dir.list().unwrap();
        assert_eq!(rows.len(), 1);
        match &rows[0] {
            InstanceRow::Known { status, .. } => assert_eq!(*status, InstanceStatus::Stale),
            InstanceRow::Corrupt { id } => panic!("expected a stale row, got corrupt {id}"),
        }
    }

    // Catches PID reuse being trusted: a live pid with a forged start token
    // must read as stale, not live.
    #[test]
    fn a_reused_pid_lists_as_stale() {
        let root = TempRoot::create();
        let dir = open_dir(&root);
        let mut sleeper = spawn_sleeper();
        let forged = InstanceState {
            pid: sleeper.id(),
            token: 1,
            comm: "not-the-real-comm".to_owned(),
            image: "hello".to_owned(),
            started: 0,
        };
        fs::write(
            root.0
                .join("run")
                .join(format!("i-{}{}", sleeper.id(), STATE_SUFFIX)),
            forged.encode(),
        )
        .unwrap();

        let rows = dir.list().unwrap();
        assert_eq!(rows.len(), 1);
        match &rows[0] {
            InstanceRow::Known { status, .. } => assert_eq!(*status, InstanceStatus::Stale),
            InstanceRow::Corrupt { id } => panic!("expected a stale row, got corrupt {id}"),
        }

        sleeper.kill().unwrap();
        sleeper.wait().unwrap();
    }

    // Catches corrupt files being hidden or crashing the listing: they must
    // surface as a corrupt row keyed by id.
    #[test]
    fn corrupt_and_oversized_files_list_as_corrupt() {
        let root = TempRoot::create();
        let dir = open_dir(&root);
        fs::write(
            root.0.join("run").join("i-garbage.state"),
            b"\x00\xffnot a state",
        )
        .unwrap();
        fs::write(root.0.join("run").join("i-huge.state"), vec![b'x'; 2048]).unwrap();
        // 無関係なfileはinstanceでもcorruptでもなく、黙って無視する。
        fs::write(root.0.join("run").join("README"), b"hi").unwrap();
        fs::write(root.0.join("run").join("not-state.txt"), b"hi").unwrap();

        let rows = dir.list().unwrap();
        assert_eq!(
            rows,
            vec![
                InstanceRow::Corrupt {
                    id: "i-garbage".to_owned()
                },
                InstanceRow::Corrupt {
                    id: "i-huge".to_owned()
                },
            ]
        );
    }

    // Catches a list concurrent with updates seeing a torn file: every read
    // must land on a complete generation or on none.
    #[test]
    fn list_never_observes_a_torn_state_file() {
        let root = TempRoot::create();
        let dir = open_dir(&root);
        let path = root.0.join("run").join("i-1.state");
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        let writer_path = path.clone();
        let writer_stop = stop.clone();
        let writer = std::thread::spawn(move || {
            let mut counter = 0_u64;
            while !writer_stop.load(Ordering::Relaxed) {
                let state = InstanceState {
                    pid: 1,
                    token: counter,
                    comm: "qemu".to_owned(),
                    image: "img".to_owned(),
                    started: counter,
                };
                super::atomic_write(&writer_path, &state.encode()).unwrap();
                counter += 1;
            }
            counter
        });

        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            for row in dir.list().unwrap() {
                assert!(
                    !matches!(row, InstanceRow::Corrupt { .. }),
                    "a torn or partial state file must never be observed"
                );
            }
        }
        stop.store(true, Ordering::Relaxed);
        assert!(writer.join().unwrap() > 0);
    }

    // Catches unregister failing a missing file: deleting an already-removed
    // instance must be an idempotent success so cleanup can run twice.
    #[test]
    fn unregister_is_idempotent() {
        let root = TempRoot::create();
        let dir = open_dir(&root);
        let mut sleeper = spawn_sleeper();
        let handle = dir.register("hello", sleeper.id()).unwrap();

        dir.unregister(&handle).unwrap();
        dir.unregister(&handle).unwrap();
        assert!(dir.list().unwrap().is_empty());

        sleeper.kill().unwrap();
        sleeper.wait().unwrap();
    }

    // Catches an unsafe image label producing a file that cannot be parsed
    // back: whitespace and '=' would corrupt the line format.
    #[test]
    fn register_rejects_an_unsafe_image_label() {
        let root = TempRoot::create();
        let dir = open_dir(&root);
        let mut sleeper = spawn_sleeper();

        for label in [
            "with space",
            "with=eq",
            "with\nnewline",
            "",
            &"x".repeat(200),
        ] {
            assert!(
                matches!(
                    dir.register(label, sleeper.id()),
                    Err(InstanceError::UnsafeImage)
                ),
                "{label:?} must be rejected"
            );
        }

        sleeper.kill().unwrap();
        sleeper.wait().unwrap();
    }
}
