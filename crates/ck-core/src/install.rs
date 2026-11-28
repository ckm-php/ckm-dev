//! Installing and removing runtime versions: one folder per version,
//! `runtimes/<name>/<version>/`. An install never touches another version's
//! folder, and an uninstall removes only its own. There is no `current`
//! pointer.
//!
//! Each version has one lock, `downloads/<name>-<version>.lock`, taken with a
//! single `try_lock`. It covers the fetch, the check, the unpack and the
//! rename; a second caller gets [`Error::RuntimeBusy`] at once. Every
//! function here expects `layout::ensure` to have created the data folders.

use std::fs::{self, File, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::catalog::Catalog;
use crate::catalog::schema::{Platform, RuntimeName, RuntimeVersion};
use crate::download::fetch::fetch_artifact;
use crate::download::policy::TransportPolicy;
use crate::download::transport::Transport;
use crate::error::{Error, Result};
use crate::extract;
use crate::layout;
use crate::paths::Paths;

#[cfg(unix)]
use self::unix as sys;
#[cfg(windows)]
use self::windows as sys;

/// How many times an install creates `runtimes/<name>/` and renames into it.
/// An uninstall of a sibling version holds another lock, so it may remove
/// the empty folder between the two.
const PARENT_TRIES: u32 = 3;

/// What [`install`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallOutcome {
    /// The version was downloaded and unpacked into this folder.
    Installed(PathBuf),
    /// The folder already existed; nothing was fetched or changed.
    AlreadyPresent(PathBuf),
}

/// One installed version, as found on disk.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct InstalledRuntime {
    pub name: RuntimeName,
    pub version: RuntimeVersion,
}

/// Installs `name` `version` from `catalog` into `runtimes/<name>/<version>/`.
///
/// 1. When the folder exists, returns [`InstallOutcome::AlreadyPresent`]
///    without reading the catalog, fetching or locking. An installed version
///    stays usable when the catalog has expired or no longer lists it, so
///    bundled defaults keep working offline (D5).
/// 2. Takes the artifact for this computer from [`Catalog::artifact`], which
///    refuses an expired catalog at `now`. This comes before the lock, so an
///    expired catalog requests, locks and changes nothing.
/// 3. On Unix, checks that `runtimes/`, `downloads/` and `runtimes/.staging/`
///    are private folders ([`Error::NotPrivate`]), then takes the version's
///    lock.
/// 4. Under the lock, checks the folder again, then removes this version's
///    leftovers ([`crate::layout::is_leftover`]) from both folders.
/// 5. Fetches under `TransportPolicy::current(paths.profile)`, unpacks into a
///    new [`Paths::new_staging_dir`], and renames it into place. On Unix,
///    `runtimes/<name>/` must be a private folder too, and the folders that
///    hold the new entry are synced after the rename. On Windows
///    the rename is retried for a bounded time while another process has a
///    file inside open.
///
/// Any failure removes the staging folder and the archive. Nothing at the
/// version's path is ever replaced: anything there other than a folder is
/// an error.
pub fn install(
    paths: &Paths,
    catalog: &Catalog,
    transport: &dyn Transport,
    name: &RuntimeName,
    version: &RuntimeVersion,
    now: SystemTime,
) -> Result<InstallOutcome> {
    if let Some(path) = installed_path(paths, name, version)? {
        return Ok(InstallOutcome::AlreadyPresent(path));
    }
    let dest = paths.runtime_dir(name, version);
    let platform = Platform::current().ok_or_else(|| Error::NotInCatalog {
        name: name.to_string(),
        version: version.to_string(),
        platform: format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
    })?;
    let artifact = catalog.artifact(name, version, platform, now)?;

    sys::check_private_folders(paths)?;
    let _lock = RuntimeLock::try_acquire(paths, name, version)?;
    if let Some(path) = installed_path(paths, name, version)? {
        return Ok(InstallOutcome::AlreadyPresent(path));
    }
    remove_leftovers(&paths.downloads_dir(), name, version)?;
    remove_leftovers(&paths.staging_dir(), name, version)?;

    let policy = TransportPolicy::current(paths.profile);
    let archive = fetch_artifact(transport, &policy, paths, name, version, artifact)?;
    let staging = paths.new_staging_dir(name, version);
    // `unpack` removes `staging` itself when it fails, and drops the archive,
    // which removes its file, either way.
    extract::unpack(archive, &staging)?;
    if let Err(error) = move_into_place(paths, name, &staging, &dest) {
        // Best effort: the move error is the one worth reporting, and the
        // next install of this version removes a staging folder left behind.
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }
    Ok(InstallOutcome::Installed(dest))
}

