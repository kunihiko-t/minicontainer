//! detached run: QEMUを起動し、guest Readyを確認してからinstance idを返す。
//!
//! 戻った後はQEMUだけが残り、guest出力はUART log fileへ流れ続ける。
//! supervisorは立てないため、guestのExit frameは誰も観測せず、終了した
//! guestの回収も含めて`minictr stop`の役目である。

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::process::process_status;
use crate::{
    CleanupFailure, HostSignal, InstanceRegistration, InterruptSource, PayloadTemp, ProcessError,
    ProcessStatus, QemuCommand, QemuResources, RuntimeError, Session, SessionEvent,
    finish_with_cleanup, remove_payload_tree,
};

/// handshakeのpoll間隔。
const HANDSHAKE_POLL: Duration = Duration::from_millis(10);
/// 一回のpollでUART logから読む上限。session側の上限を超える流入は
/// protocol errorとして弾かれるため、ここでは読みすぎだけを抑える。
const UART_CHUNK: u64 = 128 * 1024;
/// `qemu.log`の診断として拾う末尾の大きさ。
const DIAGNOSTICS_TAIL: u64 = 4 * 1024;

/// detached runのQEMUが書き出すfile。
pub struct DetachLogs {
    /// guest UART出力 (control frame + console) の行き先。
    pub uart: PathBuf,
    /// QEMU自身のstderrの行き先。起動失敗の診断に使う。
    pub diagnostics: PathBuf,
}

/// detached runの起動済みQEMU。
pub trait DetachedChild {
    /// OSが割り当てたprocess ID。
    fn id(&self) -> u32;

    /// 終了していればstatusを、走っていれば`None`を返す。
    fn try_wait(&mut self) -> Result<Option<ProcessStatus>, ProcessError>;

    /// hostが受け取ったsignalをchildのprocess group全体へ転送する。
    fn send_signal(&mut self, signal: HostSignal) -> Result<(), ProcessError>;

    /// childとそのprocess groupを停止し、終了を回収する。
    fn terminate_and_reap(&mut self) -> Result<ProcessStatus, ProcessError>;

    /// handshake成功後に所有権を手放す。dropでreapされず、以後の停止と
    /// 回収は`minictr stop`とinitによるreapへ委ねられる。
    fn release(self);
}

/// detached runのQEMU起動方法。testではQEMUなしの偽装で差し替える。
pub trait DetachBackend {
    /// spawnが返すchildの型。
    type Child: DetachedChild;

    /// commandをstdioなしで起動する。QEMU自身の診断は`logs.diagnostics`
    /// のfileへ書かれ、UART出力はcommandの`-serial file:`が`logs.uart`
    /// へ書く。
    fn spawn(&self, command: &QemuCommand, logs: &DetachLogs) -> Result<Self::Child, ProcessError>;
}

/// detached runに必要な不変入力。
pub struct DetachRequest<'a> {
    /// digestを含めて検証されるMiniBundle bytes。
    pub bundle: &'a [u8],
    /// QEMUが起動するminiOS kernel。
    pub kernel: &'a Path,
    /// guest Readyを待つ期限。
    pub deadline: Duration,
    /// QEMUへ渡すguest resource量。
    pub resources: QemuResources,
    /// hostへ届いたSIGINTとSIGTERMを観測する源。
    pub interrupts: Option<&'a dyn InterruptSource>,
    /// instance stateを書くdirectoryと`ps`の表示label。detached runは
    /// 常に記録するため必須である。
    pub instances: InstanceRegistration<'a>,
}

/// handshakeに成功して立ち上がったinstance。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetachedInstance {
    /// `minictr ps`/`stop`が受け付ける公開名。
    pub id: String,
    /// QEMU processのPID。
    pub pid: u32,
}

/// detached runを実行するruntime。
pub struct DetachedRuntime<D: DetachBackend = SystemDetachBackend> {
    backend: D,
}

impl<D: DetachBackend> DetachedRuntime<D> {
    /// 指定backendを使うruntimeを作る。
    pub const fn new(backend: D) -> Self {
        Self { backend }
    }

