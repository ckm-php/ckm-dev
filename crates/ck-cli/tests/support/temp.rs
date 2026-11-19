//! Temporary folders for the tests, and the check that a path lies inside one.
//!
//! Shared by the debug-only tests (through `support`) and the release-only
//! `release_build.rs` (through `#[path]`), so everything here is used by both.

use std::fs::{self, DirBuilder};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{env, process, thread};

/// A new, empty folder under the system's temporary folder, private to this
/// user. It is removed on drop.
#[derive(Debug)]
pub struct TempRoot(PathBuf);

impl TempRoot {
    pub fn create() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);

        // Short, to keep the socket path short. The time keeps a folder left
        // by an earlier run with the same pid from being reused: creating it
        // fails instead.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.subsec_nanos());
        let name = format!(
            "ck-it-{}-{}-{nanos}",
            process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let path = env::temp_dir().join(name);
        private_dir_builder()
            .create(&path)
            .unwrap_or_else(|error| panic!("cannot create {}: {error}", path.display()));
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        // On Windows a process that is still exiting can hold a file open for
        // a moment.
        for _ in 0..50 {
            match fs::remove_dir_all(&self.0) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => {
                    thread::sleep(Duration::from_millis(100));
                }
                _ => return,
            }
        }
    }
}

#[cfg(unix)]
fn private_dir_builder() -> DirBuilder {
    use std::os::unix::fs::DirBuilderExt;

    let mut builder = DirBuilder::new();
    builder.mode(0o700);
    builder
}

/// The user's temporary folder on Windows already admits only this user,
/// SYSTEM and Administrators.
#[cfg(windows)]
fn private_dir_builder() -> DirBuilder {
    DirBuilder::new()
}

/// Whether `path` lies strictly inside `root`, comparing canonical paths
/// component by component. A plain prefix check is not enough, because the
/// test-profile variables accept `..`.
///
/// `path` need not exist yet: its nearest existing ancestor is canonicalized
/// and the missing names are appended. Panics when that cannot be done, such as
/// for a `..` after a missing folder or a link whose target is missing.
pub fn is_inside(path: &Path, root: &Path) -> bool {
    let root = fs::canonicalize(root)
        .unwrap_or_else(|error| panic!("cannot resolve {}: {error}", root.display()));
    let path = resolve(path);
    path != root && path.starts_with(&root)
}

fn resolve(path: &Path) -> PathBuf {
    let mut missing = Vec::new();
    let mut existing = path;
    let canonical = loop {
        match fs::canonicalize(existing) {
            Ok(canonical) => break canonical,
            Err(error) => {
                let absent = fs::symlink_metadata(existing)
                    .is_err_and(|error| error.kind() == io::ErrorKind::NotFound);
                let (true, Some(name), Some(parent)) =
                    (absent, existing.file_name(), existing.parent())
                else {
                    panic!("cannot resolve {}: {error}", path.display());
                };
                missing.push(name);
                existing = parent;
            }
        }
    };
    missing
        .iter()
        .rev()
        .fold(canonical, |resolved, name| resolved.join(name))
}