/// Removes `name` `version` under its lock: removes what an earlier failed
/// install or uninstall of it left in `runtimes/.staging/`, renames the
/// folder there, deletes it, then removes `runtimes/<name>/` when it is
/// empty. [`Error::NotInstalled`] when the folder does not exist.
///
/// The rename comes first, so the version is either whole in place or gone
/// from `runtimes/<name>/`. When the delete fails after it, the error says
/// the version is uninstalled and names what is left in `runtimes/.staging/`;
/// the next install or uninstall of this version removes it.
pub fn uninstall(paths: &Paths, name: &RuntimeName, version: &RuntimeVersion) -> Result<()> {
    sys::check_private_folders(paths)?;
    let _lock = RuntimeLock::try_acquire(paths, name, version)?;
    // Check the version path before touching staging, so a link or file at
    // `runtimes/<name>/` is rejected without cleaning up through it.
    let installed = installed_path(paths, name, version)?;
    remove_leftovers(&paths.staging_dir(), name, version)?;
    let Some(dest) = installed else {
        return Err(Error::NotInstalled {
            name: name.to_string(),
            version: version.to_string(),
        });
    };
    let staging = paths.new_staging_dir(name, version);
    sys::rename_dir(&dest, &staging).map_err(|e| Error::io("move aside", &dest, e))?;
    let deleted = fs::remove_dir_all(&staging).map_err(|e| {
        Error::Internal(format!(
            "{name} {version} is uninstalled, but {} is left behind ({e}); the next install \
             or uninstall of this version removes it",
            staging.display()
        ))
    });
    let emptied = remove_if_empty(&paths.runtime_versions_dir(name));
    deleted.and(emptied)
}

/// The installed versions, sorted: each `runtimes/<name>/<version>/` folder
/// whose names are valid. `.staging` and anything else is skipped.
///
/// Only real folders count at both levels: links and files are skipped and
/// never followed.
pub fn installed(paths: &Paths) -> Result<Vec<InstalledRuntime>> {
    let mut found = Vec::new();
    for folder in folders_in(&paths.runtimes_dir())? {
        // `.staging` is not a valid runtime name, so it is skipped here.
        let Ok(name) = RuntimeName::new(folder) else {
            continue;
        };
        for folder in folders_in(&paths.runtime_versions_dir(&name))? {
            if let Ok(version) = RuntimeVersion::new(folder) {
                found.push(InstalledRuntime {
                    name: name.clone(),
                    version,
                });
            }
        }
    }
    found.sort();
    Ok(found)
}

/// Holds `downloads/<name>-<version>.lock` until dropped. The file stays
/// empty, and it is never removed: a process that opened it before the
/// removal would lock a file that no later process sees.
struct RuntimeLock {
    _file: File,
}

impl RuntimeLock {
    /// Opens (creating 0600) and tries once to lock the version's lock file.
    /// [`Error::RuntimeBusy`] when another install or uninstall holds it.
    ///
    /// The lock belongs to this open file (`flock` on Unix, `LockFileEx` on
    /// Windows), so a second call in the same process is refused too.
    fn try_acquire(paths: &Paths, name: &RuntimeName, version: &RuntimeVersion) -> Result<Self> {
        let path = paths.runtime_lock_file(name, version);
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
            Ok(()) => Ok(Self { _file: file }),
            Err(TryLockError::WouldBlock) => Err(Error::RuntimeBusy {
                name: name.to_string(),
                version: version.to_string(),
            }),
            Err(TryLockError::Error(e)) => Err(Error::io("lock", path, e)),
        }
    }
}

