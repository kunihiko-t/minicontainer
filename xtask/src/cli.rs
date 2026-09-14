use std::{
    ffi::OsString,
    fmt,
    path::{Path, PathBuf},
};

/// A public MiniContainer development command.
#[derive(Debug, Eq, PartialEq)]
pub enum Command {
    /// Diagnoses the required development tools without changing the system.
    Setup,
    /// Runs the ordered gate without the real-QEMU phase.
    CheckHost,
    /// Runs the ordered release gate.
    Check,
    /// Builds and verifies a distribution archive from prebuilt inputs.
    Dist(DistArgs),
}

/// Arguments for the `dist` command.
#[derive(Debug, Eq, PartialEq)]
pub struct DistArgs {
    /// Archive version. `None` selects the xtask package version.
    pub version: Option<String>,
    /// Archive target triple.
    pub target: String,
    /// Prebuilt `minictr` binary.
    pub minictr: PathBuf,
    /// Prebuilt miniOS kernel file.
    pub kernel: PathBuf,
    /// Output directory. `None` selects `<workspace>/dist`.
    pub output: Option<PathBuf>,
}

impl DistArgs {
    /// Resolves the archive version against the xtask package version.
    pub fn version(&self) -> &str {
        self.version.as_deref().unwrap_or(env!("CARGO_PKG_VERSION"))
    }

    /// Resolves the output directory against the workspace root.
    pub fn output(&self, workspace: &Path) -> PathBuf {
        self.output
            .clone()
            .unwrap_or_else(|| workspace.join("dist"))
    }
}

impl Command {
    /// Parses one supported xtask command.
    pub fn parse(arguments: impl IntoIterator<Item = impl AsRef<str>>) -> Result<Self, CliError> {
        parse(arguments)
    }

    /// Parses one supported xtask command from operating-system arguments.
    pub fn parse_os(arguments: impl IntoIterator<Item = OsString>) -> Result<Self, CliError> {
        parse_os(arguments)
    }
}

/// An error returned while parsing an xtask command.
#[derive(Debug, Eq, PartialEq)]
pub enum CliError {
    /// No command was supplied.
    MissingCommand,
    /// The command is not supported by the foundation harness.
    UnknownCommand(String),
    /// An argument was supplied after a complete command.
    UnexpectedArgument(String),
    /// An operating-system argument cannot be represented as UTF-8.
    NonUtf8Argument(OsString),
    /// An option is not supported by the command.
    UnknownOption(String),
    /// The same option was supplied twice.
    DuplicateOption(&'static str),
    /// An option value is missing.
    MissingValue(&'static str),
    /// A required option was not supplied.
    MissingOption(&'static str),
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingCommand => formatter.write_str("missing xtask command"),
            Self::UnknownCommand(command) => write!(formatter, "unknown xtask command: {command}"),
            Self::UnexpectedArgument(argument) => {
                write!(formatter, "unexpected xtask argument: {argument}")
            }
            Self::NonUtf8Argument(_) => formatter.write_str("xtask argument is not valid UTF-8"),
            Self::UnknownOption(option) => {
                write!(formatter, "unknown xtask option: {option}")
            }
            Self::DuplicateOption(option) => {
                write!(formatter, "duplicate xtask option: {option}")
            }
            Self::MissingValue(option) => {
                write!(formatter, "missing value for xtask option: {option}")
            }
            Self::MissingOption(option) => {
                write!(formatter, "missing required xtask option: {option}")
            }
        }
    }
}

/// Returns the complete public command syntax.
pub fn help() -> &'static str {
    "usage: cargo xtask <setup|check-host|check>\nusage: cargo xtask dist --target TARGET --minictr PATH --kernel PATH [--version VERSION] [--output DIR]"
}

/// Parses one supported command from UTF-8 arguments.
pub fn parse(arguments: impl IntoIterator<Item = impl AsRef<str>>) -> Result<Command, CliError> {
    parse_os(
        arguments
            .into_iter()
            .map(|argument| OsString::from(argument.as_ref())),
    )
}

