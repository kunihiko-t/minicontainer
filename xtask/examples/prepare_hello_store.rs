//! Quick-start helper: import the deterministic hello bundle into a store.
//!
//! Usage:
//!
//! ```sh
//! cargo run -p xtask --example prepare_hello_store -- /tmp/mc-store
//! ```
//!
//! Afterwards `minictr run --store /tmp/mc-store --kernel <kernel> hello`
//! prints `hello stdout`, `hello stderr`, and exits with code 42.

use std::path::PathBuf;

fn main() {
    let root: PathBuf = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            eprintln!("usage: prepare_hello_store <absolute-store-dir>");
            std::process::exit(2);
        });
    match xtask::runtime::import_hello_store(&root) {
        Ok(digest) => {
            println!(
                "imported hello (sha256:{}) into {}",
                hex_digest(&digest),
                root.display()
            );
        }
        Err(error) => {
            eprintln!("prepare_hello_store: {error}");
            std::process::exit(1);
        }
    }
}

fn hex_digest(digest: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}
