//! v1監視起動と保存結果への再接続。従来のdetachとは独立した経路。
use super::*;
use minicontainer_runtime::{RecordedExit, RecordedObservation, RunRecords};
use std::{
    os::fd::AsRawFd,
    os::unix::process::CommandExt,
    process::{Child, Command as ProcessCommand, Stdio},
    time::Instant,
};

const MONITOR: &str = "__minictr-supervise-v1";
const CONTROL_LIMIT: usize = 4096;
const POLL: Duration = Duration::from_millis(10);

pub fn monitor_dispatch() -> Option<i32> {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(OsStr::new(MONITOR)) {
        return None;
    }
    Some(match monitor(args.collect()) {
        Ok(()) => 0,
        Err(_) => RUNTIME_EXIT,
    })
}

struct Handshake;
impl OutputSink for Handshake {
    fn registered(
        &mut self,
        id: &str,
        _state: &minicontainer_runtime::InstanceState,
    ) -> io::Result<()> {
        let mut output = io::stdout().lock();
        writeln!(output, "instance {id}")?;
        output.flush()
    }
    fn push(&mut self, event: &SessionEvent) -> io::Result<()> {
        if matches!(event, SessionEvent::Ready) {
            let mut output = io::stdout().lock();
            writeln!(output, "ready")?;
            output.flush()?;
        }
        Ok(())
    }
}

