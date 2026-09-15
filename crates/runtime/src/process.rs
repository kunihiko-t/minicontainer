//! QEMU child processの起動、出力読み取り、終了回収。

use std::{
    error::Error,
    fmt,
    io::{self, Read, Write},
    process::{Child, Command, ExitStatus, Stdio},
    sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::{HostSignal, QemuCommand};

const MAX_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// QEMU processを起動する境界。
pub trait ProcessBackend {
    /// backendが管理するchild processの型。
    type Child: ProcessControl;

    /// commandを起動し、出力と終了を管理するchildを返す。
    fn spawn(&self, command: &QemuCommand) -> Result<Self::Child, ProcessError>;
}

/// 起動済みQEMU processの操作。
pub trait ProcessControl {
    /// OSが割り当てたprocess ID。
    fn id(&self) -> u32;

    /// deadlineまで次のUART、diagnostic、または終了eventを待つ。
    ///
    /// deadlineを過ぎてもchildは停止しない。呼び出し側は必要に応じて
    /// [`Self::terminate_and_reap`]を明示的に呼び出す。
    fn next_event(&mut self, deadline: Instant) -> Result<ProcessEvent, ProcessError>;

    /// childのstdin writerを一度だけ取り出す。stdin経路を持たないbackend
    /// は`None`を返し、入力の転送が要求されたrunは`RuntimeError`になる。
    fn take_stdin(&mut self) -> Option<Box<dyn Write + Send + 'static>> {
        None
    }

    /// hostが受け取ったsignalをchildのprocess group全体へ転送する。
    ///
    /// 既に全員が終了したgroupへの転送は何もしない成功として扱う。
    fn send_signal(&mut self, signal: HostSignal) -> Result<(), ProcessError>;

    /// childとそのprocess groupを停止し、終了を回収する。
    fn terminate_and_reap(&mut self) -> Result<ProcessStatus, ProcessError>;
}

/// child processから観測したevent。
#[derive(Debug, PartialEq, Eq)]
pub enum ProcessEvent {
    /// QEMU serial console (stdout)から読んだbytes。
    Uart(Vec<u8>),
    /// QEMU diagnostic stream (stderr)から読んだbytes。
    Diagnostic(Vec<u8>),
    /// childが終了した。
    Exited(ProcessStatus),
    /// deadlineまでにeventを観測できなかった。
    TimedOut,
}

/// child processの終了状態。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessStatus {
    /// OSが報告した終了code。signal終了などcodeを持たない場合は`None`。
    pub code: Option<i32>,
    /// 正常終了だったか。
    pub success: bool,
}

/// process lifecycleの操作が失敗した理由。
#[derive(Debug)]
pub enum ProcessError {
    /// QEMU processを起動できなかった。
    Spawn(io::Error),
    /// stdoutまたはstderrのreaderが失敗した。
    Read(io::Error),
    /// OSから終了状態を取得できなかった。
    Wait(io::Error),
    /// childへ停止要求を送れなかった。
    Terminate(io::Error),
    /// childのprocess groupへsignalを転送できなかった。
    Signal(io::Error),
    /// テスト用の待機操作がdeadlineを超えた。
    TimedOut,
}

impl fmt::Display for ProcessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(error) => write!(formatter, "process spawn failed: {error}"),
            Self::Read(error) => write!(formatter, "process output read failed: {error}"),
            Self::Wait(error) => write!(formatter, "process wait failed: {error}"),
            Self::Terminate(error) => write!(formatter, "process termination failed: {error}"),
            Self::Signal(error) => write!(formatter, "process signal forward failed: {error}"),
            Self::TimedOut => write!(formatter, "process deadline elapsed"),
        }
    }
}

impl Error for ProcessError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Spawn(error)
            | Self::Read(error)
            | Self::Wait(error)
            | Self::Terminate(error)
            | Self::Signal(error) => Some(error),
            Self::TimedOut => None,
        }
    }
}

/// std process APIでQEMUを起動するbackend。
#[derive(Debug, Default)]
pub struct SystemProcessBackend;

