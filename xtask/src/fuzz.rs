//! MiniBundle parserとUART decoderの決定性fuzz harness。
//!
//! 標準libraryのみで動作し、seedとcorpusが同じなら同じ入力列を生成する。
//! 検出対象はpanic、hang、過大allocationの三つである。parserが返す通常の
//! errorはfindingにしない。

use std::alloc::{GlobalAlloc, Layout, System};
use std::any::Any;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use minicontainer_bundle::ImageSpec;
use minios_abi::control::{FrameHeader, FrameKind};

/// Fuzz対象のparser。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FuzzTarget {
    /// `minicontainer_bundle::parse`。
    Bundle,
    /// `minicontainer_protocol::Decoder`。
    Uart,
}

impl FuzzTarget {
    /// CLI名から対象を解決する。
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "bundle" => Some(Self::Bundle),
            "uart" => Some(Self::Uart),
            _ => None,
        }
    }

    /// CLI名とcorpus directory名を返す。
    pub fn name(self) -> &'static str {
        match self {
            Self::Bundle => "bundle",
            Self::Uart => "uart",
        }
    }
}

/// 既定の乱数seed。
pub const DEFAULT_SEED: u64 = 27;
/// 既定の入力数。
pub const DEFAULT_ITERS: u64 = 10_000;
/// 既定の入力上限(bytes)。
pub const DEFAULT_MAX_BYTES: usize = 128 * 1024;
/// 既定の入力あたりtimeout。
pub const DEFAULT_INPUT_TIMEOUT: Duration = Duration::from_secs(5);
/// 既定のallocation上限(bytes)。
pub const DEFAULT_ALLOC_CAP: usize = 16 * 1024 * 1024;

/// Fuzz実行の条件。
#[derive(Debug, Clone)]
pub struct FuzzConfig {
    /// 対象parser。
    pub target: FuzzTarget,
    /// 乱数seed。同じseedは同じ入力列を生成する。
    pub seed: u64,
    /// 生成する入力数。replay時は無視する。
    pub iters: u64,
    /// 生成入力の上限(bytes)。corpusのseedはこの長さへ切り詰める。
    pub max_bytes: usize,
    /// 入力あたりのtimeout。
    pub input_timeout: Duration,
    /// 全体の制限時間。`None`は無制限。
    pub time_limit: Option<Duration>,
    /// 入力あたりのallocation上限(bytes)。
    pub alloc_cap: usize,
    /// seed corpusのdirectory。`<corpus>/<target>/` を読む。
    pub corpus_dir: PathBuf,
    /// findingの入力を保存するdirectory。
    pub output_dir: PathBuf,
    /// 指定時はこの一つのfileをそのまま再生する。
    pub replay_input: Option<PathBuf>,
}

/// 一つの入力の実行結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputOutcome {
    /// parserが復帰した。errorも正常終了に含める。
    Pass {
        /// 実行中に確保したpeak(bytes)。
        peak_alloc: usize,
    },
    /// parserがpanicした。
    Panic {
        /// panicのmessage。
        message: String,
    },
    /// timeoutを超過した。
    Timeout,
    /// allocation上限を超過した。
    OverAlloc {
        /// 観測したpeak(bytes)。
        peak: usize,
        /// 適用した上限(bytes)。
        cap: usize,
    },
}

/// findingのないfuzz実行の報告。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuzzReport {
    /// 対象parser。
    pub target: FuzzTarget,
    /// 使用した乱数seed。
    pub seed: u64,
    /// 実行した入力数。
    pub iters_run: u64,
}

/// Fuzz実行の失敗。
#[derive(Debug, PartialEq, Eq)]
pub enum FuzzError {
    /// 設定値が不正である。
    InvalidConfig(&'static str),
    /// fileの読み書きに失敗した。
    Io {
        /// 操作の内容。
        context: &'static str,
        /// 対象のpath。
        path: PathBuf,
        /// OSのmessage。
        message: String,
    },
    /// corpus directoryが存在しない。
    MissingCorpus {
        /// 期待したpath。
        path: PathBuf,
    },
    /// failing入力を検出した。詳細と再現commandを含む。
    FindingFound {
        /// 対象parser名。
        target: &'static str,
        /// 検出時の詳細。
        detail: String,
        /// 再現用の入力file。生成入力は保存先、replayは指定file。
        repro_path: PathBuf,
    },
}

impl fmt::Display for FuzzError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(reason) => {
                write!(formatter, "invalid fuzz configuration: {reason}")
            }
            Self::Io {
                context,
                path,
                message,
            } => write!(formatter, "{} {}: {message}", context, path.display()),
            Self::MissingCorpus { path } => write!(
                formatter,
                "fuzz corpus directory not found: {}",
                path.display()
            ),
            Self::FindingFound {
                target,
                detail,
                repro_path,
            } => write!(
                formatter,
                "fuzz {target} found a failing input ({detail}); reproduce with: cargo xtask fuzz --target {target} --input '{}'",
                repro_path.display()
            ),
        }
    }
}

