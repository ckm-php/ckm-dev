//! Safe extraction of tar.gz and zip archives.
//!
//! The crates' own unpack helpers are never called: the entries are read one
//! by one and each file is written here, under the name rules of [`names`].
//! Any broken rule refuses the whole archive.
//!
//! - **Entries:** regular files and folders. Symlinks only on Linux and macOS,
//!   under the link rules; on Windows any link refuses the archive. Hard links,
//!   devices, fifos, sparse entries, and encrypted zip entries or methods other
//!   than Stored and Deflate are refused. PAX global headers are skipped.
//! - **Names:** besides the rules of [`names`], no two entries, and no entry
//!   and a folder another entry implies, are equal after case folding, and
//!   nothing sits inside a file or a link. Nothing is ever overwritten, so two
//!   names that the file system treats as one refuse the archive too.
//! - **Links:** the target is relative, passes the name rules after a leading
//!   run of `..`, and, resolved from the link's folder, stays inside the
//!   archive below its top. No folder on the way to the link or to its target
//!   is a link in the archive. Links are created last.
//! - **Caps:** [`MAX_ENTRIES`] entries, where each folder that only an entry
//!   implies counts as one too, and [`MAX_UNPACKED_BYTES`], counting the bytes
//!   actually produced (tar's extension headers included), not the sizes the
//!   headers declare. A file must hold exactly the bytes its header lists.
//! - **Modes (Unix):** files 0755 when any execute bit is set, else 0644;
//!   folders 0755, whatever the umask. No setuid, setgid or sticky bit, owner
//!   or mtime is kept.

pub mod names;
mod tar_gz;
mod zip;

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use self::names::EntryPath;
use crate::catalog::schema::ArchiveFormat;
use crate::download::verified::VerifiedFile;
use crate::error::{Error, Result, escape_untrusted};

/// The most entries one archive may hold.
pub const MAX_ENTRIES: u64 = 100_000;

/// The most bytes one archive may unpack to.
pub const MAX_UNPACKED_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// The copy buffer, allocated once per unpack.
const COPY_BUFFER_BYTES: usize = 64 * 1024;

/// Unpacks `archive` into `dest`, a folder that must not exist yet (its
/// parent must), reading through the archive's own handle.
///
/// Files are created with `create_new` and synced (`sync_data`); links are
/// created last. On Unix, `dest` itself is synced too, after every folder
/// inside it. On any failure `dest` is removed, and nothing is written outside
/// it. `archive` is dropped either way, which removes its file. A broken rule
/// gives [`Error::ArchiveEntryRefused`] or [`Error::ArchiveRefused`].
pub(crate) fn unpack(archive: VerifiedFile, dest: &Path) -> Result<()> {
    unpack_with(archive, dest, Limits::DEFAULT)
}

/// The caps for one unpack: the public constants, or small ones in tests.
#[derive(Debug, Clone, Copy)]
struct Limits {
    entries: u64,
    bytes: u64,
}

impl Limits {
    const DEFAULT: Self = Self {
        entries: MAX_ENTRIES,
        bytes: MAX_UNPACKED_BYTES,
    };
}

fn unpack_with(mut archive: VerifiedFile, dest: &Path, limits: Limits) -> Result<()> {
    // `dest` is not ours until this call creates it, so a failure here must
    // leave an existing folder alone.
    sys::create_dir(dest).map_err(|e| Error::io("create", dest, e))?;
    let format = archive.format();
    let result = archive.reader().and_then(|file| {
        let mut tree = Tree::new(dest, limits);
        match format {
            ArchiveFormat::TarGz => self::tar_gz::read(file, &mut tree)?,
            ArchiveFormat::Zip => self::zip::read(file, &mut tree)?,
        }
        tree.finish()
    });
    if result.is_err() {
        // Best effort: the unpack error is the one worth reporting, and the
        // next install of this version removes a staging folder left behind.
        let _ = fs::remove_dir_all(dest);
    }
    result
}

/// What an entry, or a folder an entry implies, is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Folder,
    File,
    Link,
}

impl Kind {
    fn noun(self) -> &'static str {
        match self {
            Self::Folder => "folder",
            Self::File => "file",
            Self::Link => "link",
        }
    }
}

/// A name taken in the archive, keyed by its case-folded path.
struct Name {
    path: String,
    kind: Kind,
    /// Listed as an entry, not only implied by one inside it.
    listed: bool,
}

