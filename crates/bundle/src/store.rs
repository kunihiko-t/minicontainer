use crate::{BundleError, StoreError, format_digest, manifest, parse};
use minios_abi::{boot::BUNDLE_MAX_LEN, manifest::ManifestError};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(0);
const TEMP_CREATE_ATTEMPTS: usize = 128;
const TAG_DIGEST_LEN: usize = 64;

/// tag名と、そのtag fileが指すdigest。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagRecord {
    pub name: String,
    pub digest: [u8; 32],
}

/// Local content-addressed storage for validated MiniBundles.
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// Opens or creates the store rooted at an absolute path.
    pub fn new(root: impl AsRef<std::path::Path>) -> Result<Self, StoreError> {
        let root = root.as_ref();
        if !root.is_absolute() {
            return Err(StoreError::RootNotAbsolute);
        }
        match fs::symlink_metadata(root) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(StoreError::UnsafeStorePath);
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir_all(root)?,
            Err(error) => return Err(error.into()),
        }
        let store = Self {
            root: fs::canonicalize(root)?,
        };
        store.ensure_layout()?;
        Ok(store)
    }

    /// Imports a validated bundle under its SHA-256 digest.
    pub fn import(&self, bytes: &[u8]) -> Result<[u8; 32], StoreError> {
        self.ensure_layout()?;
        let bundle = parse(bytes)?;
        let digest = bundle.header.digest;
        atomic_write(&self.image_path(digest), bytes)?;
        Ok(digest)
    }

    /// Atomically associates a manifest-compatible name with a digest.
    pub fn tag(&self, name: &str, digest: [u8; 32]) -> Result<(), StoreError> {
        validate_tag_name(name)?;
        self.ensure_layout()?;
        let encoded = format_digest(digest);
        atomic_write(&self.root.join("tags").join(name), encoded.as_bytes())?;
        Ok(())
    }

    /// Removes one tag without touching blobs. Refuses symlinks and
    /// entries that leave the store root; a missing tag is a typed error.
    /// The tag content is not validated, so a corrupt tag stays removable.
    pub fn remove_tag(&self, name: &str) -> Result<(), StoreError> {
        validate_tag_name(name)?;
        self.ensure_layout()?;
        let path = self.root.join("tags").join(name);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(StoreError::UnsafeStorePath);
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(StoreError::TagNotFound(name.to_owned()));
            }
            Err(error) => return Err(error.into()),
        }
        let canonical = fs::canonicalize(&path)?;
        if !canonical.starts_with(&self.root) {
            return Err(StoreError::UnsafeStorePath);
        }
        fs::remove_file(&canonical)?;
        Ok(())
    }

    /// Resolves a tag and returns the validated bundle bytes it names.
    pub fn resolve(&self, name: &str) -> Result<Vec<u8>, StoreError> {
        validate_tag_name(name)?;
        self.ensure_layout()?;
        let encoded_digest = read_store_file_up_to(
            &self.root,
            &self.root.join("tags").join(name),
            TAG_DIGEST_LEN,
        )?
        .ok_or(StoreError::TagTooLarge)?;
        let digest = decode_digest(&encoded_digest).ok_or(StoreError::InvalidTagDigest)?;
        let bundle_max_len = usize::try_from(BUNDLE_MAX_LEN)
            .map_err(|_| StoreError::Bundle(BundleError::LengthOverflow))?;
        let bytes = read_store_file_up_to(&self.root, &self.image_path(digest), bundle_max_len)?
            .ok_or(StoreError::Bundle(BundleError::TooLarge))?;
        let bundle = parse(&bytes)?;
        if bundle.header.digest != digest {
            return Err(StoreError::DigestPathMismatch);
        }
        Ok(bytes)
    }

    /// Resolves stored bytes directly by digest, re-validating them.
    pub fn resolve_digest(&self, digest: [u8; 32]) -> Result<Vec<u8>, StoreError> {
        self.ensure_layout()?;
        let bundle_max_len = usize::try_from(BUNDLE_MAX_LEN)
            .map_err(|_| StoreError::Bundle(BundleError::LengthOverflow))?;
        let bytes = read_store_file_up_to(&self.root, &self.image_path(digest), bundle_max_len)?
            .ok_or(StoreError::Bundle(BundleError::TooLarge))?;
        let bundle = parse(&bytes)?;
        if bundle.header.digest != digest {
            return Err(StoreError::DigestPathMismatch);
        }
        Ok(bytes)
    }

    /// Lists blob digests that no tag references, in byte order. Entries
    /// that are not digest-named blobs stay untracked and untouched.
    pub fn orphans(&self) -> Result<Vec<[u8; 32]>, StoreError> {
        self.ensure_layout()?;
        let mut referenced = std::collections::BTreeSet::new();
        for record in self.list_tags()? {
            referenced.insert(record.digest);
        }
        let mut entries: Vec<_> =
            fs::read_dir(self.root.join("images/sha256"))?.collect::<Result<_, _>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        let mut orphans = Vec::new();
        for entry in entries {
            let Some(digest) = blob_digest_from_name(&entry.file_name()) else {
                continue;
            };
            if !referenced.contains(&digest) {
                orphans.push(digest);
            }
        }
        Ok(orphans)
    }

    /// Removes one blob after re-checking that no tag references it.
    /// Refuses symlinks and entries that leave the store root; a missing
    /// blob is already pruned and reports success.
    pub fn remove_blob(&self, digest: [u8; 32]) -> Result<(), StoreError> {
        self.ensure_layout()?;
        for record in self.list_tags()? {
            if record.digest == digest {
                return Err(StoreError::BlobReferenced);
            }
        }
        let path = self.image_path(digest);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(StoreError::UnsafeStorePath);
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        }
        let canonical = fs::canonicalize(&path)?;
        if !canonical.starts_with(&self.root) {
            return Err(StoreError::UnsafeStorePath);
        }
        fs::remove_file(&canonical)?;
        Ok(())
    }

    /// Lists every tag by UTF-8 byte order without resolving its image.
    pub fn list_tags(&self) -> Result<Vec<TagRecord>, StoreError> {
        self.ensure_layout()?;
        let tags = self.root.join("tags");
        let mut records = Vec::new();
        for entry in fs::read_dir(&tags)? {
            let entry = entry?;
            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else {
                return Err(StoreError::InvalidTag(ManifestError::InvalidUtf8));
            };
            validate_tag_name(name)?;
            let encoded_digest =
                read_store_file_up_to(&self.root, &tags.join(name), TAG_DIGEST_LEN)?
                    .ok_or(StoreError::TagTooLarge)?;
            let digest = decode_digest(&encoded_digest).ok_or(StoreError::InvalidTagDigest)?;
            records.push(TagRecord {
                name: name.to_owned(),
                digest,
            });
        }
        records.sort_by(|left, right| left.name.as_bytes().cmp(right.name.as_bytes()));
        Ok(records)
    }

    fn image_path(&self, digest: [u8; 32]) -> PathBuf {
        self.root
            .join("images/sha256")
            .join(format!("{}.mcb", format_digest(digest)))
    }

    fn ensure_layout(&self) -> Result<(), StoreError> {
        ensure_directory(&self.root)?;
        let images = self.root.join("images");
        ensure_directory(&images)?;
        ensure_directory(&images.join("sha256"))?;
        ensure_directory(&self.root.join("tags"))?;
        Ok(())
    }
}

