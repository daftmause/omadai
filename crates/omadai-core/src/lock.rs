//! The single-fetch lock on `$state/lock` and the credential-change lock on `$state/keys.lock`.

use crate::error::Error;
use crate::fsutil;
use std::fs::{File, OpenOptions, TryLockError};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// Held for the whole fetch or key change; the flock is released when this drops and the file closes.
#[derive(Debug)]
pub struct Lock {
    _file: File,
}

/// Returns `None` at once where another process (or another open handle) holds the lock.
pub fn try_acquire(path: &Path) -> Result<Option<Lock>, Error> {
    let file = open(path)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(Lock { _file: file })),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(Error::io(format!("lock {}", path.display()), e)),
    }
}

/// Waits for the lock however long another holder keeps it.
pub fn acquire(path: &Path) -> Result<Lock, Error> {
    let file = open(path)?;
    file.lock()
        .map_err(|e| Error::io(format!("lock {}", path.display()), e))?;
    Ok(Lock { _file: file })
}

fn open(path: &Path) -> Result<File, Error> {
    if let Some(dir) = path.parent() {
        fsutil::ensure_private_dir(dir)?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(fsutil::FILE_MODE)
        .open(path)
        .map_err(|e| Error::io(format!("open {}", path.display()), e))?;
    // A lock file created by something else keeps its mode on open, so set it here.
    file.set_permissions(std::fs::Permissions::from_mode(fsutil::FILE_MODE))
        .map_err(|e| Error::io(format!("chmod {}", path.display()), e))?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsutil::testdir::TempDir;

    #[test]
    fn second_handle_is_refused_until_release() {
        let t = TempDir::new("lock");
        let path = t.path().join("state").join("lock");
        let first = try_acquire(&path).unwrap();
        assert!(first.is_some());
        assert!(try_acquire(&path).unwrap().is_none());
        drop(first);
        assert!(try_acquire(&path).unwrap().is_some());
        assert_eq!(fsutil::mode_of(&path), Some(0o600));
    }

    #[test]
    fn acquire_waits_for_the_holder() {
        let t = TempDir::new("lock-wait");
        let path = t.path().join("keys.lock");
        let held = acquire(&path).unwrap();
        let start = std::time::Instant::now();
        std::thread::scope(|s| {
            let waiter = s.spawn(|| {
                let _l = acquire(&path).unwrap();
                start.elapsed()
            });
            std::thread::sleep(std::time::Duration::from_millis(300));
            drop(held);
            assert!(waiter.join().unwrap() >= std::time::Duration::from_millis(300));
        });
    }
}
