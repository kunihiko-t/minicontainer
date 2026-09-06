//! `minictr` binary: store resolveとhost runtime、host入出力を接続する。

mod cli;

use std::{ffi::OsString, io::Write, path::Path, time::Duration};

use minicontainer_bundle::Store;
use minicontainer_runtime::{RunOutcome, RunRequest, Runtime, RuntimeError, SystemProcessBackend};

use cli::{Command, Environ, RealEnv, ResolvedRun, VERSION, help, parse_os, resolve};

/// usage errorのprocess終了code。
pub const USAGE_EXIT: i32 = 2;
/// host側失敗のprocess終了code。
pub const RUNTIME_EXIT: i32 = 125;

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
    }
}

/// bundle取得の境界。testでは一時storeや偽装で差し替える。
pub trait ImageStore {
    /// image tagを検証済みbundle bytesとして返す。
    fn resolve(&self, store_root: &Path, image: &str) -> Result<Vec<u8>, StoreError>;
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

impl Runner for RealRunner {
    fn run(
        &self,
        bundle: &[u8],
        kernel: &Path,
        timeout: Duration,
    ) -> Result<RunOutcome, RuntimeError> {
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
        cell::RefCell,
        collections::VecDeque,
        ffi::OsString,
        io,
        sync::atomic::{AtomicU64, Ordering},
    };

    static NEXT_TEMP_STORE: AtomicU64 = AtomicU64::new(0);

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
}
