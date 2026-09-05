use std::{fmt, io, io::Write, process::Command};

use crate::{OutputError, XtaskError};

const RISCV_TARGET: &str = "riscv64gc-unknown-none-elf";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SetupCommand {
    program: &'static str,
    arguments: &'static [&'static str],
}

impl SetupCommand {
    #[cfg(test)]
    fn program(self) -> &'static str {
        self.program
    }

    #[cfg(test)]
    fn arguments(self) -> &'static [&'static str] {
        self.arguments
    }

    fn command_line(self) -> String {
        format!("{} {}", self.program, self.arguments.join(" "))
    }
}

fn setup_commands() -> [SetupCommand; 4] {
    [
        SetupCommand {
            program: "rustc",
            arguments: &["--version"],
        },
        SetupCommand {
            program: "rustup",
            arguments: &["target", "list", "--installed"],
        },
        SetupCommand {
            program: "qemu-system-riscv64",
            arguments: &["--version"],
        },
        SetupCommand {
            program: "git",
            arguments: &["--version"],
        },
    ]
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostPlatform {
    Macos,
    Ubuntu,
    Other,
}

impl HostPlatform {
    fn current() -> Self {
        #[cfg(target_os = "macos")]
        return Self::Macos;

        #[cfg(target_os = "linux")]
        return Self::Ubuntu;

        #[allow(unreachable_code)]
        Self::Other
    }
}

/// A parsed three-component tool version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// A setup diagnostic failure with an actionable correction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolError {
    CommandUnavailable(&'static str),
    CommandFailed {
        command: String,
        status: Option<i32>,
        stdout: String,
        stderr: String,
    },
    MissingRustTarget,
    MalformedRustcVersion,
    UnsupportedRustcVersion(String),
    MalformedQemuVersion,
    UnsupportedQemuVersion(Version),
    MalformedGitVersion,
}

impl fmt::Display for ToolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CommandUnavailable(program) => {
                formatter.write_str(&missing_tool_message(program, HostPlatform::current()))
            }
            Self::CommandFailed {
                command,
                status,
                stdout,
                stderr,
            } => {
                write!(
                    formatter,
                    "{command} failed with status {}",
                    status
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "unknown".to_owned())
                )?;
                if !stdout.is_empty() {
                    write!(formatter, "\nstdout:\n{}", stdout.trim_end())?;
                }
                if !stderr.is_empty() {
                    write!(formatter, "\nstderr:\n{}", stderr.trim_end())?;
                }
                Ok(())
            }
            Self::MissingRustTarget => formatter.write_str(&missing_target_message()),
            Self::MalformedRustcVersion => formatter.write_str(
                "could not parse rustc version; expected rustc X.Y.Z with an optional build description",
            ),
            Self::UnsupportedRustcVersion(version) => write!(
                formatter,
                "Rust {version} is not supported; exact Rust 1.98.0 stable is required. Install it with: rustup toolchain install 1.98.0"
            ),
            Self::MalformedQemuVersion => {
                formatter.write_str("could not parse QEMU emulator version X.Y.Z")
            }
            Self::UnsupportedQemuVersion(version) => write!(
                formatter,
                "QEMU {version} is too old; QEMU 8.2.0 or newer is required. {}",
                qemu_install_message(HostPlatform::current())
            ),
            Self::MalformedGitVersion => {
                formatter.write_str("could not parse git version X.Y.Z")
            }
        }
    }
}

impl std::error::Error for ToolError {}

/// Runs all read-only setup diagnostics and reports each successful result.
pub fn check_setup() -> Result<(), XtaskError> {
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    run_setup(&mut output, run_command)
}

/// Checks that an external program needed by the E2E gate runs and reports
/// its version line. Returns the captured stdout.
pub fn require_program(
    program: &'static str,
    arguments: &'static [&'static str],
) -> Result<String, ToolError> {
    run_command(SetupCommand { program, arguments })
}