impl SystemProcessBackend {
    /// system backendを作る。
    pub const fn new() -> Self {
        Self
    }
}

impl ProcessBackend for SystemProcessBackend {
    type Child = SystemProcess;

    fn spawn(&self, command: &QemuCommand) -> Result<Self::Child, ProcessError> {
        let mut process = Command::new(command.program());
        process.args(command.args());
        SystemProcess::spawn(process)
    }
}

/// std process APIで管理する起動済みchild。
pub struct SystemProcess {
    child: Child,
    reader_events: Receiver<ReaderMessage>,
    readers: Vec<JoinHandle<()>>,
    open_readers: usize,
    observed_status: Option<ProcessStatus>,
    reaped_status: Option<ProcessStatus>,
}

impl SystemProcess {
    fn spawn(mut command: Command) -> Result<Self, ProcessError> {
        // QEMUは自process groupのleaderとして起動する。端末のCtrl-Cはgroup
        // 全体ではなくhostだけへ届くため、hostが最初のsignalをgroup宛に
        // 転送し、grace後のSIGKILLもgroup宛に送れば子孫まで回収できる。
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            command.process_group(0);
        }
        // stdinは常にpipeへ向ける。hostの入力byteがUARTへrawで流れてframe
        // decodeを壊すことを防ぎ、`take_stdin`で転送側へ渡せる形にする。
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().map_err(ProcessError::Spawn)?;
        let stdout = child
            .stdout
            .take()
            .expect("piped stdout must be available after a successful spawn");
        let stderr = child
            .stderr
            .take()
            .expect("piped stderr must be available after a successful spawn");
        let (sender, reader_events) = mpsc::channel();
        let readers = vec![
            start_reader(stdout, Stream::Uart, sender.clone()),
            start_reader(stderr, Stream::Diagnostic, sender),
        ];

        Ok(Self {
            child,
            reader_events,
            readers,
            open_readers: 2,
            observed_status: None,
            reaped_status: None,
        })
    }

    #[cfg(test)]
    fn wait_until(&mut self, timeout: Duration) -> Result<ProcessEvent, ProcessError> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.next_event(deadline)? {
                ProcessEvent::TimedOut => return Err(ProcessError::TimedOut),
                ProcessEvent::Uart(_) | ProcessEvent::Diagnostic(_) => {}
                event => return Ok(event),
            }
        }
    }

    fn observe_exit(&mut self) -> Result<(), ProcessError> {
        if self.observed_status.is_none()
            && let Some(status) = self.child.try_wait().map_err(ProcessError::Wait)?
        {
            self.observed_status = Some(process_status(status));
        }
        Ok(())
    }

    fn handle_reader_message(
        &mut self,
        message: ReaderMessage,
    ) -> Result<Option<ProcessEvent>, ProcessError> {
        match reader_event(message)? {
            ReaderEvent::Event(event) => Ok(Some(event)),
            ReaderEvent::Closed => {
                self.open_readers = self.open_readers.saturating_sub(1);
                Ok(None)
            }
        }
    }

    fn join_readers(&mut self) -> Result<(), ProcessError> {
        for reader in self.readers.drain(..) {
            reader.join().map_err(|_| {
                ProcessError::Read(io::Error::other("process output reader panicked"))
            })?;
        }
        self.open_readers = 0;
        Ok(())
    }

    /// childのprocess group全体へsignalを送る。
    ///
    /// childは`process_group(0)`で起動するため、PIDがそのままPGIDであり
    /// 子孫processまで届く。
    fn signal_group(&mut self, signal: libc::c_int) -> io::Result<()> {
        let target = -(self.child.id() as libc::pid_t);
        // SAFETY: `kill(2)`の第一引数が負のときはprocess groupを指定する。
        // `id`はspawn直後の子のPIDで`pid_t`に収まる。
        let result = unsafe { libc::kill(target, signal) };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    /// process group全体へSIGKILLを送る。groupが既に消えていれば直接の
    /// 子へのkillへ落とし、それもESRCHなら成功として扱う。
    fn kill_group(&mut self) -> Result<(), ProcessError> {
        match self.signal_group(libc::SIGKILL) {
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => match self.child.kill() {
                Ok(()) => Ok(()),
                Err(direct) if direct.raw_os_error() == Some(libc::ESRCH) => Ok(()),
                Err(direct) => Err(ProcessError::Terminate(direct)),
            },
            result => result.map_err(ProcessError::Terminate),
        }
    }
}