    /// QEMUを起動し、guest Readyまで待ってinstance idを返す。
    ///
    /// 成功すればpayload dirとstate fileは`stop`の回収対象として残る。
    /// 失敗した場合はQEMUを畳み、payloadもstate fileも残さない。
    pub fn run(&self, request: DetachRequest<'_>) -> Result<DetachedInstance, RuntimeError> {
        let deadline = Instant::now()
            .checked_add(request.deadline)
            .ok_or(RuntimeError::InvalidDeadline)?;
        minicontainer_bundle::parse(request.bundle).map_err(RuntimeError::Bundle)?;
        let payload = PayloadTemp::create(request.bundle)?;
        let logs = DetachLogs {
            uart: payload.root().join("uart.log"),
            diagnostics: payload.root().join("qemu.log"),
        };
        let command = match QemuCommand::new_detached(
            request.kernel,
            payload.path(),
            request.resources,
            &logs.uart,
        ) {
            Ok(command) => command,
            Err(error) => {
                return finish_with_cleanup(Err(error), remove_payload_tree(payload.root()));
            }
        };
        let mut child = match self.backend.spawn(&command, &logs) {
            Ok(child) => child,
            Err(error) => {
                return finish_with_cleanup(
                    Err(RuntimeError::Process(error)),
                    remove_payload_tree(payload.root()),
                );
            }
        };
        let registration = request.instances;
        let handle = match registration.dir.register(
            registration.image,
            child.id(),
            registration.program,
            payload.root(),
        ) {
            Ok(handle) => handle,
            Err(error) => {
                let mut failures = Vec::new();
                if let Err(reap) = child.terminate_and_reap() {
                    failures.push(CleanupFailure::Process(reap));
                }
                failures.extend(remove_payload_tree(payload.root()));
                return finish_with_cleanup(Err(RuntimeError::Instance(error)), failures);
            }
        };
        let detached = DetachedInstance {
            id: handle.id().to_owned(),
            pid: child.id(),
        };

        match self.await_ready(&mut child, &logs, deadline, request.interrupts) {
            // payload dirはQEMUが書き続けるuart.logを含むため、以後の回収は
            // `stop`がstate file経由で行う。
            Ok(()) => {
                payload.persist();
                child.release();
                Ok(detached)
            }
            Err(error) => {
                let mut failures = Vec::new();
                if let Err(reap) = child.terminate_and_reap() {
                    failures.push(CleanupFailure::Process(reap));
                }
                failures.extend(remove_payload_tree(payload.root()));
                if let Err(unregister) = registration.dir.unregister(&handle) {
                    failures.push(CleanupFailure::Instance(unregister));
                }
                finish_with_cleanup(Err(error), failures)
            }
        }
    }

    /// guest ReadyがUART logへ現れるのを待つ。QEMUの早期終了、hostへの
    /// signal着信、期限切れはすべて起動失敗であり、呼び出し側がprocessと
    /// 残骸を畳む。
    fn await_ready(
        &self,
        child: &mut D::Child,
        logs: &DetachLogs,
        deadline: Instant,
        interrupts: Option<&dyn InterruptSource>,
    ) -> Result<(), RuntimeError> {
        let mut session = Session::new();
        let mut uart: Option<File> = None;
        loop {
            if Instant::now() >= deadline {
                return Err(RuntimeError::Process(ProcessError::TimedOut));
            }
            if let Some(source) = interrupts
                && let Some(observation) = source.poll()
            {
                // graceは不要である。handshake中にguest結果は存在せず、
                // 後始末のkillは呼び出し側が必ず行う。
                let _ = child.send_signal(observation.first);
                return Err(RuntimeError::Interrupted(observation.first));
            }
            if let Some(status) = child.try_wait().map_err(RuntimeError::Process)? {
                return Err(RuntimeError::DetachedBoot {
                    status,
                    diagnostics: read_diagnostics(&logs.diagnostics),
                });
            }
            match read_uart(&mut uart, &logs.uart) {
                Ok(bytes) if bytes.is_empty() => {}
                Ok(bytes) => {
                    let events = session.push_uart(&bytes).map_err(RuntimeError::Session)?;
                    if events
                        .iter()
                        .any(|event| matches!(event, SessionEvent::Ready))
                    {
                        return Ok(());
                    }
                }
                Err(error) => return Err(RuntimeError::Io(error)),
            }
            thread::sleep(HANDSHAKE_POLL);
        }
    }
}

