//! `minictr` binary: store resolveとhost runtime、host入出力を接続する。

mod cli;

use std::{
    ffi::OsString,
    fs::File,
    io::{Read, Write},
    path::Path,
    time::Duration,
};

use minicontainer_bundle::{
    ImageSpec, MAX_BUNDLE_LEN, Store, TagRecord, build, format_digest, parse,
};
use minicontainer_runtime::{RunOutcome, RunRequest, Runtime, RuntimeError, SystemProcessBackend};

use cli::{
    Command, Environ, ImageCommand, RealEnv, ResolvedBuild, ResolvedDoctor, ResolvedInspect,
    ResolvedList, ResolvedRun, VERSION, help, parse_os, resolve, resolve_build, resolve_doctor,
    resolve_inspect, resolve_list,
};

/// usage errorのprocess終了code。
pub const USAGE_EXIT: i32 = 2;
/// host側失敗のprocess終了code。
pub const RUNTIME_EXIT: i32 = 125;
/// 一つ以上のdoctor診断項目が失敗したときのprocess終了code。
pub const DOCTOR_EXIT: i32 = 1;

fn main() {
    let code = real_main(
        std::env::args_os().skip(1),
        &RealEnv,
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    );
    std::process::exit(code);
}

/// OS引数からparse、resolve、実行までを行う。
pub fn real_main(
    arguments: impl IntoIterator<Item = OsString>,
    env: &dyn Environ,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
    let command = match parse_os(arguments) {
        Ok(command) => command,
        Err(error) => {
            let _ = writeln!(stderr, "minictr: {error}");
            let _ = writeln!(stderr, "{help}", help = help());
            return USAGE_EXIT;
        }
    };
    match command {
        Command::Help => {
            let _ = writeln!(stdout, "{help}", help = help());
            0
        }
        Command::Version => {
            let _ = writeln!(stdout, "minictr {version}", version = VERSION);
            0
        }
        Command::Run(args) => {
            let resolved = match resolve(&args, env) {
                Ok(resolved) => resolved,
                Err(error) => {
                    let _ = writeln!(stderr, "minictr: {error}");
                    let _ = writeln!(stderr, "{help}", help = help());
                    return USAGE_EXIT;
                }
            };
            run_resolved(&resolved, &RealRunner, &RealStore, stdout, stderr)
        }
        Command::Doctor(args) => {
            let resolved = match resolve_doctor(&args, env) {
                Ok(resolved) => resolved,
                Err(error) => {
                    let _ = writeln!(stderr, "minictr: {error}");
                    let _ = writeln!(stderr, "{help}", help = help());
                    return USAGE_EXIT;
                }
            };
            doctor_resolved(&resolved, &RealQemuProbe, stdout, stderr)
        }
        Command::Image(command) => match command {
            ImageCommand::Build(args) => {
                let resolved = match resolve_build(&args, env) {
                    Ok(resolved) => resolved,
                    Err(error) => {
                        let _ = writeln!(stderr, "minictr: {error}");
                        let _ = writeln!(stderr, "{help}", help = help());
                        return USAGE_EXIT;
                    }
                };
                build_resolved(&resolved, &RealStore, stdout, stderr)
            }
            ImageCommand::List(args) => {
                let resolved = match resolve_list(&args, env) {
                    Ok(resolved) => resolved,
                    Err(error) => {
                        let _ = writeln!(stderr, "minictr: {error}");
                        let _ = writeln!(stderr, "{help}", help = help());
                        return USAGE_EXIT;
                    }
                };
                list_resolved(&resolved, &RealStore, stdout, stderr)
            }
            ImageCommand::Inspect(args) => {
                let resolved = match resolve_inspect(&args, env) {
                    Ok(resolved) => resolved,
                    Err(error) => {
                        let _ = writeln!(stderr, "minictr: {error}");
                        let _ = writeln!(stderr, "{help}", help = help());
                        return USAGE_EXIT;
                    }
                };
                inspect_resolved(&resolved, &RealStore, stdout, stderr)
            }
        },
    }
}

/// bundle取得の境界。testでは一時storeや偽装で差し替える。
pub trait ImageStore {
    /// image tagを検証済みbundle bytesとして返す。
    fn resolve(&self, store_root: &Path, image: &str) -> Result<Vec<u8>, StoreError>;
    /// bundle bytesをstoreへimportしてdigestを返す。
    fn import(&self, store_root: &Path, bytes: &[u8]) -> Result<[u8; 32], StoreError>;
    /// digestへimage tagを付ける。
    fn tag(&self, store_root: &Path, name: &str, digest: [u8; 32]) -> Result<(), StoreError>;
    /// 登録済みtagをbyte順で返す。
    fn list_tags(&self, store_root: &Path) -> Result<Vec<TagRecord>, StoreError>;
}

/// bundle store失敗の公開分類。
#[derive(Debug)]
pub enum StoreError {
    /// store操作が失敗した。
    Store(minicontainer_bundle::StoreError),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(error) => write!(formatter, "minictr: {error}"),
        }
    }
}

/// 実際のcontent-addressed store。
pub struct RealStore;

impl ImageStore for RealStore {
    fn resolve(&self, store_root: &Path, image: &str) -> Result<Vec<u8>, StoreError> {
        let store = Store::new(store_root).map_err(StoreError::Store)?;
        store.resolve(image).map_err(StoreError::Store)
    }

    fn import(&self, store_root: &Path, bytes: &[u8]) -> Result<[u8; 32], StoreError> {
        let store = Store::new(store_root).map_err(StoreError::Store)?;
        store.import(bytes).map_err(StoreError::Store)
    }

    fn tag(&self, store_root: &Path, name: &str, digest: [u8; 32]) -> Result<(), StoreError> {
        let store = Store::new(store_root).map_err(StoreError::Store)?;
        store.tag(name, digest).map_err(StoreError::Store)
    }

    fn list_tags(&self, store_root: &Path) -> Result<Vec<TagRecord>, StoreError> {
        let store = Store::new(store_root).map_err(StoreError::Store)?;
        store.list_tags().map_err(StoreError::Store)
    }
}

/// image build失敗の公開分類。
#[derive(Debug)]
pub enum BuildError {
    /// ELF入力の読み取りが失敗した。
    ElfIo(std::io::Error),
    /// ELF入力が8 MiB上限を超えた。
    ElfTooLarge,
    /// MiniBundleの構築が失敗した。
    Bundle(minicontainer_bundle::BundleError),
    /// store操作が失敗した。
    Store(minicontainer_bundle::StoreError),
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ElfIo(error) => write!(formatter, "failed to read ELF input: {error}"),
            Self::ElfTooLarge => formatter.write_str("ELF input exceeds the 8 MiB payload limit"),
            Self::Bundle(error) => write!(formatter, "invalid MiniBundle: {error}"),
            Self::Store(error) => write!(formatter, "{error}"),
        }
    }
}

impl From<minicontainer_bundle::BundleError> for BuildError {
    fn from(error: minicontainer_bundle::BundleError) -> Self {
        Self::Bundle(error)
    }
}

impl From<StoreError> for BuildError {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::Store(error) => Self::Store(error),
        }
    }
}

/// guest実行の境界。testではQEMUなしの偽装で差し替える。
pub trait Runner {
    /// bundleをQEMU上で実行してguest outcomeを返す。
    fn run(
        &self,
        bundle: &[u8],
        kernel: &Path,
        timeout: Duration,
    ) -> Result<RunOutcome, RuntimeError>;
}

/// 実際のhost runtime。
pub struct RealRunner;

extern "C" fn ignore_sigint(_signal: libc::c_int) {}

