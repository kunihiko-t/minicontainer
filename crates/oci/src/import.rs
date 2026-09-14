//! Reads one OCI Image Layout directory back into canonical MiniBundle bytes.
//!
//! Verification order is size, digest, JSON parse, semantic checks, then
//! bundle parse. The caller writes to the store only after this function
//! succeeds, so an invalid descriptor never changes the store.

use crate::{
    ARCHITECTURE, MAX_JSON_LEN, MEDIA_TYPE_CONFIG, MEDIA_TYPE_LAYER, MEDIA_TYPE_MANIFEST, OS,
    OciError,
    parse::{JsonValue, parse_json},
};
use minicontainer_bundle::{ImageSpec, format_digest};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::Read,
    path::{Component, Path, PathBuf},
};

/// One verified descriptor: its blob bytes plus the claimed digest.
struct Blob {
    /// Blob bytes as read from the layout.
    bytes: Vec<u8>,
    /// Claimed `sha256:<hex>` digest.
    digest: String,
}

/// Imports one layout directory into canonical MiniBundle bytes. The layer
/// is rebuilt from its parsed fields so the store always receives canonical
/// bytes, even if the ABI layout checks ever relax.
pub fn import_bundle(dir: &Path) -> Result<Vec<u8>, OciError> {
    let root = canonical_root(dir)?;
    let layout = read_layout_file(dir, &root, Path::new("oci-layout"), MAX_JSON_LEN)?;
    require_layout_version(&layout)?;
    let index = read_layout_file(dir, &root, Path::new("index.json"), MAX_JSON_LEN)?;
    let index = parse_json(&index)?;
    let entry = single_entry(&index, "manifests")?;
    require_media_type(entry, MEDIA_TYPE_MANIFEST, "manifest")?;
    require_platform(entry)?;
    let manifest = read_blob(dir, &root, entry, MAX_JSON_LEN, "manifest")?;
    let manifest_value = parse_json(&manifest.bytes)?;

    let config_entry = manifest_value
        .get("config")
        .ok_or_else(|| shape("manifest is missing config"))?;
    require_media_type(config_entry, MEDIA_TYPE_CONFIG, "config")?;
    let config = read_blob(dir, &root, config_entry, MAX_JSON_LEN, "config")?;
    let config_value = parse_json(&config.bytes)?;

    let layers = manifest_value
        .get("layers")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| shape("manifest layers must be an array"))?;
    if layers.len() != 1 {
        return Err(shape(&format!(
            "manifest must hold one layer, found {}",
            layers.len()
        )));
    }
    require_media_type(&layers[0], MEDIA_TYPE_LAYER, "layer")?;
    let layer = read_blob(
        dir,
        &root,
        &layers[0],
        minicontainer_bundle::MAX_BUNDLE_LEN,
        "layer",
    )?;

    let bundle = minicontainer_bundle::parse(&layer.bytes).map_err(OciError::Bundle)?;
    require_config_match(&config_value, &layer.digest, &bundle)?;
    let args: Vec<&str> = bundle.manifest.args().collect();
    let canonical = minicontainer_bundle::build(ImageSpec {
        name: bundle.manifest.name(),
        args: &args,
        elf: bundle.elf,
    })
    .map_err(OciError::Bundle)?;
    Ok(canonical)
}

/// Canonicalizes the layout root and requires a directory.
fn canonical_root(dir: &Path) -> Result<PathBuf, OciError> {
    let root = fs_canonicalize(dir).map_err(|message| OciError::Layout {
        path: dir.to_owned(),
        message,
    })?;
    if !root.is_dir() {
        return Err(OciError::Layout {
            path: dir.to_owned(),
            message: "layout root is not a directory".to_owned(),
        });
    }
    Ok(root)
}

/// Requires `imageLayoutVersion` to equal the pinned layout version.
fn require_layout_version(bytes: &[u8]) -> Result<(), OciError> {
    let value = parse_json(bytes)?;
    match value.get("imageLayoutVersion").and_then(JsonValue::as_str) {
        Some(version) if version == crate::OCI_LAYOUT_VERSION => Ok(()),
        _ => Err(shape("oci-layout must declare imageLayoutVersion 1.0.0")),
    }
}