/// A link waiting to be created, after every file.
struct Link {
    path: EntryPath,
    /// The target as it is written to disk: the leading `..` run, then the
    /// rest of the target as [`EntryPath::parse`] left it.
    target: String,
    /// The target resolved from the link's folder, relative to the root.
    resolved: String,
}

impl Link {
    /// Link rules 1 and 2: the target is relative, has `..` only as a leading
    /// run and passes the name rules after it, and, resolved from the link's
    /// folder, stays inside the archive. A target that resolves to the top of
    /// the archive is refused too.
    fn new(path: EntryPath, raw: &[u8]) -> Result<Self> {
        let refuse = |why: &str| {
            refused(
                path.as_str().as_bytes(),
                format!(
                    "is a link to {:?}, which {why}",
                    String::from_utf8_lossy(raw)
                ),
            )
        };
        if raw.is_empty() {
            return Err(refuse("is empty"));
        }
        if raw.len() > names::MAX_PATH_BYTES {
            return Err(refuse("is too long"));
        }
        let mut rest = raw;
        let mut ups = 0;
        while let Some(after) = rest.strip_prefix(b"../") {
            ups += 1;
            rest = after;
        }
        if rest == b".." {
            ups += 1;
            rest = b"";
        }
        let tail = if rest.is_empty() {
            None
        } else {
            match EntryPath::parse(rest) {
                Ok(tail) => tail,
                Err(Error::ArchiveEntryRefused { reason, .. }) => {
                    return Err(refuse(format!("breaks a name rule: it {reason}").as_str()));
                }
                Err(other) => return Err(other),
            }
        };
        let tail_components = || tail.iter().flat_map(|part| part.components());
        let resolved = {
            let folders: Vec<&str> = path.components().collect();
            let depth = folders.len().saturating_sub(1);
            let Some(kept) = depth.checked_sub(ups) else {
                return Err(refuse("leads out of the archive"));
            };
            folders
                .iter()
                .take(kept)
                .copied()
                .chain(tail_components())
                .collect::<Vec<_>>()
                .join("/")
        };
        if resolved.is_empty() {
            return Err(refuse("is the top of the archive"));
        }
        let target = std::iter::repeat_n("..", ups)
            .chain(tail_components())
            .collect::<Vec<_>>()
            .join("/");
        Ok(Self {
            path,
            target,
            resolved,
        })
    }
}

/// The unpack in progress: the caps' counters, every name taken, the links
/// still to create, and the folders to sync.
struct Tree<'a> {
    root: &'a Path,
    limits: Limits,
    entries: u64,
    bytes: u64,
    names: HashMap<String, Name>,
    links: Vec<Link>,
    /// Every folder created, the root first.
    dirs: Vec<PathBuf>,
    buffer: Vec<u8>,
}

impl<'a> Tree<'a> {
    fn new(root: &'a Path, limits: Limits) -> Self {
        Self {
            root,
            limits,
            entries: 0,
            bytes: 0,
            names: HashMap::new(),
            links: Vec::new(),
            dirs: vec![root.to_path_buf()],
            buffer: vec![0; COPY_BUFFER_BYTES],
        }
    }

    /// Counts one more entry against the entry cap.
    fn count_entry(&mut self) -> Result<()> {
        self.entries += 1;
        self.check_entries(self.entries)
    }

    /// Refuses `entries` over the entry cap, before any is read.
    fn check_entries(&self, entries: u64) -> Result<()> {
        if entries > self.limits.entries {
            return Err(archive_refused(format!(
                "it has more than {} entries",
                self.limits.entries
            )));
        }
        Ok(())
    }

    /// Counts `bytes` more against the size cap.
    fn add_bytes(&mut self, bytes: u64) -> Result<()> {
        self.bytes = self.bytes.saturating_add(bytes);
        if self.bytes > self.limits.bytes {
            return Err(self.too_big());
        }
        Ok(())
    }

    fn too_big(&self) -> Error {
        archive_refused(format!(
            "it unpacks to more than {} bytes",
            self.limits.bytes
        ))
    }

    /// A folder entry.
    fn dir(&mut self, path: &EntryPath) -> Result<()> {
        self.claim(path, Kind::Folder)
    }

