//! The data folder's layout, and private files and folders.
//!
//! ```text
//! <data>/run/supervisor.lock       empty; locked for the supervisor's life
//! <data>/run/ck.sock               Unix socket (see ipc::endpoint)
//! <data>/run/services/<id>.json    identity record of each running service
//! <data>/logs/supervisor.log
//! <data>/logs/services/<id>.log
//! <data>/state/dev-services.json   debug builds in a test profile only
//! ```
//!
//! On Unix, folders are 0700 and files 0600. On Windows the data folder gets a
//! protected DACL (`ck_sys::windows::make_private_dir`) that everything inside
//! inherits.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{Error, Result};
use crate::paths::Paths;
use crate::service::spec::ServiceId;

/// A log at or over this size is renamed to `<name>.1` when it is opened.
pub const LOG_ROTATE_BYTES: u64 = 5 * 1024 * 1024;

/// `run/` inside the data folder. The IPC endpoint puts the socket there too.
pub(crate) const RUN_DIR: &str = "run";
const SERVICES_DIR: &str = "services";
const LOGS_DIR: &str = "logs";
const STATE_DIR: &str = "state";

#[cfg(unix)]
use self::unix as sys;
#[cfg(windows)]
use self::windows as sys;

#[cfg(unix)]
pub(crate) use self::unix::check_private_dir;

impl Paths {
    /// `<data>/run`
    pub fn run_dir(&self) -> PathBuf {
        self.data.join(RUN_DIR)
    }

    /// `<data>/run/supervisor.lock`
    pub fn lock_file(&self) -> PathBuf {
        self.run_dir().join("supervisor.lock")
    }

    /// `<data>/run/services`
    pub fn records_dir(&self) -> PathBuf {
        self.run_dir().join(SERVICES_DIR)
    }

    /// `<data>/run/services/<id>.json`
    pub fn record_file(&self, id: &ServiceId) -> PathBuf {
        self.records_dir().join(format!("{id}.json"))
    }

    /// `<data>/logs`
    pub fn logs_dir(&self) -> PathBuf {
        self.data.join(LOGS_DIR)
    }

    /// `<data>/logs/supervisor.log`
    pub fn supervisor_log(&self) -> PathBuf {
        self.logs_dir().join("supervisor.log")
    }

    /// `<data>/logs/services/<id>.log`
    pub fn service_log(&self, id: &ServiceId) -> PathBuf {
        self.service_logs_dir().join(format!("{id}.log"))
    }

    /// `<data>/state`
    pub fn state_dir(&self) -> PathBuf {
        self.data.join(STATE_DIR)
    }

    /// `<data>/state/dev-services.json`
    pub fn dev_services_file(&self) -> PathBuf {
        self.state_dir().join("dev-services.json")
    }

    /// `<data>/logs/services`
    fn service_logs_dir(&self) -> PathBuf {
        self.logs_dir().join(SERVICES_DIR)
    }
}

/// Creates the folder tree privately, or checks and tightens an existing one.
///
/// On Unix the data folder is set to 0700 at every call, and a data folder
/// owned by another uid is refused. `run/` must be a real directory (not a
/// link), owned by this user, with mode 0700. Breaking a rule gives
/// [`Error::NotPrivate`](crate::Error::NotPrivate). On Windows the protected
/// DACL is set once, when this call creates the data folder.
///
/// The supervisor and `client::ensure` call it; `ck paths` never does.
pub fn ensure(paths: &Paths) -> Result<()> {
    let data = &paths.data;
    if let Some(parent) = data.parent() {
        fs::create_dir_all(parent).map_err(|e| Error::io("create", parent, e))?;
    }
    let created = create_dir_if_missing(data)?;
    sys::secure_root(data, created)?;

    // Checked before anything is created inside it.
    let run = paths.run_dir();
    create_private_dir(&run)?;
    sys::check_run_dir(&run)?;

    for dir in [
        paths.records_dir(),
        paths.logs_dir(),
        paths.service_logs_dir(),
        paths.state_dir(),
    ] {
        create_private_dir(&dir)?;
    }
    Ok(())
}

/// Opens a log file for appending, created 0600, after renaming it to
/// `<name>.1` when it has reached [`LOG_ROTATE_BYTES`].
pub fn open_log(path: &Path) -> Result<File> {
    rotate_if_full(path);
    private_file_options()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| Error::io("open", path, e))
}

