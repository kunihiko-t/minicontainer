//! Builds one deterministic distribution archive from prebuilt inputs.
//!
//! `dist` never compiles anything. The caller builds `minictr` and the miniOS
//! kernel first; this command combines those inputs with the workspace
//! licenses and generated metadata, archives everything with an entry order
//! fixed by [`STAGED_FILES`], and re-reads the finished archive before
//! reporting success. The archive format is plain `ustar` compressed with a
//! minimal stored-blocks gzip writer, so release builds need no archiver
//! beyond the Rust toolchain.

use crate::cli::DistArgs;
use sha2::{Digest, Sha256};
use std::{
    fmt, fs,
    path::{Path, PathBuf},
};

/// Archive metadata format version stored in `MANIFEST.txt`.
pub const DIST_MANIFEST_VERSION: u32 = 1;
/// Fixed permission bits of the archived `minictr` binary.
pub const MINICTR_MODE: u32 = 0o755;
/// Fixed permission bits of every other archived file.
pub const DATA_MODE: u32 = 0o644;
/// Fixed permission bits of archived directories.
pub const DIR_MODE: u32 = 0o755;

/// Staged payload files in archive order, relative to the archive top
/// directory. The order is part of the format: the writer and the readback
/// verification both follow this table.
pub const STAGED_FILES: [(&str, u32); 4] = [
    ("minictr", MINICTR_MODE),
    ("kernel/minios.bin", DATA_MODE),
    ("LICENSE-MIT", DATA_MODE),
    ("LICENSE-APACHE", DATA_MODE),
];

/// One staged payload file: archive-relative name, mode, and bytes.
struct StagedFile {
    name: &'static str,
    mode: u32,
    bytes: Vec<u8>,
}

/// Builds and verifies one distribution archive.
pub fn run(workspace: &Path, args: &DistArgs) -> Result<(), DistError> {
    let version = args.version();
    let target = args.target.as_str();
    reject_token(version, "version")?;
    reject_token(target, "target")?;
    let minictr = read_input(&args.minictr, "minictr")?;
    let kernel = read_input(&args.kernel, "kernel")?;
    let license_mit = read_input(&workspace.join("LICENSE-MIT"), "license")?;
    let license_apache = read_input(&workspace.join("LICENSE-APACHE"), "license")?;
    let output = args.output(workspace);
    fs::create_dir_all(&output).map_err(|error| DistError::CreateOutput {
        path: output.clone(),
        message: error.to_string(),
    })?;

    let name = format!("minicontainer-{version}-{target}");
    let staged = staged_payloads(minictr, kernel, license_mit, license_apache);
    let manifest = render_manifest(version, target, &staged);
    let sums = render_sums(&staged, &hex_digest(manifest.as_bytes()));

    let tar = write_tar(&name, &staged, manifest.as_bytes(), sums.as_bytes())?;
    let archive = gzip_stored(&tar);
    let archive_name = format!("{name}.tar.gz");
    let archive_path = output.join(&archive_name);
    write_file(&output, &archive_name, &archive)?;
    let archive_sum = hex_digest(&archive);
    write_file(
        &output,
        &format!("{archive_name}.sha256"),
        format!("{archive_sum}  {archive_name}\n").as_bytes(),
    )?;

    let entries = verify_archive(&archive_path, &name, version, target)?;
    println!(
        "dist: wrote {} ({} bytes)",
        archive_path.display(),
        archive.len()
    );
    println!("dist: sha256 {archive_sum}");
    println!("dist: verified {entries} archive entries");
    Ok(())
}

