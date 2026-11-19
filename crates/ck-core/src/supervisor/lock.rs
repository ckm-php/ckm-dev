//! The single-instance lock: `File::try_lock` on `run/supervisor.lock`.
//!
//! The file stays empty, because the Windows lock is mandatory and would stop
//! clients reading it. Clients learn the supervisor's pid from `hello`.

use std::fs::{File, TryLockError};

use crate::error::{Error, Result};
use crate::layout;
use crate::paths::Paths;

/// Proof that this process is the one supervisor for a data folder. Dropping
/// it releases the lock.
#[derive(Debug)]
pub struct InstanceLock {
    _file: File,
}

impl InstanceLock {
    /// Opens (creating 0600) and tries to lock `run/supervisor.lock`.
    /// `Ok(None)` when another process holds it.
    ///
    /// The lock belongs to this open file (`flock` on Unix, `LockFileEx` on
    /// Windows), so a second call in the same process is refused too. std
    /// opens the file non-inheritable, so service children never hold it.
    pub fn try_acquire(paths: &Paths) -> Result<Option<Self>> {
        let path = paths.lock_file();
        // Creating needs write access, and Windows locks only handles opened
        // for reading or writing. Nothing is ever written.
        let file = layout::private_file_options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| Error::io("open", &path, e))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(e)) => Err(Error::io("lock", path, e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::paths::Profile;
    use crate::test_support::{self, TempDir};

    #[test]
    fn a_second_lock_on_the_same_folder_is_refused_while_the_first_is_held() {
        // No other test may fork while the first lock is open: the child
        // would keep it held after the drop below.
        let _spawns = test_support::spawn_guard();
        let root = TempDir::new("lock");
        let paths = Paths {
            home: root.path().to_path_buf(),
            data: root.path().join("data"),
            profile: Profile::Isolated,
        };
        fs::create_dir_all(paths.run_dir()).unwrap();

        let first = InstanceLock::try_acquire(&paths)
            .unwrap()
            .expect("nobody holds the lock yet");
        // A separate open of the same file, as a second supervisor makes.
        assert!(InstanceLock::try_acquire(&paths).unwrap().is_none());
        assert!(InstanceLock::try_acquire(&paths).unwrap().is_none());

        drop(first);
        let again = InstanceLock::try_acquire(&paths).unwrap();
        assert!(again.is_some(), "dropping the lock must release it");
        assert_eq!(fs::metadata(paths.lock_file()).unwrap().len(), 0);
    }
}
