//! Reads a tar.gz archive entry by entry into a [`Tree`].
//!
//! The entries are read raw. In its normal mode the `tar` crate reads a GNU
//! long name or a PAX header whole, with no cap, before it returns the entry
//! that the header describes. Here each such header is capped at
//! [`MAX_EXTENSION_BYTES`], counted against the size cap, and applied to the
//! next entry:
//!
//! - its name is the GNU long name, else the PAX `path`, else the header's
//!   own (`prefix/name` for ustar); its link target likewise, from the GNU
//!   long link, the PAX `linkpath` or the header. An entry with both a GNU
//!   and a PAX name is refused, since readers disagree on which one wins;
//! - PAX records are read strictly by their length prefix. A `size` record
//!   must match the header's size, and a `GNU.sparse.*` record means a sparse
//!   file, which is refused;
//! - one entry may have at most one header of each kind, and one that
//!   describes no entry refuses the archive.
//!
//! Folders and links must hold no data, so every byte the gzip stream
//! produces is a header, padding, or counted against the size cap.

use std::fs::File;
use std::io::{self, Read};

use flate2::read::MultiGzDecoder;
use tar::{Archive, Entry, EntryType, Header};

use super::names::EntryPath;
use super::{Tree, archive_refused, refused};
use crate::error::{Error, Result, escape_untrusted};

/// The most bytes in one GNU long name or long link, or one PAX header.
const MAX_EXTENSION_BYTES: u64 = 64 * 1024;

/// Reads every entry of the tar.gz in `file` into `tree`.
pub(super) fn read(file: &mut File, tree: &mut Tree<'_>) -> Result<()> {
    // Multi-member gzip, as `gzip -d` reads it.
    let mut archive = Archive::new(MultiGzDecoder::new(file));
    let entries = archive.entries().map_err(unreadable)?.raw(true);
    let mut pending = Extensions::default();
    for entry in entries {
        let mut entry = entry.map_err(unreadable)?;
        let size = entry.size();
        let kind = entry.header().entry_type();
        match kind {
            EntryType::GNULongName => {
                let name = gnu_name(read_extension(&mut entry, size, tree)?);
                set_once(&mut pending.long_name, name, "GNU long names")?;
            }
            EntryType::GNULongLink => {
                let link = gnu_name(read_extension(&mut entry, size, tree)?);
                set_once(&mut pending.long_link, link, "GNU long links")?;
            }
            EntryType::XHeader => {
                let pax = Pax::parse(&read_extension(&mut entry, size, tree)?)?;
                set_once(&mut pending.pax, pax, "PAX headers")?;
            }
            EntryType::XGlobalHeader => {
                if !pending.is_empty() {
                    return Err(archive_refused(
                        "it has a PAX global header among one entry's extension headers",
                    ));
                }
                tree.count_entry()?;
                // Skipped: what it sets applies to no rule here.
                read_extension(&mut entry, size, tree)?;
            }
            _ => {
                tree.count_entry()?;
                unpack_entry(&mut entry, std::mem::take(&mut pending), tree)?;
            }
        }
    }
    if !pending.is_empty() {
        return Err(archive_refused(
            "it ends with a header that describes no entry",
        ));
    }
    Ok(())
}

/// Unpacks one entry that is not an extension header.
fn unpack_entry<R: Read>(
    entry: &mut Entry<'_, R>,
    extensions: Extensions,
    tree: &mut Tree<'_>,
) -> Result<()> {
    let header = entry.header();
    let kind = header.entry_type();
    let size = entry.size();
    let name = extensions.name(header)?;
    let link = extensions.link(header)?;
    let mode = header.mode();
    if extensions.pax_size().is_some_and(|pax| pax != size) {
        return Err(refused(
            &name,
            "has a PAX size that differs from its header's",
        ));
    }
    match kind {
        EntryType::Regular => {
            let path = parse_non_folder(&name, "file")?;
            let mode = mode.map_err(|e| {
                refused(
                    &name,
                    format!(
                        "has a mode that cannot be read: {}",
                        escape_untrusted(&e.to_string())
                    ),
                )
            })?;
            tree.file(&path, mode & 0o111 != 0, size, entry)
        }
        EntryType::Directory => {
            no_data(&name, size)?;
            if let Some(path) = EntryPath::parse(&name)? {
                tree.dir(&path)?;
            }
            Ok(())
        }
        EntryType::Symlink => {
            no_data(&name, size)?;
            let path = parse_non_folder(&name, "link")?;
            let target = link.ok_or_else(|| refused(&name, "is a link with no target"))?;
            tree.symlink(path, &target)
        }
        EntryType::Link => Err(refused(&name, "is a hard link")),
        EntryType::Char | EntryType::Block => Err(refused(&name, "is a device")),
        EntryType::Fifo => Err(refused(&name, "is a fifo")),
        EntryType::GNUSparse => Err(refused(&name, "is a sparse file")),
        other => Err(refused(
            &name,
            format!(
                "has the unsupported entry type {:?}",
                char::from(other.as_byte())
            ),
        )),
    }
}

/// The name of a file or link (`noun`): a name ending in `/` (which
/// [`EntryPath::parse`] would drop) or a bare `./` is refused.
fn parse_non_folder(name: &[u8], noun: &str) -> Result<EntryPath> {
    if name.ends_with(b"/") {
        return Err(refused(name, format!("is a {noun} whose name ends in '/'")));
    }
    EntryPath::parse(name)?.ok_or_else(|| refused(name, format!("is a {noun} with no name")))
}