fn monitor(args: Vec<OsString>) -> io::Result<()> {
    if args.len() != 6 {
        return Err(invalid("監視引数が不正"));
    }
    let number = |n: usize| -> io::Result<u64> {
        args[n]
            .to_str()
            .and_then(|s| s.parse::<u64>().ok())
            .ok_or_else(|| invalid("監視資源値が不正"))
    };
    let timeout = Duration::from_millis(number(3)?);
    let resources = QemuResources {
        memory_mib: u32::try_from(number(4)?).map_err(|_| invalid("memory"))?,
        cpus: u32::try_from(number(5)?).map_err(|_| invalid("cpus"))?,
    };
    if !(QemuResources::MIN_MEMORY_MIB..=QemuResources::MAX_MEMORY_MIB)
        .contains(&resources.memory_mib)
        || !(QemuResources::MIN_CPUS..=QemuResources::MAX_CPUS).contains(&resources.cpus)
    {
        return Err(invalid("監視資源値が範囲外"));
    }
    if timeout.is_zero() {
        return Err(invalid("監視期限が不正"));
    }
    let mut bundle = Vec::new();
    io::stdin()
        .take(MAX_BUNDLE_LEN + 1)
        .read_to_end(&mut bundle)?;
    validate_v1(&bundle)?;
    install_signal_handlers();
    let store = PathBuf::from(&args[0]);
    let records = RunRecords::open(&store)?;
    let run = records.begin()?;
    {
        let mut output = io::stdout().lock();
        writeln!(output, "record {}", run.id())?;
        output.flush()?;
    }
    let instances = InstanceDir::open(&store).map_err(|e| invalid(&e.to_string()))?;
    let image = args[2].to_str().ok_or_else(|| invalid("image"))?;
    // superviseはQEMUの実statusとcleanup後にのみ最終結果を保存する。
    let _outcome = run
        .supervise_with_sink(
            &Runtime::default(),
            RunRequest {
                bundle: &bundle,
                kernel: Path::new(&args[1]),
                deadline: timeout,
                resources,
                input: None,
                interrupts: Some(&CliInterrupts),
                instances: Some(InstanceRegistration {
                    dir: &instances,
                    image,
                    program: OsStr::new(QEMU_PROGRAM),
                }),
            },
            &mut Handshake,
        )
        .map_err(|e| invalid(&e.to_string()))?;
    Ok(())
}
fn validate_v1(bundle: &[u8]) -> io::Result<()> {
    let parsed = parse(bundle).map_err(|e| invalid(&e.to_string()))?;
    if parsed.manifest.version() != 1 {
        return Err(invalid("startはv1 MiniBundleのみ対応する"));
    }
    Ok(())
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn nonblocking(fd: i32) -> io::Result<()> {
    // 所有済みpipeだけを設定し、他processのdescriptorを変更しない。
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[derive(Default)]
struct Control {
    bytes: Vec<u8>,
    consumed: usize,
    record: Option<String>,
    instance: bool,
    ready: bool,
}
impl Control {
    fn push(&mut self, bytes: &[u8], pid: u32) -> io::Result<()> {
        if self.bytes.len().saturating_add(bytes.len()) > CONTROL_LIMIT {
            return Err(invalid("監視制御出力が上限を超えた"));
        }
        self.bytes.extend_from_slice(bytes);
        while let Some(end) = self.bytes[self.consumed..].iter().position(|b| *b == b'\n') {
            let end = self.consumed + end;
            let line = std::str::from_utf8(&self.bytes[self.consumed..end])
                .map_err(|_| invalid("制御出力がUTF-8でない"))?;
            if let Some(id) = line.strip_prefix("record ") {
                if self.record.is_some()
                    || !cli::is_record_id(id)
                    || id.split('-').nth(1).and_then(|s| s.parse::<u32>().ok()) != Some(pid)
                {
                    return Err(invalid("監視IDが不正"));
                }
                self.record = Some(id.to_owned());
            } else if let Some(id) = line.strip_prefix("instance ") {
                if self.record.is_none() || self.instance || !canonical_instance(id) {
                    return Err(invalid("instance通知が不正"));
                }
                self.instance = true;
            } else if line == "ready" {
                if self.record.is_none() || !self.instance || self.ready {
                    return Err(invalid("READY通知が不正"));
                }
                self.ready = true;
            } else {
                return Err(invalid("未知の制御出力"));
            }
            self.consumed = end + 1;
        }
        Ok(())
    }
}
fn canonical_instance(id: &str) -> bool {
    id.strip_prefix("i-")
        .and_then(|s| s.parse::<u32>().ok())
        .is_some_and(|pid| pid > 0 && id == format!("i-{pid}"))
}

pub fn start(args: &ResolvedRun, stdout: &mut dyn Write, stderr: &mut dyn Write) -> i32 {
    let result = (|| -> io::Result<()> {
        let bundle = RealStore
            .resolve(&args.store, &args.image)
            .map_err(|e| invalid(&e.to_string()))?;
        validate_v1(&bundle)?;
        install_signal_handlers();
        let deadline = Instant::now()
            .checked_add(args.timeout)
            .ok_or_else(|| invalid("起動期限が不正"))?;
        let mut child = ProcessCommand::new(std::env::current_exe()?)
            .arg(MONITOR)
            .arg(&args.store)
            .arg(&args.kernel)
            .arg(&args.image)
            .arg(args.timeout.as_millis().to_string())
            .arg(args.memory_mib.to_string())
            .arg(args.cpus.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()?;
        let mut control = Control::default();
        let startup = (|| -> io::Result<()> {
            let mut input = child
                .stdin
                .take()
                .ok_or_else(|| invalid("監視入力pipeがない"))?;
            let mut output = child
                .stdout
                .take()
                .ok_or_else(|| invalid("監視出力pipeがない"))?;
            nonblocking(input.as_raw_fd())?;
            nonblocking(output.as_raw_fd())?;
            let mut offset = 0;
            while offset < bundle.len() {
                check_start(deadline)?;
                match input.write(&bundle[offset..]) {
                    Ok(0) => return Err(invalid("監視入力pipeが閉じた")),
                    Ok(n) => offset += n,
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) =>
                    {
                        std::thread::sleep(POLL)
                    }
                    Err(e) => return Err(e),
                }
            }
            drop(input);
            await_ready(&mut output, &mut control, child.id(), deadline, || {
                Ok(child.try_wait()?.is_some())
            })?;
            let id = control
                .record
                .as_deref()
                .ok_or_else(|| invalid("監視IDがない"))?;
            writeln!(stdout, "{id}")?;
            stdout.flush()?;
            Ok(())
        })();
        if let Err(error) = startup {
            if let Some(id) = &control.record {
                let _ = writeln!(stderr, "minictr: record={id}");
            }
            if let Err(cleanup) = cancel_monitor(&mut child) {
                let _ = writeln!(
                    stderr,
                    "minictr: 監視役の回収未確認: {cleanup}; record={}",
                    control.record.as_deref().unwrap_or("unknown")
                );
            }
            return Err(error);
        }
        Ok(())
    })();
    match result {
        Ok(()) => 0,
        Err(e) => {
            let _ = writeln!(stderr, "minictr: {e}");
            RUNTIME_EXIT
        }
    }
}
fn await_ready(
    output: &mut impl Read,
    control: &mut Control,
    pid: u32,
    deadline: Instant,
    mut exited_now: impl FnMut() -> io::Result<bool>,
) -> io::Result<()> {
    let mut exited = false;
    loop {
        drain_control(output, control, pid)?;
        if control.ready {
            return Ok(());
        }
        if exited {
            return Err(invalid("READY前に監視役が終了した"));
        }
        if exited_now()? {
            // drainとexit観測の隙間の通知を、終了確認後に再度drainする。
            exited = true;
            continue;
        }
        check_start(deadline)?;
        std::thread::sleep(POLL);
    }
}

fn drain_control(output: &mut impl Read, control: &mut Control, pid: u32) -> io::Result<()> {
    let mut bytes = [0u8; 512];
    loop {
        match output.read(&mut bytes) {
            Ok(0) => return Ok(()),
            Ok(n) => control.push(&bytes[..n], pid)?,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
}

fn check_start(deadline: Instant) -> io::Result<()> {
    if CliInterrupts.poll().is_some() {
        return Err(invalid("起動待機が中断された"));
    }
    if Instant::now() >= deadline {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "起動待機の期限切れ",
        ));
    }
    Ok(())
}
fn cancel_monitor(child: &mut Child) -> io::Result<()> {
    if child.try_wait()?.is_some() {
        return Ok(());
    }
    // 未回収の直接childだけをsignal対象にするため、PID再利用を経由しない。
    if unsafe { libc::kill(-(child.id() as i32), libc::SIGTERM) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "既存ps/stopで回収する必要がある",
            ));
        }
        std::thread::sleep(POLL);
    }
}