fn run_setup(
    output: &mut impl Write,
    mut runner: impl FnMut(SetupCommand) -> Result<String, ToolError>,
) -> Result<(), XtaskError> {
    for command in setup_commands() {
        let command_output = runner(command).map_err(XtaskError::Tool)?;
        match command.program {
            "rustc" => {
                parse_rustc_version(&command_output).map_err(XtaskError::Tool)?;
                let line = command_output
                    .lines()
                    .next()
                    .ok_or(ToolError::MalformedRustcVersion)
                    .map_err(XtaskError::Tool)?;
                writeln!(output, "[ok] Rust: {line}").map_err(|error| {
                    XtaskError::Output(OutputError::new("setup Rust result", error))
                })?;
            }
            "rustup" => {
                check_installed_target(&command_output).map_err(XtaskError::Tool)?;
                writeln!(output, "[ok] Rust target: {RISCV_TARGET}").map_err(|error| {
                    XtaskError::Output(OutputError::new("setup Rust target result", error))
                })?;
            }
            "qemu-system-riscv64" => {
                let version = parse_qemu_version(&command_output).map_err(XtaskError::Tool)?;
                writeln!(output, "[ok] QEMU: {version}").map_err(|error| {
                    XtaskError::Output(OutputError::new("setup QEMU result", error))
                })?;
            }
            "git" => {
                parse_git_version(&command_output).map_err(XtaskError::Tool)?;
                let line = command_output
                    .lines()
                    .next()
                    .ok_or(ToolError::MalformedGitVersion)
                    .map_err(XtaskError::Tool)?;
                writeln!(output, "[ok] Git: {line}").map_err(|error| {
                    XtaskError::Output(OutputError::new("setup Git result", error))
                })?;
            }
            _ => unreachable!("setup command list is closed"),
        }
    }
    Ok(())
}

fn run_command(command: SetupCommand) -> Result<String, ToolError> {
    let output = Command::new(command.program)
        .args(command.arguments)
        .output()
        .map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => ToolError::CommandUnavailable(command.program),
            _ => ToolError::CommandFailed {
                command: command.command_line(),
                status: None,
                stdout: String::new(),
                stderr: error.to_string(),
            },
        })?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if !output.status.success() {
        return Err(ToolError::CommandFailed {
            command: command.command_line(),
            status: output.status.code(),
            stdout,
            stderr,
        });
    }
    Ok(stdout)
}

/// Parses and enforces the exact pinned stable Rust compiler.
pub fn parse_rustc_version(output: &str) -> Result<Version, ToolError> {
    let line = output
        .lines()
        .next()
        .ok_or(ToolError::MalformedRustcVersion)?;
    let mut fields = line.split_whitespace();
    if fields.next() != Some("rustc") {
        return Err(ToolError::MalformedRustcVersion);
    }
    let token = fields.next().ok_or(ToolError::MalformedRustcVersion)?;
    let (numeric, has_suffix) = match token.split_once('-') {
        Some((_numeric, "")) => return Err(ToolError::MalformedRustcVersion),
        Some((numeric, suffix))
            if suffix
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '.') =>
        {
            (numeric, true)
        }
        Some(_) => return Err(ToolError::MalformedRustcVersion),
        None => (token, false),
    };
    let version = parse_numeric_version(numeric).ok_or(ToolError::MalformedRustcVersion)?;
    if has_suffix || version != Version::new(1, 98, 0) {
        return Err(ToolError::UnsupportedRustcVersion(token.to_owned()));
    }
    Ok(version)
}

fn check_installed_target(output: &str) -> Result<(), ToolError> {
    output
        .lines()
        .any(|target| target == RISCV_TARGET)
        .then_some(())
        .ok_or(ToolError::MissingRustTarget)
}

/// Parses and enforces the QEMU compatibility floor.
pub fn parse_qemu_version(output: &str) -> Result<Version, ToolError> {
    let token = output
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("QEMU emulator version "))
        .and_then(|version| version.split_whitespace().next())
        .ok_or(ToolError::MalformedQemuVersion)?;
    let version = parse_numeric_version(token).ok_or(ToolError::MalformedQemuVersion)?;
    if version < Version::new(8, 2, 0) {
        return Err(ToolError::UnsupportedQemuVersion(version));
    }
    Ok(version)
}

/// Parses the Git version diagnostic.
pub fn parse_git_version(output: &str) -> Result<Version, ToolError> {
    let token = output
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("git version "))
        .and_then(|version| version.split_whitespace().next())
        .ok_or(ToolError::MalformedGitVersion)?;
    parse_numeric_version(token).ok_or(ToolError::MalformedGitVersion)
}

fn parse_numeric_version(token: &str) -> Option<Version> {
    let mut components = token.split('.');
    let major = components.next()?.parse().ok()?;
    let minor = components.next()?.parse().ok()?;
    let patch = components.next()?.parse().ok()?;
    if components.next().is_some() {
        return None;
    }
    Some(Version::new(major, minor, patch))
}