    /// A regular file entry holding `size` bytes, read from `data`.
    fn file(
        &mut self,
        path: &EntryPath,
        executable: bool,
        size: u64,
        data: &mut dyn Read,
    ) -> Result<()> {
        self.claim(path, Kind::File)?;
        if self.bytes.saturating_add(size) > self.limits.bytes {
            return Err(self.too_big());
        }
        let target_buf = path.to_path(self.root);
        let target: &Path = &target_buf;
        let mut file = sys::create_file(target, executable)
            .map_err(|e| create_error(path.as_str(), target, e))?;
        let written = self.copy(path, data, &mut file, target, size)?;
        if written != size {
            let held = if written > size {
                "more".to_owned()
            } else {
                written.to_string()
            };
            return Err(refused(
                path.as_str().as_bytes(),
                format!("holds {held} bytes, but its header lists {size}"),
            ));
        }
        file.sync_data().map_err(|e| Error::io("flush", target, e))
    }

    /// The counting writer: copies `data` into `file`, at most one byte past
    /// `size`, counting each byte against the size cap before it is written.
    /// Returns the bytes copied.
    fn copy(
        &mut self,
        path: &EntryPath,
        data: &mut dyn Read,
        file: &mut File,
        target: &Path,
        size: u64,
    ) -> Result<u64> {
        let mut data = data.take(size.saturating_add(1));
        let mut copied = 0;
        loop {
            let n = match data.read(&mut self.buffer) {
                Ok(0) => return Ok(copied),
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    return Err(refused(
                        path.as_str().as_bytes(),
                        format!("cannot be read: {}", escape_untrusted(&e.to_string())),
                    ));
                }
            };
            copied += n as u64;
            self.add_bytes(n as u64)?;
            file.write_all(&self.buffer[..n])
                .map_err(|e| Error::io("write", target, e))?;
        }
    }

    /// A symbolic link entry. Rules 1 and 2 are checked here, rule 3 once
    /// every entry is known. On Windows any link refuses the archive.
    fn symlink(&mut self, path: EntryPath, target: &[u8]) -> Result<()> {
        let link = Link::new(path, target)?;
        self.claim(&link.path, Kind::Link)?;
        if cfg!(windows) {
            return Err(refused(
                link.path.as_str().as_bytes(),
                format!(
                    "is a link to {:?}, and archives with links are refused on Windows",
                    link.target
                ),
            ));
        }
        self.links.push(link);
        Ok(())
    }

    /// Records `path` as a `kind`, and each folder above it as a folder,
    /// creating the folders that are new. Refuses a name taken before, or one
    /// equal to a taken name after case folding, and anything inside a file or
    /// a link. Each folder that only an entry implies counts as an entry.
    fn claim(&mut self, path: &EntryPath, kind: Kind) -> Result<()> {
        let full = path.as_str();
        let refuse = |why: String| refused(full.as_bytes(), why);
        let ends = full
            .match_indices('/')
            .map(|(end, _)| end)
            .chain([full.len()]);
        for end in ends {
            let name = &full[..end];
            let last = end == full.len();
            let want = if last { kind } else { Kind::Folder };
            let key = fold(name);
            if let Some(taken) = self.names.get_mut(&key) {
                if taken.path != name {
                    return Err(refuse(format!(
                        "differs only in case from {:?}",
                        taken.path
                    )));
                }
                if !last {
                    if taken.kind != Kind::Folder {
                        return Err(refuse(format!(
                            "is inside {:?}, which is a {}",
                            taken.path,
                            taken.kind.noun()
                        )));
                    }
                    continue;
                }
                if taken.kind != want {
                    return Err(refuse(format!(
                        "is both a {} and a {}",
                        taken.kind.noun(),
                        want.noun()
                    )));
                }
                if taken.listed {
                    return Err(refuse("is listed twice".to_owned()));
                }
                // A folder that entries inside it implied, now listed itself.
                taken.listed = true;
                continue;
            }
            if !last {
                self.count_entry()?;
            }
            if want == Kind::Folder {
                self.make_dir(name)?;
            }
            self.names.insert(
                key,
                Name {
                    path: name.to_owned(),
                    kind: want,
                    listed: last,
                },
            );
        }
        Ok(())
    }

    /// Creates the folder `name`, a `/`-separated path under the root.
    fn make_dir(&mut self, name: &str) -> Result<()> {
        let path = name
            .split('/')
            .fold(self.root.to_path_buf(), |path, component| {
                path.join(component)
            });
        sys::create_dir(&path).map_err(|e| create_error(name, &path, e))?;
        self.dirs.push(path);
        Ok(())
    }

    /// Link rule 3 for the target: the first folder on the way to it that is
    /// a link in the archive. [`Tree::claim`] refuses a link on the way to the
    /// link itself.
    fn link_on_the_way<'l>(&self, link: &'l Link) -> Option<&'l str> {
        link.resolved
            .match_indices('/')
            .map(|(end, _)| &link.resolved[..end])
            .find(|folder| {
                self.names
                    .get(&fold(folder))
                    .is_some_and(|name| name.kind == Kind::Link)
            })
    }

    /// Checks link rule 3, creates the links, then syncs every folder, the
    /// root last (on Unix; Windows has no folder sync).
    fn finish(self) -> Result<()> {
        for link in &self.links {
            if let Some(folder) = self.link_on_the_way(link) {
                return Err(refused(
                    link.path.as_str().as_bytes(),
                    format!(
                        "is a link to {:?}, which passes through the link {folder:?}",
                        link.target
                    ),
                ));
            }
        }
        #[cfg(unix)]
        for link in &self.links {
            let at_buf = link.path.to_path(self.root);
            let at: &Path = &at_buf;
            sys::symlink(&link.target, at).map_err(|e| create_error(link.path.as_str(), at, e))?;
        }
        for dir in self.dirs.iter().rev() {
            sys::sync_dir(dir).map_err(|e| Error::io("flush", dir, e))?;
        }
        Ok(())
    }
}

