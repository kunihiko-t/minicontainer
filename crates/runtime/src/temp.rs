//! payload一時fileのRAII管理。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::RuntimeError;

const PAYLOAD_FILE_NAME: &str = "payload.mcb";

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
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "minicontainer-run-{}-{}",
            std::process::id(),
            sequence
        ));
        Self::create_in(bytes, &root)
    }

    /// 指定されたroot (まだ存在しないこと) へpayloadを書き込む。
    ///
    /// 書き込みに失敗した場合は、部分書き込みのfileと空のdirectoryを
    /// できる限り取り除いてからerrorを返す。
    pub fn create_in(bytes: &[u8], root: &Path) -> Result<Self, RuntimeError> {
        // create_dirは既存entry (symlink含む) の上では失敗するため、
        // 他人のpathを横取りしない。
        std::fs::create_dir(root).map_err(RuntimeError::Io)?;
        let payload = root.join(PAYLOAD_FILE_NAME);

        let write_result = std::fs::write(&payload, bytes);
        if let Err(error) = write_result {
            // 部分書き込みのfileが残っていれば取り除き、空になったdirectoryも
            // 取り除く。directoryの削除は失敗してもerrorを上書きしない。
            let _ = std::fs::remove_file(&payload);
            let _ = std::fs::remove_dir(root);
            return Err(RuntimeError::Io(error));
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
}

impl Drop for PayloadTemp {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.payload);
        let _ = std::fs::remove_dir(&self.root);
    }
}

#[cfg(test)]
mod tests {
    use super::PayloadTemp;
    use std::path::PathBuf;

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

    // Catches retaining a partially written payload file when the write fails.
    #[cfg(unix)]
    #[test]
    fn a_failed_write_leaves_no_temporary_file_behind() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!(
            "minicontainer-runtime-test-{}-readonly",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o555)).unwrap();

        let outcome = PayloadTemp::create_in(b"MINICTR\0payload bytes", &root);

        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        let leftovers = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .count();
        std::fs::remove_dir_all(&root).unwrap();

        assert!(outcome.is_err(), "a read-only root must fail the write");
        assert_eq!(leftovers, 0, "a failed write must leave no partial file");
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
}