impl std::error::Error for FuzzError {}

/// Fuzzを実行する。findingがあれば保存して`FindingFound`を返す。
pub fn run(config: &FuzzConfig) -> Result<FuzzReport, FuzzError> {
    validate(config)?;
    if let Some(input) = &config.replay_input {
        return replay(config, input);
    }
    let seeds = load_corpus(&config.corpus_dir, config.target, config.max_bytes)?;
    fs::create_dir_all(&config.output_dir)
        .map_err(|error| io_error("could not create", &config.output_dir, &error))?;
    let mut inputs = InputStream::new(seeds, config.seed, config.max_bytes);
    let started = Instant::now();
    let mut iters_run = 0;
    while iters_run < config.iters {
        if config
            .time_limit
            .is_some_and(|limit| started.elapsed() >= limit)
        {
            break;
        }
        let Some(input) = inputs.next() else {
            break;
        };
        match run_input(
            config.target,
            &input,
            config.input_timeout,
            config.alloc_cap,
        ) {
            InputOutcome::Pass { .. } => {}
            outcome => {
                let artifact = config.output_dir.join(format!(
                    "{}-{:016x}-{iters_run}.bin",
                    config.target.name(),
                    config.seed
                ));
                fs::write(&artifact, &input)
                    .map_err(|error| io_error("could not write", &artifact, &error))?;
                return Err(FuzzError::FindingFound {
                    target: config.target.name(),
                    detail: describe(&outcome, config),
                    repro_path: artifact,
                });
            }
        }
        iters_run += 1;
    }
    Ok(FuzzReport {
        target: config.target,
        seed: config.seed,
        iters_run,
    })
}

/// 一つのfileを変異なしで再生する。replayは入力を切り詰めない。
fn replay(config: &FuzzConfig, path: &Path) -> Result<FuzzReport, FuzzError> {
    let bytes = fs::read(path).map_err(|error| io_error("could not read", path, &error))?;
    match run_input(
        config.target,
        &bytes,
        config.input_timeout,
        config.alloc_cap,
    ) {
        InputOutcome::Pass { .. } => Ok(FuzzReport {
            target: config.target,
            seed: config.seed,
            iters_run: 1,
        }),
        outcome => Err(FuzzError::FindingFound {
            target: config.target.name(),
            detail: describe(&outcome, config),
            repro_path: path.to_owned(),
        }),
    }
}

fn validate(config: &FuzzConfig) -> Result<(), FuzzError> {
    if config.replay_input.is_none() && config.iters == 0 {
        return Err(FuzzError::InvalidConfig("iters must be at least 1"));
    }
    if config.max_bytes == 0 {
        return Err(FuzzError::InvalidConfig("max-bytes must be at least 1"));
    }
    if config.input_timeout.is_zero() {
        return Err(FuzzError::InvalidConfig("input-timeout must be positive"));
    }
    if config.alloc_cap == 0 {
        return Err(FuzzError::InvalidConfig("alloc-cap must be at least 1"));
    }
    Ok(())
}

fn describe(outcome: &InputOutcome, config: &FuzzConfig) -> String {
    match outcome {
        InputOutcome::Pass { .. } => "passed".to_owned(),
        InputOutcome::Panic { message } => format!("panic: {message}"),
        InputOutcome::Timeout => format!("timeout after {:.1?}", config.input_timeout),
        InputOutcome::OverAlloc { peak, cap } => {
            format!("peak allocation {peak} exceeds cap {cap}")
        }
    }
}

fn io_error(context: &'static str, path: &Path, error: &std::io::Error) -> FuzzError {
    FuzzError::Io {
        context,
        path: path.to_owned(),
        message: error.to_string(),
    }
}

