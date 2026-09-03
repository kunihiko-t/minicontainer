//! MiniContainer host runtime。

mod command;
mod error;
mod process;
mod session;
mod temp;

pub use command::QemuCommand;
pub use error::{CleanupFailure, RuntimeError};
pub use process::{
    ProcessBackend, ProcessControl, ProcessError, ProcessEvent, ProcessStatus, SystemProcessBackend,
};
pub use session::{RunOutcome, Session, SessionError, SessionEvent};
pub use temp::PayloadTemp;

use std::{
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
    pub fn run(&self, request: RunRequest<'_>) -> Result<RunOutcome, RuntimeError> {
        minicontainer_bundle::parse(request.bundle).map_err(RuntimeError::Bundle)?;
        let payload = PayloadTemp::create(request.bundle)?;
        let command = match QemuCommand::new(request.kernel, payload.path()) {
            Ok(command) => command,
            Err(error) => return finish_without_child(Err(error), &payload),
        };
        let mut child = match self.backend.spawn(&command) {
            Ok(child) => child,
            Err(error) => return finish_without_child(Err(RuntimeError::Process(error)), &payload),
        };

        let deadline = Instant::now() + request.deadline;
        let mut session = Session::new();
        let primary = loop {
            match child.next_event(deadline) {
                Ok(ProcessEvent::Uart(bytes)) => {
                    if let Err(error) = session.push_uart(&bytes) {
                        break Err(RuntimeError::Session(error));
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
        CleanupFailure, ProcessBackend, ProcessControl, ProcessError, ProcessEvent, ProcessStatus,
        QemuCommand, RunRequest, Runtime, RuntimeError, SessionError,
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
                })
                .is_err()
        );
        assert_eq!(trace.borrow().as_slice(), ["spawn"]);
        assert!(!payload.borrow().as_ref().unwrap().exists());
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