/// Parses one supported command from operating-system arguments.
pub fn parse_os(arguments: impl IntoIterator<Item = OsString>) -> Result<Command, CliError> {
    let mut arguments = arguments.into_iter();
    let command = arguments.next().ok_or(CliError::MissingCommand)?;
    let command = command.into_string().map_err(CliError::NonUtf8Argument)?;

    match command.as_str() {
        "setup" => parse_bare(arguments, Command::Setup),
        "check-host" => parse_bare(arguments, Command::CheckHost),
        "check" => parse_bare(arguments, Command::Check),
        "dist" => parse_dist(arguments),
        unknown => Err(CliError::UnknownCommand(unknown.to_owned())),
    }
}

/// Parses a command that takes no arguments.
fn parse_bare(
    arguments: impl IntoIterator<Item = OsString>,
    command: Command,
) -> Result<Command, CliError> {
    let mut arguments = arguments.into_iter();
    if let Some(unexpected) = arguments.next() {
        let unexpected = unexpected
            .into_string()
            .map_err(CliError::NonUtf8Argument)?;
        return Err(CliError::UnexpectedArgument(unexpected));
    }

    Ok(command)
}

/// Parses the `dist` options. Every option takes a value, either as a
/// separate token or as `--option=value`.
fn parse_dist(arguments: impl IntoIterator<Item = OsString>) -> Result<Command, CliError> {
    let mut version: Option<String> = None;
    let mut target: Option<String> = None;
    let mut minictr: Option<PathBuf> = None;
    let mut kernel: Option<PathBuf> = None;
    let mut output: Option<PathBuf> = None;

    let pending: Vec<OsString> = arguments.into_iter().collect();
    let mut rest = pending.into_iter();
    while let Some(argument) = rest.next() {
        let text = argument.into_string().map_err(CliError::NonUtf8Argument)?;
        let Some(name) = text.strip_prefix("--") else {
            return Err(CliError::UnexpectedArgument(text));
        };
        let (name, inline_value) = match name.split_once('=') {
            Some((name, value)) => (name, Some(OsString::from(value))),
            None => (name, None),
        };
        let value = |option: &'static str| match inline_value {
            Some(value) => Ok(value),
            None => rest.next().ok_or(CliError::MissingValue(option)),
        };
        match name {
            "version" => {
                if version.is_some() {
                    return Err(CliError::DuplicateOption("--version"));
                }
                let value = value("--version")?;
                let value = value.into_string().map_err(CliError::NonUtf8Argument)?;
                version = Some(value);
            }
            "target" => {
                if target.is_some() {
                    return Err(CliError::DuplicateOption("--target"));
                }
                let value = value("--target")?;
                let value = value.into_string().map_err(CliError::NonUtf8Argument)?;
                target = Some(value);
            }
            "minictr" => {
                if minictr.is_some() {
                    return Err(CliError::DuplicateOption("--minictr"));
                }
                minictr = Some(PathBuf::from(value("--minictr")?));
            }
            "kernel" => {
                if kernel.is_some() {
                    return Err(CliError::DuplicateOption("--kernel"));
                }
                kernel = Some(PathBuf::from(value("--kernel")?));
            }
            "output" => {
                if output.is_some() {
                    return Err(CliError::DuplicateOption("--output"));
                }
                output = Some(PathBuf::from(value("--output")?));
            }
            unknown => return Err(CliError::UnknownOption(format!("--{unknown}"))),
        }
    }

    let Some(target) = target else {
        return Err(CliError::MissingOption("--target"));
    };
    let Some(minictr) = minictr else {
        return Err(CliError::MissingOption("--minictr"));
    };
    let Some(kernel) = kernel else {
        return Err(CliError::MissingOption("--kernel"));
    };
    Ok(Command::Dist(DistArgs {
        version,
        target,
        minictr,
        kernel,
        output,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_public_bare_commands() {
        assert_eq!(parse(["setup"]), Ok(Command::Setup));
        assert_eq!(parse(["check"]), Ok(Command::Check));
        assert_eq!(parse(["check-host"]), Ok(Command::CheckHost));
    }

    #[test]
    fn rejects_missing_unknown_and_trailing_arguments() {
        assert_eq!(parse([] as [&str; 0]), Err(CliError::MissingCommand));
        assert_eq!(
            parse(["run"]),
            Err(CliError::UnknownCommand("run".to_owned()))
        );
        assert_eq!(
            parse(["setup", "extra"]),
            Err(CliError::UnexpectedArgument("extra".to_owned()))
        );
        assert_eq!(
            parse(["check-host", "extra"]),
            Err(CliError::UnexpectedArgument("extra".to_owned()))
        );
    }

    #[test]
    fn parses_dist_with_required_and_optional_options() {
        assert_eq!(
            parse([
                "dist",
                "--target",
                "x86_64-unknown-linux-gnu",
                "--minictr",
                "target/release/minictr",
                "--kernel",
                "minios.bin",
            ]),
            Ok(Command::Dist(DistArgs {
                version: None,
                target: "x86_64-unknown-linux-gnu".to_owned(),
                minictr: PathBuf::from("target/release/minictr"),
                kernel: PathBuf::from("minios.bin"),
                output: None,
            }))
        );
        assert_eq!(
            parse([
                "dist",
                "--target=x86_64-unknown-linux-gnu",
                "--minictr=target/release/minictr",
                "--kernel=minios.bin",
                "--version=1.2.3",
                "--output=artifacts",
            ]),
            Ok(Command::Dist(DistArgs {
                version: Some("1.2.3".to_owned()),
                target: "x86_64-unknown-linux-gnu".to_owned(),
                minictr: PathBuf::from("target/release/minictr"),
                kernel: PathBuf::from("minios.bin"),
                output: Some(PathBuf::from("artifacts")),
            }))
        );
    }

    #[test]
    fn dist_rejects_missing_duplicate_and_unknown_options() {
        assert_eq!(
            parse(["dist", "--minictr", "m", "--kernel", "k"]),
            Err(CliError::MissingOption("--target"))
        );
        assert_eq!(
            parse(["dist", "--target", "t", "--kernel", "k"]),
            Err(CliError::MissingOption("--minictr"))
        );
        assert_eq!(
            parse(["dist", "--target", "t", "--minictr", "m"]),
            Err(CliError::MissingOption("--kernel"))
        );
        assert_eq!(
            parse(["dist", "--target"]),
            Err(CliError::MissingValue("--target"))
        );
        assert_eq!(
            parse([
                "dist",
                "--target",
                "a",
                "--target",
                "b",
                "--minictr",
                "m",
                "--kernel",
                "k",
            ]),
            Err(CliError::DuplicateOption("--target"))
        );
        assert_eq!(
            parse([
                "dist",
                "--target",
                "t",
                "--minictr",
                "m",
                "--kernel",
                "k",
                "--jobs",
                "4",
            ]),
            Err(CliError::UnknownOption("--jobs".to_owned()))
        );
        assert_eq!(
            parse([
                "dist",
                "--target",
                "t",
                "--minictr",
                "m",
                "--kernel",
                "k",
                "positional",
            ]),
            Err(CliError::UnexpectedArgument("positional".to_owned()))
        );
    }

    #[test]
    fn dist_args_resolves_defaults() {
        let args = DistArgs {
            version: None,
            target: "t".to_owned(),
            minictr: PathBuf::from("m"),
            kernel: PathBuf::from("k"),
            output: None,
        };
        assert_eq!(args.version(), env!("CARGO_PKG_VERSION"));
        assert_eq!(
            args.output(Path::new("/workspace")),
            PathBuf::from("/workspace/dist")
        );
    }

    #[test]
    fn help_and_diagnostics_name_the_public_contract() {
        assert_eq!(
            help(),
            "usage: cargo xtask <setup|check-host|check>\nusage: cargo xtask dist --target TARGET --minictr PATH --kernel PATH [--version VERSION] [--output DIR]"
        );
        assert_eq!(
            CliError::MissingCommand.to_string(),
            "missing xtask command"
        );
        assert_eq!(
            CliError::UnknownCommand("run".to_owned()).to_string(),
            "unknown xtask command: run"
        );
        assert_eq!(
            CliError::UnexpectedArgument("extra".to_owned()).to_string(),
            "unexpected xtask argument: extra"
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_non_utf8_arguments_as_a_typed_error() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let invalid = OsString::from_vec(vec![0xff]);
        assert_eq!(
            parse_os([invalid.clone()]),
            Err(CliError::NonUtf8Argument(invalid.clone()))
        );
        assert_eq!(
            CliError::NonUtf8Argument(invalid).to_string(),
            "xtask argument is not valid UTF-8"
        );
    }
}
