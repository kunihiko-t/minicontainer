#[cfg(unix)]
#[test]
fn non_utf8_argument_prints_usage_and_exits_2() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::process::Command;

    let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .arg(OsString::from_vec(vec![0xff]))
        .output()
        .expect("xtask process starts");

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).expect("diagnostic is UTF-8"),
        "xtask argument is not valid UTF-8\n\nusage: cargo xtask <setup|check>\n"
    );
}

#[test]
fn utf8_cli_errors_print_usage_and_exit_2() {
    use std::process::Command;

    for (arguments, diagnostic) in [
        (Vec::<&str>::new(), "missing xtask command"),
        (vec!["run"], "unknown xtask command: run"),
        (vec!["check", "extra"], "unexpected xtask argument: extra"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
            .args(arguments)
            .output()
            .expect("xtask process starts");

        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert_eq!(
            String::from_utf8(output.stderr).expect("diagnostic is UTF-8"),
            format!("{diagnostic}\n\nusage: cargo xtask <setup|check>\n")
        );
    }
}

#[cfg(unix)]
#[test]
fn operational_failure_prints_the_diagnostic_to_stderr_and_exits_1() {
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        process::{self, Command},
        sync::atomic::{AtomicU64, Ordering},
    };

    static NEXT_FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

    let id = NEXT_FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
    let fixture =
        std::env::temp_dir().join(format!("minicontainer-xtask-cli-{}-{id}", process::id()));
    fs::create_dir_all(&fixture).expect("must create executable fixture directory");
    let cargo = fixture.join("cargo");
    fs::write(
        &cargo,
        "#!/bin/sh\nprintf 'fixture cargo failure\\n' >&2\nexit 7\n",
    )
    .expect("must write fake Cargo executable");
    let mut permissions = fs::metadata(&cargo)
        .expect("must inspect fake Cargo executable")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&cargo, permissions).expect("must make fake Cargo executable runnable");

    let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .arg("check")
        .env("PATH", &fixture)
        .output()
        .expect("xtask process starts");
    fs::remove_dir_all(&fixture).expect("must remove executable fixture directory");

    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout).expect("progress output is UTF-8");
    assert!(stdout.contains("[1/10] cargo fmt --all -- --check"));
    assert!(stdout.contains("phase 1/10 failed (elapsed:"));
    assert!(stdout.contains("summary: FAILED at phase 1/10; 0 passed, 1 failed"));
    assert_eq!(
        String::from_utf8(output.stderr).expect("diagnostic is UTF-8"),
        "cargo fmt --all -- --check failed with status 7\n\
         stderr:\n\
         fixture cargo failure\n"
    );
}