impl ProcessControl for SystemProcess {
    fn id(&self) -> u32 {
        self.child.id()
    }

    fn take_stdin(&mut self) -> Option<Box<dyn Write + Send + 'static>> {
        self.child
            .stdin
            .take()
            .map(|stdin| Box::new(stdin) as Box<dyn Write + Send + 'static>)
    }

    fn next_event(&mut self, deadline: Instant) -> Result<ProcessEvent, ProcessError> {
        loop {
            // queueに残ったreader messageより時計を先に見る。期限過ぎの
            // backlogを返し続けてtimeoutを先送りしない。
            if Instant::now() >= deadline {
                return Ok(ProcessEvent::TimedOut);
            }
            match self.reader_events.try_recv() {
                Ok(message) => {
                    if let Some(event) = self.handle_reader_message(message)? {
                        return Ok(event);
                    }
                    continue;
                }
                Err(TryRecvError::Disconnected) if self.open_readers > 0 => {
                    return Err(ProcessError::Read(io::Error::other(
                        "process output reader channel closed unexpectedly",
                    )));
                }
                Err(TryRecvError::Disconnected | TryRecvError::Empty) => {}
            }

            self.observe_exit()?;
            if let Some(status) = self.observed_status.filter(|_| self.open_readers == 0) {
                return Ok(ProcessEvent::Exited(status));
            }

            let now = Instant::now();
            if now >= deadline {
                return Ok(ProcessEvent::TimedOut);
            }
            let wait = deadline.duration_since(now).min(MAX_POLL_INTERVAL);
            match self.reader_events.recv_timeout(wait) {
                Ok(message) => {
                    if let Some(event) = self.handle_reader_message(message)? {
                        return Ok(event);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) if self.open_readers > 0 => {
                    return Err(ProcessError::Read(io::Error::other(
                        "process output reader channel closed unexpectedly",
                    )));
                }
                Err(RecvTimeoutError::Disconnected) => {}
            }
        }
    }

    fn send_signal(&mut self, signal: HostSignal) -> Result<(), ProcessError> {
        match self.signal_group(signal.signo()) {
            // 全員が既に終了したgroupへの転送は何もしない成功である。
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => Ok(()),
            result => result.map_err(ProcessError::Signal),
        }
    }

    fn terminate_and_reap(&mut self) -> Result<ProcessStatus, ProcessError> {
        if let Some(status) = self.reaped_status {
            return Ok(status);
        }

        let status = match self.child.try_wait().map_err(ProcessError::Wait)? {
            Some(status) => process_status(status),
            None => match self.kill_group() {
                Ok(()) => process_status(self.child.wait().map_err(ProcessError::Wait)?),
                Err(kill_error) => match self.child.try_wait().map_err(ProcessError::Wait)? {
                    Some(status) => process_status(status),
                    None => return Err(kill_error),
                },
            },
        };
        self.observed_status = Some(status);
        self.reaped_status = Some(status);
        self.join_readers()?;
        Ok(status)
    }
}

impl Drop for SystemProcess {
    fn drop(&mut self) {
        let _ = self.terminate_and_reap();
    }
}

#[derive(Clone, Copy)]
enum Stream {
    Uart,
    Diagnostic,
}

enum ReaderMessage {
    Data(Stream, Vec<u8>),
    Closed,
    Failed(io::Error),
}

enum ReaderEvent {
    Event(ProcessEvent),
    Closed,
}

fn reader_event(message: ReaderMessage) -> Result<ReaderEvent, ProcessError> {
    match message {
        ReaderMessage::Data(Stream::Uart, bytes) => {
            Ok(ReaderEvent::Event(ProcessEvent::Uart(bytes)))
        }
        ReaderMessage::Data(Stream::Diagnostic, bytes) => {
            Ok(ReaderEvent::Event(ProcessEvent::Diagnostic(bytes)))
        }
        ReaderMessage::Closed => Ok(ReaderEvent::Closed),
        ReaderMessage::Failed(error) => Err(ProcessError::Read(error)),
    }
}

fn start_reader(
    mut reader: impl Read + Send + 'static,
    stream: Stream,
    sender: Sender<ReaderMessage>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        loop {
            let mut bytes = vec![0; 4096];
            match reader.read(&mut bytes) {
                Ok(0) => {
                    let _ = sender.send(ReaderMessage::Closed);
                    return;
                }
                Ok(length) => {
                    bytes.truncate(length);
                    if sender.send(ReaderMessage::Data(stream, bytes)).is_err() {
                        return;
                    }
                }
                Err(error) => {
                    let _ = sender.send(ReaderMessage::Failed(error));
                    return;
                }
            }
        }
    })
}