impl Default for DetachedRuntime<SystemDetachBackend> {
    fn default() -> Self {
        Self::new(SystemDetachBackend::new())
    }
}

/// uart.logを開いたまま追い、新しく書かれた分だけを読む。fileはQEMUが
/// 作るため、まだ無い間は「新しいbytesなし」として扱う。
fn read_uart(uart: &mut Option<File>, path: &Path) -> io::Result<Vec<u8>> {
    if uart.is_none() {
        match File::open(path) {
            Ok(file) => *uart = Some(file),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        }
    }
    let file = uart.as_mut().expect("uart was just opened");
    let mut bytes = Vec::new();
    file.by_ref().take(UART_CHUNK).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// 起動失敗の診断として`qemu.log`の末尾を読む。fileが無い・読めない
/// 場合は空の診断を返し、本体のerrorを曇らせない。
fn read_diagnostics(path: &Path) -> String {
    let Ok(file) = File::open(path) else {
        return String::new();
    };
    let length = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
    let start = length.saturating_sub(DIAGNOSTICS_TAIL);
    let mut bytes = Vec::new();
    let mut file = file;
    if file
        .seek(std::io::SeekFrom::Start(start))
        .and_then(|_| file.read_to_end(&mut bytes))
        .is_err()
    {
        return String::new();
    }
    String::from_utf8_lossy(&bytes).trim().to_owned()
}

/// std process APIでdetachedなQEMUを起動するbackend。
#[derive(Debug, Default)]
pub struct SystemDetachBackend;

impl SystemDetachBackend {
    /// system backendを作る。
    pub const fn new() -> Self {
        Self
    }
}

impl DetachBackend for SystemDetachBackend {
    type Child = SystemDetachedChild;

    fn spawn(&self, command: &QemuCommand, logs: &DetachLogs) -> Result<Self::Child, ProcessError> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let diagnostics = options
            .open(&logs.diagnostics)
            .map_err(ProcessError::Spawn)?;

        let mut process = Command::new(command.program());
        process.args(command.args());
        // foregroundのrunと同じく自process groupのleaderとして起動する。
        // hostが戻った後も`stop`がgroup宛signalで孫まで回収できる。
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            process.process_group(0);
        }
        process
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(diagnostics));
        let child = process.spawn().map_err(ProcessError::Spawn)?;
        Ok(SystemDetachedChild {
            child,
            reaped_status: None,
        })
    }
}

/// std process APIで管理するdetachedな起動済みchild。
pub struct SystemDetachedChild {
    child: Child,
    reaped_status: Option<ProcessStatus>,
}

