use crate::{BundleError, ImageSpec};
use minios_abi::manifest::{
    ARG_MAX_COUNT, ARG_MAX_LEN, MANIFEST_MAX_LEN, Manifest, ManifestError, NAME_MAX_LEN,
};

const VERSION_LINE: &[u8] = b"version=1\n";
const NAME_PREFIX: &[u8] = b"name=";
const ARG_PREFIX: &[u8] = b"arg=";

pub(crate) fn encode_manifest(spec: &ImageSpec<'_>) -> Result<Vec<u8>, BundleError> {
    let encoded_len = preflight_manifest_len(spec)?;
    let mut bytes = Vec::with_capacity(encoded_len);
    bytes.extend_from_slice(VERSION_LINE);
    bytes.extend_from_slice(NAME_PREFIX);
    bytes.extend_from_slice(spec.name.as_bytes());
    bytes.push(b'\n');
    for argument in spec.args {
        bytes.extend_from_slice(ARG_PREFIX);
        bytes.extend_from_slice(argument.as_bytes());
        bytes.push(b'\n');
    }
    Manifest::parse(&bytes).map_err(BundleError::Manifest)?;
    Ok(bytes)
}

fn preflight_manifest_len(spec: &ImageSpec<'_>) -> Result<usize, BundleError> {
    if spec.name.len() > NAME_MAX_LEN {
        return Err(BundleError::Manifest(ManifestError::NameTooLong));
    }
    if spec.name.as_bytes().contains(&b'\n') {
        return Err(BundleError::Manifest(ManifestError::InvalidName));
    }
    if spec.args.len() > ARG_MAX_COUNT {
        return Err(BundleError::Manifest(ManifestError::TooManyArgs));
    }

    let mut encoded_len = VERSION_LINE
        .len()
        .checked_add(NAME_PREFIX.len())
        .and_then(|length| length.checked_add(spec.name.len()))
        .and_then(|length| length.checked_add(1))
        .ok_or(BundleError::LengthOverflow)?;
    for (index, argument) in spec.args.iter().enumerate() {
        if argument.len() > ARG_MAX_LEN {
            return Err(BundleError::Manifest(ManifestError::ArgumentTooLong));
        }
        if argument.as_bytes().contains(&b'\n') {
            return Err(BundleError::ArgumentContainsLf { index });
        }
        if argument.as_bytes().contains(&b'\r') {
            return Err(BundleError::Manifest(
                ManifestError::ArgumentContainsCarriageReturn,
            ));
        }
        encoded_len = encoded_len
            .checked_add(ARG_PREFIX.len())
            .and_then(|length| length.checked_add(argument.len()))
            .and_then(|length| length.checked_add(1))
            .ok_or(BundleError::LengthOverflow)?;
        if encoded_len > MANIFEST_MAX_LEN {
            return Err(BundleError::Manifest(ManifestError::TooLong));
        }
    }
    Ok(encoded_len)
}

pub(crate) fn validate_name(name: &str) -> Result<(), ManifestError> {
    if name.len() > NAME_MAX_LEN {
        return Err(ManifestError::NameTooLong);
    }
    if name.as_bytes().contains(&b'\n') {
        return Err(ManifestError::InvalidName);
    }
    let encoded_len = VERSION_LINE
        .len()
        .checked_add(NAME_PREFIX.len())
        .and_then(|length| length.checked_add(name.len()))
        .and_then(|length| length.checked_add(1))
        .ok_or(ManifestError::TooLong)?;
    let mut bytes = Vec::with_capacity(encoded_len);
    bytes.extend_from_slice(VERSION_LINE);
    bytes.extend_from_slice(NAME_PREFIX);
    bytes.extend_from_slice(name.as_bytes());
    bytes.push(b'\n');
    Manifest::parse(&bytes).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use minios_abi::manifest::ManifestError;

    // Production break caught: encoder allocates a 4,097-byte manifest before detecting the exact 4 KiB limit.
    #[test]
    fn preflights_exact_manifest_size_boundary() {
        let full = "a".repeat(256);
        let boundary = "b".repeat(159);
        let too_long = "b".repeat(160);
        let mut args = vec![full.as_str(); 15];
        args.push(boundary.as_str());
        let at_limit = ImageSpec {
            name: "a",
            args: &args,
            elf: b"",
        };
        assert_eq!(preflight_manifest_len(&at_limit), Ok(4_096));
        assert_eq!(encode_manifest(&at_limit).unwrap().len(), 4_096);

        args.pop();
        args.push(too_long.as_str());
        let over_limit = ImageSpec {
            name: "a",
            args: &args,
            elf: b"",
        };
        assert_eq!(
            preflight_manifest_len(&over_limit),
            Err(BundleError::Manifest(ManifestError::TooLong))
        );
        assert_eq!(
            encode_manifest(&over_limit),
            Err(BundleError::Manifest(ManifestError::TooLong))
        );
    }

    // Production break caught: preflight omits name, argument-count, or per-argument source limits.
    #[test]
    fn preflights_field_and_count_limits() {
        let long_name = "n".repeat(129);
        assert_eq!(
            preflight_manifest_len(&ImageSpec {
                name: &long_name,
                args: &[],
                elf: b"",
            }),
            Err(BundleError::Manifest(ManifestError::NameTooLong))
        );

        let empty_args = [""; 17];
        assert_eq!(
            preflight_manifest_len(&ImageSpec {
                name: "a",
                args: &empty_args,
                elf: b"",
            }),
            Err(BundleError::Manifest(ManifestError::TooManyArgs))
        );

        let long_argument = "a".repeat(257);
        assert_eq!(
            preflight_manifest_len(&ImageSpec {
                name: "a",
                args: &[&long_argument],
                elf: b"",
            }),
            Err(BundleError::Manifest(ManifestError::ArgumentTooLong))
        );
    }

    // Production break caught: a source CR reaches manifest allocation before
    // the host surfaces the pinned ABI's argument-byte diagnostic.
    #[test]
    fn preflight_rejects_carriage_return_with_the_abi_error() {
        assert_eq!(
            preflight_manifest_len(&ImageSpec {
                name: "a",
                args: &["first\rsecond"],
                elf: b"",
            }),
            Err(BundleError::Manifest(
                ManifestError::ArgumentContainsCarriageReturn
            ))
        );
    }

    // Production break caught: tag validation constructs an oversized manifest before rejecting a 129-byte name.
    #[test]
    fn rejects_oversized_tag_name_at_the_field_boundary() {
        let at_limit = "a".repeat(128);
        assert_eq!(validate_name(&at_limit), Ok(()));

        let too_long = "a".repeat(129);
        assert_eq!(validate_name(&too_long), Err(ManifestError::NameTooLong));
    }
}