/// Extracts the single entry of an index-style array member.
fn single_entry<'a>(value: &'a JsonValue, member: &str) -> Result<&'a JsonValue, OciError> {
    let entries = value
        .get(member)
        .and_then(JsonValue::as_array)
        .ok_or_else(|| shape(&format!("index is missing {member}")))?;
    if entries.len() != 1 {
        return Err(shape(&format!(
            "index must hold one manifest, found {}",
            entries.len()
        )));
    }
    Ok(&entries[0])
}

/// Requires one descriptor media type.
fn require_media_type(entry: &JsonValue, expected: &str, role: &str) -> Result<(), OciError> {
    match entry.get("mediaType").and_then(JsonValue::as_str) {
        Some(media) if media == expected => Ok(()),
        Some(media) => Err(shape(&format!("{role} media type is unsupported: {media}"))),
        None => Err(shape(&format!("{role} is missing mediaType"))),
    }
}

/// Validates the platform hint of one index entry when present. A missing
/// hint is accepted: tools may drop it when copying, and the config holds
/// the authoritative declaration. A present hint must match exactly.
fn require_platform(entry: &JsonValue) -> Result<(), OciError> {
    let Some(platform) = entry.get("platform") else {
        return Ok(());
    };
    let architecture = platform
        .get("architecture")
        .and_then(JsonValue::as_str)
        .unwrap_or("missing");
    let os = platform
        .get("os")
        .and_then(JsonValue::as_str)
        .unwrap_or("missing");
    if architecture == ARCHITECTURE && os == OS {
        Ok(())
    } else {
        Err(OciError::UnsupportedPlatform {
            architecture: architecture.to_owned(),
            os: os.to_owned(),
        })
    }
}

/// Reads one descriptor blob with size-then-digest verification.
fn read_blob(
    dir: &Path,
    root: &Path,
    entry: &JsonValue,
    limit: u64,
    role: &str,
) -> Result<Blob, OciError> {
    let expected_size = entry
        .get("size")
        .and_then(JsonValue::as_u64)
        .ok_or_else(|| shape(&format!("{role} size must be a plain integer")))?;
    if expected_size > limit {
        return Err(OciError::TooLarge {
            path: dir.to_owned(),
            limit,
        });
    }
    let digest = entry
        .get("digest")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| shape(&format!("{role} is missing digest")))?;
    let hex =
        decode_digest(digest).ok_or_else(|| shape(&format!("{role} digest is not sha256 hex")))?;
    let relative = PathBuf::from(format!("blobs/sha256/{hex}"));
    let bytes = read_layout_file(dir, root, &relative, limit)?;
    let actual_size = bytes.len() as u64;
    if actual_size != expected_size {
        return Err(OciError::SizeMismatch {
            path: dir.join(&relative),
            expected: expected_size,
            actual: actual_size,
        });
    }
    let actual = format!("sha256:{}", format_digest(digest_of(&bytes)));
    if actual != digest {
        return Err(OciError::DigestMismatch {
            path: dir.join(&relative),
            expected: digest.to_owned(),
            actual,
        });
    }
    Ok(Blob {
        bytes,
        digest: digest.to_owned(),
    })
}

/// Requires the config view to match the parsed bundle and the layer digest.
fn require_config_match(
    config: &JsonValue,
    layer_digest: &str,
    bundle: &minicontainer_bundle::Bundle<'_>,
) -> Result<(), OciError> {
    let architecture = config
        .get("architecture")
        .and_then(JsonValue::as_str)
        .unwrap_or("missing");
    let os = config
        .get("os")
        .and_then(JsonValue::as_str)
        .unwrap_or("missing");
    if architecture != ARCHITECTURE || os != OS {
        return Err(OciError::UnsupportedPlatform {
            architecture: architecture.to_owned(),
            os: os.to_owned(),
        });
    }
    let entrypoint = config
        .get("config")
        .and_then(|inner| inner.get("Entrypoint"))
        .and_then(JsonValue::as_array)
        .ok_or_else(|| mismatch("config Entrypoint must be an array"))?;
    if entrypoint.len() != 1 || entrypoint[0].as_str() != Some(bundle.manifest.name()) {
        return Err(mismatch("config Entrypoint must hold the bundle name"));
    }
    let expected: Vec<&str> = bundle.manifest.args().collect();
    let actual: Vec<&str> = config
        .get("config")
        .and_then(|inner| inner.get("Cmd"))
        .and_then(JsonValue::as_array)
        .map(|items| {
            items
                .iter()
                .map(JsonValue::as_str)
                .collect::<Option<Vec<_>>>()
        })
        .unwrap_or(None)
        .ok_or_else(|| mismatch("config Cmd must be a string array"))?;
    if actual != expected {
        return Err(mismatch("config Cmd must hold the bundle arguments"));
    }
    let diff_ids = config
        .get("rootfs")
        .and_then(|rootfs| rootfs.get("diff_ids"))
        .and_then(JsonValue::as_array)
        .ok_or_else(|| mismatch("config rootfs must hold diff_ids"))?;
    if diff_ids.len() != 1 || diff_ids[0].as_str() != Some(layer_digest) {
        return Err(mismatch("config diff_ids must hold the layer digest"));
    }
    Ok(())
}

