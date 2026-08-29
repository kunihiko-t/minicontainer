use minios_abi::control::ControlError;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    Header(ControlError),
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Header(error) => write!(formatter, "invalid UART frame header: {error:?}"),
        }
    }
}

impl std::error::Error for ProtocolError {}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use super::ProtocolError;
    use minios_abi::control::ControlError;

    // Production break caught: callers cannot format or propagate a public
    // protocol failure through the standard error trait boundary.
    #[test]
    fn header_errors_have_a_display_diagnostic_and_standard_error_context() {
        let error = ProtocolError::Header(ControlError::WrongMagic);

        assert!(error.to_string().contains("header"));
        assert!(Error::source(&error).is_none());
        assert_standard_error(&error);
    }

    fn assert_standard_error(_: &(dyn Error + 'static)) {}
}
