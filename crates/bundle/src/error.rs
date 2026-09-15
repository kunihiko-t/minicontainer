use minios_abi::{boot::BootHeaderError, manifest::ManifestError};
use std::{error::Error, fmt, io};

/// A failure while constructing or validating a MiniBundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BundleError {
    /// The fixed header is malformed.
    Header(BootHeaderError),
    /// The manifest is malformed.
    Manifest(ManifestError),
    /// The encoded bundle length cannot be represented safely.
    LengthOverflow,
    /// The bundle exceeds the ABI's boot payload limit.
    TooLarge,
    /// The byte length differs from the length declared by the header.
    LengthMismatch { declared: u64, actual: usize },
    /// The bundle digest does not cover the supplied bytes.
    DigestMismatch,
    /// Alignment padding contains a non-zero byte.
    NonZeroPadding,
    /// A source argument contains LF and would encode as multiple manifest lines.
    ArgumentContainsLf { index: usize },
}

impl fmt::Display for BundleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Header(error) => write!(formatter, "invalid MiniBundle header: {error:?}"),
            Self::Manifest(error) => write!(formatter, "invalid MiniBundle manifest: {error:?}"),
            Self::LengthOverflow => formatter.write_str("MiniBundle length arithmetic overflowed"),
            Self::TooLarge => formatter.write_str("MiniBundle exceeds the 6 MiB payload limit"),
            Self::LengthMismatch { declared, actual } => write!(
                formatter,
                "MiniBundle declared {declared} bytes but contains {actual} bytes"
            ),
            Self::DigestMismatch => formatter.write_str("MiniBundle SHA-256 digest mismatch"),
            Self::NonZeroPadding => formatter.write_str("MiniBundle alignment padding is not zero"),
            Self::ArgumentContainsLf { index } => write!(
                formatter,
                "MiniBundle argument {index} contains LF and is not canonically encodable"
            ),
        }
    }
}

impl Error for BundleError {}

/// A failure while accessing the local content-addressed bundle store.
#[derive(Debug)]
pub enum StoreError {
    /// The store root must be absolute.
    RootNotAbsolute,
    /// A tag does not follow the manifest name grammar.
    InvalidTag(ManifestError),
    /// A manifest-compatible tag is still a path traversal component.
    UnsafeTagName,
    /// A store directory or entry is a symlink or otherwise leaves the root.
    UnsafeStorePath,
    /// A tag file does not contain exactly one lowercase SHA-256 digest.
    InvalidTagDigest,
    /// A tag file exceeds the exact lowercase SHA-256 digest length.
    TagTooLarge,
    /// A tag does not exist.
    TagNotFound(String),
    /// The bundle stored under a digest path has a different digest.
    DigestPathMismatch,
    /// A blob is still referenced by a tag.
    BlobReferenced,
    /// A bundle failed construction or validation.
    Bundle(BundleError),
    /// A filesystem operation failed.
    Io(io::Error),
}

impl From<BundleError> for StoreError {
    fn from(error: BundleError) -> Self {
        Self::Bundle(error)
    }
}

impl From<io::Error> for StoreError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RootNotAbsolute => formatter.write_str("store root is not an absolute path"),
            Self::InvalidTag(error) => write!(formatter, "invalid store tag: {error:?}"),
            Self::UnsafeTagName => formatter.write_str("store tag contains path traversal syntax"),
            Self::UnsafeStorePath => {
                formatter.write_str("store path is a symlink or leaves the store root")
            }
            Self::InvalidTagDigest => {
                formatter.write_str("tag does not contain a lowercase SHA-256 digest")
            }
            Self::TagTooLarge => formatter.write_str("tag exceeds the SHA-256 digest length"),
            Self::TagNotFound(name) => write!(formatter, "tag `{name}` does not exist"),
            Self::DigestPathMismatch => {
                formatter.write_str("image content digest does not match its store path")
            }
            Self::BlobReferenced => formatter.write_str("blob is still referenced by a tag"),
            Self::Bundle(error) => write!(formatter, "invalid stored bundle: {error}"),
            Self::Io(error) => write!(formatter, "store filesystem operation failed: {error}"),
        }
    }
}

impl Error for StoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Bundle(error) => Some(error),
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}
