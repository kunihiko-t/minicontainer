//! MiniBundleとOCI Image Layoutの正準変換。
//!
//! 対応の定義は`docs/reference/minibundle-oci-mapping.md`が正である。
//! このcrateはそのv1対応のexport側（storeのbundle bytesからlayout
//! directoryの生成）とimport側（layoutの検証と正準bundleへの復元）を
//! 実装する。

#![forbid(unsafe_code)]

mod error;
mod export;
mod import;
mod json;
mod parse;

pub use error::OciError;
pub use export::{ExportedLayout, export_bundle};
pub use import::import_bundle;

/// `oci-layout`が宣言するlayout版。
pub const OCI_LAYOUT_VERSION: &str = "1.0.0";
/// Image indexのmedia type。
pub const MEDIA_TYPE_INDEX: &str = "application/vnd.oci.image.index.v1+json";
/// Image manifestのmedia type。
pub const MEDIA_TYPE_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
/// MiniContainer image configのmedia type。
pub const MEDIA_TYPE_CONFIG: &str = "application/vnd.minicontainer.image.config.v1+json";
/// MiniBundle layerのmedia type。
pub const MEDIA_TYPE_LAYER: &str = "application/vnd.minicontainer.bundle.v1+mcb";
/// 受け入れる単一architecture。
pub const ARCHITECTURE: &str = "riscv64";
/// 受け入れる単一OS。
pub const OS: &str = "minios";
/// config、manifest、indexの各JSON文書の公開上限。
pub const MAX_JSON_LEN: u64 = 64 * 1024;
