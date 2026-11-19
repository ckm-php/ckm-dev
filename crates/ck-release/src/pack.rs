//! Packs a relocated runtime folder into the `tar.gz` users unpack into
//! `runtimes/<name>/<version>/`.
//!
//! The archive (frozen in `build/php/README.md`):
//!
//! - holds the folder's contents at the top level, with no wrapping folder
//!   and no `./` prefix;
//! - lists every folder and regular file, sorted by path bytes, so each
//!   folder comes before its contents;
//! - gives each entry mtime [`PACK_MTIME`], uid and gid 0 and empty owner
//!   names, mode 0755 for folders and for files with any execute bit, and
//!   0644 for other files, in GNU headers with no PAX records;
//! - holds no links, devices or other special files, and no name that
//!   `ck_core::extract` refuses: `pack` fails instead of leaving them out.
//!
//! The same tree always packs to the same bytes. Packing runs on Linux and
//! macOS only, where the builds are; on Windows [`pack`] returns an error.

use std::path::Path;

use ck_core::catalog::schema::{Platform, RuntimeName, RuntimeVersion};

#[cfg(not(unix))]
use self::other as imp;
#[cfg(unix)]
use self::unix as imp;
use crate::{Descriptor, Result};

/// The modification time of every entry, in Unix seconds.
pub const PACK_MTIME: u64 = 0;

/// What the archive is.
#[derive(Debug, Clone)]
pub struct PackSpec {
    pub name: RuntimeName,
    pub version: RuntimeVersion,
    pub platform: Platform,
    /// The SPDX licence expression for the catalog entry.
    pub license: String,
}

/// Packs `root` into `out`, whose file name must be
/// [`crate::asset_file_name`] of `spec`, and returns its descriptor. The
/// caller writes the descriptor.
///
/// The whole tree is checked before anything is written. The archive is
/// written to a temporary file next to `out`, synced, then renamed over
/// `out`; `out` must not be inside `root`.
pub fn pack(root: &Path, out: &Path, spec: &PackSpec) -> Result<Descriptor> {
    imp::pack(root, out, spec)
}

