use std::{error::Error, fmt, io, path::PathBuf};

/// host runtimeの操作が失敗した理由。
#[derive(Debug)]
pub enum RuntimeError {
    /// payload pathに、QEMU `-device` optionの構文を壊す文字 (`,`) が含まれる。
    UnsafePayloadPath(PathBuf),
    /// 一時fileやdirectoryの操作が失敗した。
    Io(io::Error),
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsafePayloadPath(path) => write!(
                formatter,
                "payload path must not contain ',': {}",
                path.display()
            ),
            Self::Io(error) => write!(formatter, "runtime io failed: {error}"),
        }
    }
}

impl Error for RuntimeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::UnsafePayloadPath(_) => None,
            Self::Io(error) => Some(error),
        }
    }
}
