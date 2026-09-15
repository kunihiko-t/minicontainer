//! MiniContainer host runtime。

mod command;
mod error;
mod instance;
mod process;
mod session;
mod temp;

pub use command::{QemuCommand, QemuResources};
pub use error::{CleanupFailure, RuntimeError};
pub use instance::{
    InstanceDir, InstanceError, InstanceHandle, InstanceRow, InstanceState, InstanceStatus,
};
pub use process::{
    ProcessBackend, ProcessControl, ProcessError, ProcessEvent, ProcessStatus, SystemProcessBackend,
};
pub use session::{RunOutcome, Session, SessionError, SessionEvent};
pub use temp::PayloadTemp;

use std::{
    ffi::OsStr,
    io,
    path::Path,
    time::{Duration, Instant},
};

/// 一つのMiniContainer runに必要な不変入力。
pub struct RunRequest<'a> {
    /// digestを含めて検証されるMiniBundle bytes。
    pub bundle: &'a [u8],
    /// QEMUが起動するminiOS kernel。
    pub kernel: &'a Path,
    /// UART control eventを待つ全体の期限。
    pub deadline: Duration,
    /// QEMUへ渡すguest resource量。
    pub resources: QemuResources,
    /// hostへ届いたSIGINTとSIGTERMを観測する源。`None`のrunはsignalを
    /// 観測せず、hostがsignalで死んだ場合はchildをreapできない。
    pub interrupts: Option<&'a dyn InterruptSource>,
    /// instance stateを書くdirectoryと`ps`の表示label。`None`のrunは
    /// state fileを作らず、`minictr ps`には出ない。
    pub instances: Option<InstanceRegistration<'a>>,
}

/// runが記録するinstance identityの入力。
#[derive(Debug, Clone, Copy)]
pub struct InstanceRegistration<'a> {
    /// `<store root>/run/`に開かれたstate directory。
    pub dir: &'a InstanceDir,
    /// `ps`のIMAGE列に出すimage tagまたはdigest。
    pub image: &'a str,
    /// spawnされるprogramのpathまたは名前。exec完了まで子のcommは親の
    /// 名前のままなので、basenameがcommへ現れるまでidentityの記録を待つ
    /// ために使う。
    pub program: &'a OsStr,
}

/// hostが受け取った中断signal。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostSignal {
    /// SIGINT (端末のCtrl-Cや`kill -INT`)。
    Interrupt,
    /// SIGTERM。
    Terminate,
}

impl HostSignal {
    /// 診断と公開error文言に使うsignal名。
    pub const fn name(self) -> &'static str {
        match self {
            Self::Interrupt => "SIGINT",
            Self::Terminate => "SIGTERM",
        }
    }

    /// 転送時にchildのprocess groupへ送るsignal番号。
    pub(crate) const fn signo(self) -> libc::c_int {
        match self {
            Self::Interrupt => libc::SIGINT,
            Self::Terminate => libc::SIGTERM,
        }
    }

    /// signal番号から`HostSignal`へ写像する。対象外の番号は`None`。
    pub fn from_signo(signo: libc::c_int) -> Option<Self> {
        match signo {
            libc::SIGINT => Some(Self::Interrupt),
            libc::SIGTERM => Some(Self::Terminate),
            _ => None,
        }
    }
}

/// `InterruptSource::poll`が返す、観測済みsignalのsnapshot。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignalObservation {
    /// 最初に届いたsignal。転送対象は常にこれである。
    pub first: HostSignal,
    /// 到着したsignalの累計数。最初の転送後に増えればrunを即時killする。
    pub count: u32,
}

/// run中のhost signalを`Runtime`へ届ける観測点。
///
/// 実装はasync-signal-safeな記録側 (例: atomicを書くだけのsignal handler) と
/// 分離し、`poll`はrun loopから任意の間隔で呼べる。`count`は単調増加の累計値
/// であり、runtimeは前回pollからの増分だけを新しいsignalとして扱う。
pub trait InterruptSource {
    /// 観測済みのsignalがあればそのsnapshotを、なければ`None`を返す。
    fn poll(&self) -> Option<SignalObservation>;
}

/// signalを観測しないsource。`RunRequest::interrupts`が`None`のrunと同じ
/// 振る舞いを明示したいembedder向け。
pub struct NeverInterrupt;

impl InterruptSource for NeverInterrupt {
    fn poll(&self) -> Option<SignalObservation> {
        None
    }
}

/// decode済みguest eventをrunの終了前に受け取る転送先。
///
/// `push`はguest frameの順序どおりに同期呼び出しされるため、遅いconsumerは
/// event loopへbackpressureをかける。ただし全体の期限は変わらず、停滞した
/// consumerを待ってrunが期限を延ばすことはない。`Err`を返すとrunは
/// [`RuntimeError::Consumer`]で中断するが、QEMUの回収とpayload削除は行う。
pub trait OutputSink {
    /// 一つのdecode済みeventを順序どおりに受け取る。
    fn push(&mut self, event: &SessionEvent) -> io::Result<()>;
}

/// eventを読み捨てる転送先。
struct Discard;

impl OutputSink for Discard {
    fn push(&mut self, _event: &SessionEvent) -> io::Result<()> {
        Ok(())
    }
}

/// 転送した最初のsignalへchildのprocess groupが応答するのを待つ期限。
/// 期限切れまたは続くsignalではprocess groupをSIGKILLで止める。
const SIGNAL_GRACE: Duration = Duration::from_secs(2);

/// signal観測のためにevent loopを起こす最大間隔。
const SIGNAL_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// MiniBundleを一時payloadとしてQEMU上で実行するruntime。
pub struct Runtime<B: ProcessBackend = SystemProcessBackend> {
    backend: B,
    signal_grace: Duration,
}

impl<B: ProcessBackend> Runtime<B> {
    /// 指定backendを使うruntimeを作る。
    pub const fn new(backend: B) -> Self {
        Self {
            backend,
            signal_grace: SIGNAL_GRACE,
        }
    }

    /// signal graceを短くするtest用builder。実QEMUと同じ契約を短時間で
    /// 検査するために使い、公開APIではない。
    #[cfg(test)]
    fn with_signal_grace(mut self, grace: Duration) -> Self {
        self.signal_grace = grace;
        self
    }

    /// MiniBundleを検証し、QEMUで実行してguest outcomeを返す。
    ///
    /// guest出力は`RunOutcome`にだけ集まり、終了前に外部へ出さない。
    pub fn run(&self, request: RunRequest<'_>) -> Result<RunOutcome, RuntimeError> {
        self.run_with_sink(request, &mut Discard)
    }

