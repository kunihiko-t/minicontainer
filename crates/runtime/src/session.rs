//! QEMU UART control frameをrun結果へ復元するstate machine。

use std::{error::Error, fmt};

use minicontainer_protocol::{Decoder, Frame, ProtocolError};
use minios_abi::{
    boot::{BOOT_ABI_MAJOR, BOOT_ABI_MINOR},
    control::{FRAME_MAGIC, FrameKind, ReadyPayload},
};

use crate::ProcessStatus;

const BOOT_PREAMBLE_LIMIT: usize = 64 * 1024;

/// stdout、stderr、diagnosticsの合計蓄積量の上限。
///
/// 三つのbufferはguest frameから伸びるため、合計を抑えないと多弁なguestが
/// timeoutまでの間にhost memoryを食い尽くす。hello guestの出力は数十byte
/// であり、1 MiBは学習用途に十分な余裕と上限の両立になる。firmware由来の
/// boot textは別上限 (`BOOT_PREAMBLE_LIMIT`) で抑える。
const GUEST_OUTPUT_MAX_LEN: usize = 1024 * 1024;

/// UART sessionで観測したguest control event。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    /// guestが期待するABIで起動を完了した。
    Ready,
    /// guest stdout。
    Stdout(Vec<u8>),
    /// guest stderr。
    Stderr(Vec<u8>),
    /// guestが送った診断bytes。
    Diagnostic(Vec<u8>),
    /// guestが終了codeを報告した。
    Exit(u32),
}

/// QEMU UART sessionの復元に失敗した理由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    /// Ready前のfirmware textが上限を超えた。
    BootPreambleTooLarge,
    /// UART control frameの形式が不正だった。
    Protocol(ProtocolError),
    /// Readyより前にcontrol frameを受信した。
    FrameBeforeReady(FrameKind),
    /// Running中にReadyを再度受信した。
    DuplicateReady,
    /// Exit後に出力・再handshake・再終了のcontrol frameを受信した。
    /// Diagnosticの成功markerとGuestErrorは別variantで扱う。
    FrameAfterExit(FrameKind),
    /// QEMU終了時までにguest Exitを受信しなかった。
    MissingExit,
    /// ReadyがMiniContainerと異なるABI versionを示した。
    UnsupportedReadyAbi(ReadyPayload),
    /// ABI固定長と異なるExit payloadを受信した。
    InvalidExitPayload,
    /// guest自身が実行失敗を報告した。
    GuestError(Vec<u8>),
    /// stdout、stderr、diagnosticsの合計蓄積量が上限を超えた。
    GuestOutputTooLarge,
    /// guest Exit後にQEMUが失敗して終了した。
    ProcessFailed(ProcessStatus),
}

impl fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BootPreambleTooLarge => write!(formatter, "boot preamble exceeds 64 KiB"),
            Self::Protocol(error) => write!(formatter, "invalid UART control frame: {error}"),
            Self::FrameBeforeReady(kind) => {
                write!(formatter, "{kind:?} frame arrived before Ready")
            }
            Self::DuplicateReady => write!(formatter, "received a second Ready frame"),
            Self::FrameAfterExit(kind) => write!(formatter, "{kind:?} frame arrived after Exit"),
            Self::MissingExit => write!(formatter, "QEMU exited without a guest Exit frame"),
            Self::UnsupportedReadyAbi(ready) => write!(
                formatter,
                "unsupported guest ABI {}.{}",
                ready.abi_major, ready.abi_minor
            ),
            Self::InvalidExitPayload => write!(formatter, "Exit payload must contain a u32"),
            Self::GuestError(bytes) => write!(
                formatter,
                "guest reported an error: {}",
                String::from_utf8_lossy(bytes)
            ),
            Self::GuestOutputTooLarge => {
                write!(formatter, "guest output exceeds 1 MiB")
            }
            Self::ProcessFailed(status) => write!(
                formatter,
                "QEMU exited unsuccessfully (code: {:?})",
                status.code
            ),
        }
    }
}

