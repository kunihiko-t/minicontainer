//! Development commands for the MiniContainer workspace.

pub mod cargo;
pub mod cli;
pub mod docs;
pub mod publication;
pub mod runtime;
pub mod tools;

use std::io::{self, Write};
use std::path::Path;
use std::time::Instant;
use std::{fmt, path::PathBuf};

pub use cli::{CliError, Command};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Format,
    DocsLinks,
    PublicationFiles,
    ClippyBundle,
    ClippyProtocol,
    ClippyRuntime,
    ClippyMinictr,
    ClippyXtask,
    BundleTests,
    ProtocolTests,
    RuntimeTests,
    MinictrTests,
    XtaskTests,
    LockedBuild,
    EndToEnd,
}

impl Phase {
    fn cargo_args(self) -> Option<&'static [&'static str]> {
        match self {
            Self::Format => Some(&["fmt", "--all", "--", "--check"]),
            Self::DocsLinks | Self::PublicationFiles => None,
            Self::ClippyBundle => Some(&[
                "clippy",
                "-p",
                "minicontainer-bundle",
                "--all-targets",
                "--locked",
                "--",
                "-D",
                "warnings",
            ]),
            Self::ClippyProtocol => Some(&[
                "clippy",
                "-p",
                "minicontainer-protocol",
                "--all-targets",
                "--locked",
                "--",
                "-D",
                "warnings",
            ]),
            Self::ClippyRuntime => Some(&[
                "clippy",
                "-p",
                "minicontainer-runtime",
                "--all-targets",
                "--locked",
                "--",
                "-D",
                "warnings",
            ]),
            Self::ClippyMinictr => Some(&[
                "clippy",
                "-p",
                "minictr",
                "--all-targets",
                "--locked",
                "--",
                "-D",
                "warnings",
            ]),
            Self::ClippyXtask => Some(&[
                "clippy",
                "-p",
                "xtask",
                "--all-targets",
                "--locked",
                "--",
                "-D",
                "warnings",
            ]),
            Self::BundleTests => Some(&["test", "-p", "minicontainer-bundle", "--locked"]),
            Self::ProtocolTests => Some(&["test", "-p", "minicontainer-protocol", "--locked"]),
            Self::RuntimeTests => Some(&["test", "-p", "minicontainer-runtime", "--locked"]),
            Self::MinictrTests => Some(&["test", "-p", "minictr", "--locked"]),
            Self::XtaskTests => Some(&["test", "-p", "xtask", "--locked"]),
            Self::LockedBuild => Some(&["build", "--workspace", "--locked"]),
            Self::EndToEnd => None,
        }
    }

    fn command(self) -> String {
        self.cargo_args().map_or_else(
            || match self {
                Self::DocsLinks => "check local Markdown links".to_owned(),
                Self::PublicationFiles => "check publication policy".to_owned(),
                Self::EndToEnd => "run real QEMU end-to-end verification".to_owned(),
                _ => unreachable!("Cargo phases returned above"),
            },
            |args| format!("cargo {}", args.join(" ")),
        )
    }

    fn cargo_invocation(self, workspace_root: &Path) -> Option<cargo::CargoInvocation<'_>> {
        self.cargo_args()
            .map(|arguments| cargo::CargoInvocation::new(workspace_root, arguments))
    }
}

fn check_phases() -> Vec<Phase> {
    vec![
        Phase::Format,
        Phase::DocsLinks,
        Phase::PublicationFiles,
        Phase::ClippyBundle,
        Phase::ClippyProtocol,
        Phase::ClippyRuntime,
        Phase::ClippyMinictr,
        Phase::ClippyXtask,
        Phase::BundleTests,
        Phase::ProtocolTests,
        Phase::RuntimeTests,
        Phase::MinictrTests,
        Phase::XtaskTests,
        Phase::LockedBuild,
        Phase::EndToEnd,
    ]
}

/// A failure while reporting harness progress.
#[derive(Debug)]
pub struct OutputError {
    context: &'static str,
    error: io::Error,
}

impl OutputError {
    fn new(context: &'static str, error: io::Error) -> Self {
        Self { context, error }
    }
}