/// 組み込みseedにcorpus directoryのfileを足した一覧を返す。
/// file名順に読むため、corpusが同じなら順序も同じである。
fn load_corpus(
    corpus_dir: &Path,
    target: FuzzTarget,
    max_bytes: usize,
) -> Result<Vec<Vec<u8>>, FuzzError> {
    let mut seeds = builtin_seeds(target);
    let dir = corpus_dir.join(target.name());
    if !dir.is_dir() {
        return Err(FuzzError::MissingCorpus { path: dir });
    }
    let mut entries: Vec<PathBuf> = Vec::new();
    let listing = fs::read_dir(&dir).map_err(|error| io_error("could not list", &dir, &error))?;
    for entry in listing {
        let path = entry
            .map_err(|error| io_error("could not list", &dir, &error))?
            .path();
        if path.is_file() {
            entries.push(path);
        }
    }
    entries.sort();
    for path in entries {
        let mut bytes =
            fs::read(&path).map_err(|error| io_error("could not read", &path, &error))?;
        bytes.truncate(max_bytes);
        seeds.push(bytes);
    }
    Ok(seeds)
}

/// 一つの入力をguard付きで実行する。
///
/// peakはparser実行中の確保だけを測り、入力buffer自体は含めない。
/// 計測はprocess全体を見るため、呼び出し側は計測中にほかのthreadが
/// 大量確保しないことを保証する。CLIは単threadで動き、corpusのreplay
/// testは直列化する。
pub fn run_input(
    target: FuzzTarget,
    bytes: &[u8],
    timeout: Duration,
    alloc_cap: usize,
) -> InputOutcome {
    let owned = bytes.to_vec();
    let outcome = run_guarded(
        move || match target {
            FuzzTarget::Bundle => execute_bundle(&owned),
            FuzzTarget::Uart => execute_uart(&owned),
        },
        timeout,
    );
    match outcome {
        GuardOutcome::Done { peak } if peak > alloc_cap => InputOutcome::OverAlloc {
            peak,
            cap: alloc_cap,
        },
        GuardOutcome::Done { peak } => InputOutcome::Pass { peak_alloc: peak },
        GuardOutcome::Panic { message } => InputOutcome::Panic { message },
        GuardOutcome::Timeout => InputOutcome::Timeout,
    }
}

fn execute_bundle(bytes: &[u8]) {
    let _ = minicontainer_bundle::parse(bytes);
}

