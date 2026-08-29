use std::{fmt, io, path::Path, process::Command};

/// A Cargo command planned for an explicit workspace root.
#[derive(Debug)]
pub(crate) struct CargoInvocation<'a> {
    workspace_root: &'a Path,
    arguments: &'a [&'a str],
}

impl<'a> CargoInvocation<'a> {
    pub(crate) fn new(workspace_root: &'a Path, arguments: &'a [&'a str]) -> Self {
        Self {
            workspace_root,
            arguments,
        }
    }

    #[cfg(test)]
    pub(crate) fn current_dir(&self) -> &Path {
        self.workspace_root
    }

    #[cfg(test)]
    pub(crate) fn arguments(&self) -> &[&str] {
        self.arguments
    }

    pub(crate) fn command_line(&self) -> String {
        format!("cargo {}", self.arguments.join(" "))
    }

    pub(crate) fn run(&self) -> Result<String, CargoError> {
        let command_line = self.command_line();
        let output = Command::new("cargo")
            .current_dir(self.workspace_root)
            .args(self.arguments)
            .output()
            .map_err(|error| CargoError::Spawn {
                command: command_line.clone(),
                error: match error.kind() {
                    io::ErrorKind::NotFound => "cargo is not installed".to_owned(),
                    _ => error.to_string(),
                },
            })?;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        if !output.status.success() {
            return Err(CargoError::Failed {
                command: command_line,
                status: output.status.code(),
                stdout,
                stderr,
            });
        }

        Ok(format!("{stdout}{stderr}"))
    }
}

/// A failed Cargo process with its reproducible invocation and output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CargoError {
    Spawn {
        command: String,
        error: String,
    },
    Failed {
        command: String,
        status: Option<i32>,
        stdout: String,
        stderr: String,
    },
}

impl fmt::Display for CargoError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn { command, error } => {
                write!(formatter, "could not start {command}: {error}")
            }
            Self::Failed {
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
        }
    }
}

impl std::error::Error for CargoError {}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;

    #[test]
    fn invocation_retains_the_workspace_root_arguments_and_command_line() {
        let root = PathBuf::from("/controlled/repository");
        let invocation = CargoInvocation::new(&root, &["build", "--workspace", "--locked"]);

        assert_eq!(
            invocation.current_dir(),
            Path::new("/controlled/repository")
        );
        assert_eq!(
            invocation.arguments(),
            &["build", "--workspace", "--locked"]
        );
        assert_eq!(
            invocation.command_line(),
            "cargo build --workspace --locked"
        );
    }

    #[test]
    fn failed_operation_preserves_command_status_and_diagnostics() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("xtask must be in a workspace");
        let error = CargoInvocation::new(root, &["minicontainer-invalid-operation"])
            .run()
            .expect_err("unknown Cargo operation must fail");

        match error {
            CargoError::Failed {
                command,
                status,
                stdout,
                stderr,
            } => {
                assert_eq!(command, "cargo minicontainer-invalid-operation");
                assert!(status.is_some_and(|status| status != 0));
                assert!(stdout.is_empty(), "unexpected Cargo stdout: {stdout:?}");
                assert!(
                    !stderr.trim().is_empty(),
                    "failed Cargo command must retain a diagnostic"
                );
            }
            other => panic!("expected failed Cargo command, got {other:?}"),
        }
    }

    #[test]
    fn diagnostics_keep_stdout_and_stderr_distinct() {
        let error = CargoError::Failed {
            command: "cargo fixture".to_owned(),
            status: Some(7),
            stdout: "stdout diagnostic\n".to_owned(),
            stderr: "stderr diagnostic\n".to_owned(),
        };

        let display = error.to_string();
        assert!(display.contains("status 7"));
        assert!(display.contains("stdout:\nstdout diagnostic"));
        assert!(display.contains("stderr:\nstderr diagnostic"));
    }
}