/// A typed `dist` failure.
#[derive(Debug, Eq, PartialEq)]
pub enum DistError {
    /// An input path does not exist.
    MissingInput {
        /// Input role: `minictr`, `kernel`, or `license`.
        role: &'static str,
        /// Supplied input path.
        path: PathBuf,
    },
    /// An input path is not a regular file.
    NotRegularFile {
        /// Input role: `minictr`, `kernel`, or `license`.
        role: &'static str,
        /// Supplied input path.
        path: PathBuf,
    },
    /// An input file cannot be read.
    ReadInput {
        /// Input role: `minictr`, `kernel`, or `license`.
        role: &'static str,
        /// Supplied input path.
        path: PathBuf,
        /// Operating-system error message.
        message: String,
    },
    /// A version or target token is empty or carries unsafe characters.
    InvalidToken {
        /// Token role: `version` or `target`.
        role: &'static str,
        /// Supplied token.
        token: String,
    },
    /// The output directory cannot be created.
    CreateOutput {
        /// Supplied output directory.
        path: PathBuf,
        /// Operating-system error message.
        message: String,
    },
    /// An emitted file cannot be written.
    WriteFile {
        /// Destination path.
        path: PathBuf,
        /// Operating-system error message.
        message: String,
    },
    /// The finished archive fails the readback verification.
    VerifyFailed {
        /// Mismatch description.
        message: String,
    },
}

impl fmt::Display for DistError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingInput { role, path } => {
                write!(formatter, "dist input {role} not found: {}", path.display())
            }
            Self::NotRegularFile { role, path } => {
                write!(
                    formatter,
                    "dist input {role} is not a regular file: {}",
                    path.display()
                )
            }
            Self::ReadInput {
                role,
                path,
                message,
            } => {
                write!(
                    formatter,
                    "dist input {role} cannot be read ({}): {message}",
                    path.display()
                )
            }
            Self::InvalidToken { role, token } => {
                write!(formatter, "dist {role} is invalid: {token}")
            }
            Self::CreateOutput { path, message } => {
                write!(
                    formatter,
                    "dist output directory cannot be created ({}): {message}",
                    path.display()
                )
            }
            Self::WriteFile { path, message } => {
                write!(
                    formatter,
                    "dist file cannot be written ({}): {message}",
                    path.display()
                )
            }
            Self::VerifyFailed { message } => {
                write!(formatter, "dist archive verification failed: {message}")
            }
        }
    }
}

impl std::error::Error for DistError {}

/// Reads one prebuilt input. Symlinks resolve to their target; anything that
/// is not a regular file is rejected so directories and devices never enter
/// an archive silently.
fn read_input(path: &Path, role: &'static str) -> Result<Vec<u8>, DistError> {
    match fs::metadata(path) {
        Ok(metadata) if !metadata.file_type().is_file() => {
            return Err(DistError::NotRegularFile {
                role,
                path: path.to_owned(),
            });
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(DistError::MissingInput {
                role,
                path: path.to_owned(),
            });
        }
        Err(error) => {
            return Err(DistError::ReadInput {
                role,
                path: path.to_owned(),
                message: error.to_string(),
            });
        }
    }
    fs::read(path).map_err(|error| DistError::ReadInput {
        role,
        path: path.to_owned(),
        message: error.to_string(),
    })
}

/// Rejects tokens that could escape the archive top directory or break the
/// file name. Only ASCII word characters plus `.`, `+`, and `-` survive, and
/// the dot-only tokens stay rejected.
fn reject_token(token: &str, role: &'static str) -> Result<(), DistError> {
    let usable = !token.is_empty()
        && token != "."
        && token != ".."
        && token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'+' | b'-' | b'_'));
    if usable {
        Ok(())
    } else {
        Err(DistError::InvalidToken {
            role,
            token: token.to_owned(),
        })
    }
}

/// Renders the exact `MANIFEST.txt` bytes.
fn render_manifest(version: &str, target: &str, staged: &[StagedFile]) -> String {
    let mut manifest = format!(
        "manifest_version={DIST_MANIFEST_VERSION}\nversion={version}\ntarget={target}\nfiles:\n"
    );
    for file in staged {
        manifest.push_str(&format!(
            "{} mode={:04o} sha256={}\n",
            file.name,
            file.mode,
            hex_digest(&file.bytes)
        ));
    }
    manifest
}