impl fmt::Display for OutputError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "could not write {}: {}",
            self.context, self.error
        )
    }
}

impl std::error::Error for OutputError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

#[derive(Debug)]
enum PhaseRunError<E> {
    Action(E),
    Output(OutputError),
}

impl<E: fmt::Display> fmt::Display for PhaseRunError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Action(error) => error.fmt(formatter),
            Self::Output(error) => error.fmt(formatter),
        }
    }
}

fn run_phases<E>(
    phases: &[Phase],
    output: &mut impl Write,
    mut action: impl FnMut(Phase) -> Result<String, E>,
) -> Result<(), PhaseRunError<E>> {
    let all_started = Instant::now();
    for (index, phase) in phases.iter().copied().enumerate() {
        let number = index + 1;
        writeln!(output, "[{number}/{}] {}", phases.len(), phase.command()).map_err(|error| {
            PhaseRunError::Output(OutputError::new("phase start output", error))
        })?;
        let started = Instant::now();
        match action(phase) {
            Ok(transcript) => {
                if !transcript.is_empty() {
                    write!(output, "{transcript}").map_err(|error| {
                        PhaseRunError::Output(OutputError::new("phase transcript", error))
                    })?;
                    if !transcript.ends_with('\n') {
                        writeln!(output).map_err(|error| {
                            PhaseRunError::Output(OutputError::new(
                                "phase transcript newline",
                                error,
                            ))
                        })?;
                    }
                }
                writeln!(
                    output,
                    "phase {number}/{} passed (elapsed: {:.3}s)",
                    phases.len(),
                    started.elapsed().as_secs_f64()
                )
                .map_err(|error| {
                    PhaseRunError::Output(OutputError::new("phase success output", error))
                })?;
            }
            Err(error) => {
                writeln!(
                    output,
                    "phase {number}/{} failed (elapsed: {:.3}s)",
                    phases.len(),
                    started.elapsed().as_secs_f64()
                )
                .map_err(|write_error| {
                    PhaseRunError::Output(OutputError::new("phase failure output", write_error))
                })?;
                writeln!(
                    output,
                    "summary: FAILED at phase {number}/{}; {index} passed, 1 failed (elapsed: {:.3}s)",
                    phases.len(),
                    all_started.elapsed().as_secs_f64()
                )
                .map_err(|write_error| {
                    PhaseRunError::Output(OutputError::new(
                        "failure summary output",
                        write_error,
                    ))
                })?;
                return Err(PhaseRunError::Action(error));
            }
        }
    }
    writeln!(
        output,
        "summary: PASSED all {} phases (elapsed: {:.3}s)",
        phases.len(),
        all_started.elapsed().as_secs_f64()
    )
    .map_err(|error| PhaseRunError::Output(OutputError::new("success summary output", error)))?;
    Ok(())
}

/// A unified harness failure.
#[derive(Debug)]
pub enum XtaskError {
    Cargo(cargo::CargoError),
    Docs(docs::DocsError),
    Publication(publication::PublicationError),
    Runtime(runtime::E2EError),
    Tool(tools::ToolError),
    Output(OutputError),
}

impl fmt::Display for XtaskError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cargo(error) => error.fmt(formatter),
            Self::Docs(error) => error.fmt(formatter),
            Self::Publication(error) => error.fmt(formatter),
            Self::Runtime(error) => error.fmt(formatter),
            Self::Tool(error) => error.fmt(formatter),
            Self::Output(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for XtaskError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Cargo(error) => Some(error),
            Self::Docs(error) => Some(error),
            Self::Publication(error) => Some(error),
            Self::Runtime(error) => Some(error),
            Self::Tool(error) => Some(error),
            Self::Output(error) => Some(error),
        }
    }
}

impl From<cargo::CargoError> for XtaskError {
    fn from(error: cargo::CargoError) -> Self {
        Self::Cargo(error)
    }
}

impl From<docs::DocsError> for XtaskError {
    fn from(error: docs::DocsError) -> Self {
        Self::Docs(error)
    }
}

