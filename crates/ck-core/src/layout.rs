//! The data folder's layout, and private files and folders.
//!
//! ```text
//! <data>/run/supervisor.lock       empty; locked for the supervisor's life
//! <data>/run/ck.sock               Unix socket (see ipc::endpoint)
//! <data>/run/services/<id>.json    identity record of each running service
//! <data>/logs/supervisor.log
//! <data>/logs/services/<id>.log
//! <data>/state/dev-services.json   debug builds in a test profile only
//! <data>/state/catalog-state.json  sequence and payload SHA-256 of the accepted catalog
//! <data>/state/catalog.lock        empty; locked while a catalog is accepted
//! <data>/catalog/catalog.json      the accepted catalog: its exact signed envelope
//! <data>/downloads/<name>-<version>.lock                 empty; locked for an install or uninstall
//! <data>/downloads/<name>-<version>.<pid>.<n>.part       a download in progress
//! <data>/runtimes/<name>/<version>/                      an installed runtime version
//! <data>/runtimes/.staging/<name>-<version>.<pid>.<n>/   an unpack or removal in progress
//! ```
//!
//! On Unix, folders are 0700 and files 0600. On Windows the data folder keeps a
//! protected DACL (`ck_sys::windows::make_private_dir`) that everything inside
//! inherits.

use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::catalog::schema::{RuntimeName, RuntimeVersion};
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
const CATALOG_DIR: &str = "catalog";
const DOWNLOADS_DIR: &str = "downloads";
const RUNTIMES_DIR: &str = "runtimes";
const STAGING_DIR: &str = ".staging";
const PART_SUFFIX: &str = ".part";

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

    /// `<data>/state/catalog-state.json`
    pub fn catalog_state_file(&self) -> PathBuf {
        self.state_dir().join("catalog-state.json")
    }

    /// `<data>/state/catalog.lock`
    pub fn catalog_lock_file(&self) -> PathBuf {
        self.state_dir().join("catalog.lock")
    }

    /// `<data>/catalog`
    pub fn catalog_dir(&self) -> PathBuf {
        self.data.join(CATALOG_DIR)
    }

    /// `<data>/catalog/catalog.json`
    pub fn catalog_file(&self) -> PathBuf {
        self.catalog_dir().join("catalog.json")
    }

    /// `<data>/downloads`
    pub fn downloads_dir(&self) -> PathBuf {
        self.data.join(DOWNLOADS_DIR)
    }

    /// `<data>/downloads/<name>-<version>.lock`
    pub fn runtime_lock_file(&self, name: &RuntimeName, version: &RuntimeVersion) -> PathBuf {
        self.downloads_dir()
            .join(format!("{}.lock", target(name, version)))
    }

    /// `<data>/downloads/<name>-<version>.<pid>.<n>.part`: a new name at
    /// every call, in this process and across processes.
    pub fn new_part_file(&self, name: &RuntimeName, version: &RuntimeVersion) -> PathBuf {
        self.downloads_dir()
            .join(format!("{}{PART_SUFFIX}", unique_name(name, version)))
    }

    /// `<data>/runtimes`
    pub fn runtimes_dir(&self) -> PathBuf {
        self.data.join(RUNTIMES_DIR)
    }

    /// `<data>/runtimes/<name>`
    pub fn runtime_versions_dir(&self, name: &RuntimeName) -> PathBuf {
        self.runtimes_dir().join(name.as_str())
    }

    /// `<data>/runtimes/<name>/<version>`
    pub fn runtime_dir(&self, name: &RuntimeName, version: &RuntimeVersion) -> PathBuf {
        self.runtime_versions_dir(name).join(version.as_str())
    }

    /// `<data>/runtimes/.staging`
    pub fn staging_dir(&self) -> PathBuf {
        self.runtimes_dir().join(STAGING_DIR)
    }

    /// `<data>/runtimes/.staging/<name>-<version>.<pid>.<n>`: a new name at
    /// every call, like [`Paths::new_part_file`].
    pub fn new_staging_dir(&self, name: &RuntimeName, version: &RuntimeVersion) -> PathBuf {
        self.staging_dir().join(unique_name(name, version))
    }

    /// `<data>/logs/services`
    fn service_logs_dir(&self) -> PathBuf {
        self.logs_dir().join(SERVICES_DIR)
    }
}

