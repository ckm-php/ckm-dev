//! Helpers shared by ck-core's unit tests.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

/// Held by a test while it spawns a child, and by a test that needs a closed
/// file lock to be free at once. Between fork and exec a Unix child holds
/// copies of every descriptor of this test process, so a lock another test
/// thread closes in that window stays held until the child execs.
static SPAWN_LOCK: Mutex<()> = Mutex::new(());

/// Takes [`SPAWN_LOCK`]; a test that panicked while holding it does not
/// poison it for the others.
pub(crate) fn spawn_guard() -> MutexGuard<'static, ()> {
    SPAWN_LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A fresh folder under the system temp folder, removed on drop.
pub(crate) struct TempDir(PathBuf);

impl TempDir {
    /// Creates a new private folder. It fails rather than reuse a folder
    /// that already exists, which another user could have planted.
    pub(crate) fn new(tag: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("ck-sup-{tag}-{}-{nanos}-{n}", std::process::id()));
        #[cfg(unix)]
        let builder = {
            let mut builder = std::fs::DirBuilder::new();
            std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
            builder
        };
        #[cfg(not(unix))]
        let builder = std::fs::DirBuilder::new();
        builder.create(&path).unwrap();
        Self(path)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