impl Runner for RealRunner {
    fn run(
        &self,
        bundle: &[u8],
        kernel: &Path,
        timeout: Duration,
    ) -> Result<RunOutcome, RuntimeError> {
        // run中のCtrl-CはQEMUにも届く。host側はsignalだけを無視して生存し、
        // runtimeの通常経路でQEMUをreapして一時payloadを削除する。捕捉する
        // handlerはexec後にdefaultへ戻るため、QEMUへ無視設定を継承しない。
        // SAFETY: 空のsignal handlerをmain threadから設定し、handler内では
        // signal-safeでない処理を一切行わない。
        unsafe {
            libc::signal(
                libc::SIGINT,
                ignore_sigint as *const () as libc::sighandler_t,
            );
        }
        let runtime = Runtime::new(SystemProcessBackend::new());
        runtime.run(RunRequest {
            bundle,
            kernel,
            deadline: timeout,
        })
    }
}

/// 解決済みrunを実行し、process終了codeを返す。
pub fn run_resolved(
    resolved: &ResolvedRun,
    runner: &dyn Runner,
    store: &dyn ImageStore,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
    let bundle = match store.resolve(&resolved.store, &resolved.image) {
        Ok(bundle) => bundle,
        Err(error) => {
            let _ = writeln!(stderr, "{error}");
            return RUNTIME_EXIT;
        }
    };
    execute(
        &bundle,
        &resolved.kernel,
        resolved.timeout,
        runner,
        stdout,
        stderr,
    )
}

/// 解決済み`image build`を実行し、process終了codeを返す。
pub fn build_resolved(
    resolved: &ResolvedBuild,
    store: &dyn ImageStore,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
    let digest = match build_and_store(resolved, store) {
        Ok(digest) => digest,
        Err(error) => {
            let _ = writeln!(stderr, "minictr: {error}");
            return RUNTIME_EXIT;
        }
    };
    if let Err(error) = writeln!(
        stdout,
        "{image} sha256:{digest}",
        image = resolved.image,
        digest = format_digest(digest)
    ) {
        let _ = writeln!(stderr, "minictr: failed to write stdout: {error}");
        return RUNTIME_EXIT;
    }
    // `main` ends with `process::exit`, which skips destructors, so a piped
    // success line must be flushed explicitly before reporting success.
    if let Err(error) = stdout.flush() {
        let _ = writeln!(stderr, "minictr: failed to flush stdout: {error}");
        return RUNTIME_EXIT;
    }
    0
}

/// 解決済み`image list`を実行し、process終了codeを返す。
pub fn list_resolved(
    resolved: &ResolvedList,
    store: &dyn ImageStore,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
    let records = match store.list_tags(&resolved.store) {
        Ok(records) => records,
        Err(error) => {
            let _ = writeln!(stderr, "{error}");
            return RUNTIME_EXIT;
        }
    };
    if let Err(error) = writeln!(stdout, "TAG\tDIGEST") {
        let _ = writeln!(stderr, "minictr: failed to write stdout: {error}");
        return RUNTIME_EXIT;
    }
    for record in &records {
        if let Err(error) = writeln!(
            stdout,
            "{name}\tsha256:{digest}",
            name = record.name,
            digest = format_digest(record.digest),
        ) {
            let _ = writeln!(stderr, "minictr: failed to write stdout: {error}");
            return RUNTIME_EXIT;
        }
    }
    if let Err(error) = stdout.flush() {
        let _ = writeln!(stderr, "minictr: failed to flush stdout: {error}");
        return RUNTIME_EXIT;
    }
    0
}

/// 解決済み`image inspect`を実行し、process終了codeを返す。
pub fn inspect_resolved(
    resolved: &ResolvedInspect,
    store: &dyn ImageStore,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
    let bytes = match store.resolve(&resolved.store, &resolved.image) {
        Ok(bytes) => bytes,
        Err(error) => {
            let _ = writeln!(stderr, "{error}");
            return RUNTIME_EXIT;
        }
    };
    let bundle = match parse(&bytes) {
        Ok(bundle) => bundle,
        Err(error) => {
            let _ = writeln!(stderr, "minictr: invalid stored bundle: {error}");
            return RUNTIME_EXIT;
        }
    };
    let output = format!(
        "tag: {tag}\nname: {name}\ndigest: sha256:{digest}\nargs: {args}\nelf-bytes: {elf}\n",
        tag = resolved.image,
        name = bundle.manifest.name(),
        digest = format_digest(bundle.header.digest),
        args = bundle.manifest.args().count(),
        elf = bundle.elf.len(),
    );
    if let Err(error) = stdout.write_all(output.as_bytes()) {
        let _ = writeln!(stderr, "minictr: failed to write stdout: {error}");
        return RUNTIME_EXIT;
    }
    if let Err(error) = stdout.flush() {
        let _ = writeln!(stderr, "minictr: failed to flush stdout: {error}");
        return RUNTIME_EXIT;
    }
    0
}

/// `run`と同じQEMU program。runtimeの起動commandと対にする。
const QEMU_PROGRAM: &str = "qemu-system-riscv64";

/// QEMU version確認の境界。testでは偽装で差し替える。
pub trait QemuProbe {
    /// `qemu-system-riscv64 --version`の標準出力を返す。
    fn version_output(&self) -> Result<String, QemuError>;
}

/// QEMU検査が失敗した理由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QemuError {
    /// programが見つからない。
    Missing,
    /// version確認が非0終了した。
    Failed { status: Option<i32> },
    /// version行を読めない。
    Malformed,
    /// versionが互換性下限未満である。
    TooOld(Version),
}

/// 実際のQEMU version確認。
pub struct RealQemuProbe;

impl QemuProbe for RealQemuProbe {
    fn version_output(&self) -> Result<String, QemuError> {
        let output = std::process::Command::new(QEMU_PROGRAM)
            .arg("--version")
            .output()
            .map_err(|error| match error.kind() {
                std::io::ErrorKind::NotFound => QemuError::Missing,
                _ => QemuError::Failed { status: None },
            })?;
        if !output.status.success() {
            return Err(QemuError::Failed {
                status: output.status.code(),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

/// 三要素のtool version。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    major: u32,
    minor: u32,
    patch: u32,
}

impl Version {
    const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// QEMUの互換性下限。`cargo xtask setup`の検査と対にする。
const QEMU_FLOOR: Version = Version::new(8, 2, 0);

/// QEMU version行をparseし、互換性下限を検査する。
fn parse_qemu_version(output: &str) -> Result<Version, QemuError> {
    let token = output
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("QEMU emulator version "))
        .and_then(|version| version.split_whitespace().next())
        .ok_or(QemuError::Malformed)?;
    let version = parse_numeric_version(token).ok_or(QemuError::Malformed)?;
    if version < QEMU_FLOOR {
        return Err(QemuError::TooOld(version));
    }
    Ok(version)
}

fn parse_numeric_version(token: &str) -> Option<Version> {
    let mut components = token.split('.');
    let major = components.next()?.parse().ok()?;
    let minor = components.next()?.parse().ok()?;
    let patch = components.next()?.parse().ok()?;
    if components.next().is_some() {
        return None;
    }
    Some(Version::new(major, minor, patch))
}

/// host別のQEMU導入手順。`cargo xtask setup`の案内と対にする。
fn qemu_install_hint() -> &'static str {
    #[cfg(target_os = "macos")]
    return "brew install qemu";
    #[cfg(target_os = "linux")]
    return "sudo apt-get install qemu-system-misc";
    #[allow(unreachable_code)]
    "install a QEMU package that provides qemu-system-riscv64"
}

/// host別のQEMU更新手順。`cargo xtask setup`の案内と対にする。
fn qemu_upgrade_hint() -> &'static str {
    #[cfg(target_os = "macos")]
    return "brew upgrade qemu";
    #[cfg(target_os = "linux")]
    return "sudo apt-get update && sudo apt-get install qemu-system-misc";
    #[allow(unreachable_code)]
    "upgrade the installed QEMU package"
}

fn qemu_failure(error: QemuError) -> String {
    match error {
        QemuError::Missing => format!(
            "{QEMU_PROGRAM} is not installed; fix: {}",
            qemu_install_hint()
        ),
        QemuError::Failed { status } => format!(
            "{QEMU_PROGRAM} --version failed with status {}; fix: reinstall QEMU",
            status
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".to_owned())
        ),
        QemuError::Malformed => {
            format!("could not parse {QEMU_PROGRAM} --version output; fix: reinstall QEMU")
        }
        QemuError::TooOld(version) => format!(
            "QEMU {version} is too old ({QEMU_FLOOR} or newer is required); fix: {}",
            qemu_upgrade_hint()
        ),
    }
}

/// QEMUの存在とversion下限を検査する。成功時は表示用のprogramとversionを返す。
fn check_qemu(probe: &dyn QemuProbe) -> Result<String, String> {
    let version = probe
        .version_output()
        .and_then(|output| parse_qemu_version(&output))
        .map_err(qemu_failure)?;
    Ok(format!("{QEMU_PROGRAM} {version}"))
}

/// kernel fileの存在と種別をmetadataだけで検査する。成功時は表示用の
/// pathとbyte数を返す。環境は変更しない。
fn check_kernel(kernel: &Path) -> Result<String, String> {
    let path = display_path(kernel);
    let metadata = std::fs::metadata(kernel).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => format!(
            "kernel file is missing at {path}; fix: build the miniOS kernel and pass --kernel PATH"
        ),
        _ => format!("cannot stat kernel file at {path}; fix: check the path"),
    })?;
    if !metadata.is_file() {
        return Err(format!(
            "kernel path is not a file at {path}; fix: build the miniOS kernel and pass --kernel PATH"
        ));
    }
    if metadata.len() == 0 {
        return Err(format!(
            "kernel file is empty at {path}; fix: rebuild the miniOS kernel and pass --kernel PATH"
        ));
    }
    Ok(format!("{path} ({} bytes)", metadata.len()))
}