impl From<publication::PublicationError> for XtaskError {
    fn from(error: publication::PublicationError) -> Self {
        Self::Publication(error)
    }
}

impl From<runtime::E2EError> for XtaskError {
    fn from(error: runtime::E2EError) -> Self {
        Self::Runtime(error)
    }
}

impl From<tools::ToolError> for XtaskError {
    fn from(error: tools::ToolError) -> Self {
        Self::Tool(error)
    }
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask must be in the workspace root")
        .to_owned()
}

fn execute_phase(workspace: &Path, phase: Phase) -> Result<String, XtaskError> {
    if let Some(invocation) = phase.cargo_invocation(workspace) {
        return invocation.run().map_err(XtaskError::Cargo);
    }
    match phase {
        Phase::DocsLinks => docs::check_local_links(workspace)?,
        Phase::PublicationFiles => publication::check(workspace)?,
        Phase::EndToEnd => return runtime::run_e2e(workspace).map_err(XtaskError::Runtime),
        _ => unreachable!("Cargo phases returned above"),
    }
    Ok(String::new())
}

fn run_check(workspace: &Path) -> Result<(), XtaskError> {
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    run_phases(&check_phases(), &mut output, |phase| {
        execute_phase(workspace, phase)
    })
    .map_err(phase_run_error)
}

fn phase_run_error(error: PhaseRunError<XtaskError>) -> XtaskError {
    match error {
        PhaseRunError::Action(error) => error,
        PhaseRunError::Output(error) => XtaskError::Output(error),
    }
}

/// Runs one parsed public command.
pub fn run(command: Command) -> Result<(), XtaskError> {
    match command {
        Command::Setup => tools::check_setup(),
        Command::Check => run_check(&workspace_root()),
    }
}

#[cfg(test)]
mod tests {
    use std::{io, path::Path};

    use super::*;

    #[test]
    fn check_runs_reproducible_release_gate_in_order() {
        assert_eq!(
            check_phases(),
            vec![
                Phase::Format,
                Phase::DocsLinks,
                Phase::PublicationFiles,
                Phase::ClippyBundle,
                Phase::ClippyProtocol,
                Phase::ClippyRuntime,
                Phase::ClippyMinictr,
                Phase::ClippyXtask,
                Phase::BundleTests,
                Phase::ProtocolTests,
                Phase::RuntimeTests,
                Phase::MinictrTests,
                Phase::XtaskTests,
                Phase::LockedBuild,
                Phase::EndToEnd,
            ]
        );
        assert_eq!(check_phases().len(), 15);
    }

    #[test]
    fn phase_runner_stops_at_first_failure_and_reports_the_gate() {
        let phases = [Phase::Format, Phase::ClippyBundle, Phase::LockedBuild];
        let mut invoked = Vec::new();
        let mut output = Vec::new();

        let result = run_phases(&phases, &mut output, |phase| {
            invoked.push(phase);
            if phase == Phase::ClippyBundle {
                Err("clippy failed")
            } else {
                Ok(String::new())
            }
        });

        assert!(matches!(
            result,
            Err(PhaseRunError::Action("clippy failed"))
        ));
        assert_eq!(invoked, vec![Phase::Format, Phase::ClippyBundle]);
        let output = String::from_utf8(output).expect("phase output must be UTF-8");
        assert!(output.contains("[1/3] cargo fmt --all -- --check"));
        assert!(output.contains(
            "[2/3] cargo clippy -p minicontainer-bundle --all-targets --locked -- -D warnings"
        ));
        assert!(output.contains("phase 1/3 passed (elapsed:"));
        assert!(output.contains("phase 2/3 failed (elapsed:"));
        assert!(output.contains("summary: FAILED at phase 2/3; 1 passed, 1 failed (elapsed:"));
    }