pub(crate) fn process_status(status: ExitStatus) -> ProcessStatus {
    ProcessStatus {
        code: status.code(),
        success: status.success(),
    }
}

#[cfg(test)]
mod tests {
    use super::{ProcessControl, ProcessError, ProcessEvent, ProcessStatus, SystemProcess};
    use crate::HostSignal;
    use std::{
        env,
        io::{self, Read, Write},
        process::Command,
        sync::mpsc,
        time::{Duration, Instant},
    };

    const HELPER_ENV: &str = "MINICONTAINER_PROCESS_HELPER";

    // Runs in a fresh copy of this test binary. The parent exercises the real
    // stdio pipes and process lifecycle without requiring QEMU in unit tests.
    #[test]
    #[ignore]
    fn process_helper() {
        match env::var(HELPER_ENV).as_deref() {
            Ok("emit") => {
                io::stdout().write_all(b"uart line\n").unwrap();
                io::stdout().flush().unwrap();
                io::stderr().write_all(b"diagnostic line\n").unwrap();
                io::stderr().flush().unwrap();
            }
            Ok("sleep") => std::thread::sleep(Duration::from_secs(5)),
            Ok("exit") => std::process::exit(23),
            Ok("sleep-long") => std::thread::sleep(Duration::from_secs(30)),
            Ok("grandchild") => {
                // 同じprocess groupに属する孫をspawnしてから寝る。group宛の
                // signalが直接の子だけを殺さないかを検査する経路になる。
                // 孫のstdioはnullへ向け、pipeを握ったまま回収側を停滞させ
                // ない。
                let executable = env::current_exe().expect("the test binary path must exist");
                // 孫はあえてwaitしない。親より長く生きる孤児を残すことが
                // group宛signalの検査対象そのものである。
                #[allow(clippy::zombie_processes)]
                let grandchild = Command::new(executable)
                    .args([
                        "--ignored",
                        "--exact",
                        "process::tests::process_helper",
                        "--nocapture",
                    ])
                    .env(HELPER_ENV, "sleep-long")
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .expect("the grandchild helper must spawn");
                io::stdout()
                    .write_all(format!("grandchild {}\n", grandchild.id()).as_bytes())
                    .unwrap();
                io::stdout().flush().unwrap();
                std::thread::sleep(Duration::from_secs(5));
            }
            mode => panic!("unknown process helper mode: {mode:?}"),
        }
    }

    // Catches a timeout path that kills the child implicitly, instead of
    // leaving the lifecycle choice with the caller.
    #[test]
    fn timeout_leaves_the_child_for_explicit_reap() {
        let mut child = spawn_helper("sleep").unwrap();
        let pid = child.id();

        assert!(matches!(
            child.wait_until(Duration::from_millis(25)),
            Err(ProcessError::TimedOut)
        ));
        assert!(pid_is_alive(pid));

        child.terminate_and_reap().unwrap();
        assert!(!pid_is_alive(pid));
    }