#[cfg(unix)]
mod unix {
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::fs::{self, File, OpenOptions};
    use std::io::{self, Read, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    use ck_core::catalog::schema::{ArchiveFormat, Sha256Digest};
    use ck_core::extract::names::EntryPath;
    use ck_core::extract::{MAX_ENTRIES, MAX_UNPACKED_BYTES};
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use sha2::{Digest, Sha256};
    use tar::{Builder, EntryType, Header};

    use super::{PACK_MTIME, PackSpec};
    use crate::{DESCRIPTOR_SCHEMA, Descriptor, Error, Result, asset_file_name, lock};

    const DIR_MODE: u32 = 0o755;
    const EXECUTABLE_MODE: u32 = 0o755;
    const FILE_MODE: u32 = 0o644;

    /// One folder or regular file of the tree.
    struct Entry {
        /// The `/`-separated path under the root, as the archive names it.
        name: String,
        path: PathBuf,
        folder: bool,
    }

    pub(super) fn pack(root: &Path, out: &Path, spec: &PackSpec) -> Result<Descriptor> {
        let file = asset_file_name(&spec.name, &spec.version, spec.platform);
        if out.file_name() != Some(file.as_ref()) {
            return Err(Error::Invalid(format!(
                "the archive {} must be named {file}",
                out.display()
            )));
        }
        lock::check_license(&spec.license)
            .map_err(|reason| Error::Invalid(format!("pack: {reason}")))?;
        check_out_is_outside(root, out)?;
        let entries = walk(root)?;

        let temp = temp_path(out, &file);
        let result = write_archive(&entries, &temp).and_then(|(sha256, size)| {
            let descriptor = Descriptor {
                schema: DESCRIPTOR_SCHEMA,
                name: spec.name.clone(),
                version: spec.version.clone(),
                license: spec.license.clone(),
                os: spec.platform.os,
                cpu: spec.platform.arch,
                file,
                sha256,
                size,
                format: ArchiveFormat::TarGz,
            };
            descriptor.validate()?;
            fs::rename(&temp, out).map_err(|e| Error::io("replace", out, e))?;
            Ok(descriptor)
        });
        if result.is_err() {
            // Best effort: the first error is the one worth reporting.
            let _ = fs::remove_file(&temp);
        }
        result
    }

    /// The archive would otherwise pack itself, or an older copy of itself.
    fn check_out_is_outside(root: &Path, out: &Path) -> Result<()> {
        let parent = out
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let root_real = fs::canonicalize(root).map_err(|e| Error::io("resolve", root, e))?;
        let parent_real = fs::canonicalize(parent).map_err(|e| Error::io("resolve", parent, e))?;
        if parent_real.starts_with(&root_real) {
            return Err(Error::Invalid(format!(
                "the archive {} is inside the folder it packs, {}",
                out.display(),
                root.display()
            )));
        }
        Ok(())
    }

    /// Every folder and regular file under `root`, sorted by name bytes.
    /// Refuses anything else, and any tree `ck_core::extract` would refuse.
    fn walk(root: &Path) -> Result<Vec<Entry>> {
        let metadata = fs::symlink_metadata(root).map_err(|e| Error::io("read", root, e))?;
        if !metadata.is_dir() {
            return Err(refused(root, "is not a folder"));
        }
        let mut entries = Vec::new();
        // ck-core's extractor refuses two names equal after `to_lowercase`.
        let mut folded: HashMap<String, String> = HashMap::new();
        let mut bytes: u64 = 0;
        let mut pending = vec![(root.to_path_buf(), String::new())];
        while let Some((dir, prefix)) = pending.pop() {
            let listing = fs::read_dir(&dir).map_err(|e| Error::io("list", &dir, e))?;
            for item in listing {
                let item = item.map_err(|e| Error::io("list", &dir, e))?;
                let path = item.path();
                let base = item
                    .file_name()
                    .into_string()
                    .map_err(|_| refused(&path, "has a name that is not UTF-8"))?;
                let name = if prefix.is_empty() {
                    base
                } else {
                    format!("{prefix}/{base}")
                };
                check_name(&name, &path)?;
                if let Some(other) = folded.insert(name.to_lowercase(), name.clone()) {
                    return Err(refused(
                        &path,
                        &format!("differs only in case from {other:?}"),
                    ));
                }
                let metadata =
                    fs::symlink_metadata(&path).map_err(|e| Error::io("read", &path, e))?;
                let kind = metadata.file_type();
                let folder = if kind.is_dir() {
                    pending.push((path.clone(), name.clone()));
                    true
                } else if kind.is_file() {
                    bytes = bytes.saturating_add(metadata.len());
                    false
                } else if kind.is_symlink() {
                    return Err(refused(&path, "is a link"));
                } else {
                    return Err(refused(&path, "is not a regular file or a folder"));
                };
                entries.push(Entry { name, path, folder });
                if entries.len() as u64 > MAX_ENTRIES {
                    return Err(refused(
                        root,
                        &format!("holds more than {MAX_ENTRIES} entries"),
                    ));
                }
            }
        }
        if bytes > MAX_UNPACKED_BYTES {
            return Err(refused(
                root,
                &format!("holds more than {MAX_UNPACKED_BYTES} bytes"),
            ));
        }
        if entries.is_empty() {
            return Err(refused(root, "is empty"));
        }
        entries.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    /// The name rules of `ck_core::extract::names`, which must keep the name
    /// as it is.
    fn check_name(name: &str, path: &Path) -> Result<()> {
        match EntryPath::parse(name.as_bytes()) {
            Ok(Some(parsed)) if parsed.as_str() == name => Ok(()),
            Ok(_) => Err(refused(path, "has a name ck-core would change")),
            Err(error) => Err(Error::Invalid(format!(
                "cannot pack {}: {error}",
                path.display()
            ))),
        }
    }

    fn refused(path: &Path, why: &str) -> Error {
        Error::Invalid(format!("cannot pack {}: it {why}", path.display()))
    }

    /// `.<file>.<pid>.tmp` next to `out`.
    fn temp_path(out: &Path, file: &str) -> PathBuf {
        let mut name = OsString::from(".");
        name.push(file);
        name.push(format!(".{}.tmp", std::process::id()));
        out.with_file_name(name)
    }

    /// Writes the archive to `temp` and syncs it. Returns its SHA-256 and
    /// size, taken from the bytes written.
    fn write_archive(entries: &[Entry], temp: &Path) -> Result<(Sha256Digest, u64)> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(temp)
            .map_err(|e| Error::io("create", temp, e))?;
        let mut builder = Builder::new(GzEncoder::new(Hashing::new(file), Compression::best()));
        for entry in entries {
            append(&mut builder, entry)?;
        }
        let hashing = builder
            .into_inner()
            .and_then(GzEncoder::finish)
            .map_err(|e| Error::io("write", temp, e))?;
        let (file, sha256, size) = hashing.finish();
        file.sync_all().map_err(|e| Error::io("flush", temp, e))?;
        Ok((sha256, size))
    }

    /// One entry with fixed metadata. A fresh GNU header has zeroed owner
    /// names, times and device numbers.
    fn append<W: Write>(builder: &mut Builder<W>, entry: &Entry) -> Result<()> {
        let mut header = Header::new_gnu();
        header.set_mtime(PACK_MTIME);
        header.set_uid(0);
        header.set_gid(0);
        let result = if entry.folder {
            header.set_entry_type(EntryType::Directory);
            header.set_mode(DIR_MODE);
            header.set_size(0);
            builder.append_data(&mut header, format!("{}/", entry.name), io::empty())
        } else {
            let (data, executable) = read_file(&entry.path)?;
            header.set_entry_type(EntryType::Regular);
            header.set_mode(if executable {
                EXECUTABLE_MODE
            } else {
                FILE_MODE
            });
            header.set_size(data.len() as u64);
            builder.append_data(&mut header, &entry.name, data.as_slice())
        };
        result.map_err(|e| Error::io("pack", &entry.path, e))
    }

    /// The file's bytes and whether any execute bit is set, both from the
    /// open handle. Reading it whole keeps the header's size and the data in
    /// step even if the file changes underneath.
    fn read_file(path: &Path) -> Result<(Vec<u8>, bool)> {
        let file = File::open(path).map_err(|e| Error::io("open", path, e))?;
        let metadata = file.metadata().map_err(|e| Error::io("read", path, e))?;
        if !metadata.is_file() {
            return Err(refused(path, "is no longer a regular file"));
        }
        let mut data = Vec::new();
        file.take(metadata.len().saturating_add(1))
            .read_to_end(&mut data)
            .map_err(|e| Error::io("read", path, e))?;
        if data.len() as u64 != metadata.len() {
            return Err(refused(path, "changed while it was packed"));
        }
        Ok((data, metadata.permissions().mode() & 0o111 != 0))
    }

    /// Hashes and counts every byte written through it.
    struct Hashing<W> {
        inner: W,
        hasher: Sha256,
        bytes: u64,
    }

    impl<W> Hashing<W> {
        fn new(inner: W) -> Self {
            Self {
                inner,
                hasher: Sha256::new(),
                bytes: 0,
            }
        }

        fn finish(self) -> (W, Sha256Digest, u64) {
            let digest = Sha256Digest::from_bytes(self.hasher.finalize().into());
            (self.inner, digest, self.bytes)
        }
    }

    impl<W: Write> Write for Hashing<W> {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let written = self.inner.write(buf)?;
            self.hasher.update(&buf[..written]);
            self.bytes += written as u64;
            Ok(written)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.inner.flush()
        }
    }
}

