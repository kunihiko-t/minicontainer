use std::{
    fs,
    path::PathBuf,
    process::{self, Command},
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let id = NEXT_FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("minicontainer-locked-alias-{}-{id}", process::id()));
        fs::create_dir_all(root.join(".cargo"))
            .expect("must create fixture Cargo configuration directory");
        fs::create_dir_all(root.join("xtask/src"))
            .expect("must create fixture xtask source directory");
        fs::write(
            root.join(".cargo/config.toml"),
            include_str!("../../.cargo/config.toml"),
        )
        .expect("must copy the repository Cargo alias");
        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"xtask\"]\nresolver = \"2\"\n",
        )
        .expect("must write fixture workspace manifest");
        fs::write(
            root.join("xtask/Cargo.toml"),
            "[package]\nname = \"xtask\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
        )
        .expect("must write fixture xtask manifest");
        fs::write(
            root.join("xtask/src/main.rs"),
            r#"fn main() {
    let marker = std::env::var_os("MINICONTAINER_LOCK_ALIAS_MARKER")
        .expect("the integration test must supply a marker path");
    std::fs::write(marker, b"invoked").expect("must write invocation marker");
}
"#,
        )
        .expect("must write fixture xtask source");
        Self { root }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn xtask_alias_rejects_a_missing_lock_before_invoking_or_creating_it() {
    let fixture = Fixture::new();
    let lock = fixture.root.join("Cargo.lock");
    let marker = fixture.root.join("xtask-invoked");
    assert!(!lock.exists());

    let output = Command::new("cargo")
        .args(["xtask", "check"])
        .current_dir(&fixture.root)
        .env("MINICONTAINER_LOCK_ALIAS_MARKER", &marker)
        .output()
        .expect("Cargo must execute the repository xtask alias");

    assert!(
        !output.status.success(),
        "an absent lockfile must be rejected"
    );
    assert!(
        !lock.exists(),
        "the rejected invocation must not create Cargo.lock"
    );
    assert!(!marker.exists(), "the xtask binary must not be invoked");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--locked"),
        "Cargo must diagnose the locked failure: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