    /// decode済みeventを`sink`へ逐次転送しながらrunを実行する。
    ///
    /// 転送しても`Session`の蓄積は変わらないため、1 MiB上限は表示済みbyteを
    /// 含めた合計で効く。consumerの失敗は[`RuntimeError::Consumer`]で返す。
    pub fn run_with_sink<S: OutputSink>(
        &self,
        request: RunRequest<'_>,
        sink: &mut S,
    ) -> Result<RunOutcome, RuntimeError> {
        // 表現できない期限は、payloadや子processを取得する前に型付きerrorへ
        // 変える。`Instant + Duration`のoverflow panicを副作用の後で起こさず、
        // Dropを実装しないbackendでも子processを残さない。
        let deadline = Instant::now()
            .checked_add(request.deadline)
            .ok_or(RuntimeError::InvalidDeadline)?;
        minicontainer_bundle::parse(request.bundle).map_err(RuntimeError::Bundle)?;
        let payload = PayloadTemp::create(request.bundle)?;
        let command = match QemuCommand::new(request.kernel, payload.path(), request.resources) {
            Ok(command) => command,
            Err(error) => return finish_without_child(Err(error), &payload),
        };
        let mut child = match self.backend.spawn(&command) {
            Ok(child) => child,
            Err(error) => return finish_without_child(Err(RuntimeError::Process(error)), &payload),
        };
        // 起動したprocessのidentityをrunの開始時点で記録する。記録できない
        // instanceを起動したままにはしないため、失敗時は通常cleanupを通す。
        let instance = match &request.instances {
            Some(registration) => match registration.dir.register(
                registration.image,
                child.id(),
                registration.program,
            ) {
                Ok(handle) => Some((registration.dir, handle)),
                Err(error) => {
                    let mut failures = Vec::new();
                    if let Err(reap) = child.terminate_and_reap() {
                        failures.push(CleanupFailure::Process(reap));
                    }
                    failures.extend(remove_payload(&payload));
                    return finish_with_cleanup(Err(RuntimeError::Instance(error)), failures);
                }
            },
            None => None,
        };

        let mut session = Session::new();
        let mut signals = SignalWatch::new(request.interrupts, self.signal_grace);
        let primary = 'events: loop {
            let bound = signals.bound(deadline);
            // queueにeventが残っていても全体の期限を強制する。backendが期限
            // 過ぎの出力を返し続けても、この検査がrunをtimeoutで終わらせる。
            // sinkの停滞でloopが止まっても、復帰後の先頭検査が期限を効かせる。
            if Instant::now() >= bound {
                break Err(signals.bound_error());
            }
            if let Err(error) = signals.poll(&mut child) {
                break Err(error);
            }
            match child.next_event(signals.event_deadline(bound)) {
                Ok(ProcessEvent::Uart(bytes)) => {
                    let events = match session.push_uart(&bytes) {
                        Ok(events) => events,
                        Err(error) => break Err(RuntimeError::Session(error)),
                    };
                    for event in &events {
                        if let Err(error) = sink.push(event) {
                            break 'events Err(RuntimeError::Consumer(error));
                        }
                    }
                }
                Ok(ProcessEvent::Diagnostic(_)) => {}
                Ok(ProcessEvent::Exited(status)) => {
                    break signals.finish(&mut session, status);
                }
                // 観測間隔に切り詰めた期限でも同じ検査から始め直すだけであり、
                // 実際の期限は`bound`が持つ。
                Ok(ProcessEvent::TimedOut) => continue,
                Err(error) => break Err(RuntimeError::Process(error)),
            }
        };

        let mut cleanup_failures = Vec::new();
        if let Err(error) = child.terminate_and_reap() {
            cleanup_failures.push(CleanupFailure::Process(error));
        }
        cleanup_failures.extend(remove_payload(&payload));
        // state fileはinstanceが完全に畳まれてから消す。
        if let Some((dir, handle)) = &instance
            && let Err(error) = dir.unregister(handle)
        {
            cleanup_failures.push(CleanupFailure::Instance(error));
        }
        finish_with_cleanup(primary, cleanup_failures)
    }
}

impl Default for Runtime<SystemProcessBackend> {
    fn default() -> Self {
        Self::new(SystemProcessBackend::new())
    }
}

fn finish_without_child(
    primary: Result<RunOutcome, RuntimeError>,
    payload: &PayloadTemp,
) -> Result<RunOutcome, RuntimeError> {
    finish_with_cleanup(primary, remove_payload(payload))
}

/// 一回のrunにおけるhost signalの転送とgraceの状態機械。
///
/// 最初のsignalはchildのprocess groupへ転送し、`grace`後も生きていれば
/// 呼び出し側の通常cleanup (SIGKILL) へ委ねる。二回目以降のsignalはgraceを
/// 待たずに即座にrunを中断する。guest Exitが確定した後に届いたsignalは
/// 結果を変えず、QEMUの早期終了を促すだけである。
struct SignalWatch<'a> {
    source: Option<&'a dyn InterruptSource>,
    grace: Duration,
    seen: u32,
    interrupted: Option<HostSignal>,
    grace_end: Option<Instant>,
}

impl<'a> SignalWatch<'a> {
    fn new(source: Option<&'a dyn InterruptSource>, grace: Duration) -> Self {
        Self {
            source,
            grace,
            seen: 0,
            interrupted: None,
            grace_end: None,
        }
    }

    /// このloop反復で有効な期限。転送後はrunのdeadlineではなくgraceの
    /// 終端が効く。
    fn bound(&self, deadline: Instant) -> Instant {
        self.grace_end.unwrap_or(deadline)
    }

    /// `next_event`へ渡す期限。sourceがあるときだけ観測間隔で切り詰め、
    /// signalの検知遅延を`SIGNAL_POLL_INTERVAL`以内に抑える。
    fn event_deadline(&self, bound: Instant) -> Instant {
        match self.source {
            Some(_) => bound.min(Instant::now() + SIGNAL_POLL_INTERVAL),
            None => bound,
        }
    }

    /// 新しく届いたsignalを処理する。戻り値の`Err`はrunの主結果になる。
    ///
    /// 転送自体に失敗した場合はそのprocess errorを返す。中断ではなく
    /// 失敗を表すほうが原因として正確であり、後始末の強制killは別途行う。
    fn poll<C: ProcessControl>(&mut self, child: &mut C) -> Result<(), RuntimeError> {
        let Some(source) = self.source else {
            return Ok(());
        };
        let Some(observation) = source.poll() else {
            return Ok(());
        };
        if observation.count <= self.seen {
            return Ok(());
        }
        self.seen = observation.count;
        if let Some(interrupted) = self.interrupted {
            return Err(RuntimeError::Interrupted(interrupted));
        }
        child
            .send_signal(observation.first)
            .map_err(RuntimeError::Process)?;
        self.interrupted = Some(observation.first);
        self.grace_end = Some(Instant::now() + self.grace);
        // 最初の観測までに複数signalが溜まっていた場合もgraceを飛ばす。
        if observation.count > 1 {
            return Err(RuntimeError::Interrupted(observation.first));
        }
        Ok(())
    }

    /// 期限到達時の主結果。転送済みなら中断、そうでなければtimeout。
    fn bound_error(&self) -> RuntimeError {
        match self.interrupted {
            Some(signal) => RuntimeError::Interrupted(signal),
            None => RuntimeError::Process(ProcessError::TimedOut),
        }
    }

    /// child終了時のrun結果。確定済みのguest結果は中断後も返し、中断が
    /// 原因で終了した経路だけを`Interrupted`として報告する。
    fn finish(
        &self,
        session: &mut Session,
        status: ProcessStatus,
    ) -> Result<RunOutcome, RuntimeError> {
        match session.finish(status) {
            Ok(outcome) => Ok(outcome),
            Err(error) => Err(match self.interrupted {
                Some(signal) => RuntimeError::Interrupted(signal),
                None => RuntimeError::Session(error),
            }),
        }
    }
}

fn finish_with_cleanup(
    primary: Result<RunOutcome, RuntimeError>,
    cleanup_failures: Vec<CleanupFailure>,
) -> Result<RunOutcome, RuntimeError> {
    match (primary, cleanup_failures.is_empty()) {
        (Ok(outcome), true) => Ok(outcome),
        (Ok(_), false) => Err(RuntimeError::CleanupOnly(cleanup_failures)),
        (Err(error), true) => Err(error),
        (Err(primary), false) => Err(RuntimeError::Cleanup {
            primary: Box::new(primary),
            failures: cleanup_failures,
        }),
    }
}