/// Windows has no Unix modes to read, and no build packs there.
#[cfg(not(unix))]
mod other {
    use std::path::Path;

    use super::PackSpec;
    use crate::{Descriptor, Error, Result};

    pub(super) fn pack(_root: &Path, _out: &Path, _spec: &PackSpec) -> Result<Descriptor> {
        Err(Error::Invalid(
            "packing is supported on Linux and macOS only".to_owned(),
        ))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs::{self, File, Permissions};
    use std::io::Read as _;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::Path;
    use std::time::{Duration, UNIX_EPOCH};

    use ck_core::catalog::schema::{Arch, Os, Sha256Digest};
    use flate2::read::GzDecoder;
    use tar::{Archive, EntryType};

    use super::*;
    use crate::asset_file_name;
    use crate::test_support::TempDir;

    /// The fixture tree: path, bytes and the mode it is created with.
    const FILES: [(&str, &[u8], u32); 5] = [
        ("bin/php", b"#!/bin/sh\n", 0o700),
        ("bin-x", b"sorts between bin and bin/php\n", 0o600),
        ("ck-runtime.json", b"{}\n", 0o644),
        ("etc/ssl/cert.pem", b"not a bundle\n", 0o664),
        ("lib/libz.so.1", b"inert\n", 0o755),
    ];

    fn spec() -> PackSpec {
        PackSpec {
            name: RuntimeName::new("php").unwrap(),
            version: RuntimeVersion::new("8.5.12-ck1").unwrap(),
            platform: Platform {
                os: Os::Linux,
                arch: Arch::X86_64,
            },
            license: "PHP-3.01".to_owned(),
        }
    }

    fn file_name() -> String {
        let spec = spec();
        asset_file_name(&spec.name, &spec.version, spec.platform)
    }

    /// [`FILES`] plus an empty `ext/`, all with the given modes.
    fn fixture(tree: &Path) {
        for (name, contents, mode) in FILES {
            let path = tree.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, contents).unwrap();
            fs::set_permissions(&path, Permissions::from_mode(mode)).unwrap();
        }
        fs::create_dir(tree.join("ext")).unwrap();
        fs::set_permissions(tree.join("ext"), Permissions::from_mode(0o700)).unwrap();
    }