    #[test]
    fn phase_runner_propagates_transcripts_and_reports_success() {
        let phases = [Phase::DocsLinks, Phase::PublicationFiles];
        let mut output = Vec::new();

        run_phases(&phases, &mut output, |phase| {
            Ok::<_, ()>(match phase {
                Phase::DocsLinks => "links checked\n".to_owned(),
                Phase::PublicationFiles => "publication checked".to_owned(),
                _ => unreachable!("fixture contains only docs phases"),
            })
        })
        .expect("all successful actions must pass");

        let output = String::from_utf8(output).expect("phase output must be UTF-8");
        assert!(output.contains("[1/2] check local Markdown links"));
        assert!(output.contains("[2/2] check publication policy"));
        assert!(output.contains("links checked\nphase 1/2 passed"));
        assert!(output.contains("publication checked\nphase 2/2 passed"));
        assert!(output.contains("summary: PASSED all 2 phases (elapsed:"));
    }

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
    fn phase_runner_propagates_an_initial_write_failure_before_starting_work() {
        let mut output = FailingWriter;
        let mut invoked = Vec::new();

        let error = run_phases(&[Phase::Format, Phase::LockedBuild], &mut output, |phase| {
            invoked.push(phase);
            Ok::<_, XtaskError>(String::new())
        })
        .map_err(phase_run_error)
        .expect_err("failed phase output must stop the gate");

        assert!(invoked.is_empty());
        let XtaskError::Output(error) = error else {
            panic!("expected an output failure");
        };
        assert_eq!(
            error.to_string(),
            "could not write phase start output: fixture writer failure"
        );
        assert_eq!(
            std::error::Error::source(&error)
                .expect("output error must retain its I/O source")
                .to_string(),
            "fixture writer failure"
        );
    }

    #[test]
    fn cargo_phases_have_exact_root_scoped_command_plans() {
        let root = Path::new("/controlled/minicontainer");
        let expected = [
            (Phase::Format, vec!["fmt", "--all", "--", "--check"]),
            (
                Phase::ClippyBundle,
                vec![
                    "clippy",
                    "-p",
                    "minicontainer-bundle",
                    "--all-targets",
                    "--locked",
                    "--",
                    "-D",
                    "warnings",
                ],
            ),
            (
                Phase::ClippyProtocol,
                vec![
                    "clippy",
                    "-p",
                    "minicontainer-protocol",
                    "--all-targets",
                    "--locked",
                    "--",
                    "-D",
                    "warnings",
                ],
            ),
            (
                Phase::ClippyXtask,
                vec![
                    "clippy",
                    "-p",
                    "xtask",
                    "--all-targets",
                    "--locked",
                    "--",
                    "-D",
                    "warnings",
                ],
            ),
            (
                Phase::ClippyRuntime,
                vec![
                    "clippy",
                    "-p",
                    "minicontainer-runtime",
                    "--all-targets",
                    "--locked",
                    "--",
                    "-D",
                    "warnings",
                ],
            ),
            (
                Phase::ClippyMinictr,
                vec![
                    "clippy",
                    "-p",
                    "minictr",
                    "--all-targets",
                    "--locked",
                    "--",
                    "-D",
                    "warnings",
                ],
            ),
            (
                Phase::BundleTests,
                vec!["test", "-p", "minicontainer-bundle", "--locked"],
            ),
            (
                Phase::ProtocolTests,
                vec!["test", "-p", "minicontainer-protocol", "--locked"],
            ),
            (
                Phase::RuntimeTests,
                vec!["test", "-p", "minicontainer-runtime", "--locked"],
            ),
            (
                Phase::MinictrTests,
                vec!["test", "-p", "minictr", "--locked"],
            ),
            (Phase::XtaskTests, vec!["test", "-p", "xtask", "--locked"]),
            (Phase::LockedBuild, vec!["build", "--workspace", "--locked"]),
        ];

        for (phase, arguments) in expected {
            let invocation = phase
                .cargo_invocation(root)
                .expect("fixture contains only Cargo phases");
            assert_eq!(invocation.current_dir(), root);
            assert_eq!(invocation.arguments(), arguments);
        }
        assert!(Phase::DocsLinks.cargo_invocation(root).is_none());
        assert!(Phase::PublicationFiles.cargo_invocation(root).is_none());
        assert!(Phase::EndToEnd.cargo_invocation(root).is_none());
    }
}
