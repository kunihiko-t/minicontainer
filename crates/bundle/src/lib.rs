//! MiniBundle construction and validation boundary.

#![forbid(unsafe_code)]

mod error;
mod manifest;
mod store;

use minios_abi::{
    boot::{BOOT_HEADER_LEN, BUNDLE_MAX_LEN, BootHeader, ByteRange},
    manifest::Manifest,
};
use sha2::{Digest, Sha256};

pub use error::{BundleError, StoreError};
pub use store::Store;

/// MiniBundle全体の公開上限。pin留めABIのboot windowと同一である。
pub const MAX_BUNDLE_LEN: u64 = BUNDLE_MAX_LEN;

/// digestをstore pathやtag、CLI表示で共有する小文字hex64桁へ変換する。
pub fn format_digest(digest: [u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

/// Source fields used to construct a canonical MiniBundle.
pub struct ImageSpec<'a> {
    pub name: &'a str,
    pub args: &'a [&'a str],
    pub elf: &'a [u8],
}

/// A validated MiniBundle borrowing its manifest and ELF from the input bytes.
pub struct Bundle<'a> {
    pub header: BootHeader,
    pub manifest: Manifest<'a>,
    pub elf: &'a [u8],
}

/// Builds a canonical MiniBundle from a manifest specification and ELF bytes.
pub fn build(spec: ImageSpec<'_>) -> Result<Vec<u8>, BundleError> {
    let manifest = manifest::encode_manifest(&spec)?;
    let manifest_end = BOOT_HEADER_LEN
        .checked_add(manifest.len())
        .ok_or(BundleError::LengthOverflow)?;
    let padding_len = (8 - manifest_end % 8) % 8;
    let elf_offset = manifest_end
        .checked_add(padding_len)
        .ok_or(BundleError::LengthOverflow)?;
    let total_len = elf_offset
        .checked_add(spec.elf.len())
        .ok_or(BundleError::LengthOverflow)?;
    let total_len_u64 = u64::try_from(total_len).map_err(|_| BundleError::LengthOverflow)?;
    if total_len_u64 > BUNDLE_MAX_LEN {
        return Err(BundleError::TooLarge);
    }

    let mut header = BootHeader {
        total_len: total_len_u64,
        manifest: ByteRange {
            offset: BOOT_HEADER_LEN as u64,
            len: u64::try_from(manifest.len()).map_err(|_| BundleError::LengthOverflow)?,
        },
        elf: ByteRange {
            offset: u64::try_from(elf_offset).map_err(|_| BundleError::LengthOverflow)?,
            len: u64::try_from(spec.elf.len()).map_err(|_| BundleError::LengthOverflow)?,
        },
        digest: [0; 32],
    };

    let mut bytes = vec![0; elf_offset];
    bytes[..BOOT_HEADER_LEN].copy_from_slice(&header.encode_with_zero_digest());
    bytes[BOOT_HEADER_LEN..manifest_end].copy_from_slice(&manifest);
    bytes.extend_from_slice(spec.elf);

    header.digest = digest_for(&header, &bytes[BOOT_HEADER_LEN..]);
    bytes[..BOOT_HEADER_LEN].copy_from_slice(&header.encode());
    Ok(bytes)
}