pub fn status(
    args: &ResolvedStop,
    wait: bool,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
    let result = (|| -> io::Result<i32> {
        if args.id.starts_with("i-") {
            let _ = InstanceDir::open(&args.store)
                .and_then(|dir| dir.lookup(&args.id))
                .map_err(|e| invalid(&e.to_string()))?;
            writeln!(stdout, "unknown\t-\t{}", args.id)?;
            return Ok(RUNTIME_EXIT);
        }
        install_signal_handlers();
        let deadline = Instant::now()
            .checked_add(args.timeout)
            .ok_or_else(|| invalid("観測期限が不正"))?;
        let records = RunRecords::open(&args.store)?;
        loop {
            let observation = records.observe(&args.id)?;
            let instance = records
                .active_instance(&args.id)?
                .unwrap_or_else(|| "-".into());
            let (state, code, exit) = match observation {
                RecordedObservation::Running => ("running", "-".into(), 0),
                RecordedObservation::Unknown => ("unknown", "-".into(), RUNTIME_EXIT),
                RecordedObservation::Finished(RecordedExit::Guest { code, .. }) => (
                    "exited",
                    code.to_string(),
                    if wait {
                        i32::try_from(code)
                            .ok()
                            .filter(|c| *c <= 255)
                            .unwrap_or(RUNTIME_EXIT)
                    } else {
                        0
                    },
                ),
                RecordedObservation::Finished(RecordedExit::HostFailure { .. }) => {
                    ("failed", RUNTIME_EXIT.to_string(), RUNTIME_EXIT)
                }
            };
            if !wait || state != "running" {
                writeln!(stdout, "{state}\t{code}\t{instance}")?;
                return Ok(exit);
            }
            if Instant::now() >= deadline || CliInterrupts.poll().is_some() {
                writeln!(stdout, "running\t-\t{instance}")?;
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "結果待機を中断した。実行は継続する",
                ));
            }
            std::thread::sleep(POLL);
        }
    })();
    match result {
        Ok(code) => code,
        Err(e) => {
            let _ = writeln!(stderr, "minictr: {e}");
            RUNTIME_EXIT
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ready_written_between_would_block_and_exit_is_drained() {
        struct Pipe {
            first: bool,
            bytes: io::Cursor<Vec<u8>>,
        }
        impl Read for Pipe {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                if self.first {
                    self.first = false;
                    return Err(io::ErrorKind::WouldBlock.into());
                }
                self.bytes.read(buffer)
            }
        }
        let mut pipe = Pipe {
            first: true,
            bytes: io::Cursor::new(b"record r-12-3-4\ninstance i-99\nready\n".to_vec()),
        };
        let mut control = Control::default();
        let mut polls = 0;
        await_ready(
            &mut pipe,
            &mut control,
            12,
            Instant::now() + Duration::from_secs(1),
            || {
                polls += 1;
                Ok(true)
            },
        )
        .unwrap();
        assert_eq!(polls, 1);
        assert!(control.ready);
    }

    #[test]
    fn control_handles_split_ready_and_short_exit() {
        let mut control = Control::default();
        for bytes in [b"record r-12-3-4\ninstance i-99\nrea".as_slice(), b"dy\n"] {
            control.push(bytes, 12).unwrap();
        }
        assert!(control.ready);
        assert_eq!(control.record.as_deref(), Some("r-12-3-4"));
    }
    #[test]
    fn control_refuses_forgery_duplicates_and_oversize() {
        for bytes in [
            b"ready\n".as_slice(),
            b"record r-13-3-4\n",
            b"record r-12-3-4\nrecord r-12-3-4\n",
            b"record r-12-3-4\ninstance i-01\n",
        ] {
            assert!(Control::default().push(bytes, 12).is_err());
        }
        assert!(
            Control::default()
                .push(&vec![b'x'; CONTROL_LIMIT + 1], 12)
                .is_err()
        );
    }
}