/// store rootの存在と種別をmetadataだけで検査する。成功時は表示用の
/// pathを返す。`Store::new`と違い、存在しないstoreを作らない。
fn check_store(store: &Path) -> Result<String, String> {
    let path = display_path(store);
    let metadata = std::fs::metadata(store).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => format!(
            "store root is missing at {path}; fix: run minictr image build --store {path} IMAGE ELF"
        ),
        _ => format!("cannot stat store root at {path}; fix: check the path"),
    })?;
    if !metadata.is_dir() {
        return Err(format!(
            "store root is not a directory at {path}; fix: pass --store PATH or set MINICTR_STORE"
        ));
    }
    Ok(path)
}

/// 診断行へ載せるpath表示。非UTF-8は置換し、改行はescapeして一行を保つ。
fn display_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\r', "\\r")
        .replace('\n', "\\n")
}

/// 解決済みdoctorを実行し、process終了codeを返す。QEMU、kernel、store
/// の検査をすべて列挙し、一つでも失敗したら1で終わる。環境は変更しない。
pub fn doctor_resolved(
    resolved: &ResolvedDoctor,
    probe: &dyn QemuProbe,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
    let checks = [
        ("qemu", check_qemu(probe)),
        ("kernel", check_kernel(&resolved.kernel)),
        ("store", check_store(&resolved.store)),
    ];
    let total = checks.len();
    let mut passed = 0;
    let mut report = String::new();
    for (name, result) in &checks {
        match result {
            Ok(detail) => {
                passed += 1;
                report.push_str(&format!("{name}: ok {detail}\n"));
            }
            Err(reason) => {
                report.push_str(&format!("{name}: fail {reason}\n"));
            }
        }
    }
    report.push_str(&format!("summary: passed {passed}/{total} checks\n"));
    if let Err(error) = stdout.write_all(report.as_bytes()) {
        let _ = writeln!(stderr, "minictr: failed to write stdout: {error}");
        return RUNTIME_EXIT;
    }
    if let Err(error) = stdout.flush() {
        let _ = writeln!(stderr, "minictr: failed to flush stdout: {error}");
        return RUNTIME_EXIT;
    }
    if passed == total { 0 } else { DOCTOR_EXIT }
}

/// ELFのmetadata確認、上限付きread、bundle構築、import、tagを順に行う。
/// ELFの中身は検証せずminiOSのloaderへ委ねる。tag失敗後に残る未参照
/// blobは消さない。同じbytesは再利用でき、rollbackのほうがstore操作を
/// 複雑にするためである。
fn build_and_store(
    resolved: &ResolvedBuild,
    store: &dyn ImageStore,
) -> Result<[u8; 32], BuildError> {
    let elf = read_elf(&resolved.elf)?;
    let args: Vec<&str> = resolved.args.iter().map(String::as_str).collect();
    let bundle = build(ImageSpec {
        name: &resolved.image,
        args: &args,
        elf: &elf,
    })?;
    let digest = store.import(&resolved.store, &bundle)?;
    store.tag(&resolved.store, &resolved.image, digest)?;
    Ok(digest)
}

/// ELF入力をmetadata確認つきの上限付きで読む。本体を読む前に8 MiBを
/// 超える入力を拒否し、metadata確認後に伸びた入力もsentinelで拒否する。
fn read_elf(path: &Path) -> Result<Vec<u8>, BuildError> {
    read_elf_with(path, &|path| File::open(path))
}

/// `read_elf`の本体。open境界だけを注入可能にし、metadata確認を先に
/// 行う順序は変えない。
fn read_elf_with(
    path: &Path,
    open: &dyn Fn(&Path) -> std::io::Result<File>,
) -> Result<Vec<u8>, BuildError> {
    if std::fs::metadata(path).map_err(BuildError::ElfIo)?.len() > MAX_BUNDLE_LEN {
        return Err(BuildError::ElfTooLarge);
    }
    let file = open(path).map_err(BuildError::ElfIo)?;
    if file.metadata().map_err(BuildError::ElfIo)?.len() > MAX_BUNDLE_LEN {
        return Err(BuildError::ElfTooLarge);
    }
    let mut bounded = file.take(MAX_BUNDLE_LEN + 1);
    let mut bytes = Vec::new();
    bounded.read_to_end(&mut bytes).map_err(BuildError::ElfIo)?;
    let len = u64::try_from(bytes.len()).map_err(|_| BuildError::ElfTooLarge)?;
    if len > MAX_BUNDLE_LEN {
        return Err(BuildError::ElfTooLarge);
    }
    Ok(bytes)
}