/// Parses and validates a MiniBundle while borrowing its variable regions.
pub fn parse(bytes: &[u8]) -> Result<Bundle<'_>, BundleError> {
    let header_bytes = bytes.get(..BOOT_HEADER_LEN).ok_or(BundleError::Header(
        minios_abi::boot::BootHeaderError::WrongLength,
    ))?;
    let header = BootHeader::decode(header_bytes).map_err(BundleError::Header)?;
    let actual_len = u64::try_from(bytes.len()).map_err(|_| BundleError::LengthOverflow)?;
    if header.total_len != actual_len {
        return Err(BundleError::LengthMismatch {
            declared: header.total_len,
            actual: bytes.len(),
        });
    }

    let actual_digest = digest_for(&header, &bytes[BOOT_HEADER_LEN..]);
    if actual_digest != header.digest {
        return Err(BundleError::DigestMismatch);
    }

    let manifest_start =
        usize::try_from(header.manifest.offset).map_err(|_| BundleError::LengthOverflow)?;
    let manifest_end = usize::try_from(
        header
            .manifest
            .offset
            .checked_add(header.manifest.len)
            .ok_or(BundleError::LengthOverflow)?,
    )
    .map_err(|_| BundleError::LengthOverflow)?;
    let elf_start = usize::try_from(header.elf.offset).map_err(|_| BundleError::LengthOverflow)?;
    let manifest_bytes = bytes
        .get(manifest_start..manifest_end)
        .ok_or(BundleError::LengthOverflow)?;
    let padding = bytes
        .get(manifest_end..elf_start)
        .ok_or(BundleError::LengthOverflow)?;
    if padding.iter().any(|byte| *byte != 0) {
        return Err(BundleError::NonZeroPadding);
    }
    let elf = bytes.get(elf_start..).ok_or(BundleError::LengthOverflow)?;
    let manifest = Manifest::parse(manifest_bytes).map_err(BundleError::Manifest)?;

    Ok(Bundle {
        header,
        manifest,
        elf,
    })
}