/// The folder of one installed version, if it is there.
///
/// This checks the path on disk, not the strings in [`installed`]: on a
/// case-insensitive volume, a request for `8.5-CK1` resolves to an installed
/// `8.5-ck1` and needs no catalog. Every folder from `runtimes/` down must be
/// a real directory; a link or a file at any level is an error, so nothing
/// planted through a link is treated as installed or replaced.
pub fn installed_path(
    paths: &Paths,
    name: &RuntimeName,
    version: &RuntimeVersion,
) -> Result<Option<PathBuf>> {
    let versions = paths.runtime_versions_dir(name);
    if !real_dir(&paths.runtimes_dir())? || !real_dir(&versions)? {
        return Ok(None);
    }
    let dest = versions.join(version.as_str());
    if real_dir(&dest)? {
        Ok(Some(dest))
    } else {
        Ok(None)
    }
}

/// Whether `path` is a real folder or absent. A link or any other entry is an
/// error; the path itself is never followed.
fn real_dir(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => Ok(true),
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
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(Error::io("inspect", path, e)),
    }
}

/// Removes what any process left in `dir` for `name` `version`: part files
/// in `downloads/`, staging folders in `runtimes/.staging/`. Only the holder
/// of the version's lock calls it, so nothing it removes is in use.
fn remove_leftovers(dir: &Path, name: &RuntimeName, version: &RuntimeVersion) -> Result<()> {
    let entries = fs::read_dir(dir).map_err(|e| Error::io("list", dir, e))?;
    for entry in entries {
        let entry = entry.map_err(|e| Error::io("list", dir, e))?;
        if !layout::is_leftover(&entry.file_name(), name, version) {
            continue;
        }
        let path = entry.path();
        // `file_type` does not follow a link, so a link is removed itself.
        let file_type = entry
            .file_type()
            .map_err(|e| Error::io("inspect", &path, e))?;
        let removed = if file_type.is_dir() {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
        match removed {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::io("remove", &path, e)),
        }
    }
    Ok(())
}

/// Renames the unpacked `staging` folder to `dest`, creating
/// `runtimes/<name>/` (0700 on Unix, and checked as a private folder) when
/// it is missing, then syncs the folders that hold the new entries (Unix).
///
/// The caller holds the version's lock and found nothing at `dest`. A rename
/// onto an existing empty folder would replace it on Unix, so that check
/// under the lock is what keeps an existing version safe.
fn move_into_place(paths: &Paths, name: &RuntimeName, staging: &Path, dest: &Path) -> Result<()> {
    let versions = paths.runtime_versions_dir(name);
    let mut tries = 0;
    loop {
        tries += 1;
        layout::create_private_dir(&versions)?;
        let moved = sys::check_private_folder(&versions).and_then(|()| {
            sys::rename_dir(staging, dest)
                .map_err(|e| Error::io("rename the unpacked runtime to", dest, e))
        });
        match moved {
            Ok(()) => break,
            Err(Error::Io { source, .. })
                if source.kind() == io::ErrorKind::NotFound && tries < PARENT_TRIES => {}
            Err(error) => return Err(error),
        }
    }
    // `runtimes/` too, because `runtimes/<name>/` may be new. Best effort:
    // the version is already in place, so a failure here is not a failed
    // install.
    for dir in [versions, paths.runtimes_dir()] {
        let _ = sys::sync_dir(&dir);
    }
    Ok(())
}

/// Removes `dir` when it is empty. A folder that is not empty, or already
/// gone, is left as it is.
fn remove_if_empty(dir: &Path) -> Result<()> {
    match fs::remove_dir(dir) {
        Ok(()) => Ok(()),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
            ) =>
        {
            Ok(())
        }
        Err(e) => Err(Error::io("remove", dir, e)),
    }
}

/// The names of the real folders directly inside `dir`, skipping links,
/// files and names that are not UTF-8. A missing `dir` has none.
fn folders_in(dir: &Path) -> Result<Vec<String>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::io("list", dir, e)),
    };
    let mut folders = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| Error::io("list", dir, e))?;
        // `file_type` does not follow a link.
        let file_type = entry
            .file_type()
            .map_err(|e| Error::io("inspect", entry.path(), e))?;
        if file_type.is_dir()
            && !file_type.is_symlink()
            && let Ok(name) = entry.file_name().into_string()
        {
            folders.push(name);
        }
    }
    Ok(folders)
}

#[cfg(unix)]
mod unix {
    use std::fs::{self, File};
    use std::io;
    use std::path::Path;

    use crate::error::Result;
    use crate::layout;
    use crate::paths::Paths;