/// Folders and links hold no data in a tar archive.
fn no_data(name: &[u8], size: u64) -> Result<()> {
    if size != 0 {
        return Err(refused(
            name,
            format!("is a folder or link that holds {size} bytes of data"),
        ));
    }
    Ok(())
}

/// Reads one extension header's data, at most [`MAX_EXTENSION_BYTES`], and
/// counts it against the size cap.
fn read_extension(entry: &mut dyn Read, size: u64, tree: &mut Tree<'_>) -> Result<Vec<u8>> {
    if size > MAX_EXTENSION_BYTES {
        return Err(archive_refused(format!(
            "it has a {size}-byte extension header, over the {MAX_EXTENSION_BYTES}-byte limit"
        )));
    }
    tree.add_bytes(size)?;
    let mut data = Vec::new();
    entry
        .take(size)
        .read_to_end(&mut data)
        .map_err(unreadable)?;
    if data.len() as u64 != size {
        return Err(archive_refused("it ends inside an extension header"));
    }
    Ok(data)
}

/// A GNU long name or link without the one NUL that ends it. Any other NUL
/// stays and breaks the name rules.
fn gnu_name(mut data: Vec<u8>) -> Vec<u8> {
    if data.last() == Some(&0) {
        data.pop();
    }
    data
}

fn set_once<T>(slot: &mut Option<T>, value: T, what: &str) -> Result<()> {
    if slot.is_some() {
        return Err(archive_refused(format!("it has two {what} for one entry")));
    }
    *slot = Some(value);
    Ok(())
}

fn unreadable(error: io::Error) -> Error {
    archive_refused(format!(
        "it cannot be read as tar.gz: {}",
        escape_untrusted(&error.to_string())
    ))
}

/// The extension headers read since the last entry, which describe the next.
#[derive(Default)]
struct Extensions {
    long_name: Option<Vec<u8>>,
    long_link: Option<Vec<u8>>,
    pax: Option<Pax>,
}

impl Extensions {
    fn is_empty(&self) -> bool {
        self.long_name.is_none() && self.long_link.is_none() && self.pax.is_none()
    }

    /// The entry's effective name.
    fn name(&self, header: &Header) -> Result<Vec<u8>> {
        let pax = self.pax.as_ref().and_then(|pax| pax.path.as_deref());
        Ok(match pick(self.long_name.as_deref(), pax, "name")? {
            Some(name) => name.to_vec(),
            None => header.path_bytes().into_owned(),
        })
    }

    /// The entry's effective link target, if it has one.
    fn link(&self, header: &Header) -> Result<Option<Vec<u8>>> {
        let pax = self.pax.as_ref().and_then(|pax| pax.linkpath.as_deref());
        Ok(match pick(self.long_link.as_deref(), pax, "link target")? {
            Some(link) => Some(link.to_vec()),
            None => header.link_name_bytes().map(|link| link.into_owned()),
        })
    }

    fn pax_size(&self) -> Option<u64> {
        self.pax.as_ref().and_then(|pax| pax.size)
    }
}

/// The GNU value or the PAX one; both is refused.
fn pick<'a>(gnu: Option<&'a [u8]>, pax: Option<&'a [u8]>, what: &str) -> Result<Option<&'a [u8]>> {
    match (gnu, pax) {
        (Some(gnu), Some(_)) => Err(refused(
            gnu,
            format!("has both a GNU long {what} and a PAX one"),
        )),
        (gnu, pax) => Ok(gnu.or(pax)),
    }
}

/// The records of one PAX header that matter here.
#[derive(Default)]
struct Pax {
    path: Option<Vec<u8>>,
    linkpath: Option<Vec<u8>>,
    size: Option<u64>,
}

impl Pax {
    /// Reads `<length> <key>=<value>\n` records strictly; a malformed one
    /// refuses the archive. Other keys (times, owners, xattrs) are ignored.
    fn parse(mut data: &[u8]) -> Result<Self> {
        let mut pax = Self::default();
        while !data.is_empty() {
            let (key, value, rest) =
                record(data).ok_or_else(|| archive_refused("it has a malformed PAX header"))?;
            data = rest;
            match key {
                b"path" => set_once(&mut pax.path, value.to_vec(), "PAX paths")?,
                b"linkpath" => set_once(&mut pax.linkpath, value.to_vec(), "PAX link paths")?,
                b"size" => {
                    let size = decimal(value)
                        .ok_or_else(|| archive_refused("it has a PAX size that is not a number"))?;
                    set_once(&mut pax.size, size, "PAX sizes")?;
                }
                _ if key.starts_with(b"GNU.sparse.") => {
                    return Err(archive_refused("it has a sparse file"));
                }
                _ => {}
            }
        }
        Ok(pax)
    }
}

/// One record, its key and value, and the bytes after it. The length counts
/// the whole record, its own digits and the newline included.
fn record(data: &[u8]) -> Option<(&[u8], &[u8], &[u8])> {
    let space = data.iter().position(|&byte| byte == b' ')?;
    let length = usize::try_from(decimal(&data[..space])?).ok()?;
    if length <= space + 1 || length > data.len() {
        return None;
    }
    let (whole, rest) = data.split_at(length);
    let body = whole[space + 1..].strip_suffix(b"\n")?;
    let equals = body.iter().position(|&byte| byte == b'=')?;
    if equals == 0 {
        return None;
    }
    Some((&body[..equals], &body[equals + 1..], rest))
}

/// A non-empty run of ASCII digits as a number.
fn decimal(digits: &[u8]) -> Option<u64> {
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(digits).ok()?.parse().ok()
}