fn missing_tool_message(program: &str, platform: HostPlatform) -> String {
    let correction = match (program, platform) {
        ("rustc", _) => "rustup toolchain install 1.98.0",
        ("rustup", HostPlatform::Macos) => "brew install rustup",
        ("rustup", HostPlatform::Ubuntu) => "sudo apt-get install rustup",
        ("qemu-system-riscv64", HostPlatform::Macos) => "brew install qemu",
        ("qemu-system-riscv64", HostPlatform::Ubuntu) => "sudo apt-get install qemu-system-misc",
        ("git", HostPlatform::Macos) => "brew install git",
        ("git", HostPlatform::Ubuntu) => "sudo apt-get install git",
        ("rustup", HostPlatform::Other) => "install rustup from https://rustup.rs",
        ("qemu-system-riscv64", HostPlatform::Other) => {
            "install a QEMU package that provides qemu-system-riscv64"
        }
        ("git", HostPlatform::Other) => "install Git from https://git-scm.com",
        (_, _) => "install the missing program",
    };
    format!("{program} is not installed. Correct it with: {correction}")
}

fn qemu_install_message(platform: HostPlatform) -> String {
    match platform {
        HostPlatform::Macos => "Upgrade it with: brew upgrade qemu".to_owned(),
        HostPlatform::Ubuntu => {
            "Upgrade it with: sudo apt-get update && sudo apt-get install qemu-system-misc"
                .to_owned()
        }
        HostPlatform::Other => "Upgrade the installed QEMU package.".to_owned(),
    }
}

