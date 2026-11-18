//! The single-instance lock: `File::try_lock` on `run/supervisor.lock`.
//!
//! The file stays empty, because the Windows lock is mandatory and would stop
//! clients reading it. Clients learn the supervisor's pid from `hello`.

use std::fs::File;

use crate::error::Result;
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
    pub fn try_acquire(paths: &Paths) -> Result<Option<Self>> {
        let _ = paths;
        todo!()
    }
}