impl Error for SessionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Protocol(error) => Some(error),
            Self::BootPreambleTooLarge
            | Self::FrameBeforeReady(_)
            | Self::DuplicateReady
            | Self::FrameAfterExit(_)
            | Self::MissingExit
            | Self::UnsupportedReadyAbi(_)
            | Self::InvalidExitPayload
            | Self::GuestError(_)
            | Self::GuestOutputTooLarge
            | Self::ProcessFailed(_) => None,
        }
    }
}

/// 正常終了したguest runの出力。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutcome {
    /// guest stdoutを連結したbytes。
    pub stdout: Vec<u8>,
    /// guest stderrを連結したbytes。
    pub stderr: Vec<u8>,
    /// guestがExit frameで報告した終了code。
    pub exit_code: u32,
    /// firmware preambleとguest Diagnostic frameを連結したbytes。
    pub diagnostics: Vec<u8>,
}

/// QEMU UARTをguest runへ復元するstate machine。
pub struct Session {
    state: State,
    decoder: Decoder,
    boot_diagnostic: Vec<u8>,
    magic_prefix: Vec<u8>,
    control_started: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    diagnostics: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    AwaitReady,
    Running,
    Exited(u32),
}

impl Session {
    /// 新しいUART sessionを作る。
    pub fn new() -> Self {
        Self {
            state: State::AwaitReady,
            decoder: Decoder::new(),
            boot_diagnostic: Vec::new(),
            magic_prefix: Vec::new(),
            control_started: false,
            stdout: Vec::new(),
            stderr: Vec::new(),
            diagnostics: Vec::new(),
        }
    }

    /// QEMU stdoutから届いたUART bytesを処理する。
    pub fn push_uart(&mut self, bytes: &[u8]) -> Result<Vec<SessionEvent>, SessionError> {
        let frames = if self.control_started {
            self.decoder.push(bytes).map_err(SessionError::Protocol)?
        } else {
            match self.first_control_bytes(bytes)? {
                Some(control_bytes) => self
                    .start_control_stream(control_bytes)
                    .map_err(SessionError::Protocol)?,
                None => return Ok(Vec::new()),
            }
        };
        self.handle_frames(frames)
    }

    /// Ready前に出力されたfirmwareの診断bytes。
    pub fn boot_diagnostic(&self) -> &[u8] {
        &self.boot_diagnostic
    }

    /// QEMU終了時にsessionを確定する。
    pub fn finish(&mut self, status: ProcessStatus) -> Result<RunOutcome, SessionError> {
        self.finish_boot_preamble()?;
        self.decoder.finish().map_err(SessionError::Protocol)?;

        let State::Exited(exit_code) = self.state else {
            return Err(SessionError::MissingExit);
        };
        if !status.success {
            return Err(SessionError::ProcessFailed(status));
        }

        let mut diagnostics = self.boot_diagnostic.clone();
        diagnostics.extend_from_slice(&self.diagnostics);
        Ok(RunOutcome {
            stdout: self.stdout.clone(),
            stderr: self.stderr.clone(),
            exit_code,
            diagnostics,
        })
    }

    fn finish_boot_preamble(&mut self) -> Result<(), SessionError> {
        if !self.control_started {
            let prefix = std::mem::take(&mut self.magic_prefix);
            self.append_boot_diagnostic(&prefix)?;
        }
        Ok(())
    }

    fn first_control_bytes(&mut self, bytes: &[u8]) -> Result<Option<Vec<u8>>, SessionError> {
        let mut candidate = std::mem::take(&mut self.magic_prefix);
        candidate.extend_from_slice(bytes);

        if let Some(offset) = candidate
            .windows(FRAME_MAGIC.len())
            .position(|window| window == FRAME_MAGIC)
        {
            self.append_boot_diagnostic(&candidate[..offset])?;
            return Ok(Some(candidate[offset..].to_vec()));
        }

        let prefix_len = trailing_magic_prefix_len(&candidate);
        let preamble_len = candidate.len() - prefix_len;
        self.append_boot_diagnostic(&candidate[..preamble_len])?;
        self.magic_prefix
            .extend_from_slice(&candidate[preamble_len..]);
        Ok(None)
    }