fn remove_payload(payload: &PayloadTemp) -> Vec<CleanupFailure> {
    let mut failures = Vec::new();
    if let Err(error) = std::fs::remove_file(payload.path()) {
        failures.push(CleanupFailure::Payload(error));
    }
    if let Err(error) = std::fs::remove_dir(payload.path().parent().expect("payload has a root")) {
        failures.push(CleanupFailure::Payload(error));
    }
    failures
}

#[cfg(test)]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        collections::VecDeque,
        env,
        ffi::OsStr,
        fs, io,
        path::PathBuf,
        process::{Child, Command},
        rc::Rc,
        sync::atomic::{AtomicU64, Ordering},
        time::{Duration, Instant},
    };

    use minicontainer_bundle::{ImageSpec, build};
    use minios_abi::control::{FrameHeader, FrameKind, ReadyPayload};

    use super::{
        CleanupFailure, HostSignal, InstanceDir, InstanceError, InstanceRegistration,
        InterruptSource, OutputSink, ProcessBackend, ProcessControl, ProcessError, ProcessEvent,
        ProcessStatus, QemuCommand, QemuResources, RunRequest, Runtime, RuntimeError, SessionError,
        SessionEvent, SignalObservation,
    };

    // Catches skipping any part of the happy-path lifecycle: only a validated
    // bundle can be materialized, decoded, reaped, and then removed.
    #[test]
    fn run_validates_materializes_decodes_reaps_and_removes_the_payload() {
        let trace = Rc::new(RefCell::new(Vec::new()));
        let payload = Rc::new(RefCell::new(None));
        let backend = FakeBackend::events(
            trace.clone(),
            payload.clone(),
            vec![
                ProcessEvent::Uart(guest_stream(7)),
                ProcessEvent::Exited(successful_process()),
            ],
        );
        let runtime = Runtime::new(backend);

        let outcome = runtime
            .run(RunRequest {
                bundle: &valid_bundle(),
                kernel: "/kernel".as_ref(),
                deadline: Duration::from_secs(1),
                resources: QemuResources::DEFAULT,
                interrupts: None,
                instances: None,
            })
            .unwrap();

        assert_eq!(outcome.exit_code, 7);
        assert_eq!(
            trace.borrow().as_slice(),
            ["spawn", "decode", "exit", "reap"]
        );
        assert!(!payload.borrow().as_ref().unwrap().exists());
    }

    // Catches leaking a child or temporary payload on every terminal failure.
    #[test]
    fn run_reaps_and_removes_the_payload_after_every_started_run_failure() {
        let cases = [
            (
                "protocol",
                vec![
                    ProcessEvent::Uart(malformed_stream()),
                    ProcessEvent::Exited(successful_process()),
                ],
            ),
            (
                "guest error",
                vec![
                    ProcessEvent::Uart(guest_error_stream()),
                    ProcessEvent::Exited(successful_process()),
                ],
            ),
            (
                "output cap",
                vec![
                    ProcessEvent::Uart(output_flood_stream()),
                    ProcessEvent::Exited(successful_process()),
                ],
            ),
            ("timeout", vec![]),
            (
                "early qemu exit",
                vec![ProcessEvent::Exited(ProcessStatus {
                    code: Some(1),
                    success: false,
                })],
            ),
        ];

        for (name, events) in cases {
            let trace = Rc::new(RefCell::new(Vec::new()));
            let payload = Rc::new(RefCell::new(None));
            let runtime = Runtime::new(FakeBackend::events(trace.clone(), payload.clone(), events));

            assert!(
                runtime
                    .run(RunRequest {
                        bundle: &valid_bundle(),
                        kernel: "/kernel".as_ref(),
                        deadline: Duration::from_secs(1),
                        resources: QemuResources::DEFAULT,
                        interrupts: None,
                        instances: None,
                    })
                    .is_err(),
                "{name} must fail"
            );
            assert_eq!(
                trace
                    .borrow()
                    .iter()
                    .filter(|event| **event == "reap")
                    .count(),
                1,
                "{name} must reap exactly once"
            );
            assert!(
                !payload.borrow().as_ref().unwrap().exists(),
                "{name} must remove its payload"
            );
        }
    }

    // Catches doing QEMU work for a malformed bundle and retaining the
    // materialized payload when spawn itself fails.
    #[test]
    fn run_rejects_invalid_bundles_before_spawn_and_cleans_up_after_spawn_failure() {
        let invalid_trace = Rc::new(RefCell::new(Vec::new()));
        let invalid_payload = Rc::new(RefCell::new(None));
        let invalid_runtime = Runtime::new(FakeBackend::events(
            invalid_trace.clone(),
            invalid_payload.clone(),
            vec![],
        ));

        assert!(
            invalid_runtime
                .run(RunRequest {
                    bundle: b"not a bundle",
                    kernel: "/kernel".as_ref(),
                    deadline: Duration::from_secs(1),
                    resources: QemuResources::DEFAULT,
                    interrupts: None,
                    instances: None,
                })
                .is_err()
        );
        assert!(invalid_trace.borrow().is_empty());
        assert!(invalid_payload.borrow().is_none());

        let trace = Rc::new(RefCell::new(Vec::new()));
        let payload = Rc::new(RefCell::new(None));
        let runtime = Runtime::new(FakeBackend::spawn_fails(trace.clone(), payload.clone()));

        assert!(
            runtime
                .run(RunRequest {
                    bundle: &valid_bundle(),
                    kernel: "/kernel".as_ref(),
                    deadline: Duration::from_secs(1),
                    resources: QemuResources::DEFAULT,
                    interrupts: None,
                    instances: None,
                })
                .is_err()
        );
        assert_eq!(trace.borrow().as_slice(), ["spawn"]);
        assert!(!payload.borrow().as_ref().unwrap().exists());
    }

    // Catches postponing the overall deadline while output keeps arriving:
    // an endless diagnostic stream must still end the run with a timeout.
    #[test]
    fn run_enforces_the_deadline_despite_continuous_output() {
        struct FloodChild;

        impl ProcessControl for FloodChild {
            fn id(&self) -> u32 {
                7
            }

            fn next_event(&mut self, _deadline: Instant) -> Result<ProcessEvent, ProcessError> {
                Ok(ProcessEvent::Diagnostic(vec![0x78]))
            }

            fn send_signal(&mut self, _signal: HostSignal) -> Result<(), ProcessError> {
                Ok(())
            }

            fn terminate_and_reap(&mut self) -> Result<ProcessStatus, ProcessError> {
                Ok(successful_process())
            }
        }

        struct FloodBackend;

        impl ProcessBackend for FloodBackend {
            type Child = FloodChild;

            fn spawn(&self, _command: &QemuCommand) -> Result<Self::Child, ProcessError> {
                Ok(FloodChild)
            }
        }

        let started = Instant::now();
        let error = Runtime::new(FloodBackend)
            .run(RunRequest {
                bundle: &valid_bundle(),
                kernel: "/kernel".as_ref(),
                deadline: Duration::from_millis(100),
                resources: QemuResources::DEFAULT,
                interrupts: None,
                instances: None,
            })
            .unwrap_err();

        assert!(
            matches!(error, RuntimeError::Process(ProcessError::TimedOut)),
            "continuous output must not bypass the deadline, got {error:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the deadline must end the run promptly"
        );
    }

    // Catches an unrepresentable public deadline panicking after the child
    // is spawned: the deadline is validated before any side effect.
    #[test]
    fn run_rejects_an_unrepresentable_deadline_before_spawn() {
        let trace = Rc::new(RefCell::new(Vec::new()));
        let payload = Rc::new(RefCell::new(None));
        let runtime = Runtime::new(FakeBackend::events(trace.clone(), payload.clone(), vec![]));

        let error = runtime
            .run(RunRequest {
                bundle: &valid_bundle(),
                kernel: "/kernel".as_ref(),
                deadline: Duration::MAX,
                resources: QemuResources::DEFAULT,
                interrupts: None,
                instances: None,
            })
            .unwrap_err();

        assert!(
            matches!(error, RuntimeError::InvalidDeadline),
            "Duration::MAX must be a typed error, got {error:?}"
        );
        assert!(
            trace.borrow().is_empty(),
            "no child may be spawned for an invalid deadline"
        );
        assert!(
            payload.borrow().is_none(),
            "no payload may be materialized for an invalid deadline"
        );
    }

    // Catches hiding the guest failure when process cleanup also fails. The
    // caller needs both causes to diagnose the guest and host sides.
    #[test]
    fn run_retains_the_primary_error_when_reap_also_fails() {
        let trace = Rc::new(RefCell::new(Vec::new()));
        let payload = Rc::new(RefCell::new(None));
        let runtime = Runtime::new(
            FakeBackend::events(
                trace,
                payload,
                vec![ProcessEvent::Uart(guest_error_stream())],
            )
            .reap_fails(),
        );

        let error = runtime
            .run(RunRequest {
                bundle: &valid_bundle(),
                kernel: "/kernel".as_ref(),
                deadline: Duration::from_secs(1),
                resources: QemuResources::DEFAULT,
                interrupts: None,
                instances: None,
            })
            .unwrap_err();

        match error {
            RuntimeError::Cleanup { primary, failures } => {
                assert!(matches!(
                    *primary,
                    RuntimeError::Session(SessionError::GuestError(ref bytes)) if bytes == b"guest failed"
                ));
                assert!(matches!(
                    failures.as_slice(),
                    [CleanupFailure::Process(ProcessError::Wait(_))]
                ));
            }
            other => panic!("expected primary and cleanup failures, got {other:?}"),
        }
    }

    // Catches buffering the whole run before showing anything: every decoded
    // chunk must reach the sink, in guest frame order with streams kept
    // distinct, before the child is reaped.
    #[test]
    fn streams_stdout_and_stderr_chunks_before_exit() {
        let mut stream = ready_frame();
        stream.extend_from_slice(&frame(FrameKind::Stdout, b"out-one\n"));
        stream.extend_from_slice(&frame(FrameKind::Stderr, b"err-one\n"));
        stream.extend_from_slice(&frame(FrameKind::Stdout, b"out-two\n"));
        stream.extend_from_slice(&frame(FrameKind::Exit, &7_u32.to_le_bytes()));
        let trace = Rc::new(RefCell::new(Vec::new()));
        let payload = Rc::new(RefCell::new(None));
        let runtime = Runtime::new(FakeBackend::events(
            trace.clone(),
            payload.clone(),
            vec![
                ProcessEvent::Uart(stream),
                ProcessEvent::Exited(successful_process()),
            ],
        ));
        let mut sink = RecordingSink::new(trace.clone());

        let outcome = runtime
            .run_with_sink(
                RunRequest {
                    bundle: &valid_bundle(),
                    kernel: "/kernel".as_ref(),
                    deadline: Duration::from_secs(1),
                    resources: QemuResources::DEFAULT,
                    interrupts: None,
                    instances: None,
                },
                &mut sink,
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 7);
        assert_eq!(
            sink.events,
            vec![
                SessionEvent::Ready,
                SessionEvent::Stdout(b"out-one\n".to_vec()),
                SessionEvent::Stderr(b"err-one\n".to_vec()),
                SessionEvent::Stdout(b"out-two\n".to_vec()),
                SessionEvent::Exit(7),
            ]
        );
        assert!(
            sink.reaps_at_push.iter().all(|reaps| *reaps == 0),
            "every chunk must arrive before the reap, got {:?}",
            sink.reaps_at_push
        );
        assert!(trace.borrow().contains(&"reap"));
    }

    // Catches losing a control frame that straddles two UART reads: the
    // decoder still reassembles it and the sink observes one whole chunk.
    #[test]
    fn reassembles_a_stdout_frame_split_across_uart_reads() {
        let mut full = ready_frame();
        let cut = full.len() + 3;
        full.extend_from_slice(&frame(FrameKind::Stdout, b"split payload"));
        full.extend_from_slice(&frame(FrameKind::Exit, &0_u32.to_le_bytes()));
        let trace = Rc::new(RefCell::new(Vec::new()));
        let payload = Rc::new(RefCell::new(None));
        let runtime = Runtime::new(FakeBackend::events(
            trace.clone(),
            payload.clone(),
            vec![
                ProcessEvent::Uart(full[..cut].to_vec()),
                ProcessEvent::Uart(full[cut..].to_vec()),
                ProcessEvent::Exited(successful_process()),
            ],
        ));
        let mut sink = RecordingSink::new(trace.clone());

        runtime
            .run_with_sink(
                RunRequest {
                    bundle: &valid_bundle(),
                    kernel: "/kernel".as_ref(),
                    deadline: Duration::from_secs(1),
                    resources: QemuResources::DEFAULT,
                    interrupts: None,
                    instances: None,
                },
                &mut sink,
            )
            .unwrap();

        let stdout: Vec<&[u8]> = sink
            .events
            .iter()
            .filter_map(|event| match event {
                SessionEvent::Stdout(bytes) => Some(bytes.as_slice()),
                _ => None,
            })
            .collect();
        assert_eq!(stdout, vec![b"split payload".as_slice()]);
    }

    // Catches leaking the child or payload when the consumer fails mid-run:
    // the failure is typed, and cleanup still runs exactly once.
    #[test]
    fn consumer_failure_aborts_the_run_but_still_reaps_qemu() {
        let mut stream = ready_frame();
        stream.extend_from_slice(&frame(FrameKind::Stdout, b"shown\n"));
        stream.extend_from_slice(&frame(FrameKind::Stderr, b"never shown\n"));
        stream.extend_from_slice(&frame(FrameKind::Exit, &0_u32.to_le_bytes()));
        let trace = Rc::new(RefCell::new(Vec::new()));
        let payload = Rc::new(RefCell::new(None));
        let runtime = Runtime::new(FakeBackend::events(
            trace.clone(),
            payload.clone(),
            vec![
                ProcessEvent::Uart(stream),
                ProcessEvent::Exited(successful_process()),
            ],
        ));
        let mut sink = RecordingSink::new(trace.clone()).fail_after(2);

        let error = runtime
            .run_with_sink(
                RunRequest {
                    bundle: &valid_bundle(),
                    kernel: "/kernel".as_ref(),
                    deadline: Duration::from_secs(1),
                    resources: QemuResources::DEFAULT,
                    interrupts: None,
                    instances: None,
                },
                &mut sink,
            )
            .unwrap_err();

        assert!(
            matches!(error, RuntimeError::Consumer(_)),
            "a sink failure must be typed, got {error:?}"
        );
        assert_eq!(sink.events.len(), 2);
        assert_eq!(
            trace
                .borrow()
                .iter()
                .filter(|event| **event == "reap")
                .count(),
            1
        );
        assert!(!payload.borrow().as_ref().unwrap().exists());
    }

    // Catches a slow consumer postponing the run past its deadline: the
    // callback applies backpressure, but the overall deadline still ends the
    // run and reaps the child.
    #[test]
    fn slow_consumer_cannot_extend_the_run_past_the_deadline() {
        let mut stream = ready_frame();
        stream.extend_from_slice(&frame(FrameKind::Stdout, b"slow\n"));
        let trace = Rc::new(RefCell::new(Vec::new()));
        let payload = Rc::new(RefCell::new(None));
        let runtime = Runtime::new(FakeBackend::events(
            trace.clone(),
            payload.clone(),
            vec![
                ProcessEvent::Uart(stream),
                ProcessEvent::Exited(successful_process()),
            ],
        ));
        let mut sink = RecordingSink::new(trace.clone()).sleep(Duration::from_millis(300));

        let error = runtime
            .run_with_sink(
                RunRequest {
                    bundle: &valid_bundle(),
                    kernel: "/kernel".as_ref(),
                    deadline: Duration::from_millis(50),
                    resources: QemuResources::DEFAULT,
                    interrupts: None,
                    instances: None,
                },
                &mut sink,
            )
            .unwrap_err();

        assert!(
            matches!(error, RuntimeError::Process(ProcessError::TimedOut)),
            "a stalled consumer must still time out, got {error:?}"
        );
        assert_eq!(
            trace
                .borrow()
                .iter()
                .filter(|event| **event == "reap")
                .count(),
            1
        );
        assert!(!payload.borrow().as_ref().unwrap().exists());
    }

    // Catches counting only buffered bytes toward the output cap: exactly
    // 1 MiB streams to the consumer, and one more byte is refused even though
    // the earlier bytes were already displayed.
    #[test]
    fn output_cap_counts_bytes_already_streamed_to_the_consumer() {
        let chunk = vec![b'o'; 32 * 1024];
        let mut flood = ready_frame();
        for _ in 0..32 {
            flood.extend_from_slice(&frame(FrameKind::Stdout, &chunk));
        }
        let trace = Rc::new(RefCell::new(Vec::new()));
        let payload = Rc::new(RefCell::new(None));
        let runtime = Runtime::new(FakeBackend::events(
            trace.clone(),
            payload.clone(),
            vec![
                ProcessEvent::Uart(flood),
                ProcessEvent::Uart(frame(FrameKind::Stderr, b"o")),
                ProcessEvent::Exited(successful_process()),
            ],
        ));
        let mut sink = RecordingSink::new(trace.clone());

        let error = runtime
            .run_with_sink(
                RunRequest {
                    bundle: &valid_bundle(),
                    kernel: "/kernel".as_ref(),
                    deadline: Duration::from_secs(5),
                    resources: QemuResources::DEFAULT,
                    interrupts: None,
                    instances: None,
                },
                &mut sink,
            )
            .unwrap_err();

        assert!(
            matches!(
                error,
                RuntimeError::Session(SessionError::GuestOutputTooLarge)
            ),
            "the byte past 1 MiB must be refused, got {error:?}"
        );
        let streamed: usize = sink
            .events
            .iter()
            .map(|event| match event {
                SessionEvent::Stdout(bytes)
                | SessionEvent::Stderr(bytes)
                | SessionEvent::Diagnostic(bytes) => bytes.len(),
                SessionEvent::Ready | SessionEvent::Exit(_) => 0,
            })
            .sum();
        assert_eq!(streamed, 1024 * 1024);
    }

    // Catches ignoring a delivered host signal: the first signal must reach
    // the child, and the run must end as an interrupt instead of waiting out
    // the deadline.
    #[test]
    fn a_first_signal_is_forwarded_and_the_run_reports_interrupted() {
        let trace = Rc::new(RefCell::new(Vec::new()));
        let payload = Rc::new(RefCell::new(None));
        let runtime = Runtime::new(FakeBackend::events(trace.clone(), payload.clone(), vec![]))
            .with_signal_grace(Duration::from_millis(60));
        let interrupts = ScriptedInterrupts::observing([Some(SignalObservation {
            first: HostSignal::Interrupt,
            count: 1,
        })]);

        let error = runtime
            .run(RunRequest {
                bundle: &valid_bundle(),
                kernel: "/kernel".as_ref(),
                deadline: Duration::from_secs(60),
                resources: QemuResources::DEFAULT,
                interrupts: Some(&interrupts),
                instances: None,
            })
            .unwrap_err();

        assert!(
            matches!(error, RuntimeError::Interrupted(HostSignal::Interrupt)),
            "the run must report the interrupt, got {error:?}"
        );
        let events = trace.borrow();
        assert!(
            events.contains(&"sigint"),
            "SIGINT must be forwarded to the child, got {events:?}"
        );
        assert_eq!(events.iter().filter(|event| **event == "reap").count(), 1);
        assert!(!payload.borrow().as_ref().unwrap().exists());
    }

    // Catches forwarding SIGTERM as the wrong signal: the child must see the
    // same signal the host received, not a rewritten one.
    #[test]
    fn a_terminate_signal_is_forwarded_as_sigterm() {
        let trace = Rc::new(RefCell::new(Vec::new()));
        let payload = Rc::new(RefCell::new(None));
        let runtime = Runtime::new(FakeBackend::events(trace.clone(), payload.clone(), vec![]))
            .with_signal_grace(Duration::from_millis(60));
        let interrupts = ScriptedInterrupts::observing([Some(SignalObservation {
            first: HostSignal::Terminate,
            count: 1,
        })]);

        let error = runtime
            .run(RunRequest {
                bundle: &valid_bundle(),
                kernel: "/kernel".as_ref(),
                deadline: Duration::from_secs(60),
                resources: QemuResources::DEFAULT,
                interrupts: Some(&interrupts),
                instances: None,
            })
            .unwrap_err();

        assert!(
            matches!(error, RuntimeError::Interrupted(HostSignal::Terminate)),
            "the run must report SIGTERM, got {error:?}"
        );
        assert!(
            trace.borrow().contains(&"sigterm"),
            "SIGTERM must be forwarded to the child"
        );
        assert!(!payload.borrow().as_ref().unwrap().exists());
    }

    // Catches discarding a finalized guest result: a signal after the Exit
    // frame still forwards to QEMU, but a guest outcome must be returned when
    // QEMU exits inside the grace period.
    #[test]
    fn a_signal_after_guest_exit_preserves_the_guest_outcome() {
        let trace = Rc::new(RefCell::new(Vec::new()));
        let payload = Rc::new(RefCell::new(None));
        let runtime = Runtime::new(FakeBackend::events(
            trace.clone(),
            payload.clone(),
            vec![
                ProcessEvent::Uart(guest_stream(7)),
                ProcessEvent::Exited(successful_process()),
            ],
        ));
        let interrupts = ScriptedInterrupts::observing([
            None,
            Some(SignalObservation {
                first: HostSignal::Interrupt,
                count: 1,
            }),
        ]);

        let outcome = runtime
            .run(RunRequest {
                bundle: &valid_bundle(),
                kernel: "/kernel".as_ref(),
                deadline: Duration::from_secs(60),
                resources: QemuResources::DEFAULT,
                interrupts: Some(&interrupts),
                instances: None,
            })
            .unwrap();

        assert_eq!(outcome.exit_code, 7);
        assert!(
            trace.borrow().contains(&"sigint"),
            "the signal must still reach QEMU for a prompt exit"
        );
    }

    // Catches freezing the output pipeline while interrupted: a signal while
    // frames are still arriving must forward to the child and keep streaming
    // to the sink for the rest of the grace period.
    #[test]
    fn a_signal_during_output_keeps_streaming_until_the_child_exits() {
        let mut before = ready_frame();
        before.extend_from_slice(&frame(FrameKind::Stdout, b"before signal\n"));
        let during = frame(FrameKind::Stdout, b"during grace\n");
        let trace = Rc::new(RefCell::new(Vec::new()));
        let payload = Rc::new(RefCell::new(None));
        let runtime = Runtime::new(FakeBackend::events(
            trace.clone(),
            payload.clone(),
            vec![
                ProcessEvent::Uart(before),
                ProcessEvent::Uart(during),
                ProcessEvent::Exited(successful_process()),
            ],
        ))
        .with_signal_grace(Duration::from_secs(60));
        let interrupts = ScriptedInterrupts::observing([
            None,
            Some(SignalObservation {
                first: HostSignal::Interrupt,
                count: 1,
            }),
        ]);
        let mut sink = RecordingSink::new(trace.clone());

        let error = runtime
            .run_with_sink(
                RunRequest {
                    bundle: &valid_bundle(),
                    kernel: "/kernel".as_ref(),
                    deadline: Duration::from_secs(60),
                    resources: QemuResources::DEFAULT,
                    interrupts: Some(&interrupts),
                    instances: None,
                },
                &mut sink,
            )
            .unwrap_err();

        assert!(
            matches!(error, RuntimeError::Interrupted(HostSignal::Interrupt)),
            "a child exit without a guest Exit must report the interrupt, got {error:?}"
        );
        assert!(
            sink.events
                .contains(&SessionEvent::Stdout(b"during grace\n".to_vec())),
            "output must keep streaming during grace, got {:?}",
            sink.events
        );
        assert!(trace.borrow().contains(&"sigint"));
    }

    // Catches letting a second signal sit behind the grace period: once a
    // signal was forwarded, any later signal must stop waiting and kill.
    #[test]
    fn a_second_signal_during_grace_kills_the_run_immediately() {
        let trace = Rc::new(RefCell::new(Vec::new()));
        let payload = Rc::new(RefCell::new(None));
        let runtime = Runtime::new(FakeBackend::events(trace.clone(), payload.clone(), vec![]))
            .with_signal_grace(Duration::from_secs(60));
        let interrupts = ScriptedInterrupts::observing([
            Some(SignalObservation {
                first: HostSignal::Interrupt,
                count: 1,
            }),
            Some(SignalObservation {
                first: HostSignal::Interrupt,
                count: 2,
            }),
        ]);

        let started = Instant::now();
        let error = runtime
            .run(RunRequest {
                bundle: &valid_bundle(),
                kernel: "/kernel".as_ref(),
                deadline: Duration::from_secs(60),
                resources: QemuResources::DEFAULT,
                interrupts: Some(&interrupts),
                instances: None,
            })
            .unwrap_err();

        assert!(
            matches!(error, RuntimeError::Interrupted(HostSignal::Interrupt)),
            "the second signal must interrupt the run, got {error:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the second signal must not wait out the grace period"
        );
        let events = trace.borrow();
        assert_eq!(events.iter().filter(|event| **event == "sigint").count(), 1);
        assert_eq!(events.iter().filter(|event| **event == "reap").count(), 1);
        assert!(!payload.borrow().as_ref().unwrap().exists());
    }

    // Catches a signal forward failure being hidden behind the interrupt: the
    // forwarding failure is the honest primary error while cleanup still runs.
    #[test]
    fn a_failed_forward_reports_the_process_error_and_still_cleans_up() {
        let trace = Rc::new(RefCell::new(Vec::new()));
        let payload = Rc::new(RefCell::new(None));
        let runtime = Runtime::new(
            FakeBackend::events(trace.clone(), payload.clone(), vec![]).signal_fails(),
        );
        let interrupts = ScriptedInterrupts::observing([Some(SignalObservation {
            first: HostSignal::Interrupt,
            count: 1,
        })]);

        let error = runtime
            .run(RunRequest {
                bundle: &valid_bundle(),
                kernel: "/kernel".as_ref(),
                deadline: Duration::from_secs(60),
                resources: QemuResources::DEFAULT,
                interrupts: Some(&interrupts),
                instances: None,
            })
            .unwrap_err();

        assert!(
            matches!(error, RuntimeError::Process(ProcessError::Signal(_))),
            "the forwarding failure must be the primary error, got {error:?}"
        );
        assert_eq!(
            trace
                .borrow()
                .iter()
                .filter(|event| **event == "reap")
                .count(),
            1
        );
        assert!(!payload.borrow().as_ref().unwrap().exists());
    }

    // Catches the interrupt swallowing cleanup diagnostics: a failed reap
    // after a signal must keep the interrupt as primary and still surface
    // the cleanup failure.
    #[test]
    fn an_interrupted_run_still_reports_cleanup_failures() {
        let trace = Rc::new(RefCell::new(Vec::new()));
        let payload = Rc::new(RefCell::new(None));
        let runtime =
            Runtime::new(FakeBackend::events(trace.clone(), payload.clone(), vec![]).reap_fails())
                .with_signal_grace(Duration::from_millis(60));
        let interrupts = ScriptedInterrupts::observing([Some(SignalObservation {
            first: HostSignal::Interrupt,
            count: 1,
        })]);

        let error = runtime
            .run(RunRequest {
                bundle: &valid_bundle(),
                kernel: "/kernel".as_ref(),
                deadline: Duration::from_secs(60),
                resources: QemuResources::DEFAULT,
                interrupts: Some(&interrupts),
                instances: None,
            })
            .unwrap_err();

        match error {
            RuntimeError::Cleanup { primary, failures } => {
                assert!(
                    matches!(*primary, RuntimeError::Interrupted(HostSignal::Interrupt)),
                    "the interrupt must stay primary, got {primary:?}"
                );
                assert!(matches!(
                    failures.as_slice(),
                    [CleanupFailure::Process(ProcessError::Wait(_))]
                ));
            }
            other => panic!("expected interrupt plus cleanup failures, got {other:?}"),
        }
    }

    // Catches a pending signal outranking an elapsed deadline: a run that is
    // already over the deadline must keep the timeout outcome rather than
    // starting a grace period.
    #[test]
    fn a_signal_pending_at_the_deadline_still_times_out() {
        let trace = Rc::new(RefCell::new(Vec::new()));
        let payload = Rc::new(RefCell::new(None));
        let runtime = Runtime::new(FakeBackend::events(trace.clone(), payload.clone(), vec![]));
        let interrupts = ScriptedInterrupts::observing([Some(SignalObservation {
            first: HostSignal::Interrupt,
            count: 1,
        })]);

        let error = runtime
            .run(RunRequest {
                bundle: &valid_bundle(),
                kernel: "/kernel".as_ref(),
                deadline: Duration::ZERO,
                resources: QemuResources::DEFAULT,
                interrupts: Some(&interrupts),
                instances: None,
            })
            .unwrap_err();

        assert!(
            matches!(error, RuntimeError::Process(ProcessError::TimedOut)),
            "the deadline must win over the pending signal, got {error:?}"
        );
        assert!(
            !trace.borrow().contains(&"sigint"),
            "a run past its deadline must not start forwarding"
        );
    }

    // Catches a run leaving its instance invisible: the state file must exist
    // for the whole run and be gone after cleanup.
    #[test]
    fn a_run_registers_its_instance_and_removes_it_at_cleanup() {
        let root = InstanceTemp::create();
        let dir = InstanceDir::open(&root.0).unwrap();
        let mut sleeper = spawn_instance_sleeper();
        let state_path = root.0.join("run").join(format!("i-{}.state", sleeper.id()));
        let trace = Rc::new(RefCell::new(Vec::new()));
        let payload = Rc::new(RefCell::new(None));
        let runtime = Runtime::new(
            FakeBackend::events(
                trace.clone(),
                payload.clone(),
                vec![
                    ProcessEvent::Uart(guest_stream(7)),
                    ProcessEvent::Exited(successful_process()),
                ],
            )
            .pid(sleeper.id()),
        );
        let sleeper_program = env::current_exe().unwrap();
        let registration = InstanceRegistration {
            dir: &dir,
            image: "hello",
            program: sleeper_program.as_os_str(),
        };
        let mut sink = RecordingSink::new(trace.clone()).probe(state_path);

        let outcome = runtime
            .run_with_sink(
                RunRequest {
                    bundle: &valid_bundle(),
                    kernel: "/kernel".as_ref(),
                    deadline: Duration::from_secs(5),
                    resources: QemuResources::DEFAULT,
                    interrupts: None,
                    instances: Some(registration),
                },
                &mut sink,
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 7);
        assert!(
            !sink.probes.is_empty() && sink.probes.iter().all(|exists| *exists),
            "the state file must exist for the whole run, got {:?}",
            sink.probes
        );
        assert!(
            dir.list().unwrap().is_empty(),
            "the state file must be removed by cleanup"
        );

        sleeper.kill().unwrap();
        sleeper.wait().unwrap();
    }

    // Catches a run that cannot record its instance starting anyway: the
    // child is reaped and the payload removed before the error returns.
    #[test]
    fn a_run_fails_before_the_loop_when_registration_fails() {
        let root = InstanceTemp::create();
        let dir = InstanceDir::open(&root.0).unwrap();
        let trace = Rc::new(RefCell::new(Vec::new()));
        let payload = Rc::new(RefCell::new(None));
        // pid_tの範囲外の値はどのplatformでも生存processを指せない。
        let runtime =
            Runtime::new(FakeBackend::events(trace.clone(), payload.clone(), vec![]).pid(u32::MAX));
        let registration = InstanceRegistration {
            dir: &dir,
            image: "hello",
            program: OsStr::new("sleep"),
        };

        let error = runtime
            .run(RunRequest {
                bundle: &valid_bundle(),
                kernel: "/kernel".as_ref(),
                deadline: Duration::from_secs(5),
                resources: QemuResources::DEFAULT,
                interrupts: None,
                instances: Some(registration),
            })
            .unwrap_err();

        assert!(
            matches!(
                error,
                RuntimeError::Instance(InstanceError::UnverifiableProcess)
            ),
            "registration failure must be a typed instance error, got {error:?}"
        );
        assert_eq!(trace.borrow().as_slice(), ["spawn", "reap"]);
        assert!(!payload.borrow().as_ref().unwrap().exists());
        assert!(dir.list().unwrap().is_empty());
    }

    // Catches losing the state removal error behind a clean run: the
    // unregister failure must compose into cleanup diagnostics.
    #[test]
    fn an_unregister_failure_composes_into_cleanup() {
        let root = InstanceTemp::create();
        let dir = InstanceDir::open(&root.0).unwrap();
        let mut sleeper = spawn_instance_sleeper();
        let run_dir = root.0.join("run");
        let trace = Rc::new(RefCell::new(Vec::new()));
        let payload = Rc::new(RefCell::new(None));
        let runtime = Runtime::new(
            FakeBackend::events(
                trace.clone(),
                payload.clone(),
                vec![
                    ProcessEvent::Uart(guest_stream(7)),
                    ProcessEvent::Exited(successful_process()),
                ],
            )
            .pid(sleeper.id()),
        );
        let sleeper_program = env::current_exe().unwrap();
        let registration = InstanceRegistration {
            dir: &dir,
            image: "hello",
            program: sleeper_program.as_os_str(),
        };
        let mut sink = RecordingSink::new(trace.clone()).on_push(move || {
            make_read_only(&run_dir);
        });

        let error = runtime
            .run_with_sink(
                RunRequest {
                    bundle: &valid_bundle(),
                    kernel: "/kernel".as_ref(),
                    deadline: Duration::from_secs(5),
                    resources: QemuResources::DEFAULT,
                    interrupts: None,
                    instances: Some(registration),
                },
                &mut sink,
            )
            .unwrap_err();

        match error {
            RuntimeError::CleanupOnly(failures) => {
                assert!(
                    matches!(failures.as_slice(), [CleanupFailure::Instance(_)]),
                    "the instance removal failure must be reported, got {failures:?}"
                );
            }
            other => panic!("expected an instance cleanup failure, got {other:?}"),
        }

        sleeper.kill().unwrap();
        sleeper.wait().unwrap();
    }

    #[cfg(unix)]
    fn make_read_only(path: &PathBuf) {
        use std::os::unix::fs::PermissionsExt as _;
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o500);
        fs::set_permissions(path, permissions).unwrap();
    }

    static NEXT_INSTANCE_ROOT: AtomicU64 = AtomicU64::new(0);

    /// store rootのscratch directory。dropで権限を戻してから消す。
    struct InstanceTemp(PathBuf);

    impl InstanceTemp {
        fn create() -> Self {
            let sequence = NEXT_INSTANCE_ROOT.fetch_add(1, Ordering::Relaxed);
            let root = env::temp_dir().join(format!(
                "minicontainer-run-state-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&root).expect("a scratch store root must be creatable");
            Self(root)
        }
    }

    impl Drop for InstanceTemp {
        fn drop(&mut self) {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                for path in [self.0.join("run"), self.0.clone()] {
                    if let Ok(metadata) = fs::metadata(&path) {
                        let mut permissions = metadata.permissions();
                        permissions.set_mode(0o700);
                        let _ = fs::set_permissions(&path, permissions);
                    }
                }
            }
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// instance記録の被写体として使う長命sleeper。`instance::tests`の
    /// helper testを同じtest binaryの別processとして起動する。
    fn spawn_instance_sleeper() -> Child {
        Command::new(env::current_exe().expect("the test binary path must exist"))
            .args([
                "--ignored",
                "--exact",
                "instance::tests::instance_helper",
                "--nocapture",
            ])
            .env("MINICONTAINER_INSTANCE_HELPER", "sleep")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("the sleeper helper must spawn")
    }

    struct RecordingSink {
        trace: Rc<RefCell<Vec<&'static str>>>,
        events: Vec<SessionEvent>,
        reaps_at_push: Vec<usize>,
        fail_after: usize,
        sleep: Duration,
        probe: Option<PathBuf>,
        probes: Vec<bool>,
        on_push: Option<Box<dyn FnMut()>>,
    }

    impl RecordingSink {
        fn new(trace: Rc<RefCell<Vec<&'static str>>>) -> Self {
            Self {
                trace,
                events: Vec::new(),
                reaps_at_push: Vec::new(),
                fail_after: usize::MAX,
                sleep: Duration::ZERO,
                probe: None,
                probes: Vec::new(),
                on_push: None,
            }
        }

        fn fail_after(mut self, pushes: usize) -> Self {
            self.fail_after = pushes;
            self
        }

        fn sleep(mut self, duration: Duration) -> Self {
            self.sleep = duration;
            self
        }

        /// 各pushで`path`の存在を記録する。run中にfileが見えるかの検査用。
        fn probe(mut self, path: PathBuf) -> Self {
            self.probe = Some(path);
            self
        }

        /// 各pushの直前に呼ぶhook。runの途中で副作用を起こす検査用。
        fn on_push(mut self, action: impl FnMut() + 'static) -> Self {
            self.on_push = Some(Box::new(action));
            self
        }
    }

    /// 決められた順序でsignalを報告する`InterruptSource`。scriptを使い
    /// 切った後は最後の観測を返し続ける (実sourceの累計countと同じ粘着性)。
    struct ScriptedInterrupts {
        script: RefCell<VecDeque<Option<SignalObservation>>>,
        last: Cell<Option<SignalObservation>>,
    }

    impl ScriptedInterrupts {
        fn observing(script: impl IntoIterator<Item = Option<SignalObservation>>) -> Self {
            Self {
                script: RefCell::new(script.into_iter().collect()),
                last: Cell::new(None),
            }
        }
    }

    impl InterruptSource for ScriptedInterrupts {
        fn poll(&self) -> Option<SignalObservation> {
            if let Some(next) = self.script.borrow_mut().pop_front() {
                self.last.set(next);
            }
            self.last.get()
        }
    }

    impl OutputSink for RecordingSink {
        fn push(&mut self, event: &SessionEvent) -> io::Result<()> {
            if let Some(action) = self.on_push.as_mut() {
                action();
            }
            if !self.sleep.is_zero() {
                std::thread::sleep(self.sleep);
            }
            if self.events.len() >= self.fail_after {
                return Err(io::Error::other("injected consumer failure"));
            }
            if let Some(probe) = &self.probe {
                self.probes.push(probe.exists());
            }
            self.reaps_at_push.push(
                self.trace
                    .borrow()
                    .iter()
                    .filter(|event| **event == "reap")
                    .count(),
            );
            self.events.push(event.clone());
            Ok(())
        }
    }

    struct FakeBackend {
        trace: Rc<RefCell<Vec<&'static str>>>,
        payload: Rc<RefCell<Option<PathBuf>>>,
        result: RefCell<FakeSpawnResult>,
        reap_fails: bool,
        signal_fails: bool,
        pid: u32,
    }

    impl FakeBackend {
        fn events(
            trace: Rc<RefCell<Vec<&'static str>>>,
            payload: Rc<RefCell<Option<PathBuf>>>,
            events: Vec<ProcessEvent>,
        ) -> Self {
            Self {
                trace,
                payload,
                result: RefCell::new(FakeSpawnResult::Events(events)),
                reap_fails: false,
                signal_fails: false,
                pid: 42,
            }
        }

        fn spawn_fails(
            trace: Rc<RefCell<Vec<&'static str>>>,
            payload: Rc<RefCell<Option<PathBuf>>>,
        ) -> Self {
            Self {
                trace,
                payload,
                result: RefCell::new(FakeSpawnResult::Fails),
                reap_fails: false,
                signal_fails: false,
                pid: 42,
            }
        }

        fn reap_fails(mut self) -> Self {
            self.reap_fails = true;
            self
        }

        fn signal_fails(mut self) -> Self {
            self.signal_fails = true;
            self
        }

        fn pid(mut self, pid: u32) -> Self {
            self.pid = pid;
            self
        }
    }

    impl ProcessBackend for FakeBackend {
        type Child = FakeChild;

        fn spawn(&self, command: &QemuCommand) -> Result<Self::Child, ProcessError> {
            let loader = command
                .args()
                .windows(2)
                .find(|pair| pair[0] == "-device")
                .map(|pair| pair[1].to_string_lossy())
                .unwrap();
            let payload = loader
                .strip_prefix("loader,file=")
                .unwrap()
                .strip_suffix(",addr=0x87800000,force-raw=on")
                .unwrap();
            let path = PathBuf::from(payload);
            assert!(path.exists(), "payload must exist before spawn");
            *self.payload.borrow_mut() = Some(path);
            self.trace.borrow_mut().push("spawn");

            match std::mem::replace(&mut *self.result.borrow_mut(), FakeSpawnResult::Used) {
                FakeSpawnResult::Events(events) => Ok(FakeChild {
                    trace: self.trace.clone(),
                    events: events.into(),
                    reap_fails: self.reap_fails,
                    signal_fails: self.signal_fails,
                    pid: self.pid,
                }),
                FakeSpawnResult::Fails => Err(ProcessError::Spawn(io::Error::other(
                    "injected spawn failure",
                ))),
                FakeSpawnResult::Used => panic!("fake backend must spawn at most once"),
            }
        }
    }

    enum FakeSpawnResult {
        Events(Vec<ProcessEvent>),
        Fails,
        Used,
    }

    struct FakeChild {
        trace: Rc<RefCell<Vec<&'static str>>>,
        events: VecDeque<ProcessEvent>,
        reap_fails: bool,
        signal_fails: bool,
        pid: u32,
    }

    impl ProcessControl for FakeChild {
        fn id(&self) -> u32 {
            self.pid
        }

        fn next_event(&mut self, _deadline: Instant) -> Result<ProcessEvent, ProcessError> {
            let event = self.events.pop_front().unwrap_or(ProcessEvent::TimedOut);
            self.trace.borrow_mut().push(match event {
                ProcessEvent::Uart(_) => "decode",
                ProcessEvent::Diagnostic(_) => "diagnostic",
                ProcessEvent::Exited(_) => "exit",
                ProcessEvent::TimedOut => "timeout",
            });
            Ok(event)
        }

        fn send_signal(&mut self, signal: HostSignal) -> Result<(), ProcessError> {
            self.trace.borrow_mut().push(match signal {
                HostSignal::Interrupt => "sigint",
                HostSignal::Terminate => "sigterm",
            });
            if self.signal_fails {
                return Err(ProcessError::Signal(io::Error::other(
                    "injected signal failure",
                )));
            }
            Ok(())
        }

        fn terminate_and_reap(&mut self) -> Result<ProcessStatus, ProcessError> {
            self.trace.borrow_mut().push("reap");
            if self.reap_fails {
                return Err(ProcessError::Wait(io::Error::other(
                    "injected reap failure",
                )));
            }
            Ok(successful_process())
        }
    }

    fn valid_bundle() -> Vec<u8> {
        build(ImageSpec {
            name: "test",
            args: &[],
            elf: b"ELF",
        })
        .unwrap()
    }

    fn successful_process() -> ProcessStatus {
        ProcessStatus {
            code: Some(0),
            success: true,
        }
    }

    fn guest_stream(exit_code: u32) -> Vec<u8> {
        let mut stream = ready_frame();
        stream.extend_from_slice(&frame(FrameKind::Exit, &exit_code.to_le_bytes()));
        stream
    }

    fn guest_error_stream() -> Vec<u8> {
        let mut stream = ready_frame();
        stream.extend_from_slice(&frame(FrameKind::GuestError, b"guest failed"));
        stream
    }

    fn malformed_stream() -> Vec<u8> {
        let mut stream = ready_frame();
        let mut malformed = frame(FrameKind::Stdout, b"ignored");
        malformed[0] = b'X';
        stream.extend_from_slice(&malformed);
        stream
    }

    fn output_flood_stream() -> Vec<u8> {
        let mut stream = ready_frame();
        let chunk = vec![b'o'; 32 * 1024];
        for _ in 0..33 {
            stream.extend_from_slice(&frame(FrameKind::Stdout, &chunk));
        }
        stream
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
}