fn digest_for(header: &BootHeader, variable_bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(header.encode_with_zero_digest());
    hasher.update(variable_bytes);
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use minios_abi::{boot::BootHeaderError, manifest::ManifestError};

    // Production break caught: build or parse stops preserving canonical manifest fields and ELF bytes.
    #[test]
    fn builds_and_parses_canonical_bundle() {
        let args = ["first", "second"];
        let elf = b"ELF fixture bytes";
        let bytes = build(ImageSpec {
            name: "hello",
            args: &args,
            elf,
        })
        .unwrap();

        let bundle = parse(&bytes).unwrap();

        assert_eq!(bundle.manifest.name(), "hello");
        assert_eq!(bundle.manifest.args().collect::<Vec<_>>(), args);
        assert_eq!(bundle.elf, elf);
        assert_eq!(bytes.len() % 8, elf.len() % 8);
    }

    // Production break caught: digest validation omits the header, manifest, padding, or ELF region.
    #[test]
    fn rejects_corruption_in_every_digest_region() {
        let bytes = build(ImageSpec {
            name: "a",
            args: &[],
            elf: b"12345678",
        })
        .unwrap();
        let cases = [
            ("header", 56),
            ("manifest", BOOT_HEADER_LEN),
            ("padding", 113),
            ("ELF", 120),
        ];

        for (case, offset) in cases {
            let mut corrupted = bytes.clone();
            corrupted[offset] ^= 1;
            assert!(
                matches!(parse(&corrupted), Err(BundleError::DigestMismatch)),
                "{case}"
            );
        }
    }

    // Production break caught: the on-disk v1 header layout drifts from the pinned ABI bytes.
    #[test]
    fn canonical_header_prefix_matches_golden_bytes() {
        let header = BootHeader {
            total_len: 120,
            manifest: ByteRange { offset: 96, len: 8 },
            elf: ByteRange {
                offset: 104,
                len: 16,
            },
            digest: [0; 32],
        };
        let bytes = header.encode_with_zero_digest();

        assert_eq!(
            &bytes[..56],
            &[
                b'M', b'I', b'N', b'I', b'C', b'T', b'R', 0, 1, 0, 0, 0, 96, 0, 0, 0, 120, 0, 0, 0,
                0, 0, 0, 0, 96, 0, 0, 0, 0, 0, 0, 0, 8, 0, 0, 0, 0, 0, 0, 0, 104, 0, 0, 0, 0, 0, 0,
                0, 16, 0, 0, 0, 0, 0, 0, 0,
            ],
        );
    }

    // Production break caught: SHA-256 excludes bytes or hashes the stored digest instead of a zeroed digest field.
    #[test]
    fn digest_matches_the_hand_derived_domain() {
        let bytes = build(ImageSpec {
            name: "a",
            args: &[],
            elf: b"",
        })
        .unwrap();

        assert_eq!(bytes.len(), 120);
        assert_eq!(
            &bytes[56..88],
            &[
                0xd2, 0xe0, 0xc6, 0x02, 0xac, 0xbf, 0x71, 0x1b, 0x5d, 0x1c, 0xb7, 0xa2, 0xae, 0x07,
                0xdd, 0x19, 0xd9, 0xed, 0xa0, 0xf6, 0x8c, 0xb7, 0x36, 0xa5, 0x07, 0xb9, 0x7a, 0x73,
                0x9e, 0xa9, 0x7d, 0x48,
            ],
        );
        assert_eq!(&bytes[96..113], b"version=1\nname=a\n");
        assert_eq!(&bytes[113..120], &[0; 7]);
    }

    // Production break caught: builder rejects the largest payload that still fits the 8 MiB boot window.
    #[test]
    fn accepts_the_maximum_total_bundle_length() {
        let elf = vec![0x5a; BUNDLE_MAX_LEN as usize - 120];

        let bytes = build(ImageSpec {
            name: "a",
            args: &[],
            elf: &elf,
        })
        .unwrap();

        assert_eq!(bytes.len(), 8 * 1024 * 1024);
        assert_eq!(parse(&bytes).unwrap().elf.len(), 8 * 1024 * 1024 - 120);
    }

    // Production break caught: builder writes an ELF larger than the entire 8 MiB boot window.
    #[test]
    fn rejects_an_elf_one_byte_larger_than_the_boot_window() {
        let elf = vec![0; BUNDLE_MAX_LEN as usize + 1];

        assert_eq!(
            build(ImageSpec {
                name: "a",
                args: &[],
                elf: &elf,
            }),
            Err(BundleError::TooLarge)
        );
    }

    // Production break caught: parser accepts non-zero alignment bytes after a valid digest is recomputed.
    #[test]
    fn rejects_non_zero_padding_in_an_otherwise_valid_bundle() {
        let mut bytes = build(ImageSpec {
            name: "a",
            args: &[],
            elf: b"",
        })
        .unwrap();
        bytes[113] = 1;
        bytes[56..88].copy_from_slice(&[
            0x10, 0x74, 0x58, 0xc1, 0xd1, 0x6f, 0x33, 0xe1, 0x58, 0x45, 0xe3, 0x3e, 0x00, 0xf7,
            0xb7, 0x9c, 0x5e, 0xd6, 0xb0, 0x9b, 0x35, 0xd2, 0xe5, 0x84, 0x9d, 0xf9, 0x4d, 0xff,
            0x3b, 0xca, 0xa0, 0xfd,
        ]);

        assert!(matches!(parse(&bytes), Err(BundleError::NonZeroPadding)));
    }

    // Production break caught: parser reads variable regions before rejecting a short or malformed fixed header.
    #[test]
    fn propagates_malformed_header_errors() {
        assert!(matches!(
            parse(&[0; BOOT_HEADER_LEN - 1]),
            Err(BundleError::Header(BootHeaderError::WrongLength))
        ));

        let mut wrong_magic = build(ImageSpec {
            name: "a",
            args: &[],
            elf: b"",
        })
        .unwrap();
        wrong_magic[0] = b'X';
        assert!(matches!(
            parse(&wrong_magic),
            Err(BundleError::Header(BootHeaderError::WrongMagic))
        ));
    }

    // Production break caught: parser hashes bytes before validating header layout and declared total length.
    #[test]
    fn rejects_malformed_layout_and_total_length() {
        let bytes = build(ImageSpec {
            name: "a",
            args: &[],
            elf: b"",
        })
        .unwrap();

        let mut unaligned_elf = bytes.clone();
        unaligned_elf[40..48].copy_from_slice(&121_u64.to_le_bytes());
        assert!(matches!(
            parse(&unaligned_elf),
            Err(BundleError::Header(BootHeaderError::ElfNotAligned))
        ));

        let mut trailing_byte = bytes;
        trailing_byte.push(0);
        assert!(matches!(
            parse(&trailing_byte),
            Err(BundleError::LengthMismatch {
                declared: 120,
                actual: 121,
            })
        ));
    }

    // Production break caught: parser accepts an ABI-invalid manifest once its digest is internally consistent.
    #[test]
    fn propagates_manifest_errors_after_integrity_checks() {
        let mut bytes = build(ImageSpec {
            name: "a",
            args: &[],
            elf: b"",
        })
        .unwrap();
        bytes[111] = b'/';
        bytes[56..88].copy_from_slice(&[
            0xde, 0xfd, 0x55, 0x0d, 0x22, 0x4b, 0x3f, 0x41, 0x99, 0x1a, 0xc8, 0x08, 0xca, 0xc4,
            0xbd, 0x7a, 0xa1, 0xce, 0x01, 0x01, 0xa9, 0xaf, 0x93, 0x70, 0xe4, 0x78, 0xee, 0xc4,
            0xef, 0x9f, 0x9f, 0xb5,
        ]);

        assert!(matches!(
            parse(&bytes),
            Err(BundleError::Manifest(ManifestError::InvalidName))
        ));
    }

    // Production break caught: builder bypasses the pinned manifest grammar for names and arguments.
    #[test]
    fn builder_propagates_manifest_validation_errors() {
        assert_eq!(
            build(ImageSpec {
                name: "../escape",
                args: &[],
                elf: b"elf",
            }),
            Err(BundleError::Manifest(ManifestError::InvalidName))
        );
    }

    // Production break caught: an embedded LF lets one source argument encode as multiple manifest arguments.
    #[test]
    fn rejects_arguments_that_cannot_round_trip_canonically() {
        let injected = ["first\narg=second"];
        assert_eq!(
            build(ImageSpec {
                name: "a",
                args: &injected,
                elf: b"elf",
            }),
            Err(BundleError::ArgumentContainsLf { index: 0 })
        );

        let canonical = ["first", "arg=second"];
        let bytes = build(ImageSpec {
            name: "a",
            args: &canonical,
            elf: b"elf",
        })
        .unwrap();
        let parsed = parse(&bytes).unwrap();
        assert_eq!(parsed.manifest.args().collect::<Vec<_>>(), canonical);
        assert_eq!(parsed.manifest.args().count(), 2);
    }

    // Production break caught: the host emits a source argument containing CR
    // instead of surfacing the pinned ABI's typed argument-byte error.
    #[test]
    fn builder_rejects_carriage_return_with_the_abi_manifest_error() {
        assert_eq!(
            build(ImageSpec {
                name: "a",
                args: &["first\rsecond"],
                elf: b"elf",
            }),
            Err(BundleError::Manifest(
                ManifestError::ArgumentContainsCarriageReturn
            ))
        );
    }

    // Production break caught: the published bundle limit drifts from
    // the pinned ABI boot window that builders and CLIs enforce.
    #[test]
    fn published_bundle_limit_matches_the_pinned_abi() {
        assert_eq!(MAX_BUNDLE_LEN, BUNDLE_MAX_LEN);
        assert_eq!(MAX_BUNDLE_LEN, 8 * 1024 * 1024);
    }

    // Production break caught: digest display diverges from the lowercase
    // hex shared by store paths, tag files, and CLI success output.
    #[test]
    fn formats_digests_as_lowercase_hex() {
        assert_eq!(
            format_digest([
                0xd2, 0xe0, 0xc6, 0x02, 0xac, 0xbf, 0x71, 0x1b, 0x5d, 0x1c, 0xb7, 0xa2, 0xae, 0x07,
                0xdd, 0x19, 0xd9, 0xed, 0xa0, 0xf6, 0x8c, 0xb7, 0x36, 0xa5, 0x07, 0xb9, 0x7a, 0x73,
                0x9e, 0xa9, 0x7d, 0x48,
            ]),
            "d2e0c602acbf711b5d1cb7a2ae07dd19d9eda0f68cb736a507b97a739ea97d48"
        );
        assert_eq!(
            format_digest([0; 32]),
            "0000000000000000000000000000000000000000000000000000000000000000"
        );
        assert_eq!(
            format_digest([0xff; 32]),
            "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
        );
    }

    // Production break caught: LF splitting lets an overlong logical source argument bypass ARG_MAX_LEN.
    #[test]
    fn rejects_overlong_source_argument_even_if_lf_would_split_it() {
        let argument = format!("{}\narg=", "a".repeat(256));
        assert_eq!(argument.len(), 261);
        assert_eq!(
            build(ImageSpec {
                name: "a",
                args: &[&argument],
                elf: b"elf",
            }),
            Err(BundleError::Manifest(ManifestError::ArgumentTooLong))
        );
    }
}