    fn start_control_stream(&mut self, bytes: Vec<u8>) -> Result<Vec<Frame>, ProtocolError> {
        self.control_started = true;
        self.decoder.push(&bytes)
    }

    /// 三つのguest出力bufferへ`extra` byteを足しても上限内に収まるか検査する。
    ///
    /// 飽和加算で合計するため、巨大なpayload長でもoverflow panicにならない。
    fn check_output_room(&self, extra: usize) -> Result<(), SessionError> {
        let total = self
            .stdout
            .len()
            .saturating_add(self.stderr.len())
            .saturating_add(self.diagnostics.len())
            .saturating_add(extra);
        if total > GUEST_OUTPUT_MAX_LEN {
            return Err(SessionError::GuestOutputTooLarge);
        }
        Ok(())
    }

    fn append_boot_diagnostic(&mut self, bytes: &[u8]) -> Result<(), SessionError> {
        if self
            .boot_diagnostic
            .len()
            .checked_add(bytes.len())
            .is_none_or(|length| length > BOOT_PREAMBLE_LIMIT)
        {
            return Err(SessionError::BootPreambleTooLarge);
        }
        self.boot_diagnostic.extend_from_slice(bytes);
        Ok(())
    }

    fn handle_frames(&mut self, frames: Vec<Frame>) -> Result<Vec<SessionEvent>, SessionError> {
        let mut events = Vec::new();
        for frame in frames {
            match self.state {
                State::AwaitReady => self.handle_initial_frame(frame, &mut events)?,
                State::Running => self.handle_running_frame(frame, &mut events)?,
                State::Exited(_) => self.handle_exited_frame(frame, &mut events)?,
            }
        }
        Ok(events)
    }

    /// Exit受信後のframeを処理する。
    ///
    /// production kernelはExitの後にresource回収の成功markerをDiagnostic
    /// frameで送り、回収失敗時はGuestError frameで異常shutdownする。どちらも
    /// hostの診断情報であり、確定した終了codeは変えない。出力と再handshakeは
    /// 終了後に意味を持たないため引き続き拒否する。
    fn handle_exited_frame(
        &mut self,
        frame: Frame,
        events: &mut Vec<SessionEvent>,
    ) -> Result<(), SessionError> {
        match frame.kind {
            FrameKind::Diagnostic => {
                self.check_output_room(frame.payload.len())?;
                self.diagnostics.extend_from_slice(&frame.payload);
                events.push(SessionEvent::Diagnostic(frame.payload));
                Ok(())
            }
            FrameKind::GuestError => Err(SessionError::GuestError(frame.payload)),
            _ => Err(SessionError::FrameAfterExit(frame.kind)),
        }
    }

    fn handle_initial_frame(
        &mut self,
        frame: Frame,
        events: &mut Vec<SessionEvent>,
    ) -> Result<(), SessionError> {
        if frame.kind != FrameKind::Ready {
            return Err(SessionError::FrameBeforeReady(frame.kind));
        }
        let ready = ReadyPayload::decode(&frame.payload).map_err(|_| {
            SessionError::UnsupportedReadyAbi(ReadyPayload {
                abi_major: 0,
                abi_minor: 0,
            })
        })?;
        if ready.abi_major != BOOT_ABI_MAJOR || ready.abi_minor != BOOT_ABI_MINOR {
            return Err(SessionError::UnsupportedReadyAbi(ready));
        }
        self.state = State::Running;
        events.push(SessionEvent::Ready);
        Ok(())
    }

