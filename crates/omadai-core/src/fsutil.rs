//! Private directories and atomic whole-file writes.

use crate::error::Error;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

pub const DIR_MODE: u32 = 0o700;
pub const FILE_MODE: u32 = 0o600;

/// Creates `dir` (and missing parents) and sets it to 0700, whatever its earlier mode.
pub fn ensure_private_dir(dir: &Path) -> Result<(), Error> {
    DirBuilder::new()
        .recursive(true)
        .mode(DIR_MODE)
        .create(dir)
        .map_err(|e| Error::io(format!("create {}", dir.display()), e))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(DIR_MODE))
        .map_err(|e| Error::io(format!("chmod {}", dir.display()), e))
}

/// Writes `bytes` to a 0600 temp file beside `path`, syncs it, renames it over `path`, then syncs the directory best-effort.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    atomic_write_with(path, bytes, |dir| fs::File::open(dir)?.sync_all())
}

/// As `atomic_write` into `dir`, which is left with its own mode, so it suits `$HOME`.
pub fn atomic_write_in(dir: &Path, name: &str, bytes: &[u8]) -> Result<(), Error> {
    write_into(dir, &dir.join(name), bytes, |dir| {
        fs::File::open(dir)?.sync_all()
    })
}

fn atomic_write_with(
    path: &Path,
    bytes: &[u8],
    sync_dir: impl FnOnce(&Path) -> std::io::Result<()>,
) -> Result<(), Error> {
    let dir = path
        .parent()
        .ok_or_else(|| Error::invalid(format!("{} has no parent", path.display())))?;
    ensure_private_dir(dir)?;
    write_into(dir, path, bytes, sync_dir)
}

/// Renames onto `path` itself; its file name only labels the temp file.
fn write_into(
    dir: &Path,
    path: &Path,
    bytes: &[u8],
    sync_dir: impl FnOnce(&Path) -> std::io::Result<()>,
) -> Result<(), Error> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let name = path
        .file_name()
        .map_or("file".into(), |n| n.to_string_lossy());
    let tmp = dir.join(format!(
        ".{name}.tmp-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(FILE_MODE)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return Err(Error::io(format!("write {}", path.display()), e));
    }
    // The new bytes are in place, so a failed directory sync only weakens durability; reporting it
    // would make callers undo a write that happened.
    let _ = sync_dir(dir);
    Ok(())
}

/// Reads a file, returning `None` where it does not exist.
pub fn read_optional(path: &Path) -> Result<Option<Vec<u8>>, Error> {
    match fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::io(format!("read {}", path.display()), e)),
    }
}

/// The permission bits of `path`, or `None` where it does not exist.
pub fn mode_of(path: &Path) -> Option<u32> {
    fs::metadata(path)
        .ok()
        .map(|m| m.permissions().mode() & 0o777)
}

#[cfg(test)]
pub(crate) mod testdir {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A unique directory under the system temp dir, removed on drop.
    pub struct TempDir(PathBuf);

    impl TempDir {
        pub fn new(tag: &str) -> Self {
            static N: AtomicU64 = AtomicU64::new(0);
            let p = std::env::temp_dir().join(format!(
                "omadai-core-{tag}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }

        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testdir::TempDir;
    use super::*;

    #[test]
    fn modes_are_private() {
        let t = TempDir::new("fsutil");
        let dir = t.path().join("state").join("omadai");
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let file = dir.join("x.json");
        atomic_write(&file, b"{}").unwrap();
        assert_eq!(mode_of(&dir), Some(0o700));
        assert_eq!(mode_of(&file), Some(0o600));
        atomic_write(&file, b"{\"a\":1}").unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"{\"a\":1}");
        assert_eq!(mode_of(&file), Some(0o600));
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn write_in_keeps_the_directory_mode_and_overwrites() {
        let t = TempDir::new("fsutil-in");
        let home = t.path().join("home");
        fs::create_dir_all(&home).unwrap();
        fs::set_permissions(&home, fs::Permissions::from_mode(0o755)).unwrap();
        atomic_write_in(&home, "out.txt", b"one").unwrap();
        atomic_write_in(&home, "out.txt", b"two").unwrap();
        assert_eq!(fs::read(home.join("out.txt")).unwrap(), b"two");
        assert_eq!(mode_of(&home.join("out.txt")), Some(0o600));
        assert_eq!(mode_of(&home), Some(0o755), "the directory keeps its mode");
        assert!(
            atomic_write_in(&t.path().join("absent"), "out.txt", b"x").is_err(),
            "a missing directory is not created"
        );
    }

    #[test]
    fn a_non_utf8_name_is_written_at_its_own_path() {
        use std::os::unix::ffi::OsStrExt;
        let t = TempDir::new("fsutil-name");
        let file = t
            .path()
            .join(std::ffi::OsStr::from_bytes(b"cache-\xff.json"));
        atomic_write(&file, b"{}").unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"{}");
        assert!(!t.path().join("file").exists());
    }

    #[test]
    fn directory_sync_failure_after_the_rename_still_succeeds() {
        let t = TempDir::new("fsutil-sync");
        let file = t.path().join("config.toml");
        atomic_write(&file, b"old").unwrap();
        let written = atomic_write_with(&file, b"new", |_| {
            Err(std::io::Error::other("fsync refused"))
        });
        assert!(written.is_ok(), "{written:?}");
        assert_eq!(fs::read(&file).unwrap(), b"new");
    }
}