/// Reads one layout file with symlink, traversal, and size enforcement.
fn read_layout_file(
    dir: &Path,
    root: &Path,
    relative: &Path,
    limit: u64,
) -> Result<Vec<u8>, OciError> {
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(OciError::UnsafePath {
            path: dir.join(relative),
        });
    }
    let path = dir.join(relative);
    let metadata = fs_symlink_metadata(&path).map_err(|message| OciError::Layout {
        path: path.clone(),
        message,
    })?;
    if !metadata.file_type().is_file() {
        return Err(OciError::UnsafePath { path });
    }
    let canonical = fs_canonicalize(&path).map_err(|message| OciError::Layout {
        path: path.clone(),
        message,
    })?;
    if !canonical.starts_with(root) {
        return Err(OciError::UnsafePath { path });
    }
    if metadata.len() > limit {
        return Err(OciError::TooLarge { path, limit });
    }
    let file = File::open(&path).map_err(|error| OciError::Layout {
        path: path.clone(),
        message: error.to_string(),
    })?;
    if file
        .metadata()
        .map_err(|error| OciError::Layout {
            path: path.clone(),
            message: error.to_string(),
        })?
        .len()
        > limit
    {
        return Err(OciError::TooLarge { path, limit });
    }
    let mut bounded = file.take(limit + 1);
    let mut bytes = Vec::new();
    bounded
        .read_to_end(&mut bytes)
        .map_err(|error| OciError::Layout {
            path: path.clone(),
            message: error.to_string(),
        })?;
    if bytes.len() as u64 > limit {
        return Err(OciError::TooLarge { path, limit });
    }
    Ok(bytes)
}

/// Decodes a strict `sha256:<64 lowercase hex>` digest to its hex part.
fn decode_digest(digest: &str) -> Option<String> {
    let hex = digest.strip_prefix("sha256:")?;
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return None;
    }
    Some(hex.to_owned())
}