fn execute_uart(bytes: &[u8]) {
    // chunk境界のfuzz: 入力から決定性に分割点を導く。
    let mut splits = FuzzRng::new(fnv1a(bytes));
    let chunks = 1 + splits.below(4);
    let mut decoder = minicontainer_protocol::Decoder::new();
    let mut start = 0;
    for chunk in 0..chunks {
        let remaining = chunks - chunk;
        let end = if remaining == 1 {
            bytes.len()
        } else {
            start + splits.below(bytes.len() - start + 1)
        };
        let _ = decoder.push(&bytes[start..end]);
        start = end;
    }
    let _ = decoder.finish();
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// 対象の最小seed。corpus fileがなくてもharnessは動作する。
pub fn builtin_seeds(target: FuzzTarget) -> Vec<Vec<u8>> {
    match target {
        FuzzTarget::Bundle => vec![
            minicontainer_bundle::build(ImageSpec {
                name: "fuzz",
                args: &[],
                elf: b"fuzz-elf",
            })
            .expect("static bundle seed must build"),
            minicontainer_bundle::build(ImageSpec {
                name: "fuzz-args",
                args: &["first", "second"],
                elf: &[0xFF; 64],
            })
            .expect("static bundle seed must build"),
        ],
        FuzzTarget::Uart => vec![encode_frame(FrameKind::Stdout, b"fuzz-stdout"), {
            let mut concat = encode_frame(FrameKind::Ready, &[1, 0, 0, 0]);
            concat.extend_from_slice(&encode_frame(FrameKind::Stdout, b" Out"));
            concat.extend_from_slice(&encode_frame(FrameKind::Exit, &[0, 0, 0, 0]));
            concat
        }],
    }
}

fn encode_frame(kind: FrameKind, payload: &[u8]) -> Vec<u8> {
    let header = FrameHeader {
        kind,
        payload_len: payload.len() as u32,
    };
    let mut bytes = header.encode().to_vec();
    bytes.extend_from_slice(payload);
    bytes
}

/// seedと設定から決定性の入力列を生成するiterator。
#[derive(Debug)]
pub struct InputStream {
    seeds: Vec<Vec<u8>>,
    rng: FuzzRng,
    max_bytes: usize,
}

impl InputStream {
    /// 入力列を作る。同じ引数は同じ列を生成する。
    pub fn new(seeds: Vec<Vec<u8>>, seed: u64, max_bytes: usize) -> Self {
        Self {
            seeds,
            rng: FuzzRng::new(seed),
            max_bytes,
        }
    }
}

impl Iterator for InputStream {
    type Item = Vec<u8>;

    fn next(&mut self) -> Option<Vec<u8>> {
        if self.seeds.is_empty() {
            return None;
        }
        let seed = &self.seeds[self.rng.below(self.seeds.len())];
        Some(mutate(&mut self.rng, seed, self.max_bytes))
    }
}

/// 決定性の乱数生成器。xorshift64*。
#[derive(Debug, Clone)]
struct FuzzRng(u64);

impl FuzzRng {
    fn new(seed: u64) -> Self {
        // xorshiftは0へ吸着するため、0だけを固定の奇数へ写す。
        Self(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// `bound`未満を返す。呼び出し側は1以上を渡す。
    /// 剰余の偏りはfuzz用途で無視する。
    fn below(&mut self, bound: usize) -> usize {
        (self.next_u64() % bound as u64) as usize
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        let mut out = Vec::new();
        while out.len() < len {
            out.extend_from_slice(&self.next_u64().to_le_bytes());
        }
        out.truncate(len);
        out
    }
}

/// seedを変異させて一つの入力を作る。結果は`max_bytes`以下である。
fn mutate(rng: &mut FuzzRng, seed: &[u8], max_bytes: usize) -> Vec<u8> {
    let mut bytes = seed.to_vec();
    bytes.truncate(max_bytes);
    // 空seedに有効な操作は挿入だけである。
    let op = if bytes.is_empty() { 2 } else { rng.below(8) };
    match op {
        0 => {
            let index = rng.below(bytes.len());
            bytes[index] ^= 1 << rng.below(8);
        }
        1 => {
            let index = rng.below(bytes.len());
            bytes[index] = rng.bytes(1)[0];
        }
        2 => {
            let count = 1 + rng.below(8);
            let position = rng.below(bytes.len() + 1);
            bytes.splice(position..position, rng.bytes(count));
        }
        3 => {
            let start = rng.below(bytes.len());
            let end = start + 1 + rng.below(bytes.len() - start);
            bytes.drain(start..end);
        }
        4 => {
            bytes.truncate(rng.below(bytes.len() + 1));
        }
        5 => {
            let start = rng.below(bytes.len());
            let width = 1 + rng.below((bytes.len() - start).min(32));
            let chunk = bytes[start..start + width].to_vec();
            let position = rng.below(bytes.len() + 1);
            bytes.splice(position..position, chunk);
        }
        6 => {
            let first = rng.below(bytes.len());
            let second = rng.below(bytes.len());
            bytes.swap(first, second);
        }
        _ => {
            const INTERESTING: [u32; 8] = [
                0,
                1,
                0xFF,
                0xFFFF,
                64 * 1024 - 1,
                64 * 1024,
                64 * 1024 + 1,
                u32::MAX,
            ];
            let value = INTERESTING[rng.below(INTERESTING.len())];
            let width = if rng.below(2) == 0 { 2 } else { 4 };
            if bytes.len() >= width {
                let position = rng.below(bytes.len() - width + 1);
                let encoded = value.to_le_bytes();
                bytes[position..position + width].copy_from_slice(&encoded[..width]);
            } else {
                bytes.extend_from_slice(&value.to_le_bytes());
            }
        }
    }
    bytes.truncate(max_bytes);
    bytes
}

/// 割り当ての現在量とpeakを数える計測器。
#[derive(Debug)]
struct PeakTracker {
    current: AtomicUsize,
    peak: AtomicUsize,
}

impl PeakTracker {
    const fn new() -> Self {
        Self {
            current: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        }
    }

    fn alloc(&self, size: usize) {
        let current = self.current.fetch_add(size, Ordering::SeqCst);
        self.peak
            .fetch_max(current.saturating_add(size), Ordering::SeqCst);
    }

    /// 計測開始前の確保の解放に対応するため飽和減算する。
    fn free(&self, size: usize) {
        let mut current = self.current.load(Ordering::SeqCst);
        while let Err(actual) = self.current.compare_exchange_weak(
            current,
            current.saturating_sub(size),
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            current = actual;
        }
    }

    fn reset(&self) {
        self.current.store(0, Ordering::SeqCst);
        self.peak.store(0, Ordering::SeqCst);
    }

    fn peak(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    fn current(&self) -> usize {
        self.current.load(Ordering::SeqCst)
    }
}

struct CountingAlloc;

static TRACKER: PeakTracker = PeakTracker::new();

#[global_allocator]
static ALLOCATOR: CountingAlloc = CountingAlloc;

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: Systemへ委譲し、成功時のlayout sizeだけを数える。
        unsafe {
            let pointer = System.alloc(layout);
            if !pointer.is_null() {
                TRACKER.alloc(layout.size());
            }
            pointer
        }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: Systemへ委譲し、確保と同じsizeを戻す。
        unsafe {
            System.dealloc(pointer, layout);
            TRACKER.free(layout.size());
        }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: Systemへ委譲し、成功時に差分だけを数える。
        unsafe {
            let grown = System.realloc(pointer, layout, new_size);
            if !grown.is_null() {
                if new_size >= layout.size() {
                    TRACKER.alloc(new_size - layout.size());
                } else {
                    TRACKER.free(layout.size() - new_size);
                }
            }
            grown
        }
    }
}

/// guard付きtaskの結果。
#[derive(Debug)]
enum GuardOutcome {
    Done { peak: usize },
    Panic { message: String },
    Timeout,
}

/// taskを分離threadで実行し、panicとtimeoutを検出する。
/// timeout時はworker threadを残したまま復帰する。CLIはprocess終了で回収し、
/// testは失敗後にbinary終了で回収する。
#[cfg(test)]
static GUARD_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn run_guarded(task: impl FnOnce() + Send + 'static, timeout: Duration) -> GuardOutcome {
    // test binaryは並列実行のため、peak計測だけを直列化する。本番は単threadである。
    #[cfg(test)]
    let _serial = GUARD_SERIAL.lock().expect("fuzz guard lock must be free");
    TRACKER.reset();
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(task));
        let _ = sender.send(result);
    });
    match receiver.recv_timeout(timeout) {
        Ok(Ok(())) => GuardOutcome::Done {
            peak: TRACKER.peak(),
        },
        Ok(Err(payload)) => GuardOutcome::Panic {
            message: panic_message(&payload),
        },
        Err(_) => GuardOutcome::Timeout,
    }
}

