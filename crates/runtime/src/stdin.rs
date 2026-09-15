//! hostのstdin byte streamをguestの`STDIN` frameへ転送するforwarder。
//!
//! kernelのstdin stagingは4 KiBのpull型であり、host→guestのflow controlは
//! QEMUのchardev bufferがguestの消費に合わせてpipeを詰まらせることで実現
//! する。forwarderは自分のthreadでblocking read/writeを行い、runのevent
//! loopは`poll`で終端状態だけを観測する。guestがExitすればQEMUはstdin側を
//! 閉じるため、書き込み側は`BrokenPipe`で自然に終わる。

use std::{
    io::{self, Read, Write},
    sync::mpsc::{self, Receiver, TryRecvError},
    thread,
};

use minios_abi::{control::FrameKind, syscall::MAX_READ_LEN};

/// `STDIN` frameひとつに載るpayloadの上限。kernelのstagingと同じ大きさに
/// 合わせ、frameをまたがずに`read`へ届く単位をそろえる。
const STDIN_CHUNK: usize = MAX_READ_LEN;

/// `STDIN` frameを持つ最小のguest ABI minor version。kernelはREADYの
/// `abi_minor`がこの値以上のときだけhost→guest frameを受理する。
pub(crate) const STDIN_ABI_MINOR: u16 = 1;

/// 転送の終端状態。threadから一度だけ届く。
#[derive(Debug)]
enum Report {
    /// 入力をEOFまで送り切った。guest側の死滅による`BrokenPipe`もここに
    /// 畳み込む — 結果は既にguestの終了で決まっている。
    Done,
    /// host stdinのreadまたはQEMU stdinへのwriteに失敗した。
    Failed(io::ErrorKind, String),
}

/// stdin転送の進行を観測するhandle。threadはdetachされ、runの終了後も
/// 残ることがある — QEMU stdinへの書き込み側はguestの死で`BrokenPipe`に
/// なるが、host入力の`read`で待つthreadは入力側がEOFかerrorになるまで
/// 残る。呼び出し側は終わらないreaderを長生きするprocessへ繰り返し
/// 渡さないこと (単発runのCLIでは問題にならない)。
pub struct StdinForwarder {
    reports: Receiver<Report>,
    terminal: Option<Report>,
}

impl StdinForwarder {
    /// `input`を`STDIN` frameへ逐次encodeして`writer` (QEMUのstdin) へ
    /// 書くforwarderを起こす。`input`がEOFに達したら長さ0のframe (EOF
    /// sentinel) を送って閉じる。
    pub fn spawn<W, R>(mut writer: W, mut input: R) -> Self
    where
        W: Write + Send + 'static,
        R: Read + Send + 'static,
    {
        let (sender, reports) = mpsc::channel();
        thread::spawn(move || {
            let _ = sender.send(forward(&mut writer, &mut input));
        });
        Self {
            reports,
            terminal: None,
        }
    }

    /// 転送の終端を一回だけ観測する。`None`はまだ転送中、`Some(Ok(()))`
    /// はEOFまで送り切った完了、`Some(Err)`はreader/writer failureである。
    /// 一度終端を返した後も同じ結果を返し続ける。
    pub fn poll(&mut self) -> Option<io::Result<()>> {
        if self.terminal.is_none() {
            self.terminal = match self.reports.try_recv() {
                Ok(report) => Some(report),
                Err(TryRecvError::Empty) => None,
                // threadが報告せず死んだ (panic) 場合もrun失敗として扱う。
                Err(TryRecvError::Disconnected) => Some(Report::Failed(
                    io::ErrorKind::Other,
                    "stdin forwarder stopped unexpectedly".to_owned(),
                )),
            };
        }
        self.terminal.as_ref().map(|report| match report {
            Report::Done => Ok(()),
            Report::Failed(kind, message) => Err(io::Error::new(*kind, message.clone())),
        })
    }
}

/// 入力を4 KiB chunkごとに`STDIN` frame化して流し、EOF sentinelで閉じる。
fn forward<W: Write, R: Read>(writer: &mut W, input: &mut R) -> Report {
    let mut chunk = vec![0; STDIN_CHUNK];
    loop {
        match input.read(&mut chunk) {
            Ok(0) => break,
            Ok(len) => {
                if let Err(report) = write_frame(writer, &chunk[..len]) {
                    return report;
                }
            }
            // 一時的な割り込みは同じ読み取りを再試行する。
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                return Report::Failed(error.kind(), error.to_string());
            }
        }
    }
    match write_frame(writer, b"") {
        Ok(()) => Report::Done,
        Err(report) => report,
    }
}