fn ensure_directory(path: &Path) -> Result<(), StoreError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => validate_directory_metadata(metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => match fs::create_dir(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                validate_directory_metadata(fs::symlink_metadata(path)?)
            }
            Err(error) => Err(error.into()),
        },
        Err(error) => Err(error.into()),
    }
}

fn validate_directory_metadata(metadata: fs::Metadata) -> Result<(), StoreError> {
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(StoreError::UnsafeStorePath);
    }
    Ok(())
}

fn read_store_file_up_to(
    root: &Path,
    path: &Path,
    max_len: usize,
) -> Result<Option<Vec<u8>>, StoreError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(StoreError::UnsafeStorePath);
    }
    let max_len_u64 =
        u64::try_from(max_len).map_err(|_| StoreError::Bundle(BundleError::LengthOverflow))?;
    let canonical = fs::canonicalize(path)?;
    if !canonical.starts_with(root) {
        return Err(StoreError::UnsafeStorePath);
    }
    if metadata.len() > max_len_u64 {
        return Ok(None);
    }
    let mut file = File::open(canonical)?;
    if file.metadata()?.len() > max_len_u64 {
        return Ok(None);
    }
    let sentinel_len = max_len
        .checked_add(1)
        .ok_or(StoreError::Bundle(BundleError::LengthOverflow))?;
    let mut bytes = vec![0; sentinel_len];
    let mut bytes_read = 0;
    loop {
        match file.read(&mut bytes[bytes_read..]) {
            Ok(0) => {
                bytes.truncate(bytes_read);
                return Ok(Some(bytes));
            }
            Ok(read_len) => {
                bytes_read = bytes_read
                    .checked_add(read_len)
                    .ok_or(StoreError::Bundle(BundleError::LengthOverflow))?;
                if bytes_read == sentinel_len {
                    return Ok(None);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
}

fn validate_tag_name(name: &str) -> Result<(), StoreError> {
    manifest::validate_name(name).map_err(StoreError::InvalidTag)?;
    let mut components = Path::new(name).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err(StoreError::UnsafeTagName);
    }
    Ok(())
}

fn atomic_write(destination: &Path, bytes: &[u8]) -> io::Result<()> {
    let directory = destination.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "atomic destination has no parent directory",
        )
    })?;

    for _ in 0..TEMP_CREATE_ATTEMPTS {
        let sequence = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
        let temporary = directory.join(format!(
            ".minicontainer-tmp-{}-{sequence}",
            std::process::id()
        ));
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };

        let write_result = file.write_all(bytes).and_then(|()| file.sync_all());
        drop(file);
        if let Err(error) = write_result {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        if let Err(error) = fs::rename(&temporary, destination) {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        return Ok(());
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique atomic temporary file",
    ))
}

