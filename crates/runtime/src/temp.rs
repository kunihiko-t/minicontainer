//! payload一時fileのRAII管理。

use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{CleanupFailure, RuntimeError};

const PAYLOAD_FILE_NAME: &str = "payload.mcb";
const TEMP_CREATE_ATTEMPTS: usize = 64;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// payload fileとそれを収める一時directoryを所有する。
///
/// drop時にpayload fileを削除してからdirectoryを削除する。directoryは
/// `create_dir`で新規に作られるため、既存entryやsymlinkを scavenging しない。
pub struct PayloadTemp {
    root: PathBuf,
    payload: PathBuf,
}

impl PayloadTemp {
    /// process固有の新しい一時directoryへpayload bytesを書き込む。
    pub fn create(bytes: &[u8]) -> Result<Self, RuntimeError> {
        Self::create_unique_in(
            bytes,
            &std::env::temp_dir(),
            std::process::id(),
            &TEMP_SEQUENCE,
        )
    }

    fn create_unique_in(
        bytes: &[u8],
        parent: &Path,
        process_id: u32,
        sequence: &AtomicU64,
    ) -> Result<Self, RuntimeError> {
        let mut last_collision = None;
        for _ in 0..TEMP_CREATE_ATTEMPTS {
            let candidate = sequence.fetch_add(1, Ordering::Relaxed);
            let root = parent.join(format!("minicontainer-run-{process_id}-{candidate}"));
            match Self::create_in(bytes, &root) {
                Err(RuntimeError::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists => {
                    last_collision = Some(error);
                }
                result => return result,
            }
        }

        Err(RuntimeError::Io(
            last_collision.expect("the retry budget is non-zero"),
        ))
    }

    /// 指定されたroot (まだ存在しないこと) へpayloadを書き込む。
    ///
    /// 書き込みに失敗した場合は、部分書き込みのfileと空のdirectoryを
    /// できる限り取り除いてからerrorを返す。取り除き自体が失敗した場合は、
    /// 書き込みerrorを主原因、取り除き失敗をcleanup診断として両方残す。
    /// 部分fileが存在しないためのNotFoundは失敗として数えない。
    fn create_in(bytes: &[u8], root: &Path) -> Result<Self, RuntimeError> {
        Self::create_in_with_writer(
            bytes,
            root,
            write_payload,
            |path: &Path| std::fs::remove_file(path),
            |path: &Path| std::fs::remove_dir(path),
        )
    }

    fn create_in_with_writer<F, RemoveFile, RemoveDir>(
        bytes: &[u8],
        root: &Path,
        write_payload: F,
        remove_file: RemoveFile,
        remove_dir: RemoveDir,
    ) -> Result<Self, RuntimeError>
    where
        F: FnOnce(&Path, &[u8]) -> io::Result<()>,
        RemoveFile: Fn(&Path) -> io::Result<()>,
        RemoveDir: Fn(&Path) -> io::Result<()>,
    {
        // create_dirは既存entry (symlink含む) の上では失敗するため、
        // 他人のpathを横取りしない。
        create_private_root(root).map_err(RuntimeError::Io)?;
        let payload = root.join(PAYLOAD_FILE_NAME);

        if let Err(write_error) = write_payload(&payload, bytes) {
            let mut failures = Vec::new();
            if let Err(error) = remove_file(&payload)
                && error.kind() != io::ErrorKind::NotFound
            {
                failures.push(CleanupFailure::Payload(error));
            }
            if let Err(error) = remove_dir(root)
                && error.kind() != io::ErrorKind::NotFound
            {
                failures.push(CleanupFailure::Payload(error));
            }
            if failures.is_empty() {
                return Err(RuntimeError::Io(write_error));
            }
            return Err(RuntimeError::Cleanup {
                primary: Box::new(RuntimeError::Io(write_error)),
                failures,
            });
        }

        Ok(Self {
            root: root.to_path_buf(),
            payload,
        })
    }

    /// QEMU `-device loader,file=...`へ渡すpayload path。
    pub fn path(&self) -> &Path {
        &self.payload
    }

    /// payloadを収める一時directory。detached runのUARTやQEMU診断の
    /// 出力先としても使われ、instance stateに記録される回収対象である。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// drop時の削除を行わず、directoryを残したまま所有権を手放す。
    ///
    /// detached runは戻った後もQEMUがUART logを書き続けるため、回収は
    /// instance stateを辿る`minictr stop`へ委ねる。
    pub fn persist(self) -> PathBuf {
        let root = self.root.clone();
        std::mem::forget(self);
        root
    }
}

fn create_private_root(path: &Path) -> io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

fn write_payload(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)
}