/// Renders the exact `SHA256SUMS` bytes in `sha256sum -c` format.
fn render_sums(staged: &[StagedFile], manifest: &str) -> String {
    let mut sums = String::new();
    for file in staged {
        sums.push_str(&format!("{}  {}\n", hex_digest(&file.bytes), file.name));
    }
    sums.push_str(&format!("{manifest}  MANIFEST.txt\n"));
    sums
}

/// Lowercase hex SHA-256 digest.
fn hex_digest(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push(char::from_digit((byte >> 4) as u32, 16).expect("hex digit"));
        hex.push(char::from_digit((byte & 0x0f) as u32, 16).expect("hex digit"));
    }
    hex
}

/// Pairs the input bytes with the [`STAGED_FILES`] names and modes. The
/// payload order must follow the table order.
fn staged_payloads(
    minictr: Vec<u8>,
    kernel: Vec<u8>,
    license_mit: Vec<u8>,
    license_apache: Vec<u8>,
) -> Vec<StagedFile> {
    let payloads = [minictr, kernel, license_mit, license_apache];
    STAGED_FILES
        .iter()
        .zip(payloads)
        .map(|(planned, bytes)| StagedFile {
            name: planned.0,
            mode: planned.1,
            bytes,
        })
        .collect()
}

/// Writes one emitted file atomically: the bytes land in a sibling temporary
/// file first, then a rename publishes them.
fn write_file(output: &Path, name: &str, bytes: &[u8]) -> Result<(), DistError> {
    let temporary = output.join(format!(".{name}.tmp"));
    fs::write(&temporary, bytes).map_err(|error| DistError::WriteFile {
        path: temporary.clone(),
        message: error.to_string(),
    })?;
    fs::rename(&temporary, output.join(name)).map_err(|error| DistError::WriteFile {
        path: output.join(name),
        message: error.to_string(),
    })
}

/// One planned archive entry: the full entry name plus the file mode, or
/// `None` for a directory entry.
fn archive_plan(name: &str) -> Vec<(String, Option<u32>)> {
    let mut plan = vec![(format!("{name}/"), None)];
    for (file, mode) in STAGED_FILES {
        let mut prefix = String::new();
        let parts: Vec<&str> = file.split('/').collect();
        for part in &parts[..parts.len() - 1] {
            prefix.push_str(part);
            prefix.push('/');
            let dir = format!("{name}/{prefix}");
            if !plan.iter().any(|(planned, _)| planned == &dir) {
                plan.push((dir, None));
            }
        }
        plan.push((format!("{name}/{file}"), Some(mode)));
    }
    for meta in ["MANIFEST.txt", "SHA256SUMS"] {
        plan.push((format!("{name}/{meta}"), Some(DATA_MODE)));
    }
    plan
}

/// Serializes the payload as deterministic `ustar`: uid/gid/mtime are zero,
/// the user/group names are empty, and the entry order follows
/// [`archive_plan`]. Every name fits the 100-byte field because version and
/// target tokens are short by policy.
fn write_tar(
    name: &str,
    staged: &[StagedFile],
    manifest: &[u8],
    sums: &[u8],
) -> Result<Vec<u8>, DistError> {
    let mut tar = Vec::new();
    for (entry, mode) in archive_plan(name) {
        let Some(mode) = mode else {
            append_dir(&mut tar, &entry)?;
            continue;
        };
        let relative = entry
            .strip_prefix(&format!("{name}/"))
            .expect("planned entry");
        let bytes = staged
            .iter()
            .find(|file| file.name == relative)
            .map(|file| file.bytes.as_slice())
            .or(match relative {
                "MANIFEST.txt" => Some(manifest),
                "SHA256SUMS" => Some(sums),
                _ => None,
            })
            .expect("planned payload");
        append_file(&mut tar, &entry, mode, bytes)?;
    }
    tar.extend_from_slice(&[0u8; 1024]);
    Ok(tar)
}