fn panic_message(payload: &Box<dyn Any + Send>) -> String {
    if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_owned()
    } else {
        "unknown panic payload".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_both_targets_by_cli_name() {
        assert_eq!(FuzzTarget::parse("bundle"), Some(FuzzTarget::Bundle));
        assert_eq!(FuzzTarget::parse("uart"), Some(FuzzTarget::Uart));
        assert_eq!(FuzzTarget::parse("qemu"), None);
        assert_eq!(FuzzTarget::parse(""), None);
        assert_eq!(FuzzTarget::Bundle.name(), "bundle");
        assert_eq!(FuzzTarget::Uart.name(), "uart");
    }

    #[test]
    fn rng_replays_the_same_stream_for_the_same_seed() {
        let first = FuzzRng::new(27);
        let second = FuzzRng::new(27);
        let third = FuzzRng::new(28);

        let first_stream = stream(first, 64);
        let second_stream = stream(second, 64);
        let third_stream = stream(third, 64);

        assert_eq!(first_stream, second_stream);
        assert_ne!(first_stream, third_stream);
        assert!(first_stream.iter().any(|value| *value != 0));
    }

    #[test]
    fn rng_seed_zero_does_not_stick_at_zero() {
        let values = stream(FuzzRng::new(0), 8);

        assert!(values.iter().all(|value| *value != 0));
    }

    #[test]
    fn rng_draws_stay_within_bounds() {
        let mut rng = FuzzRng::new(7);

        for _ in 0..256 {
            assert!(rng.below(1) < 1);
            assert!(rng.below(16) < 16);
        }
        assert_eq!(rng.bytes(0).len(), 0);
        assert_eq!(rng.bytes(17).len(), 17);
    }

    fn stream(mut rng: FuzzRng, count: usize) -> Vec<u64> {
        (0..count).map(|_| rng.next_u64()).collect()
    }

    #[test]
    fn mutation_is_deterministic_and_bounded() {
        let seed = b"version=1\nname=fuzz\n".as_slice();

        let first = mutate(&mut FuzzRng::new(3), seed, 64);
        let second = mutate(&mut FuzzRng::new(3), seed, 64);

        assert_eq!(first, second);
        assert!(first.len() <= 64);
    }

    #[test]
    fn mutation_truncates_to_the_byte_limit() {
        let seed = vec![0xA5; 4096];

        let mutated = mutate(&mut FuzzRng::new(11), &seed, 128);

        assert!(mutated.len() <= 128);
    }

    #[test]
    fn mutation_handles_an_empty_seed() {
        let mutated = mutate(&mut FuzzRng::new(5), &[], 32);

        assert!(!mutated.is_empty());
        assert!(mutated.len() <= 32);
    }

    #[test]
    fn mutation_changes_the_seed_within_a_bounded_draw() {
        let seed = b"MiniContainer fuzz seed".as_slice();
        let mut rng = FuzzRng::new(9);

        let changed = (0..100).any(|_| mutate(&mut rng, seed, 64) != seed);

        assert!(changed, "100 draws must change the seed at least once");
    }

    #[test]
    fn input_stream_replays_the_same_inputs_for_the_same_seed() {
        let seeds = vec![b"first".to_vec(), b"second seed".to_vec()];

        let first: Vec<Vec<u8>> = InputStream::new(seeds.clone(), 13, 64).take(200).collect();
        let second: Vec<Vec<u8>> = InputStream::new(seeds, 13, 64).take(200).collect();

        assert_eq!(first, second);
        assert_eq!(first.len(), 200);
        assert!(first.iter().all(|input| input.len() <= 64));
    }

    #[test]
    fn input_stream_stops_without_seeds() {
        assert_eq!(InputStream::new(Vec::new(), 1, 64).next(), None);
    }

    #[test]
    fn peak_tracker_records_current_and_peak() {
        let tracker = PeakTracker::new();

        tracker.alloc(100);
        tracker.alloc(50);
        tracker.free(100);
        tracker.alloc(200);

        assert_eq!(tracker.current(), 250);
        assert_eq!(tracker.peak(), 250);

        tracker.reset();

        assert_eq!(tracker.current(), 0);
        assert_eq!(tracker.peak(), 0);
    }

    #[test]
    fn peak_tracker_saturates_releases_before_measurement() {
        let tracker = PeakTracker::new();

        tracker.free(64);

        assert_eq!(tracker.current(), 0);
        assert_eq!(tracker.peak(), 0);
    }

    #[test]
    fn guard_runs_the_task_to_completion() {
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let task_done = std::sync::Arc::clone(&done);
        let outcome = run_guarded(
            move || {
                task_done.store(true, std::sync::atomic::Ordering::SeqCst);
            },
            Duration::from_secs(5),
        );

        match outcome {
            GuardOutcome::Done { .. } => assert!(done.load(std::sync::atomic::Ordering::SeqCst)),
            GuardOutcome::Panic { message } => panic!("must not panic: {message}"),
            GuardOutcome::Timeout => panic!("must not time out"),
        }
    }

    #[test]
    fn guard_captures_a_panic_message() {
        let outcome = run_guarded(|| panic!("boom-from-test"), Duration::from_secs(5));

        assert!(
            matches!(outcome, GuardOutcome::Panic { message } if message.contains("boom-from-test"))
        );
    }

    #[test]
    fn guard_reports_a_timeout() {
        let outcome = run_guarded(
            || std::thread::sleep(Duration::from_millis(200)),
            Duration::from_millis(20),
        );

        assert!(matches!(outcome, GuardOutcome::Timeout));
    }

    #[test]
    fn guard_counts_a_large_allocation_toward_the_peak() {
        let outcome = run_guarded(
            || {
                let held = vec![0x5A; 1024 * 1024];
                std::hint::black_box(held.len());
            },
            Duration::from_secs(5),
        );

        match outcome {
            GuardOutcome::Done { peak } => assert!(peak >= 1024 * 1024),
            other => panic!("must finish the allocation, got {other:?}"),
        }
    }

    #[test]
    fn valid_bundle_bytes_pass() {
        for seed in builtin_seeds(FuzzTarget::Bundle) {
            assert!(
                matches!(
                    run_input(
                        FuzzTarget::Bundle,
                        &seed,
                        Duration::from_secs(5),
                        DEFAULT_ALLOC_CAP
                    ),
                    InputOutcome::Pass { .. }
                ),
                "valid bundle seed must pass"
            );
        }
    }

    #[test]
    fn malformed_bundle_bytes_pass_without_a_finding() {
        for input in [&[][..], &[0; 10][..], &[0xFF; 96][..]] {
            assert!(
                matches!(
                    run_input(
                        FuzzTarget::Bundle,
                        input,
                        Duration::from_secs(5),
                        DEFAULT_ALLOC_CAP
                    ),
                    InputOutcome::Pass { .. }
                ),
                "parser errors are not findings"
            );
        }
    }

    #[test]
    fn valid_uart_bytes_pass() {
        for seed in builtin_seeds(FuzzTarget::Uart) {
            assert!(
                matches!(
                    run_input(
                        FuzzTarget::Uart,
                        &seed,
                        Duration::from_secs(5),
                        DEFAULT_ALLOC_CAP
                    ),
                    InputOutcome::Pass { .. }
                ),
                "valid uart seed must pass"
            );
        }
    }

    #[test]
    fn malformed_uart_bytes_pass_without_a_finding() {
        for input in [&[][..], b"MCF1".as_slice(), &[0xFF; 40][..]] {
            assert!(
                matches!(
                    run_input(
                        FuzzTarget::Uart,
                        input,
                        Duration::from_secs(5),
                        DEFAULT_ALLOC_CAP
                    ),
                    InputOutcome::Pass { .. }
                ),
                "decoder errors are not findings"
            );
        }
    }

    #[test]
    fn tiny_alloc_cap_reports_over_alloc() {
        let seed = builtin_seeds(FuzzTarget::Uart)
            .into_iter()
            .next()
            .expect("uart must ship a seed");

        assert!(
            matches!(
                run_input(FuzzTarget::Uart, &seed, Duration::from_secs(5), 1),
                InputOutcome::OverAlloc { cap: 1, .. }
            ),
            "a 1-byte cap must trip on any allocation"
        );
    }

    #[test]
    fn run_rejects_invalid_configs() {
        let scratch = scratch_dir("invalid-config");
        let base = FuzzConfig {
            target: FuzzTarget::Bundle,
            seed: DEFAULT_SEED,
            iters: DEFAULT_ITERS,
            max_bytes: DEFAULT_MAX_BYTES,
            input_timeout: DEFAULT_INPUT_TIMEOUT,
            time_limit: None,
            alloc_cap: DEFAULT_ALLOC_CAP,
            corpus_dir: scratch.path.join("corpus"),
            output_dir: scratch.path.join("output"),
            replay_input: None,
        };

        let zero_iters = FuzzConfig {
            iters: 0,
            ..base.clone()
        };
        assert!(matches!(run(&zero_iters), Err(FuzzError::InvalidConfig(_))));

        let zero_bytes = FuzzConfig {
            max_bytes: 0,
            ..base.clone()
        };
        assert!(matches!(run(&zero_bytes), Err(FuzzError::InvalidConfig(_))));

        let zero_timeout = FuzzConfig {
            input_timeout: Duration::ZERO,
            ..base.clone()
        };
        assert!(matches!(
            run(&zero_timeout),
            Err(FuzzError::InvalidConfig(_))
        ));

        let zero_cap = FuzzConfig {
            alloc_cap: 0,
            ..base.clone()
        };
        assert!(matches!(run(&zero_cap), Err(FuzzError::InvalidConfig(_))));
    }

    #[test]
    fn run_rejects_a_missing_corpus_directory() {
        let scratch = scratch_dir("missing-corpus");
        let config = FuzzConfig {
            target: FuzzTarget::Bundle,
            seed: DEFAULT_SEED,
            iters: 10,
            max_bytes: 1024,
            input_timeout: Duration::from_secs(5),
            time_limit: None,
            alloc_cap: DEFAULT_ALLOC_CAP,
            corpus_dir: scratch.path.join("absent"),
            output_dir: scratch.path.join("output"),
            replay_input: None,
        };

        assert!(matches!(run(&config), Err(FuzzError::MissingCorpus { .. })));
    }

    #[test]
    fn run_stops_before_the_first_input_on_a_zero_time_limit() {
        let scratch = scratch_dir("zero-time-limit");
        let corpus = scratch.path.join("corpus").join("bundle");
        fs::create_dir_all(&corpus).expect("fixture corpus must exist");
        let config = FuzzConfig {
            target: FuzzTarget::Bundle,
            seed: DEFAULT_SEED,
            iters: 100,
            max_bytes: 1024,
            input_timeout: Duration::from_secs(5),
            time_limit: Some(Duration::ZERO),
            alloc_cap: DEFAULT_ALLOC_CAP,
            corpus_dir: scratch.path.join("corpus"),
            output_dir: scratch.path.join("output"),
            replay_input: None,
        };

        assert_eq!(
            run(&config),
            Ok(FuzzReport {
                target: FuzzTarget::Bundle,
                seed: DEFAULT_SEED,
                iters_run: 0,
            })
        );
    }

    #[test]
    fn run_passes_a_small_loop_over_an_empty_corpus() {
        let scratch = scratch_dir("empty-corpus-loop");
        let corpus = scratch.path.join("corpus").join("uart");
        fs::create_dir_all(&corpus).expect("fixture corpus must exist");
        let config = FuzzConfig {
            target: FuzzTarget::Uart,
            seed: 41,
            iters: 50,
            max_bytes: 256,
            input_timeout: Duration::from_secs(5),
            time_limit: None,
            alloc_cap: DEFAULT_ALLOC_CAP,
            corpus_dir: scratch.path.join("corpus"),
            output_dir: scratch.path.join("output"),
            replay_input: None,
        };

        assert_eq!(
            run(&config),
            Ok(FuzzReport {
                target: FuzzTarget::Uart,
                seed: 41,
                iters_run: 50,
            })
        );
    }

    #[test]
    fn run_saves_the_failing_input_and_names_the_repro() {
        let scratch = scratch_dir("finding-artifact");
        let corpus = scratch.path.join("corpus").join("uart");
        fs::create_dir_all(&corpus).expect("fixture corpus must exist");
        let output = scratch.path.join("output");
        let config = FuzzConfig {
            target: FuzzTarget::Uart,
            seed: 43,
            iters: 100,
            max_bytes: 256,
            input_timeout: Duration::from_secs(5),
            time_limit: None,
            alloc_cap: 1,
            corpus_dir: scratch.path.join("corpus"),
            output_dir: output.clone(),
            replay_input: None,
        };

        match run(&config) {
            Err(FuzzError::FindingFound {
                target,
                detail,
                repro_path,
            }) => {
                assert_eq!(target, "uart");
                assert!(detail.contains("exceeds cap"), "detail: {detail}");
                let saved = fs::read(&repro_path).expect("artifact must exist");
                let generated: Vec<Vec<u8>> =
                    InputStream::new(builtin_seeds(FuzzTarget::Uart), 43, 256)
                        .take(100)
                        .collect();
                assert!(
                    generated.contains(&saved),
                    "artifact must equal one generated input"
                );
                assert!(repro_path.starts_with(&output));
            }
            other => panic!("1-byte cap must produce a finding, got {other:?}"),
        }
    }

    #[test]
    fn run_replays_one_file_exactly() {
        let scratch = scratch_dir("replay-pass");
        let input = scratch.path.join("seed.bin");
        let seed = builtin_seeds(FuzzTarget::Bundle)
            .into_iter()
            .next()
            .expect("bundle must ship a seed");
        fs::write(&input, &seed).expect("fixture input must exist");
        let config = FuzzConfig {
            target: FuzzTarget::Bundle,
            seed: DEFAULT_SEED,
            iters: DEFAULT_ITERS,
            max_bytes: 16,
            input_timeout: Duration::from_secs(5),
            time_limit: None,
            alloc_cap: DEFAULT_ALLOC_CAP,
            corpus_dir: scratch.path.join("absent-corpus"),
            output_dir: scratch.path.join("output"),
            replay_input: Some(input),
        };

        // replayはcorpusもmax-bytesも見ず、fileをそのまま実行する。
        assert_eq!(
            run(&config),
            Ok(FuzzReport {
                target: FuzzTarget::Bundle,
                seed: DEFAULT_SEED,
                iters_run: 1,
            })
        );
    }

    #[test]
    fn run_replay_reports_a_finding_against_the_input_file() {
        let scratch = scratch_dir("replay-finding");
        let input = scratch.path.join("seed.bin");
        let seed = builtin_seeds(FuzzTarget::Uart)
            .into_iter()
            .next()
            .expect("uart must ship a seed");
        fs::write(&input, &seed).expect("fixture input must exist");
        let config = FuzzConfig {
            target: FuzzTarget::Uart,
            seed: DEFAULT_SEED,
            iters: DEFAULT_ITERS,
            max_bytes: DEFAULT_MAX_BYTES,
            input_timeout: Duration::from_secs(5),
            time_limit: None,
            alloc_cap: 1,
            corpus_dir: scratch.path.join("absent-corpus"),
            output_dir: scratch.path.join("output"),
            replay_input: Some(input.clone()),
        };

        match run(&config) {
            Err(FuzzError::FindingFound {
                detail, repro_path, ..
            }) => {
                assert!(detail.contains("exceeds cap"), "detail: {detail}");
                assert_eq!(repro_path, input);
            }
            other => panic!("1-byte cap must produce a finding, got {other:?}"),
        }
    }

    #[test]
    fn finding_error_names_the_repro_command() {
        let error = FuzzError::FindingFound {
            target: "bundle",
            detail: "panic: boom".to_owned(),
            repro_path: PathBuf::from("target/fuzz/bundle-seed-7.bin"),
        };

        assert_eq!(
            error.to_string(),
            "fuzz bundle found a failing input (panic: boom); reproduce with: cargo xtask fuzz --target bundle --input 'target/fuzz/bundle-seed-7.bin'"
        );
    }

    /// testごとに一意な一時directory。Dropで後片付けする。
    struct ScratchDir {
        path: PathBuf,
    }

    fn scratch_dir(name: &str) -> ScratchDir {
        let path = std::env::temp_dir().join(format!(
            "minicontainer-fuzz-test-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("scratch dir must exist");
        ScratchDir { path }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}