    /// `runtimes/`, `downloads/` and `runtimes/.staging/` must be real
    /// folders, owned by this user, mode 0700, before anything is created or
    /// removed in them.
    pub(super) fn check_private_folders(paths: &Paths) -> Result<()> {
        layout::check_private_dir(&paths.runtimes_dir())?;
        layout::check_private_dir(&paths.downloads_dir())?;
        layout::check_private_dir(&paths.staging_dir())
    }

    /// `runtimes/<name>/`, checked the same way before a version is renamed
    /// into it.
    pub(super) fn check_private_folder(dir: &Path) -> Result<()> {
        layout::check_private_dir(dir)
    }

    /// One `rename`: atomic within the data folder.
    pub(super) fn rename_dir(from: &Path, to: &Path) -> io::Result<()> {
        fs::rename(from, to)
    }

    /// Makes the entries of `dir` durable, such as a rename into it.
    pub(super) fn sync_dir(dir: &Path) -> io::Result<()> {
        File::open(dir)?.sync_all()
    }
}

#[cfg(windows)]
mod windows {
    use std::fs;
    use std::io;
    use std::path::Path;
    use std::thread;
    use std::time::Duration;

    use crate::error::Result;
    use crate::layout;
    use crate::paths::Paths;

    /// Win32 `ERROR_ACCESS_DENIED` and `ERROR_SHARING_VIOLATION`: another
    /// process, such as a virus scanner or the search indexer, has a file
    /// inside the folder open; which one Windows gives depends on how it was
    /// opened. `ck-core` has no `windows-sys`, and `io::Error::raw_os_error`
    /// gives an `i32`.
    const ERROR_ACCESS_DENIED: i32 = 5;
    const ERROR_SHARING_VIOLATION: i32 = 32;

    /// At most about one second of retries.
    const RENAME_TRIES: u32 = 10;
    const RENAME_PAUSE: Duration = Duration::from_millis(100);

    /// These folders inherit the data folder's protected DACL, but a link
    /// planted there would send cleanup and installs outside it. Check every
    /// one before the lock is taken or any leftover is removed.
    pub(super) fn check_private_folders(paths: &Paths) -> Result<()> {
        layout::check_real_dir(&paths.downloads_dir())?;
        layout::check_real_dir(&paths.runtimes_dir())?;
        layout::check_real_dir(&paths.staging_dir())
    }

    /// The same check for `runtimes/<name>/` before a rename into it.
    pub(super) fn check_private_folder(dir: &Path) -> Result<()> {
        layout::check_real_dir(dir)
    }

    /// A rename, retried for a bounded time while another process has a file
    /// inside open.
    pub(super) fn rename_dir(from: &Path, to: &Path) -> io::Result<()> {
        let mut tries = 1;
        loop {
            match fs::rename(from, to) {
                Err(e)
                    if matches!(
                        e.raw_os_error(),
                        Some(ERROR_ACCESS_DENIED | ERROR_SHARING_VIOLATION)
                    ) && tries < RENAME_TRIES =>
                {
                    tries += 1;
                    thread::sleep(RENAME_PAUSE);
                }
                result => return result,
            }
        }
    }

    /// Nothing to do: Windows cannot sync a folder, and every file was
    /// synced before the rename.
    pub(super) fn sync_dir(_dir: &Path) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Barrier;
    use std::thread;
    use std::time::{Duration, UNIX_EPOCH};

    use flate2::Compression;
    use flate2::write::GzEncoder;
    use serde_json::{Value, json};
    use tar::{Builder, EntryType, Header};

    use super::*;
    use crate::catalog::trust::test_signing::{self, TestKey};
    use crate::catalog::trust::{SIGNING_CONTEXT, Trust};
    use crate::catalog::{accept, cached};
    use crate::download::transport::{FakeReply, FakeTransport};
    use crate::paths::Profile;
    use crate::test_support::{self, TempDir};

    const NOW: u64 = 1_800_000_000;
    const EXPIRES: u64 = NOW + 90 * 24 * 60 * 60;

    fn at(unix_seconds: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(unix_seconds)
    }

    fn php() -> RuntimeName {
        RuntimeName::new("php").unwrap()
    }

    fn ver(text: &str) -> RuntimeVersion {
        RuntimeVersion::new(text).unwrap()
    }

    /// On the first-hop host, so the policy lets the fake answer it.
    fn url(version: &str) -> String {
        format!("https://github.com/example/runtimes/releases/download/v1/php-{version}.tar.gz")
    }