    /// Packs `tree` into the new folder `dir`; the descriptor and the bytes.
    fn pack_into(dir: &Path, tree: &Path) -> (Descriptor, Vec<u8>) {
        fs::create_dir(dir).unwrap();
        let out = dir.join(file_name());
        let descriptor = pack(tree, &out, &spec()).unwrap();
        // Only the archive: the temporary file was renamed.
        assert_eq!(fs::read_dir(dir).unwrap().count(), 1);
        (descriptor, fs::read(out).unwrap())
    }

    #[test]
    fn the_same_tree_packs_to_the_same_bytes_with_fixed_metadata() {
        let temp = TempDir::new();
        let tree = temp.0.join("tree");
        fixture(&tree);
        let first = pack_into(&temp.0.join("a"), &tree);

        // Neither times nor owner-only mode bits reach the archive.
        File::options()
            .write(true)
            .open(tree.join("bin/php"))
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(1_000_000))
            .unwrap();
        fs::set_permissions(tree.join("bin/php"), Permissions::from_mode(0o750)).unwrap();
        let second = pack_into(&temp.0.join("b"), &tree);
        assert_eq!(first, second);

        let (descriptor, bytes) = first;
        assert_eq!(descriptor.file, file_name());
        assert_eq!(descriptor.sha256, Sha256Digest::of(&bytes));
        assert_eq!(descriptor.size, bytes.len() as u64);

        let mut archive = Archive::new(GzDecoder::new(bytes.as_slice()));
        let mut listed = Vec::new();
        for entry in archive.entries().unwrap().raw(true) {
            let mut entry = entry.unwrap();
            let header = entry.header();
            let name = String::from_utf8(header.path_bytes().into_owned()).unwrap();
            assert_eq!(header.mtime().unwrap(), PACK_MTIME, "{name}");
            assert_eq!(header.uid().unwrap(), 0, "{name}");
            assert_eq!(header.gid().unwrap(), 0, "{name}");
            assert!(
                header.username_bytes().is_none_or(<[u8]>::is_empty),
                "{name}"
            );
            assert!(
                header.groupname_bytes().is_none_or(<[u8]>::is_empty),
                "{name}"
            );
            let kind = header.entry_type();
            let mode = header.mode().unwrap();
            if name == "bin/php" {
                let mut data = Vec::new();
                entry.read_to_end(&mut data).unwrap();
                assert_eq!(data, b"#!/bin/sh\n");
            }
            listed.push((name, kind, mode));
        }
        let expected = [
            ("bin/", EntryType::Directory, 0o755),
            ("bin-x", EntryType::Regular, 0o644),
            ("bin/php", EntryType::Regular, 0o755),
            ("ck-runtime.json", EntryType::Regular, 0o644),
            ("etc/", EntryType::Directory, 0o755),
            ("etc/ssl/", EntryType::Directory, 0o755),
            ("etc/ssl/cert.pem", EntryType::Regular, 0o644),
            ("ext/", EntryType::Directory, 0o755),
            ("lib/", EntryType::Directory, 0o755),
            ("lib/libz.so.1", EntryType::Regular, 0o755),
        ]
        .map(|(name, kind, mode)| (name.to_owned(), kind, mode));
        assert_eq!(listed, expected);
    }

    #[test]
    fn links_and_names_ck_core_refuses_are_refused() {
        let temp = TempDir::new();
        refuses(&temp.0, "link", |tree| {
            symlink("php", tree.join("bin/php-link")).unwrap();
        });
        refuses(&temp.0, "device-name", |tree| {
            fs::write(tree.join("lib/nul.txt"), b"").unwrap();
        });
        refuses(&temp.0, "trailing-dot", |tree| {
            fs::write(tree.join("lib/libz."), b"").unwrap();
        });
        // macOS file systems usually fold case themselves.
        #[cfg(target_os = "linux")]
        refuses(&temp.0, "case-twin", |tree| {
            fs::write(tree.join("lib/LIBZ.so.1"), b"").unwrap();
        });

        // A wrong archive name, and an archive inside the folder it packs.
        let tree = temp.0.join("tree");
        fixture(&tree);
        let out = temp.0.join("out");
        fs::create_dir(&out).unwrap();
        assert!(pack(&tree, &out.join("php.tar.gz"), &spec()).is_err());
        assert!(pack(&tree, &tree.join(file_name()), &spec()).is_err());
        assert_eq!(fs::read_dir(&out).unwrap().count(), 0);
        assert!(!tree.join(file_name()).exists());
    }

    /// Packing the fixture after `change` fails and writes nothing.
    fn refuses(base: &Path, case: &str, change: impl Fn(&Path)) {
        let dir = base.join(case);
        let tree = dir.join("tree");
        fixture(&tree);
        change(&tree);
        let out = dir.join(file_name());
        let result = pack(&tree, &out, &spec());
        assert!(result.is_err(), "{case}: {result:?}");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1, "{case}");
    }
}
