use std::{fmt, path::PathBuf};

/// OCI変換の型付き失敗。
#[derive(Debug, PartialEq, Eq)]
pub enum OciError {
    /// 入力bytesがMiniBundleとして不正である。
    Bundle(minicontainer_bundle::BundleError),
    /// 出力先がすでに存在する。
    DestinationExists {
        /// 指定された出力先。
        path: PathBuf,
    },
    /// 出力先の親directoryを作れない。
    CreateParent {
        /// 親directory。
        path: PathBuf,
        /// OS errorの内容。
        message: String,
    },
    /// 一時出力directoryを作れない。
    CreateTemp {
        /// 一時directory。
        path: PathBuf,
        /// OS errorの内容。
        message: String,
    },
    /// layout fileを書けない。
    WriteFile {
        /// 書き込み先。
        path: PathBuf,
        /// OS errorの内容。
        message: String,
    },
    /// 一時directoryを出力先へ移動できない。
    Publish {
        /// 一時directory。
        temp: PathBuf,
        /// 出力先。
        dest: PathBuf,
        /// OS errorの内容。
        message: String,
    },
    /// 完成品の読み戻し検証が不一致である。
    Readback {
        /// 不一致の内容。
        message: String,
    },
    /// layoutのrootやfileを読めない。
    Layout {
        /// 対象のpath。
        path: PathBuf,
        /// OS errorの内容。
        message: String,
    },
    /// layout外を指す参照やsymlink、非通常fileを拒否した。
    UnsafePath {
        /// 対象のpath。
        path: PathBuf,
    },
    /// fileがsize上限を超えた。
    TooLarge {
        /// 対象のpath。
        path: PathBuf,
        /// 適用した上限byte数。
        limit: u64,
    },
    /// JSON文書が文法違反である。
    Json {
        /// 違反の内容。
        message: String,
    },
    /// JSONは正しいがOCIの形状違反である。
    Shape {
        /// 違反の内容。
        message: String,
    },
    /// platformが`riscv64`と`minios`ではない。
    UnsupportedPlatform {
        /// layoutが宣言したarchitecture（不在は`missing`）。
        architecture: String,
        /// layoutが宣言したOS（不在は`missing`）。
        os: String,
    },
    /// blobのdigestがdescriptorと一致しない。
    DigestMismatch {
        /// 対象のpath。
        path: PathBuf,
        /// descriptorの宣言。
        expected: String,
        /// 実測のdigest。
        actual: String,
    },
    /// blobのsizeがdescriptorと一致しない。
    SizeMismatch {
        /// 対象のpath。
        path: PathBuf,
        /// descriptorの宣言byte数。
        expected: u64,
        /// 実測のbyte数。
        actual: u64,
    },
    /// configの写しがbundle本体と一致しない。
    ConfigMismatch {
        /// 不一致の内容。
        message: String,
    },
    /// registryの参照や転送が失敗した。
    Registry {
        /// 失敗の内容。secretは含まない。
        message: String,
    },
}

impl fmt::Display for OciError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bundle(error) => write!(formatter, "invalid MiniBundle: {error}"),
            Self::DestinationExists { path } => write!(
                formatter,
                "oci destination already exists: {}",
                path.display()
            ),
            Self::CreateParent { path, message } => write!(
                formatter,
                "oci parent directory cannot be created ({}): {message}",
                path.display()
            ),
            Self::CreateTemp { path, message } => write!(
                formatter,
                "oci temporary directory cannot be created ({}): {message}",
                path.display()
            ),
            Self::WriteFile { path, message } => write!(
                formatter,
                "oci file cannot be written ({}): {message}",
                path.display()
            ),
            Self::Publish {
                temp,
                dest,
                message,
            } => write!(
                formatter,
                "oci layout cannot be published ({} -> {}): {message}",
                temp.display(),
                dest.display()
            ),
            Self::Readback { message } => {
                write!(formatter, "oci layout verification failed: {message}")
            }
            Self::Layout { path, message } => write!(
                formatter,
                "oci layout cannot be read ({}): {message}",
                path.display()
            ),
            Self::UnsafePath { path } => write!(
                formatter,
                "oci layout path escapes or is not a regular file: {}",
                path.display()
            ),
            Self::TooLarge { path, limit } => write!(
                formatter,
                "oci file exceeds {limit} bytes: {}",
                path.display()
            ),
            Self::Json { message } => {
                write!(formatter, "oci JSON is malformed: {message}")
            }
            Self::Shape { message } => {
                write!(formatter, "oci layout shape is invalid: {message}")
            }
            Self::UnsupportedPlatform { architecture, os } => write!(
                formatter,
                "unsupported oci platform: {architecture}/{os} (want riscv64/minios)"
            ),
            Self::DigestMismatch {
                path,
                expected,
                actual,
            } => write!(
                formatter,
                "oci digest mismatch for {}: want {expected}, got {actual}",
                path.display()
            ),
            Self::SizeMismatch {
                path,
                expected,
                actual,
            } => write!(
                formatter,
                "oci size mismatch for {}: want {expected} bytes, got {actual} bytes",
                path.display()
            ),
            Self::ConfigMismatch { message } => {
                write!(formatter, "oci config does not match the bundle: {message}")
            }
            Self::Registry { message } => {
                write!(formatter, "registry pull failed: {message}")
            }
        }
    }
}

impl std::error::Error for OciError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_name_the_failed_step() {
        assert_eq!(
            OciError::DestinationExists {
                path: PathBuf::from("out")
            }
            .to_string(),
            "oci destination already exists: out"
        );
        assert_eq!(
            OciError::Readback {
                message: "index.json differs".to_owned()
            }
            .to_string(),
            "oci layout verification failed: index.json differs"
        );
    }
}