/// 一つの`STDIN` frameを書き切る。payloadは常にchunk上限内にある。
fn write_frame<W: Write>(writer: &mut W, payload: &[u8]) -> Result<(), Report> {
    let frame = minicontainer_protocol::encode_frame(FrameKind::Stdin, payload)
        .expect("stdin chunks never exceed the ABI frame limit");
    match writer.write_all(&frame) {
        Ok(()) => Ok(()),
        // guestが死んでQEMUがstdinを閉じた — 結果は終了側で決まるため静かに終わる。
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Err(Report::Done),
        Err(error) => Err(Report::Failed(error.kind(), error.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::{STDIN_CHUNK, StdinForwarder};
    use minicontainer_protocol::{Decoder, Frame};
    use minios_abi::control::FrameKind;
    use std::{
        collections::VecDeque,
        io::{self, Read, Write},
        sync::{Arc, Mutex},
        thread,
        time::{Duration, Instant},
    };

    const POLL_LIMIT: Duration = Duration::from_secs(5);

    /// write内容をtest側から読める共有buffer。
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Capture {
        fn frames(&self) -> Vec<Frame> {
            Decoder::new()
                .push(&self.0.lock().unwrap())
                .expect("forwarded bytes must be valid frames")
        }
    }

    /// 指定したerrorで一度だけ失敗するreader。
    struct FailingReader {
        error: Option<io::Error>,
    }

    impl Read for FailingReader {
        fn read(&mut self, _output: &mut [u8]) -> io::Result<usize> {
            match self.error.take() {
                Some(error) => Err(error),
                None => Ok(0),
            }
        }
    }

    /// writeで即座に所定のerrorを返すwriter。
    struct FailingWriter(io::ErrorKind);

    impl Write for FailingWriter {
        fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(self.0, "synthetic write failure"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// 一度しか返さないpoll結果を、終端が現れるまで待つ。
    fn wait_terminal(forwarder: &mut StdinForwarder) -> io::Result<()> {
        let deadline = Instant::now() + POLL_LIMIT;
        loop {
            if let Some(result) = forwarder.poll() {
                return result;
            }
            assert!(Instant::now() < deadline, "forwarder never reported");
            thread::sleep(Duration::from_millis(5));
        }
    }

    // Catches a forwarder that drops the EOF sentinel or fails to wrap input
    // bytes in Stdin frames: the guest's read would never see the end.
    #[test]
    fn input_arrives_as_stdin_frames_then_eof() {
        let capture = Capture::default();
        let mut forwarder = StdinForwarder::spawn(capture.clone(), &b"hello stdin"[..]);

        assert!(wait_terminal(&mut forwarder).is_ok());
        assert_eq!(
            capture.frames(),
            vec![
                Frame {
                    kind: FrameKind::Stdin,
                    payload: b"hello stdin".to_vec(),
                },
                Frame {
                    kind: FrameKind::Stdin,
                    payload: Vec::new(),
                },
            ]
        );
    }

    // Catches a chunking regression: input larger than the kernel staging
    // window must be split into consecutive Stdin frames, never one oversized
    // frame the kernel decoder would reject.
    #[test]
    fn input_splits_at_the_staging_boundary() {
        let capture = Capture::default();
        let input: &'static [u8] = vec![0xAB; STDIN_CHUNK + 5].leak();
        let mut forwarder = StdinForwarder::spawn(capture.clone(), input);

        assert!(wait_terminal(&mut forwarder).is_ok());
        let frames = capture.frames();
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].payload.len(), STDIN_CHUNK);
        assert_eq!(frames[1].payload.len(), 5);
        assert_eq!(frames[2].payload.len(), 0);
    }

    // Catches an empty input sending nothing: even a guest that never reads
    // must see stdin as closed (read returns 0), matching Docker semantics.
    #[test]
    fn empty_input_sends_only_the_eof_frame() {
        let capture = Capture::default();
        let mut forwarder = StdinForwarder::spawn(capture.clone(), &b""[..]);

        assert!(wait_terminal(&mut forwarder).is_ok());
        assert_eq!(capture.frames().len(), 1);
        assert_eq!(capture.frames()[0].payload, Vec::<u8>::new());
    }

    // Catches a host stdin read failure being swallowed: the run must fail
    // instead of forwarding a silent truncated input.
    #[test]
    fn read_failure_is_reported() {
        let capture = Capture::default();
        let mut forwarder = StdinForwarder::spawn(
            capture,
            FailingReader {
                error: Some(io::Error::new(io::ErrorKind::InvalidData, "stdin broke")),
            },
        );

        let error = wait_terminal(&mut forwarder).unwrap_err();
        assert_eq!(error.to_string(), "stdin broke");
        // 終端の報告は安定している。
        assert!(forwarder.poll().is_some());
    }

    // Catches treating a dead guest's closed stdin as a forwarding failure:
    // the run's outcome is already decided by the Exit frame, so EPIPE is a
    // clean stop, not an error.
    #[test]
    fn broken_pipe_stops_the_forwarder_without_error() {
        let mut forwarder =
            StdinForwarder::spawn(FailingWriter(io::ErrorKind::BrokenPipe), &b"data"[..]);

        assert!(wait_terminal(&mut forwarder).is_ok());
    }

    // Catches non-EPIPE write failures being hidden: a QEMU stdin that fails
    // for another reason must surface so the run can clean up.
    #[test]
    fn other_write_failures_are_reported() {
        let mut forwarder =
            StdinForwarder::spawn(FailingWriter(io::ErrorKind::PermissionDenied), &b"data"[..]);

        let error = wait_terminal(&mut forwarder).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    // Catches Interrupted reads terminating the forwarder early: a transient
    // read interruption must retry, not end the stream or report failure.
    #[test]
    fn interrupted_reads_retry() {
        struct InterruptingReader {
            chunks: VecDeque<io::Result<Vec<u8>>>,
        }
        impl Read for InterruptingReader {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                match self.chunks.pop_front().expect("unexpected read") {
                    Ok(bytes) => {
                        output[..bytes.len()].copy_from_slice(&bytes);
                        Ok(bytes.len())
                    }
                    Err(error) => Err(error),
                }
            }
        }
        let reader = InterruptingReader {
            chunks: VecDeque::from([
                Err(io::Error::new(io::ErrorKind::Interrupted, "retry me")),
                Ok(b"after".to_vec()),
                Ok(Vec::new()),
            ]),
        };
        let capture = Capture::default();
        let mut forwarder = StdinForwarder::spawn(capture.clone(), reader);

        assert!(wait_terminal(&mut forwarder).is_ok());
        assert_eq!(capture.frames()[0].payload, b"after");
    }
}