/// SHA-256 digest of one byte string.
fn digest_of(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

/// Builds a shape error.
fn shape(message: &str) -> OciError {
    OciError::Shape {
        message: message.to_owned(),
    }
}

/// Builds a config-mismatch error.
fn mismatch(message: &str) -> OciError {
    OciError::ConfigMismatch {
        message: message.to_owned(),
    }
}

/// Canonicalizes a path with a stringified error.
fn fs_canonicalize(path: &Path) -> Result<PathBuf, String> {
    std::fs::canonicalize(path).map_err(|error| error.to_string())
}

/// Reads symlink metadata with a stringified error.
fn fs_symlink_metadata(path: &Path) -> Result<std::fs::Metadata, String> {
    std::fs::symlink_metadata(path).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export_bundle;
    use minicontainer_bundle::ImageSpec;
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
    };

    const LAYER_DIGEST: &str = "6c710671215c4929fa600956d236ae82df1f3a9f05f0b125c3fb0feed9096136";
    const CONFIG_DIGEST: &str = "3f19ea537fd3b4cdee56a34be0d9c7f90b1472d3e12c0e7aff3a505f7dfac98e";
    const MANIFEST_DIGEST: &str =
        "f095c9356a85036bc3e4a08e562e6826321aedd3569aaa15e076a9639170995b";

    /// The golden fixture: name `hello`, no arguments, ELF `0x00..0x0f`.
    fn golden_bundle() -> Vec<u8> {
        let elf: Vec<u8> = (0..16).collect();
        minicontainer_bundle::build(ImageSpec {
            name: "hello",
            args: &[],
            elf: &elf,
        })
        .expect("golden bundle builds")
    }

    /// Exports the golden bundle and returns the layout directory.
    fn golden_layout(root: &Path) -> PathBuf {
        let dest = root.join("layout");
        export_bundle(&golden_bundle(), &dest).expect("export golden layout");
        dest
    }

    #[test]
    fn import_restores_the_golden_bundle_bytes() {
        let root = fixture_root();
        let layout = golden_layout(&root);

        let restored = import_bundle(&layout).expect("import golden layout");
        assert_eq!(restored, golden_bundle());

        fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn import_round_trip_preserves_escaped_arguments() {
        let root = fixture_root();
        let args = ["a\"b", "c\\d", "e☃f", "plain"];
        let bundle = minicontainer_bundle::build(ImageSpec {
            name: "hello",
            args: &args,
            elf: b"ELF-bytes",
        })
        .expect("bundle builds");

        let dest = root.join("layout");
        export_bundle(&bundle, &dest).expect("export");
        let restored = import_bundle(&dest).expect("import");
        assert_eq!(restored, bundle);
        let parsed = minicontainer_bundle::parse(&restored).expect("parse");
        assert_eq!(
            parsed.manifest.args().collect::<Vec<_>>(),
            args,
            "arguments survive JSON escaping"
        );

        fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn import_round_trip_reproduces_the_layout() {
        let root = fixture_root();
        let first = golden_layout(&root);
        let restored = import_bundle(&first).expect("import");
        let second = root.join("second");
        export_bundle(&restored, &second).expect("re-export");
        for relative in [
            "oci-layout",
            "index.json",
            &format!("blobs/sha256/{MANIFEST_DIGEST}"),
            &format!("blobs/sha256/{CONFIG_DIGEST}"),
            &format!("blobs/sha256/{LAYER_DIGEST}"),
        ] {
            assert_eq!(
                fs::read(first.join(relative)).expect("first"),
                fs::read(second.join(relative)).expect("second"),
                "{relative}"
            );
        }
        fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn import_rejects_digest_and_size_mismatches() {
        let root = fixture_root();
        let layout = golden_layout(&root);
        let layer_path = layout.join(format!("blobs/sha256/{LAYER_DIGEST}"));

        let mut flipped = fs::read(&layer_path).expect("layer");
        flipped[0] ^= 1;
        fs::write(&layer_path, &flipped).expect("flip");
        assert!(matches!(
            import_bundle(&layout),
            Err(OciError::DigestMismatch { .. })
        ));

        let mut truncated = fs::read(&layer_path).expect("layer");
        truncated.pop();
        fs::write(&layer_path, &truncated).expect("truncate");
        assert!(matches!(
            import_bundle(&layout),
            Err(OciError::SizeMismatch { .. })
        ));
        fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn import_rejects_symlinks_and_escaped_paths() {
        let root = fixture_root();
        let layout = golden_layout(&root);

        #[cfg(unix)]
        {
            let layer_path = layout.join(format!("blobs/sha256/{LAYER_DIGEST}"));
            let outside = root.join("outside");
            fs::write(&outside, b"escape").expect("outside");
            fs::remove_file(&layer_path).expect("remove layer");
            std::os::unix::fs::symlink(&outside, &layer_path).expect("symlink");
            assert!(matches!(
                import_bundle(&layout),
                Err(OciError::UnsafePath { .. })
            ));
            fs::remove_file(&layer_path).expect("remove symlink");
            fs::write(&layer_path, golden_bundle()).expect("restore");
        }

        #[cfg(unix)]
        {
            let blobs = layout.join("blobs");
            let outside = root.join("outside-blobs");
            fs::rename(&blobs, &outside).expect("move blobs out");
            std::os::unix::fs::symlink(&outside, &blobs).expect("symlink dir");
            assert!(matches!(
                import_bundle(&layout),
                Err(OciError::UnsafePath { .. })
            ));
        }
        fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn import_rejects_unsupported_shapes_and_platforms() {
        let root = fixture_root();
        let layout = golden_layout(&root);
        let index_path = layout.join("index.json");
        let index = fs::read_to_string(&index_path).expect("index");

        let wrong_media = index.replacen(
            "application/vnd.oci.image.manifest.v1+json",
            "application/vnd.docker.distribution.manifest.v2+json",
            1,
        );
        fs::write(&index_path, &wrong_media).expect("write");
        assert!(matches!(
            import_bundle(&layout),
            Err(OciError::Shape { .. })
        ));

        let wrong_arch = index.replace("riscv64", "amd64");
        fs::write(&index_path, &wrong_arch).expect("write");
        assert_eq!(
            import_bundle(&layout),
            Err(OciError::UnsupportedPlatform {
                architecture: "amd64".to_owned(),
                os: "minios".to_owned(),
            })
        );

        let no_manifests = "{\"schemaVersion\":2,\"mediaType\":\"application/vnd.oci.image.index.v1+json\",\"manifests\":[]}";
        fs::write(&index_path, no_manifests).expect("write");
        assert!(matches!(
            import_bundle(&layout),
            Err(OciError::Shape { .. })
        ));
        fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn import_accepts_an_index_without_a_platform_hint() {
        let root = fixture_root();
        let layout = golden_layout(&root);
        let index_path = layout.join("index.json");
        let index = fs::read_to_string(&index_path).expect("index");
        // Tools may drop the hint when copying; the config still declares
        // the platform.
        let unhinted = index.replace(
            ",\"platform\":{\"architecture\":\"riscv64\",\"os\":\"minios\"}",
            "",
        );
        assert!(unhinted.len() < index.len());
        fs::write(&index_path, &unhinted).expect("write");

        let restored = import_bundle(&layout).expect("unhinted index imports");
        assert_eq!(restored, golden_bundle());
        fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn import_rejects_a_config_name_mismatch() {
        let root = fixture_root();
        let layout = golden_layout(&root);
        // Rewrite the config with a hostile Entrypoint and re-pin the
        // manifest and index digests so only the cross-check can catch it.
        let evil = "{\"architecture\":\"riscv64\",\"os\":\"minios\",\"config\":{\"Entrypoint\":[\"evil\"],\"Cmd\":[]},\"rootfs\":{\"type\":\"layers\",\"diff_ids\":[\"sha256:".to_owned()
            + LAYER_DIGEST
            + "\"]}}";
        let evil_digest = format_digest(digest_of(evil.as_bytes()));
        fs::write(layout.join(format!("blobs/sha256/{evil_digest}")), &evil).expect("evil config");
        let manifest_path = layout.join(format!("blobs/sha256/{MANIFEST_DIGEST}"));
        let manifest = fs::read_to_string(&manifest_path).expect("manifest");
        let manifest = manifest.replacen(CONFIG_DIGEST, &evil_digest, 1);
        let manifest = manifest.replacen("\"size\":197", &format!("\"size\":{}", evil.len()), 1);
        let repinned = format_digest(digest_of(manifest.as_bytes()));
        fs::write(layout.join(format!("blobs/sha256/{repinned}")), &manifest)
            .expect("repinned manifest");
        let index_path = layout.join("index.json");
        let index = fs::read_to_string(&index_path).expect("index");
        let index = index.replacen(MANIFEST_DIGEST, &repinned, 1);
        let index = index.replacen("\"size\":411", &format!("\"size\":{}", manifest.len()), 1);
        fs::write(&index_path, &index).expect("repinned index");

        assert!(matches!(
            import_bundle(&layout),
            Err(OciError::ConfigMismatch { .. })
        ));
        fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn import_rejects_missing_and_oversized_files() {
        let root = fixture_root();
        let layout = golden_layout(&root);
        fs::remove_file(layout.join("index.json")).expect("remove index");
        assert!(matches!(
            import_bundle(&layout),
            Err(OciError::Layout { .. })
        ));
        assert!(matches!(
            import_bundle(&root.join("absent")),
            Err(OciError::Layout { .. })
        ));
        fs::remove_dir_all(&root).expect("cleanup");
    }

    fn fixture_root() -> PathBuf {
        static NEXT_FIXTURE_ID: AtomicU64 = AtomicU64::new(0);
        let id = NEXT_FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "minicontainer-oci-import-{}-{id}",
            std::process::id()
        ))
    }
}