/// Appends one directory entry.
fn append_dir(tar: &mut Vec<u8>, name: &str) -> Result<(), DistError> {
    tar.extend_from_slice(&tar_header(name, DIR_MODE, 0, b'5')?);
    Ok(())
}

/// Appends one regular-file entry with zero padding to the 512-byte boundary.
fn append_file(tar: &mut Vec<u8>, name: &str, mode: u32, bytes: &[u8]) -> Result<(), DistError> {
    tar.extend_from_slice(&tar_header(name, mode, bytes.len() as u64, b'0')?);
    tar.extend_from_slice(bytes);
    tar.resize(tar.len().next_multiple_of(512), 0);
    Ok(())
}

/// Renders one 512-byte `ustar` header with fixed ownership and timestamp.
fn tar_header(name: &str, mode: u32, size: u64, kind: u8) -> Result<[u8; 512], DistError> {
    if name.len() > 100 || !name.is_ascii() {
        return Err(DistError::VerifyFailed {
            message: format!("archive entry name is not representable: {name}"),
        });
    }
    let mut header = [0u8; 512];
    header[..name.len()].copy_from_slice(name.as_bytes());
    write_octal(&mut header[100..108], mode as u64)?;
    write_octal(&mut header[108..116], 0)?;
    write_octal(&mut header[116..124], 0)?;
    write_octal(&mut header[124..136], size)?;
    write_octal(&mut header[136..148], 0)?;
    header[156] = kind;
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    let checksum: u32 = header
        .iter()
        .enumerate()
        .map(|(index, byte)| {
            if (148..156).contains(&index) {
                b' ' as u32
            } else {
                *byte as u32
            }
        })
        .sum();
    let rendered = format!("{checksum:06o}\0 ");
    header[148..156].copy_from_slice(rendered.as_bytes());
    Ok(header)
}

/// Writes an octal value into a header field, NUL-terminated. Values that
/// cannot fit are rejected instead of truncating the archive.
fn write_octal(field: &mut [u8], value: u64) -> Result<(), DistError> {
    let rendered = format!("{value:0width$o}", width = field.len() - 1);
    let bytes = rendered.as_bytes();
    if bytes.len() > field.len() - 1 {
        return Err(DistError::VerifyFailed {
            message: format!("archive value is too large: {value}"),
        });
    }
    let start = field.len() - 1 - bytes.len();
    let end = field.len() - 1;
    field[start..end].copy_from_slice(bytes);
    Ok(())
}

/// Compresses bytes with gzip using stored (uncompressed) deflate blocks.
/// Stored blocks keep the encoder small and fully deterministic; release
/// payloads are small enough that the size cost is negligible.
fn gzip_stored(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len() + 64);
    out.extend_from_slice(&[0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03]);
    if raw.is_empty() {
        out.extend_from_slice(&[0x01, 0x00, 0x00, 0xff, 0xff]);
    } else {
        for (index, chunk) in raw.chunks(65535).enumerate() {
            let last = index == raw.chunks(65535).len() - 1;
            out.push(u8::from(last));
            let len = chunk.len() as u16;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&(!len).to_le_bytes());
            out.extend_from_slice(chunk);
        }
    }
    out.extend_from_slice(&crc32(raw).to_le_bytes());
    out.extend_from_slice(&(raw.len() as u32).to_le_bytes());
    out
}

/// IEEE CRC-32, computed bit by bit. Payloads are small; clarity beats a
/// lookup table here.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = crc & 1;
            crc >>= 1;
            if mask == 1 {
                crc ^= 0xedb8_8320;
            }
        }
    }
    !crc
}

