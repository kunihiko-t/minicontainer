//! MiniContainer host runtime。

mod command;
mod error;
mod process;
mod session;
mod temp;

pub use command::{QemuCommand, QemuResources};
pub use error::{CleanupFailure, RuntimeError};
pub use process::{
    ProcessBackend, ProcessControl, ProcessError, ProcessEvent, ProcessStatus, SystemProcessBackend,
};
pub use session::{RunOutcome, Session, SessionError, SessionEvent};
pub use temp::PayloadTemp;

use std::{
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

/// MiniBundleを一時payloadとしてQEMU上で実行するruntime。
pub struct Runtime<B: ProcessBackend = SystemProcessBackend> {
    backend: B,
}

impl<B: ProcessBackend> Runtime<B> {
    /// 指定backendを使うruntimeを作る。
    pub const fn new(backend: B) -> Self {
        Self { backend }
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

        let mut session = Session::new();
        let primary = 'events: loop {
            // queueにeventが残っていても全体の期限を強制する。backendが期限
            // 過ぎの出力を返し続けても、この検査がrunをtimeoutで終わらせる。
            // sinkの停滞でloopが止まっても、復帰後の先頭検査が期限を効かせる。
            if Instant::now() >= deadline {
                break Err(RuntimeError::Process(ProcessError::TimedOut));
            }
            match child.next_event(deadline) {
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
                    break session.finish(status).map_err(RuntimeError::Session);
                }
                Ok(ProcessEvent::TimedOut) => {
                    break Err(RuntimeError::Process(ProcessError::TimedOut));
                }
                Err(error) => break Err(RuntimeError::Process(error)),
            }
        };

        let mut cleanup_failures = Vec::new();
        if let Err(error) = child.terminate_and_reap() {
            cleanup_failures.push(CleanupFailure::Process(error));
        }
        cleanup_failures.extend(remove_payload(&payload));
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
        cell::RefCell,
        collections::VecDeque,
        io,
        path::PathBuf,
        rc::Rc,
        time::{Duration, Instant},
    };

    use minicontainer_bundle::{ImageSpec, build};
    use minios_abi::control::{FrameHeader, FrameKind, ReadyPayload};

    use super::{
        CleanupFailure, OutputSink, ProcessBackend, ProcessControl, ProcessError, ProcessEvent,
        ProcessStatus, QemuCommand, QemuResources, RunRequest, Runtime, RuntimeError, SessionError,
        SessionEvent,
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

    struct RecordingSink {
        trace: Rc<RefCell<Vec<&'static str>>>,
        events: Vec<SessionEvent>,
        reaps_at_push: Vec<usize>,
        fail_after: usize,
        sleep: Duration,
    }

    impl RecordingSink {
        fn new(trace: Rc<RefCell<Vec<&'static str>>>) -> Self {
            Self {
                trace,
                events: Vec::new(),
                reaps_at_push: Vec::new(),
                fail_after: usize::MAX,
                sleep: Duration::ZERO,
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
    }

    impl OutputSink for RecordingSink {
        fn push(&mut self, event: &SessionEvent) -> io::Result<()> {
            if !self.sleep.is_zero() {
                std::thread::sleep(self.sleep);
            }
            if self.events.len() >= self.fail_after {
                return Err(io::Error::other("injected consumer failure"));
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
            }
        }

        fn reap_fails(mut self) -> Self {
            self.reap_fails = true;
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
    }

    impl ProcessControl for FakeChild {
        fn id(&self) -> u32 {
            42
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
