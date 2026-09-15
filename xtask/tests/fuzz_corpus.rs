//! Checked-in corpusの決定性replay。CIでも同じ入力を実行する。

use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use xtask::fuzz::{DEFAULT_ALLOC_CAP, FuzzTarget, InputOutcome, run_input};

static SERIAL: OnceLock<Mutex<()>> = OnceLock::new();

#[test]
fn bundle_corpus_replays_without_findings() {
    replay(FuzzTarget::Bundle);
}

#[test]
fn uart_corpus_replays_without_findings() {
    replay(FuzzTarget::Uart);
}

fn replay(target: FuzzTarget) {
    // peak計測はprocess全体を見るため、replay中は直列化する。
    let _guard = SERIAL
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("replay lock must be free");
    let dir: PathBuf = [env!("CARGO_MANIFEST_DIR"), "corpus", target.name()]
        .iter()
        .collect();
    let mut entries: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap_or_else(|_| panic!("corpus {} must exist", dir.display()))
        .map(|entry| entry.expect("corpus entry must list").path())
        .filter(|path| path.is_file())
        .collect();
    entries.sort();
    assert!(
        !entries.is_empty(),
        "corpus {} must not be empty",
        dir.display()
    );
    for path in entries {
        let bytes = fs::read(&path).expect("corpus file must read");
        match run_input(target, &bytes, Duration::from_secs(10), DEFAULT_ALLOC_CAP) {
            InputOutcome::Pass { .. } => {}
            other => panic!("corpus file {} produced {other:?}", path.display()),
        }
    }
}