impl Drop for PayloadTemp {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.payload);
        let _ = std::fs::remove_dir(&self.root);
    }
}

#[cfg(test)]
mod tests {
    use super::{PayloadTemp, write_payload};
    use crate::{CleanupFailure, RuntimeError};
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicU64;

    // Catches losing cleanup diagnostics when payload creation fails and
    // removal also fails: both the write error and the removal failure stay
    // available, while an absent partial file is not reported as a failure.
    #[test]
    fn failed_write_with_failed_removal_retains_both_diagnostics() {
        let root = std::env::temp_dir().join(format!(
            "minicontainer-runtime-test-{}-removal-failure",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);

        let outcome = PayloadTemp::create_in_with_writer(
            b"payload",
            &root,
            |payload, bytes| {
                std::fs::write(payload, bytes)?;
                Err(io::Error::other("injected write failure"))
            },
            |_| -> io::Result<()> {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "injected removal",
                ))
            },
            |path: &Path| std::fs::remove_dir(path),
        );

        let _ = std::fs::remove_dir_all(&root);
        match outcome {
            Err(error @ RuntimeError::Cleanup { .. }) => {
                let RuntimeError::Cleanup { primary, failures } = error else {
                    unreachable!("matched Cleanup")
                };
                assert!(
                    matches!(*primary, RuntimeError::Io(_)),
                    "the write error stays primary"
                );
                assert!(
                    matches!(
                        failures.as_slice(),
                        [CleanupFailure::Payload(_), CleanupFailure::Payload(_)]
                    ),
                    "both failed removals stay available, got {failures:?}"
                );
            }
            Err(error) => panic!("expected primary and cleanup failures, got {error:?}"),
            Ok(_) => panic!("the injected writer must fail"),
        }
    }

    // Catches reporting a missing partial file as a cleanup failure when the
    // writer fails before creating anything.
    #[test]
    fn failed_write_without_a_partial_file_reports_only_the_write_error() {
        let root = std::env::temp_dir().join(format!(
            "minicontainer-runtime-test-{}-absent-partial",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);

        let outcome: Result<PayloadTemp, RuntimeError> = PayloadTemp::create_in_with_writer(
            b"payload",
            &root,
            |_, _| -> io::Result<()> { Err(io::Error::other("injected write failure")) },
            |path| -> io::Result<()> {
                assert_eq!(path.file_name().unwrap(), "payload.mcb");
                Err(io::Error::new(io::ErrorKind::NotFound, "no partial file"))
            },
            |path: &Path| std::fs::remove_dir(path),
        );

        let _ = std::fs::remove_dir_all(&root);
        match outcome {
            Err(RuntimeError::Io(_)) => {}
            Err(error) => panic!("an absent partial file is not a cleanup failure: {error:?}"),
            Ok(_) => panic!("the injected writer must fail"),
        }
    }

    // Catches leaving the payload file or its directory behind after the run.
    #[test]
    fn payload_temp_writes_the_bundle_and_removes_everything_on_drop() {
        let bytes = b"MINICTR\0payload bytes";
        let path: PathBuf = {
            let temp = PayloadTemp::create(bytes).unwrap();
            assert_eq!(std::fs::read(temp.path()).unwrap(), bytes);
            assert_eq!(temp.path().file_name().unwrap(), "payload.mcb");
            temp.path().to_path_buf()
        };

        assert!(!path.exists(), "payload file must not survive the drop");
        assert!(
            !path.parent().unwrap().exists(),
            "temporary root must not survive the drop"
        );
    }

    // Catches retaining a partially written payload file when the writer
    // reports an error after creating it.
    #[test]
    fn a_failed_partial_write_removes_the_payload_and_root() {
        use std::io::Write;

        let root = std::env::temp_dir().join(format!(
            "minicontainer-runtime-test-{}-partial-write",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);

        let outcome: Result<PayloadTemp, RuntimeError> = PayloadTemp::create_in_with_writer(
            b"MINICTR\0payload bytes",
            &root,
            |payload, bytes| {
                let mut file = std::fs::File::create(payload)?;
                file.write_all(&bytes[..4])?;
                Err(io::Error::other("injected write failure"))
            },
            |path: &Path| std::fs::remove_file(path),
            |path: &Path| std::fs::remove_dir(path),
        );

        assert!(outcome.is_err(), "the injected writer must fail");
        assert!(!root.join("payload.mcb").exists());
        assert!(!root.exists());
    }

    // Catches truncating or following a payload entry that appears before the
    // exclusive create.
    #[test]
    fn payload_writer_rejects_an_existing_entry_without_changing_it() {
        let root = std::env::temp_dir().join(format!(
            "minicontainer-runtime-test-{}-exclusive-write",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir(&root).unwrap();
        let payload = root.join("payload.mcb");
        std::fs::write(&payload, b"existing").unwrap();

        let error = write_payload(&payload, b"replacement").unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&payload).unwrap(), b"existing");
        std::fs::remove_dir_all(&root).unwrap();
    }

    // Catches exposing runtime-owned payload storage through group or other
    // permission bits inherited from the process umask.
    #[cfg(unix)]
    #[test]
    fn payload_storage_denies_group_and_other_access() {
        use std::os::unix::fs::PermissionsExt;

        let temp = PayloadTemp::create(b"private payload").unwrap();
        let payload_mode = std::fs::metadata(temp.path()).unwrap().permissions().mode();
        let root_mode = std::fs::metadata(temp.path().parent().unwrap())
            .unwrap()
            .permissions()
            .mode();

        assert_eq!(root_mode & 0o077, 0, "temporary root must be private");
        assert_eq!(payload_mode & 0o077, 0, "payload file must be private");
    }

    // Catches reusing the same temporary root for sequential runs within one
    // process, which would silently overwrite a live payload.
    #[test]
    fn sequential_creates_use_distinct_roots() {
        let first = PayloadTemp::create(b"first").unwrap();
        let second = PayloadTemp::create(b"second").unwrap();
        assert_ne!(first.path(), second.path());
        assert_eq!(std::fs::read(second.path()).unwrap(), b"second");
    }

    // Catches aborting on a stale first candidate and verifies that counter
    // wraparound advances to the next available root without removing the
    // pre-existing directory.
    #[test]
    fn unique_creation_retries_a_collision_across_counter_wraparound() {
        let parent = std::env::temp_dir().join(format!(
            "minicontainer-runtime-test-{}-collision-parent",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&parent);
        std::fs::create_dir(&parent).unwrap();
        let fake_pid = 4242;
        let stale = parent.join(format!("minicontainer-run-{fake_pid}-{}", u64::MAX));
        std::fs::create_dir(&stale).unwrap();
        let sequence = AtomicU64::new(u64::MAX);

        let temp = PayloadTemp::create_unique_in(b"payload", &parent, fake_pid, &sequence).unwrap();

        assert_eq!(
            temp.path().parent().unwrap().file_name().unwrap(),
            "minicontainer-run-4242-0"
        );
        assert!(stale.exists(), "a colliding root is not owned by this run");
        drop(temp);
        std::fs::remove_dir(&stale).unwrap();
        std::fs::remove_dir(&parent).unwrap();
    }
}