    // Catches Drop leaking a still-running QEMU child when a caller abandons
    // a run after an earlier error path.
    #[test]
    fn drop_is_a_final_child_reap_fallback() {
        let pid = {
            let child = spawn_helper("sleep").unwrap();
            child.id()
        };

        assert!(!pid_is_alive(pid));
    }

    // Catches returning stale queued or exit state after the deadline has
    // passed: the clock governs even when output remains available.
    #[test]
    fn expired_deadline_wins_over_an_observed_exit() {
        let mut child = spawn_helper("emit").unwrap();
        let generous = Instant::now() + Duration::from_secs(5);
        loop {
            match child.next_event(generous).unwrap() {
                ProcessEvent::Exited(_) => break,
                ProcessEvent::Uart(_) | ProcessEvent::Diagnostic(_) => {}
                ProcessEvent::TimedOut => panic!("the helper must exit before the deadline"),
            }
        }

        assert!(matches!(
            child
                .next_event(Instant::now() - Duration::from_secs(1))
                .unwrap(),
            ProcessEvent::TimedOut
        ));
    }

    // Catches wiring both QEMU pipes to one stream or dropping their bytes
    // before the parent observes them.
    #[test]
    fn stdout_and_stderr_are_reported_as_distinct_events() {
        let mut child = spawn_helper("emit").unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut uart = Vec::new();
        let mut diagnostic = Vec::new();

        loop {
            match child.next_event(deadline).unwrap() {
                ProcessEvent::Uart(bytes) => uart.extend(bytes),
                ProcessEvent::Diagnostic(bytes) => diagnostic.extend(bytes),
                ProcessEvent::Exited(status) => {
                    assert!(status.success);
                    break;
                }
                ProcessEvent::TimedOut => panic!("helper did not exit before deadline"),
            }
        }

        assert!(
            uart.windows(b"uart line\n".len())
                .any(|part| part == b"uart line\n")
        );
        assert!(
            diagnostic
                .windows(b"diagnostic line\n".len())
                .any(|part| part == b"diagnostic line\n")
        );
    }

    // Catches normalizing an unsuccessful child termination into success or
    // losing its exit code during the reap path.
    #[test]
    fn nonzero_exit_status_is_preserved() {
        let mut child = spawn_helper("exit").unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);