    /// A tar.gz holding `bin/tool`, whose contents are `text`.
    fn archive(text: &str) -> Vec<u8> {
        let mut header = Header::new_ustar();
        header.set_entry_type(EntryType::Regular);
        header.set_mode(0o755);
        header.set_size(text.len() as u64);
        let mut builder = Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
        builder
            .append_data(&mut header, "bin/tool", text.as_bytes())
            .unwrap();
        builder.into_inner().unwrap().finish().unwrap()
    }

    /// An isolated data folder, a fake that serves each php version's
    /// archive, and a catalog accepted at [`NOW`] that pins them for this
    /// computer.
    struct Setup {
        paths: Paths,
        trust: Trust,
        catalog: Catalog,
        transport: FakeTransport,
        _root: TempDir,
    }

    impl Setup {
        fn new(versions: &[(&str, Vec<u8>)]) -> Self {
            let root = TempDir::new("install");
            let paths = Paths {
                home: root.path().to_path_buf(),
                data: root.path().join("data"),
                profile: Profile::Isolated,
            };
            layout::ensure(&paths).unwrap();
            // The artifact must be for the computer the test runs on.
            let platform = Platform::current().expect("a v1 computer");
            let transport = FakeTransport::new();
            let runtimes = versions
                .iter()
                .map(|(version, bytes)| {
                    transport.reply(&url(version), FakeReply::ok(bytes.clone()));
                    test_signing::runtime(
                        "php",
                        version,
                        json!([test_signing::artifact(
                            platform.os.as_str(),
                            platform.arch.as_str(),
                            &url(version),
                            "tar.gz",
                            bytes,
                        )]),
                    )
                })
                .collect();
            let key = TestKey::generate();
            let trust = Trust::new(vec![key.verifying_key()], 0).unwrap();
            let payload = test_signing::payload(1, EXPIRES, Value::Array(runtimes));
            let envelope = key.sign(SIGNING_CONTEXT, &payload).to_bytes();
            let catalog = accept(&paths, &trust, &envelope, at(NOW)).unwrap();
            Self {
                paths,
                trust,
                catalog,
                transport,
                _root: root,
            }
        }

        fn install(&self, version: &str) -> Result<InstallOutcome> {
            install(
                &self.paths,
                &self.catalog,
                &self.transport,
                &php(),
                &ver(version),
                at(NOW),
            )
        }

        fn uninstall(&self, version: &str) -> Result<()> {
            uninstall(&self.paths, &php(), &ver(version))
        }

        fn requests(&self) -> usize {
            self.transport.requests().len()
        }

        fn lock_name(&self, version: &str) -> String {
            name_of(&self.paths.runtime_lock_file(&php(), &ver(version)))
        }
    }

    fn installed_php(versions: &[&str]) -> Vec<InstalledRuntime> {
        versions
            .iter()
            .map(|version| InstalledRuntime {
                name: php(),
                version: ver(version),
            })
            .collect()
    }

    /// The names in `dir`, sorted.
    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    fn name_of(path: &Path) -> String {
        path.file_name().unwrap().to_str().unwrap().to_owned()
    }

    fn is_gone(path: &Path) -> bool {
        fs::symlink_metadata(path).is_err()
    }