    fn handle_running_frame(
        &mut self,
        frame: Frame,
        events: &mut Vec<SessionEvent>,
    ) -> Result<(), SessionError> {
        match frame.kind {
            FrameKind::Ready => Err(SessionError::DuplicateReady),
            FrameKind::Stdout => {
                self.check_output_room(frame.payload.len())?;
                self.stdout.extend_from_slice(&frame.payload);
                events.push(SessionEvent::Stdout(frame.payload));
                Ok(())
            }
            FrameKind::Stderr => {
                self.check_output_room(frame.payload.len())?;
                self.stderr.extend_from_slice(&frame.payload);
                events.push(SessionEvent::Stderr(frame.payload));
                Ok(())
            }
            FrameKind::Diagnostic => {
                self.check_output_room(frame.payload.len())?;
                self.diagnostics.extend_from_slice(&frame.payload);
                events.push(SessionEvent::Diagnostic(frame.payload));
                Ok(())
            }
            FrameKind::Exit => {
                let exit_code = u32::from_le_bytes(
                    frame
                        .payload
                        .as_slice()
                        .try_into()
                        .map_err(|_| SessionError::InvalidExitPayload)?,
                );
                self.state = State::Exited(exit_code);
                events.push(SessionEvent::Exit(exit_code));
                Ok(())
            }
            FrameKind::GuestError => Err(SessionError::GuestError(frame.payload)),
        }
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

fn trailing_magic_prefix_len(bytes: &[u8]) -> usize {
    (1..FRAME_MAGIC.len())
        .rev()
        .find(|length| bytes.ends_with(&FRAME_MAGIC[..*length]))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::{RunOutcome, Session, SessionError, SessionEvent};
    use crate::ProcessStatus;
    use minicontainer_protocol::ProtocolError;
    use minios_abi::control::ControlError;
    use minios_abi::control::{FrameHeader, FrameKind, ReadyPayload};

    // Catches treating a partial MCF1 magic prefix as boot text, which would
    // make a normal transport chunk split lose the initial Ready frame.
    #[test]
    fn bounded_boot_text_precedes_a_split_ready_frame() {
        let mut session = Session::new();

        assert_eq!(session.push_uart(b"OpenSBI\r\nMC").unwrap(), Vec::new());
        assert_eq!(session.boot_diagnostic(), b"OpenSBI\r\n");
        assert_eq!(
            session.push_uart(&ready_frame()[2..]).unwrap(),
            vec![SessionEvent::Ready]
        );
    }

    // Catches accepting an unlimited firmware preamble, which lets a guest
    // without a control stream grow host memory until the process timeout.
    #[test]
    fn boot_preamble_over_64_kib_is_rejected() {
        let mut session = Session::new();
        let boot_text = vec![b'B'; 64 * 1024 + 1];

        assert_eq!(
            session.push_uart(&boot_text),
            Err(SessionError::BootPreambleTooLarge)
        );
    }

    // Catches growing host memory without a bound: exactly 1 MiB of
    // accumulated guest output is kept, one byte more is refused.
    #[test]
    fn guest_output_over_1_mib_is_rejected_at_the_total_boundary() {
        let mut session = ready_session();
        let chunk = vec![b'o'; 32 * 1024];
        let mut flood = Vec::new();
        for _ in 0..32 {
            flood.extend_from_slice(&frame(FrameKind::Stdout, &chunk));
        }

        assert!(session.push_uart(&flood).is_ok());
        assert_eq!(
            session.push_uart(&frame(FrameKind::Stdout, b"o")),
            Err(SessionError::GuestOutputTooLarge)
        );
    }

    // Catches capping each stream separately, which lets a guest triple
    // host memory by spreading output across stdout, stderr, and
    // diagnostics.
    #[test]
    fn output_cap_counts_all_three_buffers_together() {
        let mut session = ready_session();
        let chunk = vec![b'o'; 32 * 1024];
        for kind in [FrameKind::Stdout, FrameKind::Stderr] {
            let mut half = Vec::new();
            for _ in 0..16 {
                half.extend_from_slice(&frame(kind, &chunk));
            }
            assert!(session.push_uart(&half).is_ok());
        }

        assert_eq!(
            session.push_uart(&frame(FrameKind::Diagnostic, b"o")),
            Err(SessionError::GuestOutputTooLarge)
        );
    }

    // Catches flooding diagnostics after Exit, which arrives after the
    // guest result is final but still grows host memory until QEMU exits.
    #[test]
    fn diagnostic_flood_after_exit_is_rejected() {
        let mut session = ready_session();
        let chunk = vec![b'o'; 32 * 1024];
        let mut flood = Vec::new();
        for _ in 0..32 {
            flood.extend_from_slice(&frame(FrameKind::Stdout, &chunk));
        }
        assert!(session.push_uart(&flood).is_ok());
        session
            .push_uart(&frame(FrameKind::Exit, &42_u32.to_le_bytes()))
            .unwrap();

        assert_eq!(
            session.push_uart(&frame(FrameKind::Diagnostic, b"o")),
            Err(SessionError::GuestOutputTooLarge)
        );
    }

    // Catches drifting the triage diagnostic that operators and the
    // end-to-end gate match for the output-cap failure.
    #[test]
    fn output_cap_failure_reports_a_stable_diagnostic() {
        assert_eq!(
            SessionError::GuestOutputTooLarge.to_string(),
            "guest output exceeds 1 MiB"
        );
    }

    // Catches treating output as trusted before the guest has proved the
    // protocol version through its Ready control frame.
    #[test]
    fn output_before_ready_is_rejected() {
        let mut session = Session::new();

        assert_eq!(
            session.push_uart(&frame(FrameKind::Stdout, b"unframed order")),
            Err(SessionError::FrameBeforeReady(FrameKind::Stdout))
        );
    }

    // Catches accepting a guest that changes or repeats its handshake after
    // output has begun.
    #[test]
    fn a_second_ready_is_rejected() {
        let mut session = ready_session();

        assert_eq!(
            session.push_uart(&ready_frame()),
            Err(SessionError::DuplicateReady)
        );
    }

    // Catches allowing frames to alter an already final guest result.
    #[test]
    fn a_frame_after_exit_is_rejected() {
        let mut session = ready_session();
        session
            .push_uart(&frame(FrameKind::Exit, &7_u32.to_le_bytes()))
            .unwrap();

        assert_eq!(
            session.push_uart(&frame(FrameKind::Stdout, b"late")),
            Err(SessionError::FrameAfterExit(FrameKind::Stdout))
        );
    }

    // Catches rejecting the production kernel's post-Exit resource-cleanup
    // marker, which arrives as a Diagnostic frame after the Exit frame.
    #[test]
    fn diagnostic_after_exit_is_kept_as_host_diagnostics() {
        let mut session = ready_session();
        session
            .push_uart(&frame(FrameKind::Exit, &42_u32.to_le_bytes()))
            .unwrap();

        assert_eq!(
            session.push_uart(&frame(FrameKind::Diagnostic, b"MiniOS payload: ok code=42")),
            Ok(vec![SessionEvent::Diagnostic(
                b"MiniOS payload: ok code=42".to_vec()
            )])
        );
        assert_eq!(
            session.finish(successful_process()),
            Ok(RunOutcome {
                stdout: Vec::new(),
                stderr: Vec::new(),
                exit_code: 42,
                diagnostics: b"MiniOS payload: ok code=42".to_vec(),
            })
        );
    }

    // Catches masking a post-Exit guest failure (for example a failed
    // resource-recovery check) as a successful run.
    #[test]
    fn guest_error_after_exit_is_a_typed_failure() {
        let mut session = ready_session();
        session
            .push_uart(&frame(FrameKind::Exit, &42_u32.to_le_bytes()))
            .unwrap();

        assert_eq!(
            session.push_uart(&frame(FrameKind::GuestError, b"MiniOS payload: recovery")),
            Err(SessionError::GuestError(
                b"MiniOS payload: recovery".to_vec()
            ))
        );
    }

    // Catches allowing output, a second handshake, or a second exit to revise
    // an already final guest result.
    #[test]
    fn output_handshake_and_exit_after_exit_are_rejected() {
        let cases = [
            (
                FrameKind::Stderr,
                frame(FrameKind::Stderr, b"late"),
                SessionError::FrameAfterExit(FrameKind::Stderr),
            ),
            (
                FrameKind::Ready,
                ready_frame(),
                SessionError::FrameAfterExit(FrameKind::Ready),
            ),
            (
                FrameKind::Exit,
                frame(FrameKind::Exit, &7_u32.to_le_bytes()),
                SessionError::FrameAfterExit(FrameKind::Exit),
            ),
        ];

        for (kind, bytes, expected) in cases {
            let mut session = ready_session();
            session
                .push_uart(&frame(FrameKind::Exit, &7_u32.to_le_bytes()))
                .unwrap();

            assert_eq!(session.push_uart(&bytes), Err(expected), "{kind:?}");
        }
    }

    // Catches reporting a host-successful QEMU exit as a successful guest run
    // when no Exit control frame was received.
    #[test]
    fn finish_rejects_eof_without_an_exit_frame() {
        let mut session = ready_session();

        assert_eq!(
            session.finish(successful_process()),
            Err(SessionError::MissingExit)
        );
    }

    // Catches accepting a control protocol generated for a different pinned
    // miniOS ABI.
    #[test]
    fn incompatible_ready_abi_is_rejected() {
        let mut session = Session::new();
        let payload = ReadyPayload {
            abi_major: 9,
            abi_minor: 4,
        }
        .encode();

        assert_eq!(
            session.push_uart(&frame(FrameKind::Ready, &payload)),
            Err(SessionError::UnsupportedReadyAbi(ReadyPayload {
                abi_major: 9,
                abi_minor: 4,
            }))
        );
    }

    // Catches treating a guest-declared execution failure as ordinary stderr
    // output, which would let Runtime::run report a false success.
    #[test]
    fn guest_error_is_reported_as_a_typed_failure() {
        let mut session = ready_session();

        assert_eq!(
            session.push_uart(&frame(FrameKind::GuestError, b"bad syscall")),
            Err(SessionError::GuestError(b"bad syscall".to_vec()))
        );
    }

    // Catches discarding the boot diagnostic or mixing stdout and stderr when
    // several complete frames arrive in one UART delivery.
    #[test]
    fn finish_returns_separate_streams_and_all_diagnostics() {
        let mut session = Session::new();
        let mut uart = b"OpenSBI\n".to_vec();
        uart.extend_from_slice(&ready_frame());
        uart.extend_from_slice(&frame(FrameKind::Stdout, b"hello"));
        uart.extend_from_slice(&frame(FrameKind::Stderr, b"warning"));
        uart.extend_from_slice(&frame(FrameKind::Diagnostic, b"guest note"));
        uart.extend_from_slice(&frame(FrameKind::Exit, &7_u32.to_le_bytes()));

        assert_eq!(
            session.push_uart(&uart).unwrap(),
            vec![
                SessionEvent::Ready,
                SessionEvent::Stdout(b"hello".to_vec()),
                SessionEvent::Stderr(b"warning".to_vec()),
                SessionEvent::Diagnostic(b"guest note".to_vec()),
                SessionEvent::Exit(7),
            ]
        );
        assert_eq!(
            session.finish(successful_process()),
            Ok(RunOutcome {
                stdout: b"hello".to_vec(),
                stderr: b"warning".to_vec(),
                exit_code: 7,
                diagnostics: b"OpenSBI\nguest note".to_vec(),
            })
        );
    }

    // Catches ignoring the QEMU process status after receiving an otherwise
    // valid guest Exit frame.
    #[test]
    fn finish_rejects_an_unsuccessful_qemu_status() {
        let mut session = ready_session();
        session
            .push_uart(&frame(FrameKind::Exit, &0_u32.to_le_bytes()))
            .unwrap();
        let status = ProcessStatus {
            code: Some(1),
            success: false,
        };

        assert_eq!(
            session.finish(status),
            Err(SessionError::ProcessFailed(status))
        );
    }

    // Catches returning a successful guest outcome when UART ends after Exit
    // but the decoder still owns an incomplete subsequent control frame.
    #[test]
    fn finish_rejects_a_truncated_frame_after_exit() {
        let mut session = ready_session();
        session
            .push_uart(&frame(FrameKind::Exit, &0_u32.to_le_bytes()))
            .unwrap();
        assert_eq!(session.push_uart(b"MCF1").unwrap(), Vec::new());

        assert_eq!(
            session.finish(successful_process()),
            Err(SessionError::Protocol(ProtocolError::TruncatedFrame))
        );
    }

    // Catches delaying the final 1–3 boot bytes that happened to match the
    // start of MCF1, thereby bypassing the boot preamble size limit at EOF.
    #[test]
    fn finish_counts_a_partial_magic_prefix_against_the_boot_limit() {
        let mut session = Session::new();
        session.push_uart(&vec![b'B'; 64 * 1024]).unwrap();
        assert_eq!(session.push_uart(b"MCF").unwrap(), Vec::new());

        assert_eq!(
            session.finish(successful_process()),
            Err(SessionError::BootPreambleTooLarge)
        );
    }

    // Catches a Session layer that accepts a malformed frame after Ready or
    // tries to interpret the decoder's protocol error as guest output.
    #[test]
    fn malformed_frame_after_ready_is_a_protocol_error() {
        let mut session = ready_session();
        let mut malformed = frame(FrameKind::Stdout, b"ignored");
        malformed[0] = b'X';

        assert_eq!(
            session.push_uart(&malformed),
            Err(SessionError::Protocol(ProtocolError::Header(
                ControlError::WrongMagic
            )))
        );
    }

    // Catches coupling the session state machine to one preferred UART chunk
    // boundary rather than the arbitrary fragmentation of a real pipe.
    #[test]
    fn ready_output_and_exit_survive_every_two_chunk_split() {
        let stream = complete_stream();
        let expected_events = complete_events();
        let expected_outcome = complete_outcome();

        for split in 0..=stream.len() {
            let mut session = Session::new();
            let mut events = session.push_uart(&stream[..split]).unwrap();
            events.extend(session.push_uart(&stream[split..]).unwrap());

            assert_eq!(events, expected_events, "split at byte {split}");
            assert_eq!(
                session.finish(successful_process()),
                Ok(expected_outcome.clone()),
                "split at byte {split}"
            );
        }
    }

    // Catches losing a partial magic, header, or payload when every UART byte
    // is delivered separately.
    #[test]
    fn ready_output_and_exit_survive_one_byte_uart_input() {
        let mut session = Session::new();
        let mut events = Vec::new();

        for byte in complete_stream() {
            events.extend(session.push_uart(&[byte]).unwrap());
        }

        assert_eq!(events, complete_events());
        assert_eq!(session.finish(successful_process()), Ok(complete_outcome()));
    }

    fn ready_frame() -> Vec<u8> {
        let payload = ReadyPayload {
            abi_major: 1,
            abi_minor: 0,
        }
        .encode();
        frame(FrameKind::Ready, &payload)
    }

    fn frame(kind: FrameKind, payload: &[u8]) -> Vec<u8> {
        let mut frame = FrameHeader {
            kind,
            payload_len: payload.len() as u32,
        }
        .encode()
        .to_vec();
        frame.extend_from_slice(payload);
        frame
    }

    fn ready_session() -> Session {
        let mut session = Session::new();
        assert_eq!(
            session.push_uart(&ready_frame()).unwrap(),
            vec![SessionEvent::Ready]
        );
        session
    }

    fn successful_process() -> ProcessStatus {
        ProcessStatus {
            code: Some(0),
            success: true,
        }
    }

    fn complete_stream() -> Vec<u8> {
        let mut stream = b"OpenSBI\n".to_vec();
        stream.extend_from_slice(&ready_frame());
        stream.extend_from_slice(&frame(FrameKind::Stdout, b"hello"));
        stream.extend_from_slice(&frame(FrameKind::Stderr, b"warning"));
        stream.extend_from_slice(&frame(FrameKind::Diagnostic, b"guest note"));
        stream.extend_from_slice(&frame(FrameKind::Exit, &7_u32.to_le_bytes()));
        stream
    }

    fn complete_events() -> Vec<SessionEvent> {
        vec![
            SessionEvent::Ready,
            SessionEvent::Stdout(b"hello".to_vec()),
            SessionEvent::Stderr(b"warning".to_vec()),
            SessionEvent::Diagnostic(b"guest note".to_vec()),
            SessionEvent::Exit(7),
        ]
    }

    fn complete_outcome() -> RunOutcome {
        RunOutcome {
            stdout: b"hello".to_vec(),
            stderr: b"warning".to_vec(),
            exit_code: 7,
            diagnostics: b"OpenSBI\nguest note".to_vec(),
        }
    }
}