        loop {
            match child.next_event(deadline).unwrap() {
                ProcessEvent::Exited(status) => {
                    assert_eq!(
                        status,
                        ProcessStatus {
                            code: Some(23),
                            success: false,
                        }
                    );
                    break;
                }
                ProcessEvent::Uart(_) | ProcessEvent::Diagnostic(_) => {}
                ProcessEvent::TimedOut => panic!("helper did not exit before deadline"),
            }
        }
    }

    // Catches forwarding only to the direct child: a forwarded host signal
    // must reach the child's whole process group or QEMU's own children
    // survive as orphans.
    #[test]
    fn send_signal_reaches_the_whole_process_group() {
        let mut child = spawn_helper("grandchild").unwrap();
        let grandchild = read_grandchild_pid(&mut child);

        child.send_signal(HostSignal::Terminate).unwrap();

        let dead = wait_for_death(grandchild, Duration::from_secs(3));
        let _ = signal_pid(grandchild, libc::SIGKILL);
        child.terminate_and_reap().unwrap();
        assert!(dead, "the forwarded signal must reach the grandchild");
    }

    // Catches a forced cleanup that kills only the direct child: the final
    // reap must also stop processes the child spawned.
    #[test]
    fn terminate_and_reap_kills_the_whole_process_group() {
        let mut child = spawn_helper("grandchild").unwrap();
        let grandchild = read_grandchild_pid(&mut child);

        child.terminate_and_reap().unwrap();

        let dead = wait_for_death(grandchild, Duration::from_secs(3));
        let _ = signal_pid(grandchild, libc::SIGKILL);
        assert!(dead, "the forced reap must reach the grandchild");
    }

    // Catches reporting an error when the forwarded signal lands on a group
    // whose members already exited: a finished child makes forwarding a no-op.
    #[test]
    fn send_signal_to_a_finished_group_is_a_no_op() {
        let mut child = spawn_helper("exit").unwrap();
        child
            .wait_until(Duration::from_secs(1))
            .expect("the helper must exit before the deadline");

        child.send_signal(HostSignal::Interrupt).unwrap();
    }

    // Catches collapsing an OS spawn failure into a timeout or a generic
    // reader error, which would hide a missing QEMU executable.
    #[test]
    fn missing_program_is_reported_as_spawn_error() {
        let error = match SystemProcess::spawn(Command::new("/definitely/not/a/program")) {
            Ok(_) => panic!("a missing executable must not spawn"),
            Err(error) => error,
        };
        assert!(matches!(error, ProcessError::Spawn(_)));
    }

    // Catches reader-side I/O failures being silently treated as EOF. The
    // reader loop itself is real; only its input is a deterministic failing
    // Read implementation because a child pipe cannot reliably be made to
    // fail on demand.
    #[test]
    fn reader_failure_is_reported_as_read_error() {
        let (sender, receiver) = mpsc::channel();
        let reader = super::start_reader(FailingReader, super::Stream::Uart, sender);
        reader.join().unwrap();

        assert!(matches!(
            super::reader_event(receiver.recv().expect("reader must send its failure")),
            Err(ProcessError::Read(_))
        ));
    }

    /// helperがprintした`grandchild <pid>`行をUART eventから読み取る。
    fn read_grandchild_pid(child: &mut SystemProcess) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut text = String::new();
        loop {
            match child.next_event(deadline).unwrap() {
                ProcessEvent::Uart(bytes) => {
                    text.push_str(&String::from_utf8_lossy(&bytes));
                    // libtestのbannerが`grandchild`行より先にstdoutへ来る
                    // ため、全体ではなく行単位で照合する。
                    if let Some(pid) = text.lines().find_map(|line| {
                        line.strip_prefix("grandchild ")
                            .and_then(|field| field.trim().parse().ok())
                    }) {
                        return pid;
                    }
                }
                ProcessEvent::Diagnostic(_) => {}
                ProcessEvent::Exited(_) => panic!("the grandchild helper exited early"),
                ProcessEvent::TimedOut => panic!("the grandchild pid never arrived"),
            }
        }
    }

    /// processが`deadline`までに死ねばtrueを返す。pollだけ行い、killしない。
    fn wait_for_death(pid: u32, deadline: Duration) -> bool {
        let end = Instant::now() + deadline;
        while Instant::now() < end {
            if !pid_is_alive(pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        !pid_is_alive(pid)
    }

    /// 一つのprocessへsignalを送る。group宛ではない直接指定である。
    fn signal_pid(pid: u32, signal: libc::c_int) -> io::Result<()> {
        // SAFETY: `kill(2)`に正のPIDを渡す呼び出しは対象processだけへ送る。
        let result = unsafe { libc::kill(pid as libc::pid_t, signal) };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn spawn_helper(mode: &str) -> Result<SystemProcess, ProcessError> {
        let executable = env::current_exe().expect("the test binary path must exist");
        let mut command = Command::new(executable);
        command
            .args([
                "--ignored",
                "--exact",
                "process::tests::process_helper",
                "--nocapture",
            ])
            .env(HELPER_ENV, mode);
        SystemProcess::spawn(command)
    }

    fn pid_is_alive(pid: u32) -> bool {
        // 外部の`kill` binaryは使わない。platformで挙動が異なるため、
        // `kill(2)`を直接呼ぶ。signal 0はprocessを殺さず存在と権限だけを
        // 検査する。
        // SAFETY: `kill(2)`にsignal 0を渡す呼び出しは副作用がなく、
        // `pid`はspawn直後の子のPIDで`pid_t`に収まる。
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }

    struct FailingReader;

    impl Read for FailingReader {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("injected reader failure"))
        }
    }
}