/// Whether `file_name`, in `downloads/` or `runtimes/.staging/`, is a part
/// file or staging folder of `name` `version` that any process left:
/// `<name>-<version>.<pid>.<n>`, plus `.part` in `downloads/`.
///
/// The match is exact, so `php` `8.2` never matches what `php` `8.2.1`
/// left. Only the holder of the version's lock may remove what matches.
pub fn is_leftover(file_name: &OsStr, name: &RuntimeName, version: &RuntimeVersion) -> bool {
    let Some(file_name) = file_name.to_str() else {
        return false;
    };
    let stem = file_name.strip_suffix(PART_SUFFIX).unwrap_or(file_name);
    let mut parts = stem.rsplitn(3, '.');
    let (Some(n), Some(pid), Some(rest)) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    let is_number = |part: &str| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
    is_number(n) && is_number(pid) && rest == target(name, version)
}

/// `<name>-<version>`.
fn target(name: &RuntimeName, version: &RuntimeVersion) -> String {
    format!("{name}-{version}")
}

/// `<name>-<version>.<pid>.<n>`, unique in this process and across processes.
fn unique_name(name: &RuntimeName, version: &RuntimeVersion) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);

    format!(
        "{}.{}.{}",
        target(name, version),
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// Creates the folder tree privately, or checks and tightens an existing one.
///
/// On Unix the data folder is set to 0700 at every call, and a data folder
/// owned by another uid is refused. Every folder inside must be a real
/// directory (not a link), owned by this user, with mode 0700; each is checked
/// before anything is created inside it. On Windows the data folder must keep
/// its protected DACL: it is re-applied when this user owns the folder and has
/// lost it. Breaking a rule gives [`Error::NotPrivate`](crate::Error::NotPrivate).
///
/// The supervisor and `client::ensure` call it; `ck paths` never does.
pub fn ensure(paths: &Paths) -> Result<()> {
    let data = &paths.data;
    if let Some(parent) = data.parent() {
        fs::create_dir_all(parent).map_err(|e| Error::io("create", parent, e))?;
    }
    let created = create_dir_if_missing(data)?;
    sys::secure_root(data, created)?;

    // Parents come before their children, so a planted link is refused
    // before anything is created through it.
    for dir in [
        paths.run_dir(),
        paths.records_dir(),
        paths.logs_dir(),
        paths.service_logs_dir(),
        paths.state_dir(),
        paths.catalog_dir(),
        paths.downloads_dir(),
        paths.runtimes_dir(),
        paths.staging_dir(),
    ] {
        // Check before creating anything through the folder, on every OS:
        // on Windows the DACL check below is only a no-op inheritance rule.
        check_real_dir(&dir)?;
        create_private_dir(&dir)?;
        // Windows folders inherit the data folder's protected DACL instead.
        #[cfg(unix)]
        check_private_dir(&dir)?;
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

/// Refuses a link or file at `path` before anything is created through it;
/// a missing folder is fine. Unix also checks ownership and mode after
/// creation, and Windows inherits the data folder's DACL, so this one check
/// is what stops a planted link on Windows.
pub(crate) fn check_real_dir(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => Ok(()),
        Ok(meta) => {
            let reason = if meta.file_type().is_symlink() {
                "it is a symbolic link"
            } else {
                "it is not a folder"
            };
            Err(Error::NotPrivate {
                path: path.to_path_buf(),
                reason: reason.to_owned(),
            })
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::io("inspect", path, e)),
    }
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

    /// Checks with `lstat` that `path` is a real folder (not a link), owned by
    /// this user, with mode exactly 0700. Used for every folder inside the
    /// data folder and for the `/tmp` folder that holds the socket when the
    /// data path is too long.
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
    use std::fs::{self, DirBuilder, OpenOptions};
    use std::path::Path;

    use crate::error::{Error, Result};

    pub(super) fn dir_builder() -> DirBuilder {
        DirBuilder::new()
    }

    pub(super) fn file_options() -> OpenOptions {
        OpenOptions::new()
    }

    /// Gives the data folder the protected DACL (current user, SYSTEM,
    /// Administrators) that every file and folder inside inherits.
    ///
    /// A folder this call created gets it at once; if that fails, the empty
    /// folder is removed so the next start creates it again. An existing
    /// folder is checked at every start: one this user owns gets the DACL
    /// again (as Unix re-applies 0700), and one another account owns is
    /// refused. `make_private_dir` re-applies the DACL to every child, which
    /// is slow on a big tree, so it runs only when the folder is not private.
    pub(super) fn secure_root(data: &Path, created: bool) -> Result<()> {
        if created {
            return ck_sys::windows::make_private_dir(data).map_err(|e| {
                // Best effort: the DACL error is the one worth reporting.
                let _ = fs::remove_dir(data);
                Error::io("restrict access to", data, e)
            });
        }
        let private = ck_sys::windows::is_private_dir(data)
            .map_err(|e| Error::io("read the access rules of", data, e))?;
        if private {
            return Ok(());
        }
        let owned = ck_sys::windows::is_owned_by_current_user(data)
            .map_err(|e| Error::io("read the owner of", data, e))?;
        if !owned {
            return Err(Error::NotPrivate {
                path: data.to_path_buf(),
                reason: "it belongs to another account".to_owned(),
            });
        }
        ck_sys::windows::make_private_dir(data)
            .map_err(|e| Error::io("restrict access to", data, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Profile;
    use crate::test_support::TempDir;

    fn profile(root: &TempDir) -> Paths {
        Paths {
            home: root.path().to_path_buf(),
            data: root.path().join("data"),
            profile: Profile::Isolated,
        }
    }

    #[test]
    fn ensure_creates_the_runtime_folders_privately() {
        let root = TempDir::new("layout");
        let paths = profile(&root);
        ensure(&paths).unwrap();
        for dir in [
            paths.catalog_dir(),
            paths.downloads_dir(),
            paths.runtimes_dir(),
            paths.staging_dir(),
        ] {
            #[cfg(unix)]
            check_private_dir(&dir).unwrap();
            #[cfg(windows)]
            assert!(dir.is_dir(), "{}", dir.display());
        }
    }

    #[cfg(unix)]
    #[test]
    fn ensure_refuses_a_planted_link_inside_the_data_folder() {
        let root = TempDir::new("layout");
        let paths = profile(&root);
        let elsewhere = root.path().join("elsewhere");
        create_private_dir(&elsewhere).unwrap();
        create_private_dir(&paths.data).unwrap();
        std::os::unix::fs::symlink(&elsewhere, paths.logs_dir()).unwrap();

        assert!(matches!(ensure(&paths), Err(Error::NotPrivate { .. })));
        // Nothing was created through the link.
        assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
    }

    #[test]
    fn leftovers_match_their_own_version_exactly() {
        let root = TempDir::new("layout");
        let paths = profile(&root);
        let php = RuntimeName::new("php").unwrap();
        let line = RuntimeVersion::new("8.2").unwrap();
        let patch = RuntimeVersion::new("8.2.1").unwrap();

        let part = paths.new_part_file(&php, &line);
        assert_ne!(part, paths.new_part_file(&php, &line));
        let staging = paths.new_staging_dir(&php, &line);
        for leftover in [&part, &staging] {
            let file_name = leftover.file_name().unwrap();
            assert!(
                is_leftover(file_name, &php, &line),
                "{}",
                leftover.display()
            );
            assert!(
                !is_leftover(file_name, &php, &patch),
                "{}",
                leftover.display()
            );
        }

        let patch_part = paths.new_part_file(&php, &patch);
        assert!(!is_leftover(patch_part.file_name().unwrap(), &php, &line));
        let lock = paths.runtime_lock_file(&php, &line);
        assert!(!is_leftover(lock.file_name().unwrap(), &php, &line));
        for other in [
            "php-8.2.part",
            "php-8.2.x.1.part",
            "php-8.2..1",
            "node-8.2.1.2",
        ] {
            assert!(!is_leftover(OsStr::new(other), &php, &line), "{other}");
        }
    }
}