/// Parses the finished archive back: gzip framing, stored deflate blocks,
/// entry set and order, metadata consistency, and payload hashes.
fn verify_archive(
    archive: &Path,
    top: &str,
    version: &str,
    target: &str,
) -> Result<usize, DistError> {
    let bytes = fs::read(archive).map_err(|error| DistError::VerifyFailed {
        message: format!("finished archive cannot be read: {error}"),
    })?;
    let tar = gunzip_stored(&bytes)?;
    let entries = parse_tar(&tar)?;
    let expected: Vec<String> = archive_plan(top)
        .into_iter()
        .map(|(entry, _)| entry)
        .collect();
    let actual: Vec<&str> = entries.iter().map(|(name, _)| name.as_str()).collect();
    if actual != expected {
        return Err(DistError::VerifyFailed {
            message: format!("archive entries differ: {}", actual.join(", ")),
        });
    }
    let payload = |wanted: &str| {
        entries
            .iter()
            .find(|(name, _)| name == wanted)
            .map(|(_, bytes)| bytes.clone())
            .ok_or_else(|| DistError::VerifyFailed {
                message: format!("archive entry is missing: {wanted}"),
            })
    };
    let mut staged = Vec::new();
    for (name, mode) in STAGED_FILES {
        staged.push(StagedFile {
            name,
            mode,
            bytes: payload(&format!("{top}/{name}"))?,
        });
    }
    let manifest = payload(&format!("{top}/MANIFEST.txt"))?;
    let sums = payload(&format!("{top}/SHA256SUMS"))?;
    let manifest = String::from_utf8(manifest).map_err(|_| DistError::VerifyFailed {
        message: "MANIFEST.txt is not UTF-8".to_owned(),
    })?;
    let expected_manifest = render_manifest(version, target, &staged);
    if manifest != expected_manifest {
        return Err(DistError::VerifyFailed {
            message: "MANIFEST.txt does not match the archived payloads".to_owned(),
        });
    }
    let sums = String::from_utf8(sums).map_err(|_| DistError::VerifyFailed {
        message: "SHA256SUMS is not UTF-8".to_owned(),
    })?;
    let expected_sums = render_sums(&staged, &hex_digest(manifest.as_bytes()));
    if sums != expected_sums {
        return Err(DistError::VerifyFailed {
            message: "SHA256SUMS does not match the archived payloads".to_owned(),
        });
    }
    Ok(entries.len())
}

/// Decodes the gzip framing this module writes: fixed header, stored deflate
/// blocks only, then the CRC-32 and length trailer.
fn gunzip_stored(bytes: &[u8]) -> Result<Vec<u8>, DistError> {
    let corrupt = |detail: &str| DistError::VerifyFailed {
        message: format!("gzip framing is corrupt: {detail}"),
    };
    if bytes.len() < 18 {
        return Err(corrupt("truncated"));
    }
    if bytes[0..3] != [0x1f, 0x8b, 0x08] || bytes[3] != 0x00 {
        return Err(corrupt("header"));
    }
    if bytes[8] != 0x00 || bytes[9] != 0x03 {
        return Err(corrupt("header flags"));
    }
    let mut raw = Vec::new();
    let mut cursor = 10;
    loop {
        if cursor + 5 > bytes.len() {
            return Err(corrupt("truncated block"));
        }
        let last = bytes[cursor] == 0x01;
        if bytes[cursor] != 0x00 && !last {
            return Err(corrupt("only stored blocks are supported"));
        }
        let len = u16::from_le_bytes([bytes[cursor + 1], bytes[cursor + 2]]) as usize;
        let check = u16::from_le_bytes([bytes[cursor + 3], bytes[cursor + 4]]);
        if check != !(len as u16) {
            return Err(corrupt("block length"));
        }
        cursor += 5;
        if cursor + len + 8 > bytes.len() {
            return Err(corrupt("truncated payload"));
        }
        raw.extend_from_slice(&bytes[cursor..cursor + len]);
        cursor += len;
        if last {
            break;
        }
    }
    if cursor + 8 != bytes.len() {
        return Err(corrupt("trailing bytes"));
    }
    let expected_crc = u32::from_le_bytes(bytes[cursor..cursor + 4].try_into().expect("slice"));
    let expected_len = u32::from_le_bytes(bytes[cursor + 4..cursor + 8].try_into().expect("slice"));
    if expected_crc != crc32(&raw) || expected_len != raw.len() as u32 {
        return Err(corrupt("trailer"));
    }
    Ok(raw)
}