    #[test]
    fn an_install_lands_in_its_version_folder_and_a_second_one_fetches_nothing() {
        let setup = Setup::new(&[("8.4.3", archive("php 8.4.3"))]);
        let dir = setup.paths.runtime_dir(&php(), &ver("8.4.3"));

        assert_eq!(
            setup.install("8.4.3").unwrap(),
            InstallOutcome::Installed(dir.clone())
        );
        assert_eq!(
            fs::read(dir.join("bin").join("tool")).unwrap(),
            b"php 8.4.3"
        );
        assert_eq!(setup.requests(), 1);
        // The archive and the staging folder are gone; the lock file stays.
        assert_eq!(
            names_in(&setup.paths.downloads_dir()),
            [setup.lock_name("8.4.3")]
        );
        assert!(names_in(&setup.paths.staging_dir()).is_empty());

        assert_eq!(
            setup.install("8.4.3").unwrap(),
            InstallOutcome::AlreadyPresent(dir)
        );
        assert_eq!(setup.requests(), 1);

        assert_eq!(installed(&setup.paths).unwrap(), installed_php(&["8.4.3"]));

        // A link to a name's folder is never followed, and `.staging` is not
        // a runtime. The shared presence check refuses that link too, so a
        // request for `node 8.4.3` cannot be answered from the linked folder.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(
                setup.paths.runtime_versions_dir(&php()),
                setup.paths.runtimes_dir().join("node"),
            )
            .unwrap();
            assert_eq!(installed(&setup.paths).unwrap(), installed_php(&["8.4.3"]));
            let node = RuntimeName::new("node").unwrap();
            assert!(matches!(
                installed_path(&setup.paths, &node, &ver("8.4.3")),
                Err(Error::NotPrivate { .. })
            ));
        }
    }

    #[test]
    fn two_installs_at_once_fetch_once() {
        for _ in 0..10 {
            let setup = Setup::new(&[("8.4.3", archive("php 8.4.3"))]);
            let barrier = Barrier::new(2);
            let race = || {
                barrier.wait();
                setup.install("8.4.3")
            };
            let outcomes = thread::scope(|scope| {
                let first = scope.spawn(race);
                let second = scope.spawn(race);
                [first.join().unwrap(), second.join().unwrap()]
            });

            let installs = outcomes
                .iter()
                .filter(|outcome| matches!(outcome, Ok(InstallOutcome::Installed(_))))
                .count();
            assert_eq!(installs, 1, "{outcomes:?}");
            assert!(
                outcomes.iter().all(|outcome| matches!(
                    outcome,
                    Ok(InstallOutcome::Installed(_) | InstallOutcome::AlreadyPresent(_))
                        | Err(Error::RuntimeBusy { .. })
                )),
                "{outcomes:?}"
            );
            assert_eq!(setup.requests(), 1, "{outcomes:?}");
            assert_eq!(installed(&setup.paths).unwrap(), installed_php(&["8.4.3"]));
        }
    }

    #[test]
    fn an_install_removes_its_own_leftovers_and_keeps_a_sibling_versions() {
        let setup = Setup::new(&[("8.4.3", archive("php 8.4.3"))]);
        let paths = &setup.paths;
        // `8.4` is a prefix of `8.4.3`, so only an exact match may remove.
        let leave = |version: &str| {
            let part = paths.new_part_file(&php(), &ver(version));
            fs::write(&part, b"partial").unwrap();
            let staging = paths.new_staging_dir(&php(), &ver(version));
            fs::create_dir_all(staging.join("bin")).unwrap();
            fs::write(staging.join("bin").join("tool"), b"half").unwrap();
            (part, staging)
        };
        let (stale_part, stale_staging) = leave("8.4.3");
        let (kept_part, kept_staging) = leave("8.4");

        setup.install("8.4.3").unwrap();
        assert!(is_gone(&stale_part), "{}", stale_part.display());
        assert!(is_gone(&stale_staging), "{}", stale_staging.display());
        let mut downloads = vec![setup.lock_name("8.4.3"), name_of(&kept_part)];
        downloads.sort();
        assert_eq!(names_in(&paths.downloads_dir()), downloads);
        assert_eq!(names_in(&paths.staging_dir()), [name_of(&kept_staging)]);
        assert_eq!(
            fs::read(kept_staging.join("bin").join("tool")).unwrap(),
            b"half"
        );
    }

    #[test]
    fn an_uninstall_removes_one_version_and_leaves_its_sibling() {
        // No other test may fork while a version's lock is open: the child
        // would keep it held after the drop, and the next call on that
        // version would be refused as busy.
        let _spawns = test_support::spawn_guard();
        let setup = Setup::new(&[
            ("8.4.3", archive("php 8.4.3")),
            ("8.4.2", archive("php 8.4.2")),
        ]);
        let paths = &setup.paths;
        for version in ["8.4.3", "8.4.2"] {
            setup.install(version).unwrap();
        }

        setup.uninstall("8.4.3").unwrap();
        assert!(is_gone(&paths.runtime_dir(&php(), &ver("8.4.3"))));
        assert_eq!(
            fs::read(
                paths
                    .runtime_dir(&php(), &ver("8.4.2"))
                    .join("bin")
                    .join("tool")
            )
            .unwrap(),
            b"php 8.4.2"
        );
        assert!(names_in(&paths.staging_dir()).is_empty());
        assert_eq!(installed(paths).unwrap(), installed_php(&["8.4.2"]));

        let result = setup.uninstall("8.4.3");
        assert!(
            matches!(result, Err(Error::NotInstalled { .. })),
            "{result:?}"
        );

        // The last version takes its name's folder with it.
        setup.uninstall("8.4.2").unwrap();
        assert!(is_gone(&paths.runtime_versions_dir(&php())));
        assert_eq!(names_in(&paths.runtimes_dir()), [".staging"]);
        assert!(names_in(&paths.staging_dir()).is_empty());
        assert!(installed(paths).unwrap().is_empty());
    }

    #[test]
    fn a_failed_install_leaves_nothing_behind() {
        // An expired cached catalog installs nothing new: nothing is
        // requested, locked or written. A version already installed needs no
        // catalog, so it is still found.
        let expired = Setup::new(&[
            ("8.4.3", archive("php 8.4.3")),
            ("8.4.2", archive("php 8.4.2")),
        ]);
        expired.install("8.4.2").unwrap();
        let catalog = cached(&expired.paths, &expired.trust).unwrap().unwrap();
        let at_expiry = |version: &str| {
            install(
                &expired.paths,
                &catalog,
                &expired.transport,
                &php(),
                &ver(version),
                at(EXPIRES),
            )
        };
        let result = at_expiry("8.4.3");
        assert!(
            matches!(result, Err(Error::CatalogExpired { expires: EXPIRES })),
            "{result:?}"
        );
        assert_eq!(expired.requests(), 1);
        assert_eq!(
            names_in(&expired.paths.downloads_dir()),
            [expired.lock_name("8.4.2")]
        );
        assert!(is_gone(&expired.paths.runtime_dir(&php(), &ver("8.4.3"))));
        assert_eq!(
            at_expiry("8.4.2").unwrap(),
            InstallOutcome::AlreadyPresent(expired.paths.runtime_dir(&php(), &ver("8.4.2")))
        );
        assert_eq!(expired.requests(), 1);

        // An archive that matches its pins but does not unpack.
        let corrupt = Setup::new(&[("8.4.3", b"not a tar.gz".to_vec())]);
        let result = corrupt.install("8.4.3");
        assert!(
            matches!(result, Err(Error::ArchiveRefused { .. })),
            "{result:?}"
        );
        assert_eq!(corrupt.requests(), 1);
        assert_eq!(
            names_in(&corrupt.paths.downloads_dir()),
            [corrupt.lock_name("8.4.3")]
        );
        assert!(names_in(&corrupt.paths.staging_dir()).is_empty());
        assert!(is_gone(&corrupt.paths.runtime_versions_dir(&php())));
        assert!(installed(&corrupt.paths).unwrap().is_empty());

        // A name's folder that is not private is never renamed into.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let loose = Setup::new(&[("8.4.3", archive("php 8.4.3"))]);
            let versions = loose.paths.runtime_versions_dir(&php());
            fs::create_dir(&versions).unwrap();
            fs::set_permissions(&versions, fs::Permissions::from_mode(0o755)).unwrap();
            let result = loose.install("8.4.3");
            assert!(
                matches!(result, Err(Error::NotPrivate { .. })),
                "{result:?}"
            );
            assert!(names_in(&loose.paths.staging_dir()).is_empty());
            assert!(names_in(&versions).is_empty());
        }
    }

    #[cfg(windows)]
    #[test]
    fn a_rename_waits_while_a_file_inside_is_open() {
        use std::sync::mpsc;

        let root = TempDir::new("rename");
        let from = root.path().join("from");
        let to = root.path().join("to");
        fs::create_dir(&from).unwrap();
        let held = from.join("held");
        fs::write(&held, b"x").unwrap();

        // Held for 300 ms, well inside the retry budget of 10 x 100 ms.
        let (opened, wait) = mpsc::channel();
        let holder = thread::spawn(move || {
            let file = fs::File::open(&held).unwrap();
            opened.send(()).unwrap();
            thread::sleep(Duration::from_millis(300));
            drop(file);
        });
        wait.recv().unwrap();
        super::windows::rename_dir(&from, &to).unwrap();
        holder.join().unwrap();
        assert_eq!(fs::read(to.join("held")).unwrap(), b"x");
    }
}
