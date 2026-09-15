use std::{error::Error, fmt, io, path::PathBuf};

use minicontainer_bundle::BundleError;

use crate::{HostSignal, InstanceError, ProcessError, SessionError};

/// runの後始末中に追加で起きた失敗。
#[derive(Debug)]
pub enum CleanupFailure {
    /// childの停止または終了回収に失敗した。
    Process(ProcessError),
    /// temporary payloadの削除に失敗した。
    Payload(io::Error),
    /// instance state fileの削除に失敗した。
    Instance(InstanceError),
}

impl fmt::Display for CleanupFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Process(error) => write!(formatter, "process cleanup failed: {error}"),
            Self::Payload(error) => write!(formatter, "payload cleanup failed: {error}"),
            Self::Instance(error) => write!(formatter, "instance cleanup failed: {error}"),
        }
    }
}

impl Error for CleanupFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Process(error) => Some(error),
            Self::Payload(error) => Some(error),
            Self::Instance(error) => Some(error),
        }
    }
}

/// host runtimeの操作が失敗した理由。
#[derive(Debug)]
pub enum RuntimeError {
    /// MiniBundleを検証できなかった。
    Bundle(BundleError),
    /// 要求された待機期限が時刻として表現できない。
    InvalidDeadline,
    /// payload pathに、QEMU `-device` optionの構文を壊す文字 (`,`) が含まれる。
    UnsafePayloadPath(PathBuf),
    /// 一時fileやdirectoryの操作が失敗した。
    Io(io::Error),
    /// instance stateの登録に失敗した。QEMUは起動前に畳まれる。
    Instance(InstanceError),
    /// QEMU processの操作に失敗した。
    Process(ProcessError),
    /// QEMU UART sessionを復元できなかった。
    Session(SessionError),
    /// 逐次転送先のconsumerがchunkの受け取りに失敗した。
    Consumer(io::Error),
    /// hostがSIGINTまたはSIGTERMを受け取り、runを中断した。
    Interrupted(HostSignal),
    /// 主操作の失敗を維持したまま、後始末でも失敗した。
    Cleanup {
        /// runが最初に失敗した理由。
        primary: Box<RuntimeError>,
        /// その後に起きた後始末の失敗。
        failures: Vec<CleanupFailure>,
    },
    /// 主操作は成功したが、後始末に失敗した。
    CleanupOnly(Vec<CleanupFailure>),
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bundle(error) => write!(formatter, "invalid MiniBundle: {error}"),
            Self::InvalidDeadline => {
                write!(
                    formatter,
                    "invalid deadline: the timeout cannot be represented"
                )
            }
            Self::UnsafePayloadPath(path) => write!(
                formatter,
                "payload path must not contain ',': {}",
                path.display()
            ),
            Self::Io(error) => write!(formatter, "runtime io failed: {error}"),
            Self::Instance(error) => write!(formatter, "instance state failed: {error}"),
            Self::Process(error) => write!(formatter, "runtime process failed: {error}"),
            Self::Session(error) => write!(formatter, "runtime session failed: {error}"),
            Self::Consumer(error) => {
                write!(formatter, "guest output consumer failed: {error}")
            }
            Self::Interrupted(signal) => {
                write!(formatter, "run interrupted by {}", signal.name())
            }
            Self::Cleanup { primary, failures } => {
                write!(formatter, "{primary}; cleanup also failed")?;
                for failure in failures {
                    write!(formatter, ": {failure}")?;
                }
                Ok(())
            }
            Self::CleanupOnly(failures) => {
                write!(formatter, "runtime cleanup failed")?;
                for failure in failures {
                    write!(formatter, ": {failure}")?;
                }
                Ok(())
            }
        }
    }
}

impl Error for RuntimeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Bundle(error) => Some(error),
            Self::InvalidDeadline => None,
            Self::UnsafePayloadPath(_) => None,
            Self::Io(error) | Self::Consumer(error) => Some(error),
            Self::Instance(error) => Some(error),
            Self::Interrupted(_) => None,
            Self::Process(error) => Some(error),
            Self::Session(error) => Some(error),
            Self::Cleanup { primary, .. } => Some(primary),
            Self::CleanupOnly(failures) => failures.first().map(|failure| failure as _),
        }
    }
}