/// Parses `ustar` entries into `(name, payload)` pairs in archive order.
fn parse_tar(bytes: &[u8]) -> Result<Vec<(String, Vec<u8>)>, DistError> {
    let corrupt = |detail: &str| DistError::VerifyFailed {
        message: format!("tar framing is corrupt: {detail}"),
    };
    if !bytes.len().is_multiple_of(512) || bytes.len() < 1024 {
        return Err(corrupt("size"));
    }
    let mut entries = Vec::new();
    let mut cursor = 0;
    loop {
        if cursor + 512 > bytes.len() {
            return Err(corrupt("truncated header"));
        }
        let header = &bytes[cursor..cursor + 512];
        if header.iter().all(|byte| *byte == 0) {
            let rest = &bytes[cursor..];
            if rest.iter().all(|byte| *byte == 0) {
                break;
            }
            return Err(corrupt("zero block inside entries"));
        }
        if header[257..263] != *b"ustar\0" {
            return Err(corrupt("magic"));
        }
        let name = header[..100]
            .iter()
            .take_while(|byte| **byte != 0)
            .copied()
            .collect::<Vec<u8>>();
        let name = String::from_utf8(name).map_err(|_| corrupt("entry name"))?;
        let size = parse_octal(&header[124..136]).ok_or_else(|| corrupt("entry size"))?;
        cursor += 512;
        if cursor + size.next_multiple_of(512) > bytes.len() {
            return Err(corrupt("truncated entry"));
        }
        entries.push((name, bytes[cursor..cursor + size].to_vec()));
        cursor += size.next_multiple_of(512);
    }
    Ok(entries)
}

