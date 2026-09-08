use std::{ffi::OsString, fmt};

/// A public MiniContainer development command.
#[derive(Debug, Eq, PartialEq)]
pub enum Command {
    /// Diagnoses the required development tools without changing the system.
    Setup,
    /// Runs the ordered gate without the real-QEMU phase.
    CheckHost,
    /// Runs the ordered release gate.
    Check,
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
        }
    }
}

/// Returns the complete public command syntax.
pub fn help() -> &'static str {
    "usage: cargo xtask <setup|check-host|check>"
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

    let parsed = match command.as_str() {
        "setup" => Command::Setup,
        "check-host" => Command::CheckHost,
        "check" => Command::Check,
        unknown => return Err(CliError::UnknownCommand(unknown.to_owned())),
    };

    if let Some(unexpected) = arguments.next() {
        let unexpected = unexpected
            .into_string()
            .map_err(CliError::NonUtf8Argument)?;
        return Err(CliError::UnexpectedArgument(unexpected));
    }

    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_only_three_public_commands() {
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
    fn help_and_diagnostics_name_the_public_contract() {
        assert_eq!(help(), "usage: cargo xtask <setup|check-host|check>");
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