/// Reads a blob digest from a directory entry name. Anything that is
/// not a lowercase hex digest with a `.mcb` suffix stays untracked,
/// including undecodable names: lossy replacement never yields hex.
fn blob_digest_from_name(name: &std::ffi::OsStr) -> Option<[u8; 32]> {
    let text = name.to_string_lossy();
    let hex = text.strip_suffix(".mcb")?;
    decode_digest(hex.as_bytes())
}

pub(crate) fn decode_digest(encoded: &[u8]) -> Option<[u8; 32]> {
    if encoded.len() != 64 {
        return None;
    }
    let mut digest = [0; 32];
    let (pairs, []) = encoded.as_chunks::<2>() else {
        return None;
    };
    for (output, pair) in digest.iter_mut().zip(pairs) {
        *output = decode_nibble(pair[0])?
            .checked_mul(16)?
            .checked_add(decode_nibble(pair[1])?)?;
    }
    Some(digest)
}

const fn decode_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ImageSpec, build};
    use minios_abi::boot::BUNDLE_MAX_LEN;
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    static NEXT_TEMP_HOME: AtomicU64 = AtomicU64::new(0);

    struct TempHome {
        path: PathBuf,
    }

    impl TempHome {
        fn new() -> Self {
            loop {
                let id = NEXT_TEMP_HOME.fetch_add(1, Ordering::Relaxed);
                let path = std::env::temp_dir().join(format!(
                    "minicontainer-bundle-test-{}-{id}",
                    std::process::id()
                ));
                match fs::create_dir(&path) {
                    Ok(()) => return Self { path },
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => panic!("failed to create isolated test directory: {error}"),
                }
            }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    // Production break caught: import uses a non-content digest/path or tag resolution returns different bytes.
    #[test]
    fn imports_by_digest_and_resolves_tag() {
        let home = TempHome::new();
        let store = Store::new(home.path()).unwrap();
        let bytes = build(ImageSpec {
            name: "a",
            args: &[],
            elf: b"",
        })
        .unwrap();

        let digest = store.import(&bytes).unwrap();
        store.tag("hello", digest).unwrap();

        assert_eq!(
            digest,
            [
                0xd2, 0xe0, 0xc6, 0x02, 0xac, 0xbf, 0x71, 0x1b, 0x5d, 0x1c, 0xb7, 0xa2, 0xae, 0x07,
                0xdd, 0x19, 0xd9, 0xed, 0xa0, 0xf6, 0x8c, 0xb7, 0x36, 0xa5, 0x07, 0xb9, 0x7a, 0x73,
                0x9e, 0xa9, 0x7d, 0x48,
            ]
        );
        assert_eq!(
            fs::read(home.path().join(
                "images/sha256/d2e0c602acbf711b5d1cb7a2ae07dd19d9eda0f68cb736a507b97a739ea97d48.mcb"
            ))
            .unwrap(),
            bytes
        );
        assert_eq!(
            fs::read(home.path().join("tags/hello")).unwrap(),
            b"d2e0c602acbf711b5d1cb7a2ae07dd19d9eda0f68cb736a507b97a739ea97d48"
        );
        assert_eq!(store.resolve("hello").unwrap(), bytes);
    }

    // Production break caught: resolving by digest returns other bytes,
    // skips validation, or reports a missing blob as a tag error.
    #[test]
    fn resolves_stored_bytes_directly_by_digest() {
        let home = TempHome::new();
        let store = Store::new(home.path()).unwrap();
        let bytes = build(ImageSpec {
            name: "a",
            args: &[],
            elf: b"ELF",
        })
        .unwrap();
        let digest = store.import(&bytes).unwrap();

        assert_eq!(store.resolve_digest(digest).unwrap(), bytes);

        let missing = store.resolve_digest([0x11; 32]);
        assert!(
            matches!(&missing, Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound),
            "a missing blob must surface its I/O error, got {missing:?}"
        );
    }

    // Production break caught: a blob stored under the wrong digest path
    // or corrupted in place resolves without complaint.
    #[test]
    fn resolve_digest_revalidates_bytes_and_path() {
        let home = TempHome::new();
        let store = Store::new(home.path()).unwrap();
        let bytes = build(ImageSpec {
            name: "a",
            args: &[],
            elf: b"ELF",
        })
        .unwrap();
        let digest = store.import(&bytes).unwrap();
        let image = home
            .path()
            .join(format!("images/sha256/{}.mcb", format_digest(digest)));

        let misplaced = [0x22; 32];
        fs::copy(
            &image,
            home.path()
                .join(format!("images/sha256/{}.mcb", format_digest(misplaced))),
        )
        .unwrap();
        assert!(
            matches!(
                store.resolve_digest(misplaced),
                Err(StoreError::DigestPathMismatch)
            ),
            "a misplaced blob must fail resolution"
        );

        let mut corrupted = bytes.clone();
        let last = corrupted.len() - 1;
        corrupted[last] ^= 0x01;
        fs::write(&image, &corrupted).unwrap();
        assert!(matches!(
            store.resolve_digest(digest),
            Err(StoreError::Bundle(_))
        ));
    }

    // Production break caught: removing a tag deletes its blob, sibling
    // tags, or shared bytes that another tag still resolves.
    #[test]
    fn removes_only_the_named_tag() {
        let home = TempHome::new();
        let store = Store::new(home.path()).unwrap();
        let shared = build(ImageSpec {
            name: "a",
            args: &[],
            elf: b"shared",
        })
        .unwrap();
        let other = build(ImageSpec {
            name: "a",
            args: &[],
            elf: b"other",
        })
        .unwrap();
        let shared_digest = store.import(&shared).unwrap();
        let other_digest = store.import(&other).unwrap();
        store.tag("first", shared_digest).unwrap();
        store.tag("second", shared_digest).unwrap();
        store.tag("third", other_digest).unwrap();

        store.remove_tag("first").unwrap();

        assert!(store.resolve("first").is_err());
        assert_eq!(store.resolve("second").unwrap(), shared);
        assert_eq!(store.resolve("third").unwrap(), other);
        assert_eq!(
            fs::read_dir(home.path().join("images/sha256"))
                .unwrap()
                .count(),
            2
        );
        let mut tags: Vec<String> = fs::read_dir(home.path().join("tags"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        tags.sort();
        assert_eq!(tags, vec!["second".to_owned(), "third".to_owned()]);
    }

    // Production break caught: removing a missing tag succeeds silently
    // or reports a bare I/O error instead of a typed diagnostic.
    #[test]
    fn removing_a_missing_tag_is_a_typed_error() {
        let home = TempHome::new();
        let store = Store::new(home.path()).unwrap();

        let missing = store.remove_tag("absent");
        assert!(
            matches!(missing, Err(StoreError::TagNotFound(_))),
            "got {missing:?}"
        );
        assert_eq!(
            missing.unwrap_err().to_string(),
            "tag `absent` does not exist"
        );

        let bytes = build(ImageSpec {
            name: "a",
            args: &[],
            elf: b"ELF",
        })
        .unwrap();
        let digest = store.import(&bytes).unwrap();
        store.tag("once", digest).unwrap();
        store.remove_tag("once").unwrap();
        assert!(matches!(
            store.remove_tag("once"),
            Err(StoreError::TagNotFound(_))
        ));
    }

    // Production break caught: removing a tag follows a symlink or a
    // planted directory, deleting outside the tags namespace.
    #[cfg(unix)]
    #[test]
    fn remove_tag_refuses_symlinked_entries() {
        use std::os::unix::fs::symlink;

        let home = TempHome::new();
        let outside = TempHome::new();
        let store = Store::new(home.path()).unwrap();
        let bytes = build(ImageSpec {
            name: "a",
            args: &[],
            elf: b"ELF",
        })
        .unwrap();
        let digest = store.import(&bytes).unwrap();
        store.tag("linked", digest).unwrap();
        let link = home.path().join("tags/linked");
        fs::remove_file(&link).unwrap();
        let target = outside.path().join("precious");
        fs::write(&target, b"precious").unwrap();
        symlink(&target, &link).unwrap();

        assert!(matches!(
            store.remove_tag("linked"),
            Err(StoreError::UnsafeStorePath)
        ));
        assert_eq!(fs::read(&target).unwrap(), b"precious");
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );

        let directory = home.path().join("tags/subdir");
        fs::create_dir(&directory).unwrap();
        assert!(matches!(
            store.remove_tag("subdir"),
            Err(StoreError::UnsafeStorePath)
        ));
        assert!(directory.is_dir());
    }

    // Production break caught: a tag with corrupt content cannot be
    // removed, or an invalid name reaches the filesystem.
    #[test]
    fn remove_tag_ignores_content_but_validates_names() {
        let home = TempHome::new();
        let store = Store::new(home.path()).unwrap();
        let bytes = build(ImageSpec {
            name: "a",
            args: &[],
            elf: b"ELF",
        })
        .unwrap();
        let digest = store.import(&bytes).unwrap();
        store.tag("broken", digest).unwrap();
        fs::write(home.path().join("tags/broken"), b"not a digest").unwrap();

        store.remove_tag("broken").unwrap();
        assert!(!home.path().join("tags/broken").exists());

        for name in ["..", "../escape", "nested/name"] {
            assert!(
                matches!(
                    store.remove_tag(name),
                    Err(StoreError::InvalidTag(_) | StoreError::UnsafeTagName)
                ),
                "remove accepted {name:?}"
            );
        }
        assert!(!home.path().join("escape").exists());
        assert!(!home.path().join("tags/nested").exists());
    }

    // Production break caught: orphan detection misses an unreferenced
    // blob, reports a referenced one, or stops at stray directory entries.
    #[test]
    fn orphans_lists_only_unreferenced_blobs_in_order() {
        let home = TempHome::new();
        let store = Store::new(home.path()).unwrap();
        assert_eq!(store.orphans().unwrap(), Vec::<[u8; 32]>::new());

        let mut digests = Vec::new();
        for elf in ["first", "second", "third"] {
            let bytes = build(ImageSpec {
                name: "a",
                args: &[],
                elf: elf.as_bytes(),
            })
            .unwrap();
            digests.push(store.import(&bytes).unwrap());
        }
        store.tag("kept", digests[1]).unwrap();
        let images = home.path().join("images/sha256");
        fs::write(images.join("notes.txt"), b"stray").unwrap();
        fs::write(images.join("deadbeef.mcb"), b"short").unwrap();
        fs::create_dir(images.join("scratch")).unwrap();

        let mut expected = vec![digests[0], digests[2]];
        expected.sort();
        assert_eq!(store.orphans().unwrap(), expected);
    }

    // Production break caught: an undecodable stray entry breaks orphan
    // detection instead of being skipped. APFS rejects such names, so the
    // filesystem half runs on Linux only.
    #[test]
    fn blob_names_classify_without_touching_the_filesystem() {
        use std::os::unix::ffi::OsStringExt;

        let hex = "ab".repeat(32);
        assert_eq!(
            blob_digest_from_name(std::ffi::OsStr::new(&format!("{hex}.mcb"))),
            Some([0xab; 32])
        );
        for stray in [
            std::ffi::OsString::from("notes.txt"),
            std::ffi::OsString::from("deadbeef.mcb"),
            std::ffi::OsString::from_vec(vec![0xab, 0xff]),
            std::ffi::OsString::from_vec([b"a".repeat(60), b".mcb".to_vec()].concat()),
        ] {
            assert_eq!(
                blob_digest_from_name(&stray),
                None,
                "stray must stay untracked: {stray:?}"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn orphans_skips_a_non_utf8_stray_on_disk() {
        use std::os::unix::ffi::OsStringExt;

        let home = TempHome::new();
        let store = Store::new(home.path()).unwrap();
        let bytes = build(ImageSpec {
            name: "a",
            args: &[],
            elf: b"ELF",
        })
        .unwrap();
        let digest = store.import(&bytes).unwrap();
        let stray = std::ffi::OsString::from_vec(vec![0xab, 0xff]);
        fs::write(home.path().join("images/sha256").join(&stray), b"stray").unwrap();

        assert_eq!(store.orphans().unwrap(), vec![digest]);
    }

    // Production break caught: blob removal deletes a referenced blob,
    // follows a symlink, or fails on an already-absent blob.
    #[test]
    fn remove_blob_deletes_only_unreferenced_regular_files() {
        let home = TempHome::new();
        let store = Store::new(home.path()).unwrap();
        let bytes = build(ImageSpec {
            name: "a",
            args: &[],
            elf: b"ELF",
        })
        .unwrap();
        let digest = store.import(&bytes).unwrap();
        store.tag("kept", digest).unwrap();

        assert!(matches!(
            store.remove_blob(digest),
            Err(StoreError::BlobReferenced)
        ));
        assert!(store.resolve("kept").is_ok());

        let orphan = build(ImageSpec {
            name: "a",
            args: &[],
            elf: b"orphan",
        })
        .unwrap();
        let orphan_digest = store.import(&orphan).unwrap();
        let orphan_path = home.path().join(format!(
            "images/sha256/{}.mcb",
            format_digest(orphan_digest)
        ));
        store.remove_blob(orphan_digest).unwrap();
        assert!(!orphan_path.exists());
        assert!(store.resolve("kept").is_ok());

        store.remove_blob(orphan_digest).unwrap();
    }

    // Production break caught: blob removal follows a symlinked blob
    // name to an outside file.
    #[cfg(unix)]
    #[test]
    fn remove_blob_refuses_symlinked_blobs() {
        use std::os::unix::fs::symlink;

        let home = TempHome::new();
        let outside = TempHome::new();
        let store = Store::new(home.path()).unwrap();
        let bytes = build(ImageSpec {
            name: "a",
            args: &[],
            elf: b"ELF",
        })
        .unwrap();
        let digest = store.import(&bytes).unwrap();
        let link = home
            .path()
            .join(format!("images/sha256/{}.mcb", format_digest(digest)));
        fs::remove_file(&link).unwrap();
        let target = outside.path().join("precious");
        fs::write(&target, b"precious").unwrap();
        symlink(&target, &link).unwrap();

        assert!(matches!(
            store.remove_blob(digest),
            Err(StoreError::UnsafeStorePath)
        ));
        assert_eq!(fs::read(&target).unwrap(), b"precious");
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    // Production break caught: Store::new accepts a relative root and writes beneath the process working directory.
    #[test]
    fn requires_an_absolute_store_root() {
        let id = NEXT_TEMP_HOME.fetch_add(1, Ordering::Relaxed);
        let relative = PathBuf::from(format!(
            "target/minicontainer-relative-store-test-{}-{id}",
            std::process::id()
        ));

        let result = Store::new(&relative);
        if result.is_ok() {
            fs::remove_dir_all(&relative).unwrap();
        }

        assert!(matches!(result, Err(StoreError::RootNotAbsolute)));
    }

    // Production break caught: a tag name containing path syntax escapes the tags namespace or is treated as a directory.
    #[test]
    fn rejects_tag_path_separators_and_traversal_components() {
        let home = TempHome::new();
        let store = Store::new(home.path()).unwrap();

        for name in [".", "..", "../escape", "nested/name"] {
            assert!(
                matches!(
                    store.tag(name, [0; 32]),
                    Err(StoreError::InvalidTag(_) | StoreError::UnsafeTagName)
                ),
                "tag accepted {name:?}"
            );
            assert!(
                matches!(
                    store.resolve(name),
                    Err(StoreError::InvalidTag(_) | StoreError::UnsafeTagName)
                ),
                "resolve accepted {name:?}"
            );
        }

        assert!(!home.path().join("escape").exists());
        assert!(!home.path().join("tags/nested").exists());
    }

    // Production break caught: import truncates the destination in place instead of replacing it atomically.
    #[test]
    fn import_atomically_replaces_a_read_only_destination() {
        let home = TempHome::new();
        let store = Store::new(home.path()).unwrap();
        let bytes = build(ImageSpec {
            name: "a",
            args: &[],
            elf: b"",
        })
        .unwrap();
        let digest = store.import(&bytes).unwrap();
        let image = store.image_path(digest);
        let mut permissions = fs::metadata(&image).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&image, permissions).unwrap();

        assert_eq!(store.import(&bytes).unwrap(), digest);
        assert_eq!(fs::read(image).unwrap(), bytes);
    }

    // Production break caught: retagging truncates the current tag file in place instead of atomically replacing it.
    #[test]
    fn tag_atomically_replaces_a_read_only_destination() {
        let home = TempHome::new();
        let store = Store::new(home.path()).unwrap();
        let first = build(ImageSpec {
            name: "a",
            args: &[],
            elf: b"first",
        })
        .unwrap();
        let second = build(ImageSpec {
            name: "a",
            args: &[],
            elf: b"second",
        })
        .unwrap();
        let first_digest = store.import(&first).unwrap();
        let second_digest = store.import(&second).unwrap();
        store.tag("latest", first_digest).unwrap();
        let tag = home.path().join("tags/latest");
        let mut permissions = fs::metadata(&tag).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&tag, permissions).unwrap();

        store.tag("latest", second_digest).unwrap();

        assert_eq!(store.resolve("latest").unwrap(), second);
    }

    // Production break caught: a failed atomic rename leaves a temporary file in the destination directory.
    #[test]
    fn failed_atomic_replacement_cleans_up_its_temporary_file() {
        let home = TempHome::new();
        let store = Store::new(home.path()).unwrap();
        fs::create_dir(home.path().join("tags/blocked")).unwrap();

        assert!(matches!(
            store.tag("blocked", [0; 32]),
            Err(StoreError::Io(_))
        ));
        let temporary_files = fs::read_dir(home.path().join("tags"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".minicontainer-tmp-")
            })
            .count();
        assert_eq!(temporary_files, 0);
    }

    // Production break caught: Store::new follows a pre-existing symlinked tags directory outside the root.
    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_store_directories() {
        use std::os::unix::fs::symlink;

        let home = TempHome::new();
        let outside = TempHome::new();
        symlink(outside.path(), home.path().join("tags")).unwrap();

        assert!(matches!(
            Store::new(home.path()),
            Err(StoreError::UnsafeStorePath)
        ));
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    // Production break caught: resolve follows attacker-controlled tag or image symlinks outside the store root.
    #[cfg(unix)]
    #[test]
    fn resolve_rejects_symlinked_store_entries() {
        use std::os::unix::fs::symlink;

        let home = TempHome::new();
        let outside = TempHome::new();
        let store = Store::new(home.path()).unwrap();
        let bytes = build(ImageSpec {
            name: "a",
            args: &[],
            elf: b"",
        })
        .unwrap();
        let digest = store.import(&bytes).unwrap();
        let encoded = format_digest(digest);
        fs::write(outside.path().join("tag"), encoded.as_bytes()).unwrap();
        symlink(outside.path().join("tag"), home.path().join("tags/link")).unwrap();

        assert!(matches!(
            store.resolve("link"),
            Err(StoreError::UnsafeStorePath)
        ));

        store.tag("image-link", digest).unwrap();
        let image_path = store.image_path(digest);
        fs::remove_file(&image_path).unwrap();
        fs::write(outside.path().join("image.mcb"), &bytes).unwrap();
        symlink(outside.path().join("image.mcb"), image_path).unwrap();

        assert!(matches!(
            store.resolve("image-link"),
            Err(StoreError::UnsafeStorePath)
        ));
    }

    // Production break caught: tag follows a tags directory replaced by a symlink after Store construction.
    #[cfg(unix)]
    #[test]
    fn tag_rechecks_directory_confinement_before_writing() {
        use std::os::unix::fs::symlink;

        let home = TempHome::new();
        let outside = TempHome::new();
        let store = Store::new(home.path()).unwrap();
        fs::remove_dir(home.path().join("tags")).unwrap();
        symlink(outside.path(), home.path().join("tags")).unwrap();

        assert!(matches!(
            store.tag("escape", [0; 32]),
            Err(StoreError::UnsafeStorePath)
        ));
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    // Production break caught: resolve accepts non-canonical tag digest text with a newline, uppercase, or wrong length.
    #[test]
    fn rejects_malformed_tag_digest_text() {
        let home = TempHome::new();
        let store = Store::new(home.path()).unwrap();
        let bytes = build(ImageSpec {
            name: "a",
            args: &[],
            elf: b"",
        })
        .unwrap();
        store.import(&bytes).unwrap();
        let cases: [(&str, &[u8]); 3] = [
            (
                "newline",
                b"d2e0c602acbf711b5d1cb7a2ae07dd19d9eda0f68cb736a507b97a739ea97d48\n",
            ),
            (
                "uppercase",
                b"D2E0C602ACBF711B5D1CB7A2AE07DD19D9EDA0F68CB736A507B97A739EA97D48",
            ),
            ("short", b"d2e0"),
        ];

        for (case, encoded) in cases {
            fs::write(home.path().join("tags/bad"), encoded).unwrap();
            let result = store.resolve("bad");
            if case == "newline" {
                assert!(matches!(result, Err(StoreError::TagTooLarge)), "{case}");
            } else {
                assert!(
                    matches!(result, Err(StoreError::InvalidTagDigest)),
                    "{case}"
                );
            }
        }
    }

    // Production break caught: import writes an image before validating its digest and propagating the bundle error.
    #[test]
    fn invalid_import_has_no_store_side_effect() {
        let home = TempHome::new();
        let store = Store::new(home.path()).unwrap();
        let mut bytes = build(ImageSpec {
            name: "a",
            args: &[],
            elf: b"elf",
        })
        .unwrap();
        bytes[120] ^= 1;

        assert!(matches!(
            store.import(&bytes),
            Err(StoreError::Bundle(crate::BundleError::DigestMismatch))
        ));
        assert_eq!(
            fs::read_dir(home.path().join("images/sha256"))
                .unwrap()
                .count(),
            0
        );
    }

    // Production break caught: resolve trusts the image pathname without comparing it to the validated header digest.
    #[test]
    fn resolve_rejects_content_stored_under_the_wrong_digest() {
        let home = TempHome::new();
        let store = Store::new(home.path()).unwrap();
        let first = build(ImageSpec {
            name: "a",
            args: &[],
            elf: b"first",
        })
        .unwrap();
        let second = build(ImageSpec {
            name: "a",
            args: &[],
            elf: b"second",
        })
        .unwrap();
        let first_digest = store.import(&first).unwrap();
        fs::write(store.image_path(first_digest), second).unwrap();
        store.tag("wrong", first_digest).unwrap();

        assert!(matches!(
            store.resolve("wrong"),
            Err(StoreError::DigestPathMismatch)
        ));
    }

    // Production break caught: public typed errors cannot participate in standard error propagation.
    #[test]
    fn bundle_and_store_errors_implement_std_error() {
        fn assert_error<E: std::error::Error>() {}

        assert_error::<crate::BundleError>();
        assert_error::<StoreError>();
    }

    // Production break caught: resolve reads beyond the exact 64-byte digest boundary before rejecting a tag file.
    #[test]
    fn resolve_bounds_tag_files_at_the_digest_boundary() {
        let home = TempHome::new();
        let store = Store::new(home.path()).unwrap();
        fs::write(home.path().join("tags/oversized"), [b'a'; 65]).unwrap();
        assert!(matches!(
            store.resolve("oversized"),
            Err(StoreError::TagTooLarge)
        ));

        fs::write(home.path().join("tags/corrupt"), [b'g'; 64]).unwrap();
        assert!(matches!(
            store.resolve("corrupt"),
            Err(StoreError::InvalidTagDigest)
        ));
    }

    // Production break caught: read_to_end grows the allocation beyond the one-byte sentinel when an exact-limit file fills its buffer.
    #[test]
    fn bounded_store_reads_reserve_only_the_sentinel_capacity() {
        let home = TempHome::new();
        let store = Store::new(home.path()).unwrap();
        let exact_path = store.root.join("tags/exact-capacity");
        fs::write(&exact_path, [0x5a; 10_000]).unwrap();

        let exact = read_store_file_up_to(&store.root, &exact_path, 10_000)
            .unwrap()
            .unwrap();
        assert_eq!(exact.len(), 10_000);
        assert_eq!(exact.capacity(), 10_001);
        assert_eq!(&exact[..8], &[0x5a; 8]);
        assert_eq!(&exact[9_992..], &[0x5a; 8]);

        let oversized_path = store.root.join("tags/oversized-capacity");
        fs::write(&oversized_path, [0xa5; 10_001]).unwrap();
        assert!(matches!(
            read_store_file_up_to(&store.root, &oversized_path, 10_000),
            Ok(None)
        ));
    }

    // Production break caught: resolve allocates and parses an image one byte beyond the ABI bundle maximum.
    #[test]
    fn resolve_rejects_an_image_one_byte_over_the_abi_limit() {
        let home = TempHome::new();
        let store = Store::new(home.path()).unwrap();
        let digest = [0; 32];
        store.tag("oversized", digest).unwrap();
        let image = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(store.image_path(digest))
            .unwrap();
        image.set_len(BUNDLE_MAX_LEN + 1).unwrap();
        assert_eq!(image.metadata().unwrap().len(), 8 * 1024 * 1024 + 1);

        assert!(matches!(
            store.resolve("oversized"),
            Err(StoreError::Bundle(crate::BundleError::TooLarge))
        ));
    }

    // Production break caught: bounded resolve rejects a bundle exactly at the 8 MiB ABI limit.
    #[test]
    fn resolve_accepts_the_maximum_bundle_length() {
        let home = TempHome::new();
        let store = Store::new(home.path()).unwrap();
        let elf = vec![0x5a; BUNDLE_MAX_LEN as usize - 120];
        let bytes = build(ImageSpec {
            name: "a",
            args: &[],
            elf: &elf,
        })
        .unwrap();
        let digest = store.import(&bytes).unwrap();
        store.tag("maximum", digest).unwrap();

        let resolved = store.resolve("maximum").unwrap();
        assert_eq!(resolved.len(), 8 * 1024 * 1024);
        assert_eq!(&resolved[resolved.len() - 8..], &[0x5a; 8]);
    }

    // Production break caught: an empty store lists a phantom tag or fails instead of returning no records.
    #[test]
    fn lists_empty_store_as_empty() {
        let home = TempHome::new();
        let store = Store::new(home.path()).unwrap();

        assert_eq!(store.list_tags().unwrap(), Vec::new());
    }

    // Production break caught: tag listing is unordered or drops tags instead of sorting by UTF-8 byte order.
    #[test]
    fn lists_multiple_tags_in_byte_order() {
        let home = TempHome::new();
        let store = Store::new(home.path()).unwrap();
        let first = build(ImageSpec {
            name: "a",
            args: &[],
            elf: b"first",
        })
        .unwrap();
        let second = build(ImageSpec {
            name: "a",
            args: &[],
            elf: b"second",
        })
        .unwrap();
        let third = build(ImageSpec {
            name: "a",
            args: &[],
            elf: b"third",
        })
        .unwrap();
        let first_digest = store.import(&first).unwrap();
        let second_digest = store.import(&second).unwrap();
        let third_digest = store.import(&third).unwrap();
        store.tag("b", first_digest).unwrap();
        store.tag("a-", second_digest).unwrap();
        store.tag("a", third_digest).unwrap();

        let listed = store.list_tags().unwrap();

        assert_eq!(
            listed,
            vec![
                crate::TagRecord {
                    name: "a".to_owned(),
                    digest: third_digest,
                },
                crate::TagRecord {
                    name: "a-".to_owned(),
                    digest: second_digest,
                },
                crate::TagRecord {
                    name: "b".to_owned(),
                    digest: first_digest,
                },
            ]
        );
    }

    // Production break caught: a dangling tag disappears from the listing or resolves instead of failing.
    #[test]
    fn dangling_tag_is_listed_but_resolve_fails() {
        let home = TempHome::new();
        let store = Store::new(home.path()).unwrap();
        let digest = [0x5a; 32];
        store.tag("dangling", digest).unwrap();

        let listed = store.list_tags().unwrap();

        assert_eq!(
            listed,
            vec![crate::TagRecord {
                name: "dangling".to_owned(),
                digest,
            }]
        );
        assert!(store.resolve("dangling").is_err());
    }

    // Production break caught: listing silently skips a non-UTF-8 tag name instead of returning a typed error.
    // macOS APFS requires UTF-8 file names, so this runs only where the file system can store them.
    #[cfg(target_os = "linux")]
    #[test]
    fn list_rejects_non_utf8_tag_name() {
        use std::os::unix::ffi::OsStringExt;

        let home = TempHome::new();
        let store = Store::new(home.path()).unwrap();
        let raw = std::ffi::OsString::from_vec(vec![0xff, 0xfe]);
        fs::write(home.path().join("tags").join(raw), [b'a'; 64]).unwrap();

        assert!(matches!(store.list_tags(), Err(StoreError::InvalidTag(_))));
    }

    // Production break caught: listing follows a symlinked tag instead of rejecting it as unsafe.
    #[cfg(unix)]
    #[test]
    fn list_rejects_symlinked_tag() {
        use std::os::unix::fs::symlink;

        let home = TempHome::new();
        let outside = TempHome::new();
        let store = Store::new(home.path()).unwrap();
        fs::write(outside.path().join("tag"), [b'a'; 64]).unwrap();
        symlink(outside.path().join("tag"), home.path().join("tags/link")).unwrap();

        assert!(matches!(
            store.list_tags(),
            Err(StoreError::UnsafeStorePath)
        ));
    }

    // Production break caught: listing silently skips a corrupt tag digest instead of returning a typed error.
    #[test]
    fn list_rejects_broken_tag_digest() {
        let home = TempHome::new();
        let store = Store::new(home.path()).unwrap();
        fs::write(home.path().join("tags/corrupt"), [b'g'; 64]).unwrap();

        assert!(matches!(
            store.list_tags(),
            Err(StoreError::InvalidTagDigest)
        ));

        fs::write(home.path().join("tags/corrupt"), [b'a'; 65]).unwrap();

        assert!(matches!(store.list_tags(), Err(StoreError::TagTooLarge)));
    }
}