/// 取得済みbundleを実行し、guest入出力をhostへ接続する。
pub fn execute(
    bundle: &[u8],
    kernel: &Path,
    timeout: Duration,
    runner: &dyn Runner,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
    let outcome = match runner.run(bundle, kernel, timeout) {
        Ok(outcome) => outcome,
        Err(error) => {
            let _ = writeln!(stderr, "minictr: {error}");
            return RUNTIME_EXIT;
        }
    };
    if let Err(error) = stdout.write_all(&outcome.stdout) {
        let _ = writeln!(stderr, "minictr: failed to write stdout: {error}");
        return RUNTIME_EXIT;
    }
    if let Err(error) = stderr.write_all(&outcome.stderr) {
        let _ = writeln!(stderr, "minictr: failed to write stderr: {error}");
        return RUNTIME_EXIT;
    }
    // `main` ends with `process::exit`, which skips destructors, so buffered
    // standard streams must be flushed explicitly before reporting success.
    if let Err(error) = stdout.flush() {
        let _ = writeln!(stderr, "minictr: failed to flush stdout: {error}");
        return RUNTIME_EXIT;
    }
    if let Err(error) = stderr.flush() {
        let _ = writeln!(stderr, "minictr: failed to flush stderr: {error}");
        return RUNTIME_EXIT;
    }
    match u8::try_from(outcome.exit_code) {
        Ok(code) => i32::from(code),
        Err(_) => {
            let _ = writeln!(
                stderr,
                "minictr: guest exit code {} is outside 0-255",
                outcome.exit_code
            );
            RUNTIME_EXIT
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use minicontainer_bundle::{ImageSpec, build};
    use std::{
        cell::{Cell, RefCell},
        collections::VecDeque,
        ffi::OsString,
        io,
        sync::atomic::{AtomicU64, Ordering},
    };

    static NEXT_TEMP_STORE: AtomicU64 = AtomicU64::new(0);

    struct UnusedEnv;

    impl Environ for UnusedEnv {
        fn store_override(&self) -> Option<OsString> {
            None
        }

        fn kernel_override(&self) -> Option<OsString> {
            None
        }

        fn home(&self) -> Option<OsString> {
            None
        }
    }

    struct FakeRunner {
        results: RefCell<VecDeque<Result<RunOutcome, RuntimeError>>>,
    }

    impl FakeRunner {
        fn ok(outcome: RunOutcome) -> Self {
            Self {
                results: RefCell::new(VecDeque::from([Ok(outcome)])),
            }
        }

        fn err(error: RuntimeError) -> Self {
            Self {
                results: RefCell::new(VecDeque::from([Err(error)])),
            }
        }
    }

    impl Runner for FakeRunner {
        fn run(
            &self,
            _bundle: &[u8],
            _kernel: &Path,
            _timeout: Duration,
        ) -> Result<RunOutcome, RuntimeError> {
            self.results.borrow_mut().pop_front().expect("one run")
        }
    }

    struct FakeStore {
        result: Result<Vec<u8>, String>,
    }

    impl ImageStore for FakeStore {
        fn resolve(&self, _store_root: &Path, _image: &str) -> Result<Vec<u8>, StoreError> {
            self.result
                .clone()
                .map_err(|_| StoreError::Store(minicontainer_bundle::StoreError::UnsafeStorePath))
        }

        fn import(&self, _store_root: &Path, _bytes: &[u8]) -> Result<[u8; 32], StoreError> {
            panic!("run tests do not import bundles");
        }

        fn tag(
            &self,
            _store_root: &Path,
            _name: &str,
            _digest: [u8; 32],
        ) -> Result<(), StoreError> {
            panic!("run tests do not tag bundles");
        }

        fn list_tags(&self, _store_root: &Path) -> Result<Vec<TagRecord>, StoreError> {
            panic!("run tests do not list tags");
        }
    }

    struct QueryStore {
        bundle: Vec<u8>,
        records: Vec<TagRecord>,
    }

    impl ImageStore for QueryStore {
        fn resolve(&self, _store_root: &Path, _image: &str) -> Result<Vec<u8>, StoreError> {
            Ok(self.bundle.clone())
        }

        fn import(&self, _store_root: &Path, _bytes: &[u8]) -> Result<[u8; 32], StoreError> {
            panic!("image query tests do not import bundles");
        }

        fn tag(
            &self,
            _store_root: &Path,
            _name: &str,
            _digest: [u8; 32],
        ) -> Result<(), StoreError> {
            panic!("image query tests do not tag bundles");
        }

        fn list_tags(&self, _store_root: &Path) -> Result<Vec<TagRecord>, StoreError> {
            Ok(self.records.clone())
        }
    }

    struct RecordingStore {
        calls: RefCell<Vec<String>>,
        imported: RefCell<Vec<u8>>,
        tagged: RefCell<Vec<(String, [u8; 32])>>,
        digest: [u8; 32],
        fail_tag: bool,
    }

    impl RecordingStore {
        fn ok(digest: [u8; 32]) -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                imported: RefCell::new(Vec::new()),
                tagged: RefCell::new(Vec::new()),
                digest,
                fail_tag: false,
            }
        }

        fn tag_fails(digest: [u8; 32]) -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                imported: RefCell::new(Vec::new()),
                tagged: RefCell::new(Vec::new()),
                digest,
                fail_tag: true,
            }
        }
    }

    impl ImageStore for RecordingStore {
        fn resolve(&self, _store_root: &Path, _image: &str) -> Result<Vec<u8>, StoreError> {
            panic!("image build tests do not resolve tags");
        }

        fn import(&self, _store_root: &Path, bytes: &[u8]) -> Result<[u8; 32], StoreError> {
            self.calls.borrow_mut().push("import".to_owned());
            *self.imported.borrow_mut() = bytes.to_vec();
            Ok(self.digest)
        }

        fn tag(&self, _store_root: &Path, name: &str, digest: [u8; 32]) -> Result<(), StoreError> {
            self.calls.borrow_mut().push(format!("tag {name}"));
            self.tagged.borrow_mut().push((name.to_owned(), digest));
            if self.fail_tag {
                return Err(StoreError::Store(
                    minicontainer_bundle::StoreError::UnsafeStorePath,
                ));
            }
            Ok(())
        }

        fn list_tags(&self, _store_root: &Path) -> Result<Vec<TagRecord>, StoreError> {
            panic!("image build tests do not list tags");
        }
    }

    fn resolved_build(elf: &Path) -> ResolvedBuild {
        ResolvedBuild {
            image: "hello".to_owned(),
            elf: elf.to_path_buf(),
            args: vec!["fast".to_owned()],
            store: Path::new("/store").to_path_buf(),
        }
    }

    fn write_temp_elf(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let id = NEXT_TEMP_STORE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "minictr-test-elf-{}-{id}-{name}",
            std::process::id()
        ));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn outcome(stdout: &[u8], stderr: &[u8], exit_code: u32) -> RunOutcome {
        RunOutcome {
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
            exit_code,
            diagnostics: Vec::new(),
        }
    }

    fn resolved() -> ResolvedRun {
        ResolvedRun {
            image: "hello".to_owned(),
            store: Path::new("/store").to_path_buf(),
            kernel: Path::new("/kernel").to_path_buf(),
            timeout: Duration::from_secs(5),
        }
    }

    struct FailingWriter {
        message: &'static str,
    }

    impl Write for FailingWriter {
        fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
            Err(io::Error::other(self.message))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct FailAfterWriter {
        remaining: usize,
        message: &'static str,
    }

    impl Write for FailAfterWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.remaining == 0 {
                return Err(io::Error::other(self.message));
            }
            let written = bytes.len().min(self.remaining);
            self.remaining -= written;
            Ok(written)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Buffers writes like a piped standard stream and reports the buffered
    /// bytes only through an explicit flush, which can fail.
    struct FlushFailingWriter {
        buffered: Vec<u8>,
        message: &'static str,
    }

    impl Write for FlushFailingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.buffered.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other(self.message))
        }
    }

    // Catches losing guest bytes or remapping a normal guest exit code.
    #[test]
    fn forwards_guest_streams_and_exit_code() {
        let runner = FakeRunner::ok(outcome(b"out", b"err", 42));
        let store = FakeStore {
            result: Ok(vec![1, 2, 3]),
        };
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = run_resolved(&resolved(), &runner, &store, &mut stdout, &mut stderr);

        assert_eq!(code, 42);
        assert_eq!(stdout, b"out");
        assert_eq!(stderr, b"err");
    }

    // Catches mapping a nonzero guest exit to success or to a host error.
    #[test]
    fn preserves_a_nonzero_guest_exit() {
        let runner = FakeRunner::ok(outcome(b"", b"", 3));
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = execute(
            b"bundle",
            Path::new("/kernel"),
            Duration::from_secs(5),
            &runner,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(code, 3);
        assert!(stderr.is_empty());
    }

    // Catches truncating a guest exit code that does not fit a Unix status.
    #[test]
    fn maps_an_out_of_range_guest_exit_to_a_host_error() {
        let runner = FakeRunner::ok(outcome(b"out", b"", 256));
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = execute(
            b"bundle",
            Path::new("/kernel"),
            Duration::from_secs(5),
            &runner,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(code, RUNTIME_EXIT);
        assert_eq!(stdout, b"out");
        assert!(String::from_utf8_lossy(&stderr).contains("outside 0-255"));
    }

    // Catches reporting a host runtime failure as a guest exit.
    #[test]
    fn maps_a_runtime_failure_to_a_host_error() {
        let runner = FakeRunner::err(RuntimeError::Io(io::Error::other("qemu gone")));
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = execute(
            b"bundle",
            Path::new("/kernel"),
            Duration::from_secs(5),
            &runner,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(code, RUNTIME_EXIT);
        assert!(String::from_utf8_lossy(&stderr).contains("minictr:"));
    }

    // Catches ignoring a broken host stdout pipe and exiting zero.
    #[test]
    fn maps_a_stdout_write_failure_to_a_host_error() {
        let runner = FakeRunner::ok(outcome(b"out", b"", 0));
        let mut stdout = FailingWriter {
            message: "stdout broken",
        };
        let mut stderr = Vec::new();

        let code = execute(
            b"bundle",
            Path::new("/kernel"),
            Duration::from_secs(5),
            &runner,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(code, RUNTIME_EXIT);
        assert!(String::from_utf8_lossy(&stderr).contains("failed to write stdout"));
    }

    // Catches ignoring a broken host stderr pipe and exiting zero.
    #[test]
    fn maps_a_stderr_write_failure_to_a_host_error() {
        let runner = FakeRunner::ok(outcome(b"", b"err", 0));
        let mut stdout = Vec::new();
        let mut stderr = FailingWriter {
            message: "stderr broken",
        };

        let code = execute(
            b"bundle",
            Path::new("/kernel"),
            Duration::from_secs(5),
            &runner,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(code, RUNTIME_EXIT);
        assert_eq!(stdout, b"");
    }

    // Catches losing buffered guest bytes when the host stdout flush fails:
    // without an explicit flush, `process::exit` would still report success.
    #[test]
    fn maps_a_stdout_flush_failure_to_a_host_error() {
        let runner = FakeRunner::ok(outcome(b"out", b"", 42));
        let mut stdout = FlushFailingWriter {
            buffered: Vec::new(),
            message: "stdout flush broken",
        };
        let mut stderr = Vec::new();

        let code = execute(
            b"bundle",
            Path::new("/kernel"),
            Duration::from_secs(5),
            &runner,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(code, RUNTIME_EXIT);
        assert!(
            String::from_utf8_lossy(&stderr).contains("failed to flush stdout"),
            "stderr was {:?}",
            String::from_utf8_lossy(&stderr)
        );
    }

    // Catches losing buffered guest bytes when the host stderr flush fails.
    #[test]
    fn maps_a_stderr_flush_failure_to_a_host_error() {
        let runner = FakeRunner::ok(outcome(b"", b"err", 42));
        let mut stdout = Vec::new();
        let mut stderr = FlushFailingWriter {
            buffered: Vec::new(),
            message: "stderr flush broken",
        };

        let code = execute(
            b"bundle",
            Path::new("/kernel"),
            Duration::from_secs(5),
            &runner,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(code, RUNTIME_EXIT);
        assert_eq!(stdout, b"");
    }

    // Catches reporting a store failure as usage or as a guest exit.
    #[test]
    fn maps_a_store_failure_to_a_host_error() {
        struct FailingStore;
        impl ImageStore for FailingStore {
            fn resolve(&self, _root: &Path, _image: &str) -> Result<Vec<u8>, StoreError> {
                Err(StoreError::Store(
                    minicontainer_bundle::StoreError::RootNotAbsolute,
                ))
            }

            fn import(&self, _root: &Path, _bytes: &[u8]) -> Result<[u8; 32], StoreError> {
                panic!("run tests do not import bundles");
            }

            fn tag(&self, _root: &Path, _name: &str, _digest: [u8; 32]) -> Result<(), StoreError> {
                panic!("run tests do not tag bundles");
            }

            fn list_tags(&self, _root: &Path) -> Result<Vec<TagRecord>, StoreError> {
                panic!("run tests do not list tags");
            }
        }

        let runner = FakeRunner::ok(outcome(b"", b"", 0));
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = run_resolved(
            &resolved(),
            &runner,
            &FailingStore,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(code, RUNTIME_EXIT);
        assert!(String::from_utf8_lossy(&stderr).contains("minictr:"));
    }

    // Catches exiting nonzero without telling the operator how to invoke us.
    #[test]
    fn usage_errors_print_usage_and_exit_2() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = real_main(
            [OsString::from("run")],
            &UnusedEnv,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(code, USAGE_EXIT);
        assert!(String::from_utf8_lossy(&stderr).contains("usage: minictr run"));
    }

    // Catches rejecting undecodable OS arguments with a panic or success.
    #[cfg(unix)]
    #[test]
    fn non_utf8_argument_prints_usage_and_exits_2() {
        use std::os::unix::ffi::OsStringExt;

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = real_main(
            [OsString::from("run"), OsString::from_vec(vec![0xff])],
            &UnusedEnv,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(code, USAGE_EXIT);
        assert!(String::from_utf8_lossy(&stderr).contains("usage: minictr run"));
    }

    // Catches printing help to stderr or exiting nonzero for a help request.
    #[test]
    fn help_commands_print_help_to_stdout_and_exit_0() {
        for argv in [[OsString::from("help")], [OsString::from("--help")]] {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            let code = real_main(argv, &UnusedEnv, &mut stdout, &mut stderr);

            assert_eq!(code, 0);
            assert_eq!(stdout, format!("{}\n", help()).into_bytes());
            assert!(stderr.is_empty());
        }
    }

    // Catches reporting the version anywhere but stdout, or with unstable text.
    #[test]
    fn version_command_prints_name_and_version_and_exits_0() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = real_main(
            [OsString::from("--version")],
            &UnusedEnv,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(code, 0);
        assert_eq!(stdout, b"minictr 0.1.0\n");
        assert!(stderr.is_empty());
    }

    // Catches resolving a real bundle through a temporary store.
    #[test]
    fn resolves_a_real_bundle_from_a_temporary_store() {
        let id = NEXT_TEMP_STORE.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("minictr-test-store-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let bytes = build(ImageSpec {
            name: "hello",
            args: &[],
            elf: b"ELF",
        })
        .unwrap();
        let digest = Store::new(&root).unwrap().import(&bytes).unwrap();
        Store::new(&root).unwrap().tag("hello", digest).unwrap();

        let resolved = Store::new(&root).unwrap().resolve("hello").unwrap();
        assert_eq!(resolved, bytes);
        std::fs::remove_dir_all(&root).unwrap();
    }

    // Catches returning success when the store cannot even be opened.
    #[test]
    fn operational_failure_prints_the_diagnostic_to_stderr_and_exits_125() {
        let runner = FakeRunner::ok(outcome(b"", b"", 0));
        let store = FakeStore {
            result: Err("no such image".to_owned()),
        };
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = run_resolved(&resolved(), &runner, &store, &mut stdout, &mut stderr);

        assert_eq!(code, RUNTIME_EXIT);
        assert!(!stderr.is_empty());
    }

    // Catches reordering build, import, and tag, or printing anything but
    // the single `IMAGE sha256:DIGEST` success line.
    #[test]
    fn build_imports_tags_and_reports_the_digest_in_order() {
        let elf_path = write_temp_elf("order.elf", b"ELF");
        let digest = [0xab; 32];
        let store = RecordingStore::ok(digest);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = build_resolved(&resolved_build(&elf_path), &store, &mut stdout, &mut stderr);

        let expected_bundle = build(ImageSpec {
            name: "hello",
            args: &["fast"],
            elf: b"ELF",
        })
        .unwrap();
        assert_eq!(code, 0);
        assert_eq!(
            stdout,
            format!("hello sha256:{}\n", "ab".repeat(32)).into_bytes()
        );
        assert!(stderr.is_empty());
        assert_eq!(store.calls.borrow().as_slice(), ["import", "tag hello"]);
        assert_eq!(*store.imported.borrow(), expected_bundle);
        assert_eq!(
            store.tagged.borrow().as_slice(),
            [("hello".to_owned(), digest)]
        );
        std::fs::remove_file(&elf_path).unwrap();
    }

    // Catches reading an ELF larger than the 8 MiB boot window into memory:
    // the metadata length rejects it before import runs.
    #[test]
    fn rejects_an_elf_larger_than_the_bundle_limit_before_importing() {
        let id = NEXT_TEMP_STORE.fetch_add(1, Ordering::Relaxed);
        let elf_path = std::env::temp_dir().join(format!(
            "minictr-test-oversized-elf-{}-{id}",
            std::process::id()
        ));
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&elf_path)
            .unwrap();
        file.set_len(minicontainer_bundle::MAX_BUNDLE_LEN + 1)
            .unwrap();
        drop(file);
        let store = RecordingStore::ok([0; 32]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = build_resolved(&resolved_build(&elf_path), &store, &mut stdout, &mut stderr);

        assert_eq!(code, RUNTIME_EXIT);
        assert!(stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&stderr).contains("exceeds the 8 MiB"),
            "stderr was {:?}",
            String::from_utf8_lossy(&stderr)
        );
        assert!(store.calls.borrow().is_empty());
        std::fs::remove_file(&elf_path).unwrap();
    }

    // Catches removing the metadata pre-check: an ELF whose metadata length
    // already exceeds the 8 MiB limit must be rejected before the open
    // boundary runs. A sentinel-only read returns the same error, so this
    // test observes the open boundary directly instead of the result alone.
    #[test]
    fn rejects_an_oversized_elf_before_opening_it() {
        let id = NEXT_TEMP_STORE.fetch_add(1, Ordering::Relaxed);
        let elf_path = std::env::temp_dir().join(format!(
            "minictr-test-unopened-oversized-elf-{}-{id}",
            std::process::id()
        ));
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&elf_path)
            .unwrap();
        file.set_len(minicontainer_bundle::MAX_BUNDLE_LEN + 1)
            .unwrap();
        drop(file);
        let opened = Cell::new(false);

        let result = read_elf_with(&elf_path, &|_| {
            opened.set(true);
            Err(std::io::Error::other("open must not run"))
        });

        assert!(matches!(result, Err(BuildError::ElfTooLarge)));
        assert!(!opened.get(), "oversized ELF must not reach open");
        std::fs::remove_file(&elf_path).unwrap();
    }

    // Catches reporting success or exiting zero when tagging the imported
    // bundle fails.
    #[test]
    fn maps_a_tag_failure_to_a_host_error_without_a_success_line() {
        let elf_path = write_temp_elf("tag-fail.elf", b"ELF");
        let store = RecordingStore::tag_fails([0xab; 32]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = build_resolved(&resolved_build(&elf_path), &store, &mut stdout, &mut stderr);

        assert_eq!(code, RUNTIME_EXIT);
        assert!(stdout.is_empty());
        assert!(!stderr.is_empty());
        assert_eq!(store.calls.borrow().as_slice(), ["import", "tag hello"]);
        std::fs::remove_file(&elf_path).unwrap();
    }

    // Catches reporting success when the ELF input cannot be read.
    #[test]
    fn maps_a_missing_elf_to_a_host_error() {
        let id = NEXT_TEMP_STORE.fetch_add(1, Ordering::Relaxed);
        let elf_path = std::env::temp_dir().join(format!(
            "minictr-test-missing-elf-{}-{id}.elf",
            std::process::id()
        ));
        let store = RecordingStore::ok([0; 32]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = build_resolved(&resolved_build(&elf_path), &store, &mut stdout, &mut stderr);

        assert_eq!(code, RUNTIME_EXIT);
        assert!(stdout.is_empty());
        assert!(!stderr.is_empty());
        assert!(store.calls.borrow().is_empty());
    }

    // Catches wiring `image build` to anything but parse, store resolution,
    // bounded ELF read, bundle build, import, and tag.
    #[test]
    fn image_build_registers_a_tag_through_the_public_cli() {
        let id = NEXT_TEMP_STORE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "minictr-test-build-store-{}-{id}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let elf_path = root.join("hello.elf");
        std::fs::write(&elf_path, b"ELF").unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = real_main(
            [
                OsString::from("image"),
                OsString::from("build"),
                OsString::from("--store"),
                root.clone().into_os_string(),
                OsString::from("hello"),
                elf_path.into_os_string(),
            ],
            &UnusedEnv,
            &mut stdout,
            &mut stderr,
        );

        let expected_bundle = build(ImageSpec {
            name: "hello",
            args: &[],
            elf: b"ELF",
        })
        .unwrap();
        assert_eq!(code, 0);
        assert!(stderr.is_empty());
        let stored = Store::new(&root).unwrap().resolve("hello").unwrap();
        assert_eq!(stored, expected_bundle);
        let digest = minicontainer_bundle::parse(&stored).unwrap().header.digest;
        assert_eq!(
            stdout,
            format!(
                "hello sha256:{}\n",
                minicontainer_bundle::format_digest(digest)
            )
            .into_bytes()
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    // Catches drifting list output from the stable header and byte-sorted `TAG\tDIGEST` rows.
    #[test]
    fn image_list_prints_header_and_byte_sorted_tags() {
        let id = NEXT_TEMP_STORE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "minictr-test-list-store-{}-{id}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let store = Store::new(&root).unwrap();
        let first = build(ImageSpec {
            name: "a",
            args: &[],
            elf: b"first",
        })
        .unwrap();
        let second = build(ImageSpec {
            name: "a",
            args: &[],
            elf: b"second",
        })
        .unwrap();
        let first_digest = store.import(&first).unwrap();
        let second_digest = store.import(&second).unwrap();
        store.tag("b", first_digest).unwrap();
        store.tag("a", second_digest).unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = real_main(
            [
                OsString::from("image"),
                OsString::from("list"),
                OsString::from("--store"),
                root.clone().into_os_string(),
            ],
            &UnusedEnv,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(code, 0);
        assert!(stderr.is_empty());
        assert_eq!(
            stdout,
            format!(
                "TAG\tDIGEST\na\tsha256:{}\nb\tsha256:{}\n",
                minicontainer_bundle::format_digest(second_digest),
                minicontainer_bundle::format_digest(first_digest),
            )
            .into_bytes()
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    // Catches printing phantom rows or failing for an empty store instead of the header alone.
    #[test]
    fn image_list_prints_header_only_for_an_empty_store() {
        let id = NEXT_TEMP_STORE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "minictr-test-list-empty-{}-{id}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = real_main(
            [
                OsString::from("image"),
                OsString::from("list"),
                OsString::from("--store"),
                root.clone().into_os_string(),
            ],
            &UnusedEnv,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(code, 0);
        assert!(stderr.is_empty());
        assert_eq!(stdout, b"TAG\tDIGEST\n");
        std::fs::remove_dir_all(&root).unwrap();
    }

    // Catches mapping a missing default store path differently between image queries.
    #[test]
    fn image_queries_map_missing_default_store_to_usage_error() {
        for argv in [
            vec![OsString::from("image"), OsString::from("list")],
            vec![
                OsString::from("image"),
                OsString::from("inspect"),
                OsString::from("hello"),
            ],
        ] {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            let code = real_main(argv, &UnusedEnv, &mut stdout, &mut stderr);

            assert_eq!(code, USAGE_EXIT);
            assert!(stdout.is_empty());
            assert!(String::from_utf8_lossy(&stderr).contains("usage: minictr run"));
        }
    }

    // Catches mapping store-opening failures to success or usage errors.
    #[test]
    fn image_queries_map_store_failures_to_host_error() {
        for argv in [
            vec![
                OsString::from("image"),
                OsString::from("list"),
                OsString::from("--store"),
                OsString::from("relative-store"),
            ],
            vec![
                OsString::from("image"),
                OsString::from("inspect"),
                OsString::from("--store"),
                OsString::from("relative-store"),
                OsString::from("hello"),
            ],
        ] {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            let code = real_main(argv, &UnusedEnv, &mut stdout, &mut stderr);

            assert_eq!(code, RUNTIME_EXIT);
            assert!(stdout.is_empty());
            assert!(String::from_utf8_lossy(&stderr).contains("minictr:"));
        }
    }

    // Catches image query output write or flush failures being reported as success.
    #[test]
    fn image_queries_map_output_failures_to_host_error() {
        let bundle = build(ImageSpec {
            name: "hello",
            args: &[],
            elf: b"ELF",
        })
        .unwrap();
        let store = QueryStore {
            bundle,
            records: vec![TagRecord {
                name: "hello".to_owned(),
                digest: [0x5a; 32],
            }],
        };
        let list = ResolvedList {
            store: Path::new("/store").to_path_buf(),
        };
        let inspect = ResolvedInspect {
            image: "hello".to_owned(),
            store: Path::new("/store").to_path_buf(),
        };

        let mut stderr = Vec::new();
        assert_eq!(
            list_resolved(
                &list,
                &store,
                &mut FailingWriter {
                    message: "list write broken",
                },
                &mut stderr,
            ),
            RUNTIME_EXIT
        );
        assert!(String::from_utf8_lossy(&stderr).contains("failed to write stdout"));

        let mut stderr = Vec::new();
        assert_eq!(
            list_resolved(
                &list,
                &store,
                &mut FailAfterWriter {
                    remaining: b"TAG\tDIGEST\n".len(),
                    message: "list row write broken",
                },
                &mut stderr,
            ),
            RUNTIME_EXIT
        );
        assert!(String::from_utf8_lossy(&stderr).contains("failed to write stdout"));

        let mut stderr = Vec::new();
        assert_eq!(
            list_resolved(
                &list,
                &store,
                &mut FlushFailingWriter {
                    buffered: Vec::new(),
                    message: "list flush broken",
                },
                &mut stderr,
            ),
            RUNTIME_EXIT
        );
        assert!(String::from_utf8_lossy(&stderr).contains("failed to flush stdout"));

        let mut stderr = Vec::new();
        assert_eq!(
            inspect_resolved(
                &inspect,
                &store,
                &mut FailingWriter {
                    message: "inspect write broken",
                },
                &mut stderr,
            ),
            RUNTIME_EXIT
        );
        assert!(String::from_utf8_lossy(&stderr).contains("failed to write stdout"));

        let mut stderr = Vec::new();
        assert_eq!(
            inspect_resolved(
                &inspect,
                &store,
                &mut FlushFailingWriter {
                    buffered: Vec::new(),
                    message: "inspect flush broken",
                },
                &mut stderr,
            ),
            RUNTIME_EXIT
        );
        assert!(String::from_utf8_lossy(&stderr).contains("failed to flush stdout"));
    }

    // Catches drifting inspect output from the stable five lines for tag, manifest, digest, args, and ELF bytes.
    #[test]
    fn image_inspect_prints_the_stable_five_lines() {
        let id = NEXT_TEMP_STORE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "minictr-test-inspect-store-{}-{id}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let store = Store::new(&root).unwrap();
        let bytes = build(ImageSpec {
            name: "inner",
            args: &["first", "second"],
            elf: b"ELF-bytes",
        })
        .unwrap();
        let digest = store.import(&bytes).unwrap();
        store.tag("outer", digest).unwrap();
        let header_digest = minicontainer_bundle::parse(&bytes).unwrap().header.digest;
        assert_eq!(digest, header_digest);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = real_main(
            [
                OsString::from("image"),
                OsString::from("inspect"),
                OsString::from("--store"),
                root.clone().into_os_string(),
                OsString::from("outer"),
            ],
            &UnusedEnv,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(code, 0);
        assert!(stderr.is_empty());
        assert_eq!(
            stdout,
            format!(
                "tag: outer\nname: inner\ndigest: sha256:{}\nargs: 2\nelf-bytes: 9\n",
                minicontainer_bundle::format_digest(header_digest),
            )
            .into_bytes()
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    // Catches hiding a dangling tag from the listing or inspecting it without failing through resolve.
    #[test]
    fn dangling_tag_is_listed_but_inspect_fails() {
        let id = NEXT_TEMP_STORE.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("minictr-test-dangling-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let store = Store::new(&root).unwrap();
        let digest = [0x5a; 32];
        store.tag("dangling", digest).unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = real_main(
            [
                OsString::from("image"),
                OsString::from("list"),
                OsString::from("--store"),
                root.clone().into_os_string(),
            ],
            &UnusedEnv,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(code, 0);
        assert!(stderr.is_empty());
        assert_eq!(
            stdout,
            format!(
                "TAG\tDIGEST\ndangling\tsha256:{}\n",
                minicontainer_bundle::format_digest(digest),
            )
            .into_bytes()
        );

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = real_main(
            [
                OsString::from("image"),
                OsString::from("inspect"),
                OsString::from("--store"),
                root.clone().into_os_string(),
                OsString::from("dangling"),
            ],
            &UnusedEnv,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(code, RUNTIME_EXIT);
        assert!(stdout.is_empty());
        assert!(!stderr.is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    struct FakeQemuProbe {
        output: Result<String, QemuError>,
    }

    impl FakeQemuProbe {
        fn ok(output: &str) -> Self {
            Self {
                output: Ok(output.to_owned()),
            }
        }

        fn err(error: QemuError) -> Self {
            Self { output: Err(error) }
        }
    }

    impl QemuProbe for FakeQemuProbe {
        fn version_output(&self) -> Result<String, QemuError> {
            self.output.clone()
        }
    }

    /// doctor fixture用の所有つき一時path。drop時にfileもdirectoryも消す。
    struct DoctorScratch {
        path: std::path::PathBuf,
    }

    impl DoctorScratch {
        fn create(prefix: &str) -> Self {
            let id = NEXT_TEMP_STORE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "minictr-test-doctor-{}-{id}-{prefix}",
                std::process::id()
            ));
            Self { path }
        }

        fn missing(prefix: &str) -> Self {
            Self::create(prefix)
        }

        fn file(prefix: &str, bytes: &[u8]) -> Self {
            let scratch = Self::create(prefix);
            std::fs::write(&scratch.path, bytes).unwrap();
            scratch
        }

        fn dir(prefix: &str) -> Self {
            let scratch = Self::create(prefix);
            std::fs::create_dir_all(&scratch.path).unwrap();
            scratch
        }
    }

    impl Drop for DoctorScratch {
        fn drop(&mut self) {
            if std::fs::remove_dir_all(&self.path).is_err() {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }

    fn resolved_doctor(kernel: &Path, store: &Path) -> ResolvedDoctor {
        ResolvedDoctor {
            store: store.to_path_buf(),
            kernel: kernel.to_path_buf(),
        }
    }

    fn directory_snapshot(dir: &Path) -> Vec<OsString> {
        let mut entries: Vec<OsString> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        entries.sort();
        entries
    }

    // Catches drifting the all-pass doctor report or its exit code.
    #[test]
    fn doctor_reports_all_checks_passing_and_exits_0() {
        let probe = FakeQemuProbe::ok("QEMU emulator version 8.2.2\n");
        let kernel = DoctorScratch::file("kernel", b"kernel-bytes");
        let store = DoctorScratch::dir("store");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = doctor_resolved(
            &resolved_doctor(&kernel.path, &store.path),
            &probe,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(code, 0);
        assert_eq!(
            String::from_utf8_lossy(&stdout),
            format!(
                "qemu: ok qemu-system-riscv64 8.2.2\nkernel: ok {} (12 bytes)\nstore: ok {}\nsummary: passed 3/3 checks\n",
                kernel.path.to_string_lossy(),
                store.path.to_string_lossy(),
            )
        );
        assert!(stderr.is_empty());
    }

    // Catches hiding a failing check: every failure is listed and the
    // exit code reports the diagnosis, not success.
    #[test]
    fn doctor_lists_every_failure_and_exits_1() {
        let probe = FakeQemuProbe::err(QemuError::Missing);
        let kernel = DoctorScratch::missing("kernel");
        let store = DoctorScratch::missing("store");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = doctor_resolved(
            &resolved_doctor(&kernel.path, &store.path),
            &probe,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(code, DOCTOR_EXIT);
        let report = String::from_utf8_lossy(&stdout);
        assert!(
            report.contains(&format!(
                "qemu: fail qemu-system-riscv64 is not installed; fix: {}",
                qemu_install_hint()
            )),
            "unexpected report: {report}"
        );
        assert!(
            report.contains(&format!(
                "kernel: fail kernel file is missing at {}",
                kernel.path.to_string_lossy()
            )),
            "unexpected report: {report}"
        );
        assert!(
            report.contains(&format!(
                "store: fail store root is missing at {}",
                store.path.to_string_lossy()
            )),
            "unexpected report: {report}"
        );
        assert!(report.ends_with("summary: passed 0/3 checks\n"));
        assert!(stderr.is_empty());
    }

    // Catches miscounting the summary when only some checks pass.
    #[test]
    fn doctor_counts_a_partial_pass() {
        let probe = FakeQemuProbe::ok("QEMU emulator version 9.0.0\n");
        let kernel = DoctorScratch::missing("kernel");
        let store = DoctorScratch::missing("store");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = doctor_resolved(
            &resolved_doctor(&kernel.path, &store.path),
            &probe,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(code, DOCTOR_EXIT);
        let report = String::from_utf8_lossy(&stdout);
        assert!(report.contains("qemu: ok qemu-system-riscv64 9.0.0\n"));
        assert!(report.contains("kernel: fail "));
        assert!(report.contains("store: fail "));
        assert!(report.ends_with("summary: passed 1/3 checks\n"));
        assert!(stderr.is_empty());
    }

    // Catches accepting a QEMU below the 8.2.0 floor, an unreadable
    // version line, or a probe failure without its status.
    #[test]
    fn qemu_check_rejects_old_malformed_and_failed_versions() {
        assert_eq!(
            parse_qemu_version("QEMU emulator version 8.2.0\n"),
            Ok(Version::new(8, 2, 0))
        );
        assert_eq!(
            parse_qemu_version("QEMU emulator version 11.1.0 (custom)\nCopyright"),
            Ok(Version::new(11, 1, 0))
        );
        assert_eq!(
            parse_qemu_version("QEMU emulator version 7.2.9\n"),
            Err(QemuError::TooOld(Version::new(7, 2, 9)))
        );
        for output in [
            "",
            "qemu 8.2.0\n",
            "QEMU emulator version 8.2\n",
            "QEMU emulator version 8.2.0.1\n",
            "QEMU emulator version x.y.z\n",
        ] {
            assert_eq!(
                parse_qemu_version(output),
                Err(QemuError::Malformed),
                "output must not parse: {output:?}"
            );
        }

        let probe = FakeQemuProbe::err(QemuError::Failed { status: Some(3) });
        assert_eq!(
            check_qemu(&probe),
            Err(
                "qemu-system-riscv64 --version failed with status 3; fix: reinstall QEMU"
                    .to_owned()
            )
        );
        let probe = FakeQemuProbe::err(QemuError::Failed { status: None });
        assert!(
            check_qemu(&probe)
                .unwrap_err()
                .contains("failed with status unknown")
        );
        let probe = FakeQemuProbe::ok("QEMU emulator version 7.2.0\n");
        assert!(
            check_qemu(&probe)
                .unwrap_err()
                .contains("8.2.0 or newer is required")
        );
    }

    // Catches accepting a kernel path that run cannot use: a directory,
    // an empty file, or an unstattable path.
    #[test]
    fn kernel_check_rejects_unusable_paths() {
        let dir = DoctorScratch::dir("kernel-dir");
        assert!(
            check_kernel(&dir.path)
                .unwrap_err()
                .contains("is not a file")
        );

        let empty = DoctorScratch::file("kernel-empty", b"");
        assert!(check_kernel(&empty.path).unwrap_err().contains("is empty"));

        let reason = check_kernel(Path::new("kernel\0name")).unwrap_err();
        assert!(reason.starts_with("cannot stat kernel file"));

        let good = DoctorScratch::file("kernel-ok", b"12345");
        assert_eq!(
            check_kernel(&good.path),
            Ok(format!("{} (5 bytes)", good.path.to_string_lossy()))
        );
    }

    // Catches accepting a store root that cannot hold images, and
    // creating a missing store as a side effect.
    #[test]
    fn store_check_rejects_unusable_roots_without_creating_them() {
        let file = DoctorScratch::file("store-file", b"x");
        assert!(
            check_store(&file.path)
                .unwrap_err()
                .contains("is not a directory")
        );

        let reason = check_store(Path::new("store\0name")).unwrap_err();
        assert!(reason.starts_with("cannot stat store root"));

        let missing = DoctorScratch::missing("store-never-created");
        assert!(
            check_store(&missing.path)
                .unwrap_err()
                .contains("is missing")
        );
        assert!(!missing.path.exists(), "doctor must not create the store");
    }

    // Catches breaking the one-line report with undecodable or multiline paths.
    #[test]
    fn display_path_sanitizes_lossy_and_multiline_paths() {
        use std::os::unix::ffi::OsStringExt;

        let raw = OsString::from_vec(vec![0x2f, 0x74, 0xff, 0x0a, 0x70]);
        assert_eq!(display_path(Path::new(&raw)), "/t\u{fffd}\\np");
        assert_eq!(display_path(Path::new("a\rb")), "a\\rb");
    }

    // Catches doctor changing the environment: a failing diagnosis must
    // leave the filesystem exactly as it found it.
    #[test]
    fn doctor_changes_nothing_on_failure() {
        let parent = DoctorScratch::dir("parent");
        let before = directory_snapshot(&parent.path);
        let probe = FakeQemuProbe::err(QemuError::Missing);
        let kernel = parent.path.join("kernel");
        let store = parent.path.join("store");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = doctor_resolved(
            &resolved_doctor(&kernel, &store),
            &probe,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(code, DOCTOR_EXIT);
        assert_eq!(directory_snapshot(&parent.path), before);
    }

    // Catches panicking on undecodable store or kernel paths.
    #[test]
    fn doctor_renders_non_utf8_paths_without_panicking() {
        use std::os::unix::ffi::OsStringExt;

        let probe = FakeQemuProbe::ok("QEMU emulator version 8.2.0\n");
        let raw = OsString::from_vec(vec![0x2f, 0x74, 0x6d, 0x70, 0xff]);
        let kernel = Path::new(&raw).to_path_buf();
        let store = Path::new(&raw).to_path_buf();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = doctor_resolved(
            &resolved_doctor(&kernel, &store),
            &probe,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(code, DOCTOR_EXIT);
        let report = String::from_utf8(stdout).expect("the report is UTF-8");
        assert!(report.contains("kernel: fail kernel file is missing at /tmp\u{fffd}"));
        assert!(report.contains("store: fail store root is missing at /tmp\u{fffd}"));
        assert!(stderr.is_empty());
    }

    // Catches ignoring a broken stdout pipe and exiting as diagnosed.
    #[test]
    fn doctor_stdout_write_failure_is_a_host_error() {
        let probe = FakeQemuProbe::ok("QEMU emulator version 8.2.0\n");
        let kernel = DoctorScratch::file("kernel", b"bytes");
        let store = DoctorScratch::dir("store");
        let mut stdout = FailingWriter {
            message: "doctor pipe",
        };
        let mut stderr = Vec::new();

        let code = doctor_resolved(
            &resolved_doctor(&kernel.path, &store.path),
            &probe,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(code, RUNTIME_EXIT);
        assert!(String::from_utf8_lossy(&stderr).contains("failed to write stdout"));
    }

    // Catches running diagnostics without resolvable paths: a missing
    // HOME is a usage error, not a diagnosis.
    #[test]
    fn doctor_without_home_is_a_usage_error() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = real_main(
            [OsString::from("doctor")],
            &UnusedEnv,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(code, USAGE_EXIT);
        assert!(stdout.is_empty());
        let diagnostic = String::from_utf8_lossy(&stderr);
        assert!(diagnostic.contains("without HOME"));
        assert!(diagnostic.contains("usage: minictr doctor"));
    }
}