fn missing_target_message() -> String {
    format!(
        "Rust target {RISCV_TARGET} is not installed. Correct it with: rustup target add {RISCV_TARGET} --toolchain 1.98.0"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FailingWriter;

    impl io::Write for FailingWriter {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "fixture writer failure",
            ))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn setup_plans_the_exact_four_read_only_diagnostics() {
        let commands = setup_commands();
        let planned = commands
            .iter()
            .map(|command| (command.program(), command.arguments()))
            .collect::<Vec<_>>();

        assert_eq!(
            planned,
            vec![
                ("rustc", &["--version"][..]),
                ("rustup", &["target", "list", "--installed"][..]),
                ("qemu-system-riscv64", &["--version"][..]),
                ("git", &["--version"][..]),
            ]
        );
    }

    #[test]
    fn setup_reports_every_success_in_command_order() {
        let mut output = Vec::new();
        let mut invoked = Vec::new();
        let fixture = [
            "rustc 1.98.0 (88d9e12ae 2026-08-18)\n",
            "aarch64-apple-darwin\nriscv64gc-unknown-none-elf\n",
            "QEMU emulator version 11.1.0\nCopyright\n",
            "git version 2.45.0\n",
        ];

        run_setup(&mut output, |command| {
            let index = invoked.len();
            invoked.push(command.program());
            Ok(fixture[index].to_owned())
        })
        .expect("valid diagnostic output must pass");

        assert_eq!(
            invoked,
            vec!["rustc", "rustup", "qemu-system-riscv64", "git"]
        );
        assert_eq!(
            String::from_utf8(output).expect("setup output is UTF-8"),
            "[ok] Rust: rustc 1.98.0 (88d9e12ae 2026-08-18)\n\
[ok] Rust target: riscv64gc-unknown-none-elf\n\
[ok] QEMU: 11.1.0\n\
[ok] Git: git version 2.45.0\n"
        );
    }

    #[test]
    fn setup_stops_after_the_first_result_cannot_be_written() {
        let mut output = FailingWriter;
        let mut invoked = Vec::new();

        let error = run_setup(&mut output, |command| {
            invoked.push(command.program());
            Ok("rustc 1.98.0 (fixture)\n".to_owned())
        })
        .expect_err("failed setup output must stop diagnostics");

        assert_eq!(invoked, vec!["rustc"]);
        let XtaskError::Output(error) = error else {
            panic!("expected an output failure");
        };
        assert_eq!(
            error.to_string(),
            "could not write setup Rust result: fixture writer failure"
        );
        assert_eq!(
            std::error::Error::source(&error)
                .expect("output error must retain its I/O source")
                .to_string(),
            "fixture writer failure"
        );
    }

    #[test]
    fn accepts_only_the_exact_pinned_stable_rust_version() {
        assert_eq!(
            parse_rustc_version("rustc 1.98.0 (88d9e12ae 2026-08-18)\n"),
            Ok(Version::new(1, 98, 0))
        );
        for (output, unsupported) in [
            ("rustc 1.97.0 (old)\n", "1.97.0"),
            ("rustc 1.98.1 (future)\n", "1.98.1"),
            ("rustc 1.98.0-nightly (nightly)\n", "1.98.0-nightly"),
        ] {
            assert_eq!(
                parse_rustc_version(output),
                Err(ToolError::UnsupportedRustcVersion(unsupported.to_owned()))
            );
        }
    }

    #[test]
    fn rejects_malformed_rustc_output_without_panicking() {
        for output in [
            "",
            "cargo 1.98.0\n",
            "rustc\n",
            "rustc 1.98\n",
            "rustc 1.x.0\n",
            "rustc 1.98.0.1\n",
            "rustc 1.98.0-\n",
        ] {
            assert_eq!(
                parse_rustc_version(output),
                Err(ToolError::MalformedRustcVersion),
                "unexpectedly accepted {output:?}"
            );
        }
    }

    #[test]
    fn installed_target_requires_an_exact_output_line() {
        assert_eq!(
            check_installed_target("aarch64-apple-darwin\nriscv64gc-unknown-none-elf\n"),
            Ok(())
        );
        assert_eq!(
            check_installed_target("prefix-riscv64gc-unknown-none-elf-suffix\n"),
            Err(ToolError::MissingRustTarget)
        );
    }

    #[test]
    fn qemu_parser_accepts_the_floor_newer_and_packaged_versions() {
        for (output, version) in [
            ("QEMU emulator version 8.2.0\n", Version::new(8, 2, 0)),
            (
                "QEMU emulator version 8.2.2 (Debian 1:8.2.2+ds-0ubuntu1.6)\n",
                Version::new(8, 2, 2),
            ),
            ("QEMU emulator version 11.1.0\n", Version::new(11, 1, 0)),
        ] {
            assert_eq!(parse_qemu_version(output), Ok(version));
        }
        assert_eq!(
            parse_qemu_version("QEMU emulator version 8.1.9\n"),
            Err(ToolError::UnsupportedQemuVersion(Version::new(8, 1, 9)))
        );
    }

    #[test]
    fn rejects_malformed_qemu_output_without_panicking() {
        for output in [
            "",
            "QEMU version 9.2.3\n",
            "QEMU emulator version 9.2\n",
            "QEMU emulator version 9.x.3\n",
            "QEMU emulator version 9.2.3.1\n",
        ] {
            assert_eq!(
                parse_qemu_version(output),
                Err(ToolError::MalformedQemuVersion),
                "unexpectedly accepted {output:?}"
            );
        }
    }

    #[test]
    fn parses_plain_apple_and_packaged_git_versions() {
        for (output, version) in [
            ("git version 2.45.0\n", Version::new(2, 45, 0)),
            (
                "git version 2.39.5 (Apple Git-154)\n",
                Version::new(2, 39, 5),
            ),
        ] {
            assert_eq!(parse_git_version(output), Ok(version));
        }
        for output in [
            "",
            "Git 2.45.0\n",
            "git version 2.45\n",
            "git version x.y.z\n",
        ] {
            assert_eq!(
                parse_git_version(output),
                Err(ToolError::MalformedGitVersion),
                "unexpectedly accepted {output:?}"
            );
        }
    }

    #[test]
    fn missing_tools_have_actionable_macos_and_ubuntu_commands() {
        for (program, macos, ubuntu) in [
            (
                "rustc",
                "rustup toolchain install 1.98.0",
                "rustup toolchain install 1.98.0",
            ),
            (
                "rustup",
                "brew install rustup",
                "sudo apt-get install rustup",
            ),
            (
                "qemu-system-riscv64",
                "brew install qemu",
                "sudo apt-get install qemu-system-misc",
            ),
            ("git", "brew install git", "sudo apt-get install git"),
        ] {
            assert!(missing_tool_message(program, HostPlatform::Macos).contains(macos));
            assert!(missing_tool_message(program, HostPlatform::Ubuntu).contains(ubuntu));
        }
        assert!(
            missing_target_message()
                .contains("rustup target add riscv64gc-unknown-none-elf --toolchain 1.98.0")
        );
    }

    #[test]
    fn failed_tool_preserves_status_stdout_and_stderr() {
        let error = ToolError::CommandFailed {
            command: "git --version".to_owned(),
            status: Some(9),
            stdout: "stdout diagnostic\n".to_owned(),
            stderr: "stderr diagnostic\n".to_owned(),
        };

        let display = error.to_string();
        assert!(display.contains("git --version failed with status 9"));
        assert!(display.contains("stdout:\nstdout diagnostic"));
        assert!(display.contains("stderr:\nstderr diagnostic"));
    }
}