/// The key that finds names equal after case folding. Best effort: file
/// systems fold case (and Unicode forms) their own way, and since nothing is
/// ever overwritten, a create refuses whatever this misses.
fn fold(name: &str) -> String {
    name.to_lowercase()
}

/// An entry that breaks a rule. Its name is shown as it is in the archive,
/// lossily when it is not UTF-8.
fn refused(entry: &[u8], reason: impl Into<String>) -> Error {
    Error::ArchiveEntryRefused {
        entry: String::from_utf8_lossy(entry).into_owned(),
        reason: reason.into(),
    }
}

/// An archive that breaks a whole-archive rule, or cannot be read.
fn archive_refused(reason: impl Into<String>) -> Error {
    Error::ArchiveRefused {
        reason: reason.into(),
    }
}

/// A failed create. Nothing is ever overwritten, so "already exists" means
/// two entries that this file system treats as one name.
fn create_error(entry: &str, path: &Path, error: io::Error) -> Error {
    if error.kind() == io::ErrorKind::AlreadyExists {
        refused(
            entry.as_bytes(),
            "is the same name as another entry on this file system",
        )
    } else {
        Error::io("create", path, error)
    }
}

#[cfg(unix)]
mod sys {
    use std::fs::{self, DirBuilder, File, OpenOptions, Permissions};
    use std::io;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    use std::path::Path;

    const DIR_MODE: u32 = 0o755;
    const EXECUTABLE_MODE: u32 = 0o755;
    const FILE_MODE: u32 = 0o644;

    /// A new folder, mode 0755. The mode is set again after the create, so
    /// the umask does not change it.
    pub(super) fn create_dir(path: &Path) -> io::Result<()> {
        DirBuilder::new().mode(DIR_MODE).create(path)?;
        fs::set_permissions(path, Permissions::from_mode(DIR_MODE))
    }

    /// A new file, never an existing one or a link, mode 0755 or 0644 whatever
    /// the umask.
    pub(super) fn create_file(path: &Path, executable: bool) -> io::Result<File> {
        let mode = if executable {
            EXECUTABLE_MODE
        } else {
            FILE_MODE
        };
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(path)?;
        file.set_permissions(Permissions::from_mode(mode))?;
        Ok(file)
    }

    pub(super) fn symlink(target: &str, link: &Path) -> io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    pub(super) fn sync_dir(path: &Path) -> io::Result<()> {
        File::open(path)?.sync_all()
    }
}

#[cfg(windows)]
mod sys {
    use std::fs::{self, File, OpenOptions};
    use std::io;
    use std::path::Path;

    /// A new folder; it inherits the data folder's DACL.
    pub(super) fn create_dir(path: &Path) -> io::Result<()> {
        fs::create_dir(path)
    }

    /// A new file, never an existing one. Windows has no mode bits to set.
    pub(super) fn create_file(path: &Path, _executable: bool) -> io::Result<File> {
        OpenOptions::new().write(true).create_new(true).open(path)
    }

    /// Nothing to do: Windows cannot sync a folder, and every file is synced.
    pub(super) fn sync_dir(_path: &Path) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests;