/// Parses a NUL- or space-terminated octal header field.
fn parse_octal(field: &[u8]) -> Option<usize> {
    let text = field
        .iter()
        .take_while(|byte| **byte != 0 && **byte != b' ')
        .copied()
        .collect::<Vec<u8>>();
    if text.is_empty() {
        return Some(0);
    }
    let mut value = 0usize;
    for digit in text {
        if !(b'0'..=b'7').contains(&digit) {
            return None;
        }
        value = value.checked_mul(8)?.checked_add((digit - b'0') as usize)?;
    }
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn archive_plan_pins_the_eight_entry_order() {
        let name = "minicontainer-0.1.0-test-target";
        let plan: Vec<String> = archive_plan(name)
            .into_iter()
            .map(|(entry, _)| entry)
            .collect();
        assert_eq!(
            plan,
            [
                format!("{name}/"),
                format!("{name}/minictr"),
                format!("{name}/kernel/"),
                format!("{name}/kernel/minios.bin"),
                format!("{name}/LICENSE-MIT"),
                format!("{name}/LICENSE-APACHE"),
                format!("{name}/MANIFEST.txt"),
                format!("{name}/SHA256SUMS"),
            ]
        );
    }

    #[test]
    fn metadata_renders_byte_exact_files() {
        let staged = [
            StagedFile {
                name: "minictr",
                mode: MINICTR_MODE,
                bytes: b"m".to_vec(),
            },
            StagedFile {
                name: "kernel/minios.bin",
                mode: DATA_MODE,
                bytes: b"k".to_vec(),
            },
        ];
        let manifest = render_manifest("0.1.0", "t", &staged);
        assert_eq!(
            manifest,
            format!(
                "manifest_version=1\nversion=0.1.0\ntarget=t\nfiles:\nminictr mode=0755 sha256={}\nkernel/minios.bin mode=0644 sha256={}\n",
                hex_digest(b"m"),
                hex_digest(b"k")
            )
        );
        assert_eq!(
            render_sums(&staged, "cc"),
            format!(
                "{}  minictr\n{}  kernel/minios.bin\ncc  MANIFEST.txt\n",
                hex_digest(b"m"),
                hex_digest(b"k")
            )
        );
    }

    #[test]
    fn tokens_reject_empty_dot_and_unsafe_characters() {
        for token in ["", ".", "..", "a/b", "a\\b", "../x", "v 1", "v:1"] {
            assert_eq!(
                reject_token(token, "version"),
                Err(DistError::InvalidToken {
                    role: "version",
                    token: token.to_owned(),
                })
            );
        }
        for token in [
            "0.1.0",
            "1.0.0-rc.1",
            "x86_64-unknown-linux-gnu",
            "a+b_c-d.e",
        ] {
            assert_eq!(reject_token(token, "target"), Ok(()));
        }
    }

    #[test]
    fn gzip_round_trip_covers_empty_and_multi_block_payloads() {
        for raw in [vec![], vec![0xabu8; 3], vec![0xcd; 200_000]] {
            let encoded = gzip_stored(&raw);
            assert_eq!(encoded[0..3], [0x1f, 0x8b, 0x08]);
            assert_eq!(gunzip_stored(&encoded), Ok(raw));
        }
    }

    #[test]
    fn gunzip_rejects_deflated_and_corrupt_inputs() {
        let raw = vec![7u8; 100];
        let mut encoded = gzip_stored(&raw);
        assert!(matches!(
            gunzip_stored(&encoded[..encoded.len() - 1]),
            Err(DistError::VerifyFailed { .. })
        ));
        let last = encoded.len() - 1;
        encoded[last] ^= 0xff;
        assert!(matches!(
            gunzip_stored(&encoded),
            Err(DistError::VerifyFailed { .. })
        ));
    }

    #[test]
    fn tar_round_trip_preserves_names_modes_and_padding() {
        let mut tar = Vec::new();
        append_dir(&mut tar, "top/").expect("dir");
        append_file(&mut tar, "top/a", 0o755, b"hello").expect("file");
        append_file(&mut tar, "top/empty", 0o644, b"").expect("empty");
        tar.extend_from_slice(&[0u8; 1024]);
        let entries = parse_tar(&tar).expect("parse");
        assert_eq!(
            entries,
            [
                ("top/".to_owned(), Vec::new()),
                ("top/a".to_owned(), b"hello".to_vec()),
                ("top/empty".to_owned(), Vec::new()),
            ]
        );
        assert_eq!(&tar[100..108], b"0000755\0");
        assert_eq!(&tar[124 + 512..136 + 512], b"00000000005\0");
    }

    #[test]
    fn run_builds_a_verified_archive_from_fixture_inputs() {
        let workspace = fixture_workspace();
        let inputs = workspace.join("inputs");
        fs::create_dir_all(inputs.join("nested")).expect("inputs");
        fs::write(inputs.join("minictr"), b"fake-minictr-bytes").expect("minictr");
        fs::write(inputs.join("kernel.bin"), b"fake-kernel-bytes").expect("kernel");
        fs::write(workspace.join("LICENSE-MIT"), b"fake-mit").expect("mit");
        fs::write(workspace.join("LICENSE-APACHE"), b"fake-apache").expect("apache");
        let args = DistArgs {
            version: Some("0.1.0".to_owned()),
            target: "fixture-target".to_owned(),
            minictr: inputs.join("minictr"),
            kernel: inputs.join("kernel.bin"),
            output: Some(workspace.join("dist")),
        };
        run(&workspace, &args).expect("dist run");

        let archive = workspace.join("dist/minicontainer-0.1.0-fixture-target.tar.gz");
        let bytes = fs::read(&archive).expect("archive");
        let sidecar = fs::read_to_string(workspace.join(format!(
            "dist/{}.sha256",
            archive.file_name().expect("name").to_string_lossy()
        )))
        .expect("sidecar");
        assert_eq!(
            sidecar,
            format!(
                "{}  {}\n",
                hex_digest(&bytes),
                "minicontainer-0.1.0-fixture-target.tar.gz"
            )
        );
        let tar = gunzip_stored(&bytes).expect("gunzip");
        let entries = parse_tar(&tar).expect("tar");
        assert_eq!(entries.len(), 8);
        assert_eq!(entries[1].1, b"fake-minictr-bytes");
        assert_eq!(entries[3].1, b"fake-kernel-bytes");
        assert_eq!(entries[4].1, b"fake-mit");
        assert_eq!(entries[5].1, b"fake-apache");

        let again = workspace.join("dist-again");
        let again_args = DistArgs {
            output: Some(again.clone()),
            ..args_clone(&args)
        };
        run(&workspace, &again_args).expect("second run");
        let again_bytes = fs::read(again.join("minicontainer-0.1.0-fixture-target.tar.gz"))
            .expect("second archive");
        assert_eq!(bytes, again_bytes, "dist output must be deterministic");

        fs::remove_dir_all(&workspace).expect("cleanup");
    }

    #[test]
    fn run_rejects_missing_inputs_and_bad_tokens() {
        let workspace = fixture_workspace();
        let missing = DistArgs {
            version: Some("0.1.0".to_owned()),
            target: "t".to_owned(),
            minictr: workspace.join("absent"),
            kernel: workspace.join("absent-too"),
            output: Some(workspace.join("dist")),
        };
        assert_eq!(
            run(&workspace, &missing),
            Err(DistError::MissingInput {
                role: "minictr",
                path: workspace.join("absent"),
            })
        );
        fs::create_dir_all(&workspace).expect("workspace");
        fs::write(workspace.join("m"), b"m").expect("m");
        fs::write(workspace.join("LICENSE-MIT"), b"mit").expect("mit");
        fs::write(workspace.join("LICENSE-APACHE"), b"apache").expect("apache");
        let directory = DistArgs {
            version: Some("0.1.0".to_owned()),
            target: "t".to_owned(),
            minictr: workspace.join("m"),
            kernel: workspace.clone(),
            output: Some(workspace.join("dist")),
        };
        assert_eq!(
            run(&workspace, &directory),
            Err(DistError::NotRegularFile {
                role: "kernel",
                path: workspace.clone(),
            })
        );
        let bad_token = DistArgs {
            version: Some("../evil".to_owned()),
            target: "t".to_owned(),
            minictr: workspace.join("m"),
            kernel: workspace.join("m"),
            output: Some(workspace.join("dist")),
        };
        assert_eq!(
            run(&workspace, &bad_token),
            Err(DistError::InvalidToken {
                role: "version",
                token: "../evil".to_owned(),
            })
        );
        fs::remove_dir_all(&workspace).expect("cleanup");

        let bare = fixture_workspace();
        fs::create_dir_all(&bare).expect("bare workspace");
        fs::write(bare.join("m"), b"m").expect("m");
        let no_license = DistArgs {
            version: Some("0.1.0".to_owned()),
            target: "t".to_owned(),
            minictr: bare.join("m"),
            kernel: bare.join("m"),
            output: Some(bare.join("dist")),
        };
        assert_eq!(
            run(&bare, &no_license),
            Err(DistError::MissingInput {
                role: "license",
                path: bare.join("LICENSE-MIT"),
            })
        );
        fs::remove_dir_all(&bare).expect("cleanup");
    }

    fn args_clone(args: &DistArgs) -> DistArgs {
        DistArgs {
            version: args.version.clone(),
            target: args.target.clone(),
            minictr: args.minictr.clone(),
            kernel: args.kernel.clone(),
            output: None,
        }
    }

    fn fixture_workspace() -> PathBuf {
        static NEXT_DIST_FIXTURE_ID: AtomicU64 = AtomicU64::new(0);
        let id = NEXT_DIST_FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "minicontainer-dist-fixture-{}-{id}",
            std::process::id()
        ))
    }
}
