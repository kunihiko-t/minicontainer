//! Canonical JSON writer for the OCI descriptors this crate emits.
//!
//! The byte rules follow `docs/reference/minibundle-oci-mapping.md`: fixed
//! key order, `,` and `:` separators without whitespace, no trailing newline,
//! and pinned string escaping. Import-side parsing arrives with #17.

use crate::{
    ARCHITECTURE, MEDIA_TYPE_CONFIG, MEDIA_TYPE_INDEX, MEDIA_TYPE_LAYER, MEDIA_TYPE_MANIFEST, OS,
};

/// Renders the exact `config` blob bytes for one bundle identity.
pub fn render_config(name: &str, args: &[&str], layer_digest: &str) -> String {
    let mut rendered = String::from("{\"architecture\":\"");
    rendered.push_str(ARCHITECTURE);
    rendered.push_str("\",\"os\":\"");
    rendered.push_str(OS);
    rendered.push_str("\",\"config\":{\"Entrypoint\":[\"");
    rendered.push_str(&escape(name));
    rendered.push_str("\"],\"Cmd\":[");
    for (index, argument) in args.iter().enumerate() {
        if index > 0 {
            rendered.push(',');
        }
        rendered.push('"');
        rendered.push_str(&escape(argument));
        rendered.push('"');
    }
    rendered.push_str("]},\"rootfs\":{\"type\":\"layers\",\"diff_ids\":[\"sha256:");
    rendered.push_str(layer_digest);
    rendered.push_str("\"]}}");
    rendered
}

/// Renders the exact image manifest bytes for one config and one layer.
pub fn render_manifest(
    config_digest: &str,
    config_size: u64,
    layer_digest: &str,
    layer_size: u64,
) -> String {
    format!(
        "{{\"schemaVersion\":2,\"mediaType\":\"{MEDIA_TYPE_MANIFEST}\",\"config\":{{\"mediaType\":\"{MEDIA_TYPE_CONFIG}\",\"size\":{config_size},\"digest\":\"sha256:{config_digest}\"}},\"layers\":[{{\"mediaType\":\"{MEDIA_TYPE_LAYER}\",\"size\":{layer_size},\"digest\":\"sha256:{layer_digest}\"}}]}}"
    )
}

/// Renders the exact `index.json` bytes for one manifest.
pub fn render_index(manifest_digest: &str, manifest_size: u64) -> String {
    format!(
        "{{\"schemaVersion\":2,\"mediaType\":\"{MEDIA_TYPE_INDEX}\",\"manifests\":[{{\"mediaType\":\"{MEDIA_TYPE_MANIFEST}\",\"size\":{manifest_size},\"digest\":\"sha256:{manifest_digest}\",\"platform\":{{\"architecture\":\"{ARCHITECTURE}\",\"os\":\"{OS}\"}}}}]}}",
    )
}

/// Renders the exact `oci-layout` bytes.
pub fn render_oci_layout() -> String {
    format!(
        "{{\"imageLayoutVersion\":\"{version}\"}}",
        version = crate::OCI_LAYOUT_VERSION
    )
}

/// Escapes one JSON string with the pinned rules: `"` and `\` are
/// backslash-escaped, control bytes become lowercase `\u00XX`, `/` stays
/// unescaped, and non-ASCII bytes pass through as UTF-8.
pub fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for token in text.chars() {
        match token {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\u{0}'..='\u{1f}' => {
                let byte = token as u8;
                escaped.push_str("\\u00");
                escaped.push(char::from_digit((byte >> 4) as u32, 16).expect("hex digit"));
                escaped.push(char::from_digit((byte & 0x0f) as u32, 16).expect("hex digit"));
            }
            _ => escaped.push(token),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping_pins_quotes_backslashes_and_controls() {
        assert_eq!(escape("hello"), "hello");
        assert_eq!(escape("a\"b\\c"), "a\\\"b\\\\c");
        assert_eq!(escape("a/b"), "a/b");
        assert_eq!(escape("a\x01b\x1fz"), "a\\u0001b\\u001fz");
        assert_eq!(escape("あ"), "あ");
    }

    #[test]
    fn config_renders_the_pinned_key_order() {
        assert_eq!(
            render_config("hello", &[], &"ab".repeat(32)),
            format!(
                "{{\"architecture\":\"riscv64\",\"os\":\"minios\",\"config\":{{\"Entrypoint\":[\"hello\"],\"Cmd\":[]}},\"rootfs\":{{\"type\":\"layers\",\"diff_ids\":[\"sha256:{}\"]}}}}",
                "ab".repeat(32)
            )
        );
        assert_eq!(
            render_config("n", &["a", "b\"c"], "00"),
            "{\"architecture\":\"riscv64\",\"os\":\"minios\",\"config\":{\"Entrypoint\":[\"n\"],\"Cmd\":[\"a\",\"b\\\"c\"]},\"rootfs\":{\"type\":\"layers\",\"diff_ids\":[\"sha256:00\"]}}"
        );
    }

    #[test]
    fn manifest_index_and_layout_render_byte_exact_bytes() {
        assert_eq!(
            render_manifest(&"c".repeat(64), 197, &"d".repeat(64), 136),
            format!(
                "{{\"schemaVersion\":2,\"mediaType\":\"{MEDIA_TYPE_MANIFEST}\",\"config\":{{\"mediaType\":\"{MEDIA_TYPE_CONFIG}\",\"size\":197,\"digest\":\"sha256:{}\"}},\"layers\":[{{\"mediaType\":\"{MEDIA_TYPE_LAYER}\",\"size\":136,\"digest\":\"sha256:{}\"}}]}}",
                "c".repeat(64),
                "d".repeat(64)
            )
        );
        assert_eq!(
            render_index(&"e".repeat(64), 411),
            format!(
                "{{\"schemaVersion\":2,\"mediaType\":\"{MEDIA_TYPE_INDEX}\",\"manifests\":[{{\"mediaType\":\"{MEDIA_TYPE_MANIFEST}\",\"size\":411,\"digest\":\"sha256:{}\",\"platform\":{{\"architecture\":\"riscv64\",\"os\":\"minios\"}}}}]}}",
                "e".repeat(64)
            )
        );
        assert_eq!(render_oci_layout(), "{\"imageLayoutVersion\":\"1.0.0\"}");
    }
}