/// Replaces `path` atomically: writes a 0600 temporary file in the same
/// folder, then renames it over `path`.
pub fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let temp = temp_sibling(path)?;
    let result = write_synced(&temp, contents)
        .and_then(|()| fs::rename(&temp, path).map_err(|e| Error::io("replace", path, e)));
    if result.is_err() {
        // Best effort: the original error is the one worth reporting.
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Creates one folder, 0700 on Unix. An existing folder is left as it is.
pub(crate) fn create_private_dir(path: &Path) -> Result<()> {
    create_dir_if_missing(path).map(|_| ())
}

/// [`create_private_dir`], telling whether this call created the folder.
fn create_dir_if_missing(path: &Path) -> Result<bool> {
    match sys::dir_builder().create(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(Error::io("create", path, e)),
    }
}

/// Options that create files 0600 on Unix. The caller adds the access mode.
pub(crate) fn private_file_options() -> OpenOptions {
    sys::file_options()
}

/// Renames a full log to `<name>.1`, replacing an older one.
///
/// Best effort: on Windows another process may still hold the log open, and
/// a log that cannot rotate must not stop a start; it rotates on a later open.
fn rotate_if_full(path: &Path) {
    let full = fs::metadata(path).is_ok_and(|meta| meta.len() >= LOG_ROTATE_BYTES);
    if full {
        let mut rotated = path.as_os_str().to_owned();
        rotated.push(".1");
        let _ = fs::rename(path, rotated);
    }
}

/// `.<name>.<pid>.<n>.tmp` next to `path`: unique in this process and
/// across processes, so concurrent writers never share a temporary file.
fn temp_sibling(path: &Path) -> Result<PathBuf> {
    static NEXT: AtomicU64 = AtomicU64::new(0);

    let name = path
        .file_name()
        .ok_or_else(|| Error::Internal(format!("{} has no file name", path.display())))?;
    let mut temp = OsString::from(".");
    temp.push(name);
    temp.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    Ok(path.with_file_name(temp))
}

/// Writes `contents` to a new 0600 file and flushes it to disk. The file is
/// closed on return, which Windows needs before the rename.
fn write_synced(path: &Path, contents: &[u8]) -> Result<()> {
    let mut file = private_file_options()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .map_err(|e| Error::io("create", path, e))?;
    file.write_all(contents)
        .map_err(|e| Error::io("write", path, e))?;
    file.sync_all().map_err(|e| Error::io("flush", path, e))
}

#[cfg(unix)]
mod unix {
    use std::fs::{self, DirBuilder, Metadata, OpenOptions, Permissions};
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
    use std::path::Path;

    use crate::error::{Error, Result};

    const DIR_MODE: u32 = 0o700;
    const FILE_MODE: u32 = 0o600;
    /// Permission bits plus setuid, setgid and sticky.
    const MODE_BITS: u32 = 0o7777;

    pub(super) fn dir_builder() -> DirBuilder {
        let mut builder = DirBuilder::new();
        builder.mode(DIR_MODE);
        builder
    }

    pub(super) fn file_options() -> OpenOptions {
        let mut options = OpenOptions::new();
        options.mode(FILE_MODE);
        options
    }

    /// Refuses a data folder that another uid owns, then sets it to 0700, at
    /// every call whether or not it was just created.
    ///
    /// `metadata` follows a link here: the data folder may be a link to
    /// another disk. What matters is who owns the folder and who may enter it.
    pub(super) fn secure_root(data: &Path, _created: bool) -> Result<()> {
        let meta = fs::metadata(data).map_err(|e| Error::io("inspect", data, e))?;
        if !meta.is_dir() {
            return Err(not_private(data, "it is not a folder".to_owned()));
        }
        if let Some(reason) = owner_problem(&meta, ck_sys::unix::effective_uid()) {
            return Err(not_private(data, reason));
        }
        if meta.mode() & MODE_BITS != DIR_MODE {
            fs::set_permissions(data, Permissions::from_mode(DIR_MODE))
                .map_err(|e| Error::io("set permissions on", data, e))?;
        }
        Ok(())
    }

    pub(super) fn check_run_dir(run: &Path) -> Result<()> {
        check_private_dir(run)
    }

    /// Checks with `lstat` that `path` is a real folder (not a link), owned by
    /// this user, with mode exactly 0700. Used for `run/` and for the `/tmp`
    /// folder that holds the socket when the data path is too long.
    pub(crate) fn check_private_dir(path: &Path) -> Result<()> {
        let meta = fs::symlink_metadata(path).map_err(|e| Error::io("inspect", path, e))?;
        match private_dir_problem(&meta, ck_sys::unix::effective_uid()) {
            None => Ok(()),
            Some(reason) => Err(not_private(path, reason)),
        }
    }

    fn private_dir_problem(meta: &Metadata, uid: u32) -> Option<String> {
        let file_type = meta.file_type();
        if file_type.is_symlink() {
            return Some("it is a symbolic link".to_owned());
        }
        if !file_type.is_dir() {
            return Some("it is not a folder".to_owned());
        }
        if let Some(reason) = owner_problem(meta, uid) {
            return Some(reason);
        }
        let mode = meta.mode() & MODE_BITS;
        if mode != DIR_MODE {
            return Some(format!("its mode is {mode:o}, not {DIR_MODE:o}"));
        }
        None
    }

    fn owner_problem(meta: &Metadata, uid: u32) -> Option<String> {
        let owner = meta.uid();
        (owner != uid).then(|| format!("it belongs to uid {owner}, not {uid}"))
    }

    fn not_private(path: &Path, reason: String) -> Error {
        Error::NotPrivate {
            path: path.to_path_buf(),
            reason,
        }
    }
}

#[cfg(windows)]
mod windows {
    use std::fs::{DirBuilder, OpenOptions};
    use std::path::Path;

    use crate::error::{Error, Result};

    pub(super) fn dir_builder() -> DirBuilder {
        DirBuilder::new()
    }

    pub(super) fn file_options() -> OpenOptions {
        OpenOptions::new()
    }

    /// Sets the protected DACL (current user, SYSTEM, Administrators) that
    /// every file and folder inside inherits, only when `ensure` has just
    /// created the data folder.
    ///
    /// `make_private_dir` re-applies the DACL to every existing child, which
    /// is slow on a big tree, so it does not run at every start. Nothing is
    /// inside the folder yet when it does, so every child inherits the DACL.
    pub(super) fn secure_root(data: &Path, created: bool) -> Result<()> {
        if !created {
            return Ok(());
        }
        ck_sys::windows::make_private_dir(data)
            .map_err(|e| Error::io("restrict access to", data, e))
    }

    /// Nothing to check: `run/` inherits the data folder's protected DACL.
    pub(super) fn check_run_dir(_run: &Path) -> Result<()> {
        Ok(())
    }
}