impl SystemDetachedChild {
    /// childのprocess group全体へsignalを送る。既に消えたgroupへの
    /// ESRCHは成功として扱う。
    #[cfg(unix)]
    fn signal_group(&mut self, signal: libc::c_int) -> io::Result<()> {
        let target = -(self.child.id() as libc::pid_t);
        // SAFETY: `kill(2)`の第一引数が負のときはprocess groupを指定する。
        let result = unsafe { libc::kill(target, signal) };
        if result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

impl DetachedChild for SystemDetachedChild {
    fn id(&self) -> u32 {
        self.child.id()
    }

    fn try_wait(&mut self) -> Result<Option<ProcessStatus>, ProcessError> {
        self.child
            .try_wait()
            .map(|status| status.map(process_status))
            .map_err(ProcessError::Wait)
    }

    fn send_signal(&mut self, signal: HostSignal) -> Result<(), ProcessError> {
        #[cfg(unix)]
        {
            self.signal_group(signal.signo())
                .map_err(ProcessError::Signal)
        }
        #[cfg(not(unix))]
        {
            let _ = signal;
            Err(ProcessError::Signal(io::Error::new(
                io::ErrorKind::Unsupported,
                "signal forwarding is not supported on this platform",
            )))
        }
    }

    fn terminate_and_reap(&mut self) -> Result<ProcessStatus, ProcessError> {
        if let Some(status) = self.reaped_status {
            return Ok(status);
        }
        let status = match self.child.try_wait().map_err(ProcessError::Wait)? {
            Some(status) => process_status(status),
            None => {
                #[cfg(unix)]
                self.signal_group(libc::SIGKILL)
                    .map_err(ProcessError::Terminate)?;
                #[cfg(not(unix))]
                self.child.kill().map_err(ProcessError::Terminate)?;
                process_status(self.child.wait().map_err(ProcessError::Wait)?)
            }
        };
        self.reaped_status = Some(status);
        Ok(status)
    }

    fn release(self) {
        // dropによるreapを飛ばし、orphanとして生き続けさせる。
        std::mem::forget(self);
    }
}

impl Drop for SystemDetachedChild {
    fn drop(&mut self) {
        let _ = self.terminate_and_reap();
    }
}

#[cfg(test)]
mod tests {
    use super::{DetachBackend, DetachLogs, DetachRequest, DetachedChild, DetachedRuntime};
    use crate::{
        HostSignal, InstanceDir, InstanceRegistration, InstanceRow, InstanceStatus,
        InterruptSource, ProcessError, ProcessStatus, QemuCommand, QemuResources, RuntimeError,
        SessionError, SignalObservation,
    };
    use minicontainer_bundle::{ImageSpec, build};
    use minios_abi::control::{FrameHeader, FrameKind, ReadyPayload};
    use std::{
        cell::RefCell,
        env, fs,
        path::PathBuf,
        process::{Child, Command, Stdio},
        rc::Rc,
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
                "minicontainer-detach-test-{}-{sequence}",
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

    fn helper_program() -> PathBuf {
        env::current_exe().expect("the test binary path must exist")
    }

    /// detached runが登録するQEMUと同じく、自process groupを持つsleeper。
    #[cfg(unix)]
    fn spawn_sleeper() -> Child {
        use std::os::unix::process::CommandExt;
        Command::new(env::current_exe().expect("the test binary path must exist"))
            .args([
                "--ignored",
                "--exact",
                "detach::tests::instance_helper",
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

    fn valid_bundle() -> Vec<u8> {
        build(ImageSpec {
            name: "test",
            args: &[],
            elf: b"ELF",
        })
        .unwrap()
    }

    fn frame(kind: FrameKind, payload: &[u8]) -> Vec<u8> {
        let mut bytes = FrameHeader {
            kind,
            payload_len: payload.len() as u32,
        }
        .encode()
        .to_vec();
        bytes.extend_from_slice(payload);
        bytes
    }

    fn ready_frame() -> Vec<u8> {
        frame(
            FrameKind::Ready,
            &ReadyPayload {
                abi_major: 1,
                abi_minor: 0,
            }
            .encode(),
        )
    }

    /// handshakeでUART logへ書き込むbytesとchildの挙動を制御するbackend。
    /// `sleeper`に実processを渡せばregistrationのidentity照合が本物の
    /// kernel情報で動く。
    struct FakeDetachBackend {
        uart_bytes: Vec<u8>,
        diagnostics: String,
        sleeper: RefCell<Option<Child>>,
        scripted_exit: Option<ProcessStatus>,
        fail_spawn: bool,
        trace: Rc<RefCell<Vec<&'static str>>>,
        payload_root: Rc<RefCell<Option<PathBuf>>>,
        released: Rc<RefCell<Option<Child>>>,
    }

    impl FakeDetachBackend {
        fn new(
            trace: Rc<RefCell<Vec<&'static str>>>,
            payload_root: Rc<RefCell<Option<PathBuf>>>,
            released: Rc<RefCell<Option<Child>>>,
        ) -> Self {
            Self {
                uart_bytes: Vec::new(),
                diagnostics: String::new(),
                sleeper: RefCell::new(None),
                scripted_exit: None,
                fail_spawn: false,
                trace,
                payload_root,
                released,
            }
        }

        fn ready(mut self) -> Self {
            self.uart_bytes = ready_frame();
            self
        }

        fn with_sleeper(self, sleeper: Child) -> Self {
            *self.sleeper.borrow_mut() = Some(sleeper);
            self
        }
    }

    impl DetachBackend for FakeDetachBackend {
        type Child = FakeDetachedChild;

        fn spawn(
            &self,
            _command: &QemuCommand,
            logs: &DetachLogs,
        ) -> Result<Self::Child, ProcessError> {
            self.trace.borrow_mut().push("spawn");
            if self.fail_spawn {
                return Err(ProcessError::Spawn(std::io::Error::other("injected spawn")));
            }
            if !self.uart_bytes.is_empty() {
                fs::write(&logs.uart, &self.uart_bytes).expect("the fake must write uart.log");
            }
            if !self.diagnostics.is_empty() {
                fs::write(&logs.diagnostics, &self.diagnostics)
                    .expect("the fake must write qemu.log");
            }
            *self.payload_root.borrow_mut() = logs.uart.parent().map(|parent| parent.to_path_buf());
            Ok(FakeDetachedChild {
                sleeper: self.sleeper.borrow_mut().take(),
                scripted_exit: self.scripted_exit,
                trace: self.trace.clone(),
                released: self.released.clone(),
            })
        }
    }

    struct FakeDetachedChild {
        sleeper: Option<Child>,
        scripted_exit: Option<ProcessStatus>,
        trace: Rc<RefCell<Vec<&'static str>>>,
        released: Rc<RefCell<Option<Child>>>,
    }

    impl DetachedChild for FakeDetachedChild {
        fn id(&self) -> u32 {
            self.sleeper.as_ref().map_or(u32::MAX - 3, Child::id)
        }

        fn try_wait(&mut self) -> Result<Option<ProcessStatus>, ProcessError> {
            if let Some(status) = self.scripted_exit {
                return Ok(Some(status));
            }
            match &mut self.sleeper {
                Some(child) => child
                    .try_wait()
                    .map(|status| {
                        status.map(|status| ProcessStatus {
                            code: status.code(),
                            success: status.success(),
                        })
                    })
                    .map_err(ProcessError::Wait),
                None => Ok(None),
            }
        }

        fn send_signal(&mut self, signal: HostSignal) -> Result<(), ProcessError> {
            self.trace
                .borrow_mut()
                .push(if signal == HostSignal::Interrupt {
                    "sigint"
                } else {
                    "sigterm"
                });
            Ok(())
        }

        fn terminate_and_reap(&mut self) -> Result<ProcessStatus, ProcessError> {
            self.trace.borrow_mut().push("reap");
            if let Some(mut child) = self.sleeper.take() {
                let _ = child.kill();
                let status = child.wait().map_err(ProcessError::Wait)?;
                return Ok(ProcessStatus {
                    code: status.code(),
                    success: status.success(),
                });
            }
            Ok(ProcessStatus {
                code: Some(0),
                success: true,
            })
        }

        fn release(mut self) {
            // sleeperを生かしたままtest側へ引き渡す。reapはtestが行い、
            // dropはsleeperを持たないため何もしない。
            *self.released.borrow_mut() = self.sleeper.take();
        }
    }

    impl Drop for FakeDetachedChild {
        fn drop(&mut self) {
            if let Some(mut child) = self.sleeper.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    struct StaticInterrupt(Option<SignalObservation>);

    impl InterruptSource for StaticInterrupt {
        fn poll(&self) -> Option<SignalObservation> {
            self.0
        }
    }

    fn detached_request<'a>(
        bundle: &'a [u8],
        dir: &'a InstanceDir,
        program: &'a std::ffi::OsStr,
        interrupts: Option<&'a dyn InterruptSource>,
    ) -> DetachRequest<'a> {
        DetachRequest {
            bundle,
            kernel: "/kernel".as_ref(),
            deadline: Duration::from_secs(5),
            resources: QemuResources::DEFAULT,
            interrupts,
            instances: InstanceRegistration {
                dir,
                image: "hello",
                program,
            },
        }
    }

    /// このtestのrunが使ったpayload dir。spawn時にbackendがcaptureする
    /// ため、並行testの一時dirと混ざらない。
    type CapturedRoot = Rc<RefCell<Option<PathBuf>>>;

    fn captured_root() -> CapturedRoot {
        Rc::new(RefCell::new(None))
    }

    // Catches a detached run returning before the guest is actually up:
    // the instance id must only be printed after Ready, with the state file
    // and the payload dir both surviving for `stop` to collect.
    #[test]
    #[cfg(unix)]
    fn a_detached_run_registers_and_persists_after_ready() {
        let root = TempRoot::create();
        let dir = InstanceDir::open(&root.0).unwrap();
        let trace = Rc::new(RefCell::new(Vec::new()));
        let payload_root = captured_root();
        let released: Rc<RefCell<Option<Child>>> = Rc::new(RefCell::new(None));
        let sleeper = spawn_sleeper();
        let pid = sleeper.id();
        let runtime = DetachedRuntime::new(
            FakeDetachBackend::new(trace.clone(), payload_root.clone(), released.clone())
                .ready()
                .with_sleeper(sleeper),
        );
        let program = helper_program();

        let instance = runtime
            .run(detached_request(
                &valid_bundle(),
                &dir,
                program.as_os_str(),
                None,
            ))
            .unwrap();

        assert_eq!(instance.id, format!("i-{pid}"));
        assert_eq!(instance.pid, pid);
        let rows = dir.list().unwrap();
        assert_eq!(rows.len(), 1);
        match &rows[0] {
            InstanceRow::Known { id, status, state } => {
                assert_eq!(id, &instance.id);
                assert_eq!(*status, InstanceStatus::Live);
                let uart = state.payload.join("uart.log");
                assert!(state.payload.join("payload.mcb").exists());
                assert!(uart.exists(), "the UART log must persist for inspection");
            }
            InstanceRow::Corrupt { id } => panic!("expected a live row, got corrupt {id}"),
        }
        assert_eq!(trace.borrow().as_slice(), ["spawn"]);

        // 残したinstanceを実際にstopで回収できることを確認する。detached
        // されたprocessはtestの子のままなので、reapを別threadへ委譲して
        // zombieが残らないようにする。
        let released_child = released
            .borrow_mut()
            .take()
            .expect("release parks the child");
        let reaper = std::thread::spawn(move || {
            let mut child = released_child;
            let _ = child.wait();
        });
        let report = dir.stop(&instance.id, Duration::from_secs(2)).unwrap();
        reaper.join().unwrap();
        assert_eq!(
            report.outcome,
            crate::StopOutcome::Stopped,
            "the detached instance must be stoppable"
        );
        assert!(dir.list().unwrap().is_empty());
        let payload = payload_root.borrow().clone().unwrap();
        assert!(
            !payload.exists(),
            "stop must remove the persisted payload dir"
        );
    }

    // Catches a startup failure leaving state behind: a handshake timeout
    // must reap the child, remove the payload dir, and drop the state file.
    #[test]
    #[cfg(unix)]
    fn a_handshake_timeout_cleans_everything() {
        let root = TempRoot::create();
        let dir = InstanceDir::open(&root.0).unwrap();
        let trace = Rc::new(RefCell::new(Vec::new()));
        let payload_root = captured_root();
        let sleeper = spawn_sleeper();
        let runtime = DetachedRuntime::new(
            FakeDetachBackend::new(
                trace.clone(),
                payload_root.clone(),
                Rc::new(RefCell::new(None)),
            )
            .with_sleeper(sleeper),
        );
        let program = helper_program();

        let bundle = valid_bundle();
        let mut request = detached_request(&bundle, &dir, program.as_os_str(), None);
        request.deadline = Duration::from_millis(200);
        let error = runtime.run(request).unwrap_err();

        assert!(
            matches!(error, RuntimeError::Process(ProcessError::TimedOut)),
            "expected a handshake timeout, got {error:?}"
        );
        assert_eq!(trace.borrow().as_slice(), ["spawn", "reap"]);
        assert!(dir.list().unwrap().is_empty());
        assert!(
            !payload_root.borrow().as_ref().unwrap().exists(),
            "the payload dir must be removed"
        );
    }

    // Catches a QEMU that dies before Ready surfacing as a boot failure with
    // its diagnostics, while still cleaning up child, payload, and state.
    #[test]
    #[cfg(unix)]
    fn an_early_exit_reports_diagnostics_and_cleans_up() {
        let root = TempRoot::create();
        let dir = InstanceDir::open(&root.0).unwrap();
        let trace = Rc::new(RefCell::new(Vec::new()));
        let payload_root = captured_root();
        let sleeper = spawn_sleeper();
        let mut backend = FakeDetachBackend::new(
            trace.clone(),
            payload_root.clone(),
            Rc::new(RefCell::new(None)),
        )
        .with_sleeper(sleeper);
        backend.diagnostics = "qemu-system-riscv64: could not open /kernel".to_owned();
        backend.scripted_exit = Some(ProcessStatus {
            code: Some(1),
            success: false,
        });
        let runtime = DetachedRuntime::new(backend);
        let program = helper_program();

        let error = runtime
            .run(detached_request(
                &valid_bundle(),
                &dir,
                program.as_os_str(),
                None,
            ))
            .unwrap_err();

        match error {
            RuntimeError::DetachedBoot {
                status,
                diagnostics,
            } => {
                assert_eq!(status.code, Some(1));
                assert!(diagnostics.contains("could not open /kernel"));
            }
            other => panic!("expected a detached boot failure, got {other:?}"),
        }
        assert_eq!(trace.borrow().as_slice(), ["spawn", "reap"]);
        assert!(dir.list().unwrap().is_empty());
        assert!(!payload_root.borrow().as_ref().unwrap().exists());
    }

    // Catches a malformed UART stream being treated as a startup failure
    // rather than a successful detach.
    #[test]
    #[cfg(unix)]
    fn a_protocol_error_before_ready_fails_the_detach() {
        let root = TempRoot::create();
        let dir = InstanceDir::open(&root.0).unwrap();
        let trace = Rc::new(RefCell::new(Vec::new()));
        let payload_root = captured_root();
        let sleeper = spawn_sleeper();
        let mut backend = FakeDetachBackend::new(
            trace.clone(),
            payload_root.clone(),
            Rc::new(RefCell::new(None)),
        )
        .with_sleeper(sleeper);
        backend.uart_bytes = frame(FrameKind::Stdout, b"early");
        let runtime = DetachedRuntime::new(backend);
        let program = helper_program();

        let error = runtime
            .run(detached_request(
                &valid_bundle(),
                &dir,
                program.as_os_str(),
                None,
            ))
            .unwrap_err();

        assert!(
            matches!(
                error,
                RuntimeError::Session(SessionError::FrameBeforeReady(_))
            ),
            "expected a pre-Ready frame error, got {error:?}"
        );
        assert_eq!(trace.borrow().as_slice(), ["spawn", "reap"]);
        assert!(dir.list().unwrap().is_empty());
        assert!(!payload_root.borrow().as_ref().unwrap().exists());
    }

    // Catches a host signal during the handshake leaving a detached QEMU
    // behind: the interrupt must surface as Interrupted and the child must
    // still be reaped with everything removed.
    #[test]
    #[cfg(unix)]
    fn an_interrupt_during_the_handshake_aborts_the_detach() {
        let root = TempRoot::create();
        let dir = InstanceDir::open(&root.0).unwrap();
        let trace = Rc::new(RefCell::new(Vec::new()));
        let payload_root = captured_root();
        let sleeper = spawn_sleeper();
        let runtime = DetachedRuntime::new(
            FakeDetachBackend::new(
                trace.clone(),
                payload_root.clone(),
                Rc::new(RefCell::new(None)),
            )
            .with_sleeper(sleeper),
        );
        let program = helper_program();
        let interrupts = StaticInterrupt(Some(SignalObservation {
            first: HostSignal::Interrupt,
            count: 1,
        }));

        let error = runtime
            .run(detached_request(
                &valid_bundle(),
                &dir,
                program.as_os_str(),
                Some(&interrupts),
            ))
            .unwrap_err();

        assert!(
            matches!(error, RuntimeError::Interrupted(HostSignal::Interrupt)),
            "expected Interrupted, got {error:?}"
        );
        assert_eq!(trace.borrow().as_slice(), ["spawn", "sigint", "reap"]);
        assert!(dir.list().unwrap().is_empty());
        assert!(!payload_root.borrow().as_ref().unwrap().exists());
    }

    // Catches a spawn failure leaving payload or state behind: the error
    // must surface without any instance debris.
    #[test]
    fn a_spawn_failure_removes_the_payload() {
        let root = TempRoot::create();
        let dir = InstanceDir::open(&root.0).unwrap();
        let trace = Rc::new(RefCell::new(Vec::new()));
        let payload_root = captured_root();
        let mut backend =
            FakeDetachBackend::new(trace.clone(), payload_root, Rc::new(RefCell::new(None)));
        backend.fail_spawn = true;
        let runtime = DetachedRuntime::new(backend);
        let program = helper_program();

        let error = runtime
            .run(detached_request(
                &valid_bundle(),
                &dir,
                program.as_os_str(),
                None,
            ))
            .unwrap_err();

        assert!(matches!(
            error,
            RuntimeError::Process(ProcessError::Spawn(_))
        ));
        assert!(dir.list().unwrap().is_empty());
    }

    // Catches a registration failure leaving a running QEMU: the child must
    // be reaped and the payload removed before the error returns.
    #[test]
    fn a_registration_failure_reaps_the_child() {
        let root = TempRoot::create();
        let dir = InstanceDir::open(&root.0).unwrap();
        let trace = Rc::new(RefCell::new(Vec::new()));
        let payload_root = captured_root();
        // pidが実在しないsleeper無しのfake childはregisterに失敗する。
        let runtime = DetachedRuntime::new(
            FakeDetachBackend::new(trace.clone(), payload_root, Rc::new(RefCell::new(None)))
                .ready(),
        );
        let program = helper_program();

        let error = runtime
            .run(detached_request(
                &valid_bundle(),
                &dir,
                program.as_os_str(),
                None,
            ))
            .unwrap_err();

        assert!(
            matches!(error, RuntimeError::Instance(_)),
            "expected an instance error, got {error:?}"
        );
        assert_eq!(trace.borrow().as_slice(), ["spawn", "reap"]);
        assert!(dir.list().unwrap().is_empty());
    }

    // Catches a bundle being spawned before validation: parse must reject
    // it before QEMU and payload work starts.
    #[test]
    fn an_invalid_bundle_is_rejected_before_any_spawn() {
        let root = TempRoot::create();
        let dir = InstanceDir::open(&root.0).unwrap();
        let trace = Rc::new(RefCell::new(Vec::new()));
        let runtime = DetachedRuntime::new(FakeDetachBackend::new(
            trace.clone(),
            captured_root(),
            Rc::new(RefCell::new(None)),
        ));
        let program = helper_program();

        let error = runtime
            .run(detached_request(
                b"not a bundle",
                &dir,
                program.as_os_str(),
                None,
            ))
            .unwrap_err();

        assert!(matches!(error, RuntimeError::Bundle(_)));
        assert!(trace.borrow().is_empty(), "spawn must not be attempted");
    }
}
