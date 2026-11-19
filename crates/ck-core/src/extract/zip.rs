//! Reads a zip archive entry by entry into a [`Tree`].
//!
//! Before any entry is read, the `zip` crate's `validate_header_consistency`
//! refuses duplicate names, entries whose bytes (headers included) overlap
//! each other or run into the central directory, local headers that disagree
//! with the central directory, and local headers it does not list.
//! `ZipArchive::new` parses the whole central directory before the entry cap
//! can be checked, so that step is bounded only by the archive's pinned size.
//!
//! Each entry:
//!
//! - is named by the raw bytes the crate keeps (after an Info-ZIP Unicode
//!   Path field, which the crate checks against the header's name). A name
//!   that is not UTF-8, such as one in a legacy code page, is refused;
//! - is refused when it is encrypted or uses a method other than Stored or
//!   Deflate;
//! - is a folder when its name ends in `/`, its Unix mode (if any) is a
//!   folder or a file, and it holds no data. Otherwise the mode makes it a
//!   file or a link; any other type, or a folder mode without the `/`, is
//!   refused;
//! - must inflate to exactly the size it declares. The crate also fails a
//!   read past that size and a CRC mismatch.

use std::fs::File;
use std::io::Read;

use ::zip::result::ZipError;
use ::zip::{CompressionMethod, ZipArchive};

use super::names::{EntryPath, MAX_PATH_BYTES};
use super::{Kind, Tree, archive_refused, refused};
use crate::error::{Error, Result, escape_untrusted};

const TYPE_MASK: u32 = 0o170000;
const TYPE_FILE: u32 = 0o100000;
const TYPE_FOLDER: u32 = 0o040000;
const TYPE_LINK: u32 = 0o120000;

/// Reads every entry of the zip in `file` into `tree`.
pub(super) fn read(file: &mut File, tree: &mut Tree<'_>) -> Result<()> {
    let mut archive = ZipArchive::new(file).map_err(invalid)?;
    tree.check_entries(u64::try_from(archive.len()).unwrap_or(u64::MAX))?;
    archive.validate_header_consistency().map_err(invalid)?;
    for index in 0..archive.len() {
        tree.count_entry()?;
        let entry = Meta::of(&archive, index)?;
        match entry.kind()? {
            Kind::Folder => {
                if let Some(path) = EntryPath::parse(&entry.name)? {
                    tree.dir(&path)?;
                }
            }
            Kind::File => {
                let path = entry.path()?;
                let mut data = archive.by_index(index).map_err(|e| entry.unreadable(&e))?;
                tree.file(&path, entry.executable(), entry.size, &mut data)?;
            }
            Kind::Link => {
                let path = entry.path()?;
                let target = entry.read_target(&mut archive, index)?;
                tree.symlink(path, &target)?;
            }
        }
    }
    Ok(())
}

/// What the central directory says about one entry.
struct Meta {
    name: Vec<u8>,
    encrypted: bool,
    method: CompressionMethod,
    mode: Option<u32>,
    size: u64,
}

impl Meta {
    fn of(archive: &ZipArchive<&mut File>, index: usize) -> Result<Self> {
        let entry = archive.by_index_data(index).map_err(invalid)?;
        Ok(Self {
            name: entry.name_raw().to_vec(),
            encrypted: entry.encrypted(),
            method: entry.compression(),
            mode: entry.unix_mode(),
            size: entry.size(),
        })
    }

    /// Checks the encryption flag and the method, then gives the entry's type.
    fn kind(&self) -> Result<Kind> {
        if self.encrypted {
            return Err(refused(&self.name, "is encrypted"));
        }
        if !matches!(
            self.method,
            CompressionMethod::Stored | CompressionMethod::Deflated
        ) {
            return Err(refused(
                &self.name,
                format!("uses the compression method {}", self.method),
            ));
        }
        let file_type = self.mode.map_or(0, |mode| mode & TYPE_MASK);
        if self.name.ends_with(b"/") {
            return match file_type {
                0 | TYPE_FILE | TYPE_FOLDER if self.size == 0 => Ok(Kind::Folder),
                0 | TYPE_FILE | TYPE_FOLDER => Err(refused(
                    &self.name,
                    format!("is a folder that holds {} bytes of data", self.size),
                )),
                _ => Err(refused(
                    &self.name,
                    "has a name that ends in '/' but is not a folder",
                )),
            };
        }
        match file_type {
            0 | TYPE_FILE => Ok(Kind::File),
            TYPE_LINK => Ok(Kind::Link),
            TYPE_FOLDER => Err(refused(
                &self.name,
                "is a folder whose name does not end in '/'",
            )),
            _ => Err(refused(&self.name, "is a device, fifo or socket")),
        }
    }

    /// The name of a file or link.
    fn path(&self) -> Result<EntryPath> {
        EntryPath::parse(&self.name)?.ok_or_else(|| refused(&self.name, "has no name"))
    }

    fn executable(&self) -> bool {
        self.mode.is_some_and(|mode| mode & 0o111 != 0)
    }

    /// A link's target: its data, at most [`MAX_PATH_BYTES`] bytes.
    fn read_target(&self, archive: &mut ZipArchive<&mut File>, index: usize) -> Result<Vec<u8>> {
        let cap = MAX_PATH_BYTES as u64;
        if self.size > cap {
            return Err(refused(
                &self.name,
                format!("is a link whose target is longer than {MAX_PATH_BYTES} bytes"),
            ));
        }
        let mut target = Vec::new();
        archive
            .by_index(index)
            .map_err(|e| self.unreadable(&e))?
            .take(cap + 1)
            .read_to_end(&mut target)
            .map_err(|e| {
                refused(
                    &self.name,
                    format!("cannot be read: {}", escape_untrusted(&e.to_string())),
                )
            })?;
        if target.len() as u64 != self.size {
            return Err(refused(
                &self.name,
                format!(
                    "holds {} bytes, but its header lists {}",
                    target.len(),
                    self.size
                ),
            ));
        }
        Ok(target)
    }

    fn unreadable(&self, error: &ZipError) -> Error {
        refused(
            &self.name,
            format!("cannot be read: {}", escape_untrusted(&error.to_string())),
        )
    }
}

fn invalid(error: ZipError) -> Error {
    archive_refused(format!(
        "it is not a valid zip: {}",
        escape_untrusted(&error.to_string())
    ))
}
