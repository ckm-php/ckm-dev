//! T9–T12: archives built byte by byte, so names, types and sizes can break
//! every rule. A refused archive must leave only the empty staging folder:
//! the destination and the archive are gone, and nothing was written next to
//! them. Each builder also has a control archive that unpacks. Nothing
//! unpacked is ever executed.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use flate2::Compression;
use flate2::write::{DeflateEncoder, GzEncoder};
use tar::{EntryType, Header};

use super::{Limits, MAX_ENTRIES, MAX_UNPACKED_BYTES, unpack_with};
use crate::catalog::schema::ArchiveFormat::{self, TarGz, Zip};
use crate::download::verified::VerifiedFile;
use crate::error::{Error, Result};
use crate::test_support::TempDir;

/// A temporary root holding the archive and `staging/`, the destination's
/// parent, so a name that climbs one or two levels lands where
/// [`Fixture::assert_clean`] looks.
struct Fixture(TempDir);

impl Fixture {
    fn new() -> Self {
        let root = TempDir::new("extract");
        fs::create_dir(root.path().join("staging")).unwrap();
        Self(root)
    }

    fn dest(&self) -> PathBuf {
        self.0.path().join("staging").join("out")
    }

    /// `name` inside the root, as an absolute archive name with `/`.
    fn absolute(&self, name: &str) -> String {
        let root = self.0.path().display().to_string().replace('\\', "/");
        format!("{root}/{name}")
    }

    fn unpack(&self, bytes: &[u8], format: ArchiveFormat, limits: Limits) -> Result<()> {
        let archive = self.0.path().join("archive.part");
        fs::write(&archive, bytes).unwrap();
        unpack_with(
            VerifiedFile::for_test(&archive, format),
            &self.dest(),
            limits,
        )
    }

    fn assert_clean(&self) {
        assert_eq!(entries_of(self.0.path()), ["staging"]);
        assert!(entries_of(&self.0.path().join("staging")).is_empty());
    }

    /// Unpacks `bytes` and checks that `entry` refused the archive for a
    /// reason that contains `reason`.
    fn assert_entry_refused(&self, bytes: &[u8], format: ArchiveFormat, entry: &str, reason: &str) {
        match self.unpack(bytes, format, Limits::DEFAULT) {
            Err(Error::ArchiveEntryRefused {
                entry: refused,
                reason: why,
            }) => {
                assert_eq!(refused, entry, "{why}");
                assert!(why.contains(reason), "{entry:?}: {why:?} lacks {reason:?}");
            }
            other => panic!("{entry:?}, {reason:?}: {other:?}"),
        }
        self.assert_clean();
    }
}

fn assert_entry_refused(bytes: &[u8], format: ArchiveFormat, entry: &str, reason: &str) {
    Fixture::new().assert_entry_refused(bytes, format, entry, reason);
}

fn assert_archive_refused(bytes: &[u8], format: ArchiveFormat, limits: Limits, reason: &str) {
    let fixture = Fixture::new();
    match fixture.unpack(bytes, format, limits) {
        Err(Error::ArchiveRefused { reason: why }) => {
            assert!(why.contains(reason), "{why:?} lacks {reason:?}");
        }
        other => panic!("{reason:?}: {other:?}"),
    }
    fixture.assert_clean();
}

/// A link that breaks a rule. On Linux and macOS it is refused for that
/// rule; on Windows any link refuses the archive, so only the refusal is
/// checked there.
fn assert_link_refused(bytes: &[u8], format: ArchiveFormat, entry: &str, reason: &str) {
    if cfg!(windows) {
        let fixture = Fixture::new();
        let result = fixture.unpack(bytes, format, Limits::DEFAULT);
        assert!(
            matches!(result, Err(Error::ArchiveEntryRefused { .. })),
            "{entry:?}: {result:?}"
        );
        fixture.assert_clean();
    } else {
        assert_entry_refused(bytes, format, entry, reason);
    }
}

/// Unpacks `bytes`, which must succeed, and leaves only the result.
fn unpacked(bytes: &[u8], format: ArchiveFormat) -> Fixture {
    let fixture = Fixture::new();
    fixture.unpack(bytes, format, Limits::DEFAULT).unwrap();
    assert_eq!(entries_of(fixture.0.path()), ["staging"]);
    fixture
}

/// What the name rules say about an absolute name: a Unix one starts with
/// `/`, and a Windows one has a drive's `:`.
fn absolute_reason(name: &str) -> &'static str {
    if name.starts_with('/') {
        "absolute path"
    } else {
        "has the character"
    }
}

fn entries_of(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// A tar archive built header by header. The `tar` crate's builder refuses
/// hostile names, so each header's fields are written directly.
#[derive(Default)]
struct Tar(Vec<u8>);

/// A ustar header with `name` in its 100-byte name field.
fn ustar(kind: u8, name: &[u8], link: &[u8], mode: u32, size: usize) -> Header {
    let mut header = Header::new_ustar();
    header.as_mut_bytes()[..name.len()].copy_from_slice(name);
    header.set_mode(mode);
    header.set_size(size as u64);
    header.set_entry_type(EntryType::new(kind));
    if !link.is_empty() {
        header.set_link_name_literal(link).unwrap();
    }
    header
}

impl Tar {
    fn push(mut self, mut header: Header, data: &[u8]) -> Self {
        header.set_cksum();
        self.0.extend_from_slice(header.as_bytes());
        self.0.extend_from_slice(data);
        self.0.resize(self.0.len().next_multiple_of(512), 0);
        self
    }

    fn raw(self, kind: u8, name: &[u8], link: &[u8], mode: u32, data: &[u8]) -> Self {
        self.push(ustar(kind, name, link, mode, data.len()), data)
    }

    fn file(self, name: &str, mode: u32, data: &[u8]) -> Self {
        self.raw(b'0', name.as_bytes(), b"", mode, data)
    }

    /// A folder, mode 0700 in the archive.
    fn dir(self, name: &str) -> Self {
        self.raw(b'5', name.as_bytes(), b"", 0o700, b"")
    }

    fn symlink(self, name: &str, target: &str) -> Self {
        self.raw(b'2', name.as_bytes(), target.as_bytes(), 0o777, b"")
    }

    /// A file whose name is split between the ustar prefix and name fields.
    fn prefixed_file(self, prefix: &str, name: &str, data: &[u8]) -> Self {
        let mut header = ustar(b'0', name.as_bytes(), b"", 0o644, data.len());
        header.as_mut_bytes()[345..345 + prefix.len()].copy_from_slice(prefix.as_bytes());
        self.push(header, data)
    }

    /// A PAX header for the next entry.
    fn pax(self, records: &[(&str, &[u8])]) -> Self {
        self.raw(b'x', b"PaxHeader", b"", 0o644, &pax_records(records))
    }

    fn pax_global(self, records: &[(&str, &[u8])]) -> Self {
        self.raw(b'g', b"GlobalHead", b"", 0o644, &pax_records(records))
    }

    /// A GNU long name (`b'L'`) or long link (`b'K'`) for the next entry.
    fn gnu(self, kind: u8, value: &[u8]) -> Self {
        let mut data = value.to_vec();
        data.push(0);
        self.raw(kind, b"././@LongLink", b"", 0o644, &data)
    }

    fn gz(mut self) -> Vec<u8> {
        self.0.extend_from_slice(&[0; 1024]);
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&self.0).unwrap();
        encoder.finish().unwrap()
    }
}

/// `<length> <key>=<value>\n` records, where the length counts the whole
/// record, its own digits included.
fn pax_records(records: &[(&str, &[u8])]) -> Vec<u8> {
    let mut data = Vec::new();
    for (key, value) in records {
        let rest = key.len() + value.len() + 3;
        let mut length = rest + 1;
        while length != rest + length.to_string().len() {
            length = rest + length.to_string().len();
        }
        data.extend_from_slice(format!("{length} {key}=").as_bytes());
        data.extend_from_slice(value);
        data.push(b'\n');
    }
    data
}

/// 1980-01-01, the earliest date a zip can hold.
const DOS_DATE: u16 = 0x21;

/// One zip entry. Its local and central headers carry the same fields.
struct ZipEntry {
    name: Vec<u8>,
    flags: u16,
    method: u16,
    /// The Unix mode; 0 writes a DOS entry with no attributes.
    mode: u32,
    crc: u32,
    /// The declared uncompressed size.
    size: u32,
    data: Vec<u8>,
}

impl ZipEntry {
    fn stored(name: &str, mode: u32, data: &[u8]) -> Self {
        let mut crc = flate2::Crc::new();
        crc.update(data);
        Self {
            name: name.as_bytes().to_vec(),
            flags: 0,
            method: 0,
            mode,
            crc: crc.sum(),
            size: len32(data),
            data: data.to_vec(),
        }
    }

    fn deflated(name: &str, mode: u32, data: &[u8]) -> Self {
        let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data).unwrap();
        Self {
            method: 8,
            data: encoder.finish().unwrap(),
            ..Self::stored(name, mode, data)
        }
    }

    fn folder(name: &str) -> Self {
        Self::stored(name, 0o040755, b"")
    }

    fn link(name: &str, target: &str) -> Self {
        Self::stored(name, 0o120777, target.as_bytes())
    }

    /// The fields both headers share, from the flags to the extra field's
    /// length.
    fn common(&self, out: &mut Vec<u8>) {
        for field in [self.flags, self.method, 0, DOS_DATE] {
            out.extend_from_slice(&field.to_le_bytes());
        }
        for field in [self.crc, len32(&self.data), self.size] {
            out.extend_from_slice(&field.to_le_bytes());
        }
        let name_length = u16::try_from(self.name.len()).unwrap();
        for field in [name_length, 0] {
            out.extend_from_slice(&field.to_le_bytes());
        }
    }

    /// The local header, then the data.
    fn local(&self) -> Vec<u8> {
        let mut out = 0x0403_4b50_u32.to_le_bytes().to_vec();
        out.extend_from_slice(&20_u16.to_le_bytes());
        self.common(&mut out);
        out.extend_from_slice(&self.name);
        out.extend_from_slice(&self.data);
        out
    }

    /// The central directory header for a local header at `offset`.
    fn central(&self, offset: u32) -> Vec<u8> {
        // Made by Unix (3) or DOS (0), spec version 2.0.
        let made_by: u16 = if self.mode == 0 { 20 } else { 0x0314 };
        let mut out = 0x0201_4b50_u32.to_le_bytes().to_vec();
        for field in [made_by, 20] {
            out.extend_from_slice(&field.to_le_bytes());
        }
        self.common(&mut out);
        // Comment length, disk, internal attributes.
        for field in [0_u16, 0, 0] {
            out.extend_from_slice(&field.to_le_bytes());
        }
        for field in [self.mode << 16, offset] {
            out.extend_from_slice(&field.to_le_bytes());
        }
        out.extend_from_slice(&self.name);
        out
    }
}

fn zip(entries: &[ZipEntry]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for entry in entries {
        central.extend(entry.central(len32(&out)));
        out.extend(entry.local());
    }
    end(out, &central, entries.len())
}

/// Appends the central directory and its end record to the local headers.
fn end(mut out: Vec<u8>, central: &[u8], count: usize) -> Vec<u8> {
    let offset = len32(&out);
    out.extend_from_slice(central);
    out.extend_from_slice(&0x0605_4b50_u32.to_le_bytes());
    let count = u16::try_from(count).unwrap();
    // Disk, disk with the directory, entries on this disk, entries.
    for field in [0, 0, count, count] {
        out.extend_from_slice(&field.to_le_bytes());
    }
    for field in [len32(central), offset] {
        out.extend_from_slice(&field.to_le_bytes());
    }
    out.extend_from_slice(&0_u16.to_le_bytes());
    out
}

fn len32(bytes: &[u8]) -> u32 {
    u32::try_from(bytes.len()).unwrap()
}

// T9: tar.gz

#[test]
fn tar_dot_slash_prefixes_are_dropped_and_long_names_are_honoured() {
    let pax_name = format!("pax/{}", "p".repeat(150));
    let gnu_name = format!("gnu/{}", "g".repeat(120));
    let bytes = Tar::default()
        .pax_global(&[("comment", b"skipped")])
        .dir("./")
        .dir("./bin/")
        .file("./bin/tool", 0o755, b"tool")
        .pax(&[("path", pax_name.as_bytes()), ("mtime", b"1.5")])
        .file("short", 0o644, b"pax")
        .gnu(b'L', gnu_name.as_bytes())
        .file("short", 0o644, b"gnu")
        .gz();
    let fixture = unpacked(&bytes, TarGz);
    let out = fixture.dest();
    assert_eq!(entries_of(&out), ["bin", "gnu", "pax"]);
    assert_eq!(fs::read(out.join("bin/tool")).unwrap(), b"tool");
    assert_eq!(fs::read(out.join(pax_name)).unwrap(), b"pax");
    assert_eq!(fs::read(out.join(gnu_name)).unwrap(), b"gnu");
}

#[test]
fn tar_names_that_break_a_rule_refuse_the_archive() {
    let deep = ["d"; 33].join("/");
    let cases = [
        ("../evil", "\"..\" component"),
        ("ok/../../evil", "\"..\" component"),
        ("..\\evil", "has the character"),
        ("evil:stream", "has the character"),
        ("evil<", "has the character"),
        ("evil>", "has the character"),
        ("evil\"", "has the character"),
        ("evil|", "has the character"),
        ("evil?", "has the character"),
        ("evil*", "has the character"),
        ("evil\nname", "control character"),
        ("a//evil", "empty component"),
        ("a/./evil", "\".\" component"),
        ("././evil", "\".\" component"),
        (".//evil", "empty component"),
        ("CON", "Windows device name"),
        ("dir/nul.txt", "Windows device name"),
        ("Lpt1", "Windows device name"),
        ("aux.", "Windows device name"),
        ("COM¹.log", "Windows device name"),
        ("evil.", "ends in a dot or a space"),
        ("evil ", "ends in a dot or a space"),
        (deep.as_str(), "more than 32 levels"),
    ];
    for (name, reason) in cases {
        let bytes = Tar::default()
            .file("ok", 0o644, b"fine")
            .file(name, 0o644, b"evil")
            .gz();
        assert_entry_refused(&bytes, TarGz, name, reason);
    }

    let legacy = Tar::default()
        .raw(b'0', b"\xffevil", b"", 0o644, b"evil")
        .gz();
    assert_entry_refused(&legacy, TarGz, "\u{FFFD}evil", "not valid UTF-8");

    let fixture = Fixture::new();
    let absolute = fixture.absolute("evil");
    let bytes = Tar::default()
        .pax(&[("path", absolute.as_bytes())])
        .file("safe", 0o644, b"evil")
        .gz();
    fixture.assert_entry_refused(&bytes, TarGz, &absolute, absolute_reason(&absolute));
}

#[test]
fn tar_hostile_names_in_long_fields_refuse_the_archive() {
    let pax = Tar::default()
        .pax(&[("path", b"../evil")])
        .file("safe", 0o644, b"evil")
        .gz();
    assert_entry_refused(&pax, TarGz, "../evil", "\"..\" component");

    let gnu = Tar::default()
        .gnu(b'L', b"../evil")
        .file("safe", 0o644, b"evil")
        .gz();
    assert_entry_refused(&gnu, TarGz, "../evil", "\"..\" component");

    let prefixed = Tar::default().prefixed_file("..", "evil", b"evil").gz();
    assert_entry_refused(&prefixed, TarGz, "../evil", "\"..\" component");

    let nul = Tar::default()
        .pax(&[("path", b"evil\0name")])
        .file("safe", 0o644, b"evil")
        .gz();
    assert_entry_refused(&nul, TarGz, "evil\0name", "control character");

    let both = Tar::default()
        .gnu(b'L', b"gnu")
        .pax(&[("path", b"pax")])
        .file("safe", 0o644, b"evil")
        .gz();
    assert_entry_refused(&both, TarGz, "gnu", "both a GNU long name and a PAX one");

    let long = "a".repeat(241);
    let too_long = Tar::default()
        .pax(&[("path", long.as_bytes())])
        .file("safe", 0o644, b"evil")
        .gz();
    assert_entry_refused(&too_long, TarGz, &long, "longer than 240 bytes");
}

#[test]
fn tar_entry_types_other_than_files_folders_and_links_refuse_the_archive() {
    let cases = [
        (b'1', "is a hard link"),
        (b'3', "is a device"),
        (b'4', "is a device"),
        (b'6', "is a fifo"),
        (b'S', "is a sparse file"),
        (b'7', "unsupported entry type '7'"),
    ];
    for (kind, reason) in cases {
        let bytes = Tar::default()
            .file("ok", 0o644, b"fine")
            .raw(kind, b"odd", b"ok", 0o644, b"")
            .gz();
        assert_entry_refused(&bytes, TarGz, "odd", reason);
    }

    let sparse = Tar::default()
        .pax(&[("GNU.sparse.major", b"1")])
        .file("odd", 0o644, b"x")
        .gz();
    assert_archive_refused(&sparse, TarGz, Limits::DEFAULT, "sparse file");

    let resized = Tar::default()
        .pax(&[("size", b"2")])
        .file("odd", 0o644, b"x")
        .gz();
    assert_entry_refused(&resized, TarGz, "odd", "PAX size");

    let slash = Tar::default().raw(b'0', b"odd/", b"", 0o644, b"x").gz();
    assert_entry_refused(&slash, TarGz, "odd/", "whose name ends in '/'");

    let full = Tar::default().raw(b'5', b"odd/", b"", 0o755, b"x").gz();
    assert_entry_refused(&full, TarGz, "odd/", "holds 1 bytes of data");
}

#[test]
fn tar_names_that_clash_refuse_the_archive() {
    let cases = [
        ("README", "readme", "differs only in case from \"README\""),
        ("Bin/a", "bin/b", "differs only in case from \"Bin\""),
        ("same", "same", "is listed twice"),
        ("file", "file/inside", "is inside \"file\", which is a file"),
    ];
    for (first, second, reason) in cases {
        let bytes = Tar::default()
            .file(first, 0o644, b"1")
            .file(second, 0o644, b"2")
            .gz();
        assert_entry_refused(&bytes, TarGz, second, reason);
    }

    let both = Tar::default().dir("both/").file("both", 0o644, b"x").gz();
    assert_entry_refused(&both, TarGz, "both", "is both a folder and a file");
}

#[test]
fn tar_links_that_break_a_rule_refuse_the_archive() {
    let cases = [
        // Rule 1: relative, `..` only as a leading run, then the name rules.
        (
            Tar::default().symlink("l", "/etc/passwd"),
            "l",
            "absolute path",
        ),
        (
            Tar::default().dir("a/").symlink("a/l", "../a/../../evil"),
            "a/l",
            "\"..\" component",
        ),
        (
            Tar::default().symlink("l", "evil:stream"),
            "l",
            "has the character",
        ),
        // Rule 2: resolved from the link's folder, the target stays inside.
        (
            Tar::default().symlink("l", "../evil"),
            "l",
            "leads out of the archive",
        ),
        (
            Tar::default().dir("a/").symlink("a/l", "../../evil"),
            "a/l",
            "leads out of the archive",
        ),
        // The chain: l2 comes first and is refused for its inner `..`; l1
        // alone is refused for landing on the top of the archive.
        (
            Tar::default()
                .dir("a/b/")
                .symlink("l2", "a/b/l1/..")
                .symlink("a/b/l1", "../.."),
            "l2",
            "\"..\" component",
        ),
        (
            Tar::default().dir("a/b/").symlink("a/b/l1", "../.."),
            "a/b/l1",
            "top of the archive",
        ),
        // Rule 3: no link on the way to the target, or to the link.
        (
            Tar::default()
                .dir("d/sub/")
                .symlink("d/l1", "sub")
                .symlink("l2", "d/l1/file"),
            "l2",
            "passes through the link \"d/l1\"",
        ),
        (
            Tar::default()
                .dir("real/")
                .symlink("l", "real")
                .symlink("l/m", "x"),
            "l/m",
            "is inside \"l\", which is a link",
        ),
        (
            Tar::default()
                .dir("real/")
                .symlink("l", "real")
                .file("l/x", 0o644, b"x"),
            "l/x",
            "is inside \"l\", which is a link",
        ),
        (
            Tar::default().file("d/x", 0o644, b"x").symlink("d", "x"),
            "d",
            "is both a folder and a link",
        ),
        // Extension headers carry targets too.
        (
            Tar::default()
                .pax(&[("linkpath", b"../evil")])
                .symlink("l", "safe"),
            "l",
            "leads out of the archive",
        ),
        (
            Tar::default().gnu(b'K', b"../evil").symlink("l", "safe"),
            "l",
            "leads out of the archive",
        ),
        (
            Tar::default()
                .dir("a/")
                .pax(&[("linkpath", b"")])
                .symlink("a/l", "safe"),
            "a/l",
            "which is empty",
        ),
    ];
    for (tar, entry, reason) in cases {
        assert_link_refused(&tar.gz(), TarGz, entry, reason);
    }
}

#[test]
fn an_existing_destination_is_refused_and_left_alone() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.dest()).unwrap();
    fs::write(fixture.dest().join("keep"), b"mine").unwrap();
    let bytes = Tar::default().file("x", 0o644, b"x").gz();
    let result = fixture.unpack(&bytes, TarGz, Limits::DEFAULT);
    assert!(matches!(result, Err(Error::Io { .. })), "{result:?}");
    assert_eq!(fs::read(fixture.dest().join("keep")).unwrap(), b"mine");
}

// T10: zip

#[test]
fn zip_folders_and_stored_and_deflated_files_unpack() {
    let bytes = zip(&[
        ZipEntry::folder("bin/"),
        ZipEntry::deflated("bin/tool", 0o100755, b"tool tool tool"),
        ZipEntry::stored("readme", 0, b"hello"),
    ]);
    let fixture = unpacked(&bytes, Zip);
    let out = fixture.dest();
    assert_eq!(entries_of(&out), ["bin", "readme"]);
    assert_eq!(fs::read(out.join("bin/tool")).unwrap(), b"tool tool tool");
    assert_eq!(fs::read(out.join("readme")).unwrap(), b"hello");
}

#[test]
fn zip_names_that_break_a_rule_refuse_the_archive() {
    let cases = [
        ("../evil", "\"..\" component"),
        ("..\\evil", "has the character"),
        ("evil:stream", "has the character"),
        ("CON", "Windows device name"),
        ("dir/nul.txt", "Windows device name"),
        ("evil.", "ends in a dot or a space"),
        ("a//evil", "empty component"),
    ];
    for (name, reason) in cases {
        let bytes = zip(&[
            ZipEntry::stored("ok", 0o100644, b"fine"),
            ZipEntry::stored(name, 0o100644, b"evil"),
        ]);
        assert_entry_refused(&bytes, Zip, name, reason);
    }

    // A name in a legacy code page, not UTF-8.
    let mut legacy = ZipEntry::stored("x", 0o100644, b"evil");
    legacy.name = b"\x81evil".to_vec();
    assert_entry_refused(&zip(&[legacy]), Zip, "\u{FFFD}evil", "not valid UTF-8");

    let fixture = Fixture::new();
    let absolute = fixture.absolute("evil");
    let bytes = zip(&[ZipEntry::stored(&absolute, 0o100644, b"evil")]);
    fixture.assert_entry_refused(&bytes, Zip, &absolute, absolute_reason(&absolute));
}

#[test]
fn zip_entries_that_break_a_rule_refuse_the_archive() {
    let mut secret = ZipEntry::stored("secret", 0o100644, b"x");
    secret.flags = 1;
    let mut bzip2 = ZipEntry::stored("bzip2", 0o100644, b"x");
    bzip2.method = 12;
    let cases = [
        (secret, "secret", "is encrypted"),
        (bzip2, "bzip2", "compression method"),
        (
            ZipEntry::stored("pipe", 0o010644, b""),
            "pipe",
            "device, fifo or socket",
        ),
        (
            ZipEntry::stored("dir", 0o040755, b""),
            "dir",
            "does not end in '/'",
        ),
        (
            ZipEntry::stored("dir/", 0o040755, b"x"),
            "dir/",
            "holds 1 bytes of data",
        ),
        (ZipEntry::link("dir/", "x"), "dir/", "is not a folder"),
    ];
    for (entry, name, reason) in cases {
        let bytes = zip(&[ZipEntry::stored("ok", 0o100644, b"fine"), entry]);
        assert_entry_refused(&bytes, Zip, name, reason);
    }

    let clash = zip(&[
        ZipEntry::stored("README", 0o100644, b"1"),
        ZipEntry::stored("readme", 0o100644, b"2"),
    ]);
    assert_entry_refused(&clash, Zip, "readme", "differs only in case");

    let twice = zip(&[
        ZipEntry::stored("same", 0o100644, b"1"),
        ZipEntry::stored("same", 0o100644, b"2"),
    ]);
    assert_archive_refused(&twice, Zip, Limits::DEFAULT, "duplicate file names");

    // b's local header and data sit inside a's data, after a's 30-byte local
    // header and 1-byte name.
    let b = ZipEntry::stored("b", 0o100644, b"bee");
    let a = ZipEntry::stored("a", 0o100644, &b.local());
    let mut central = a.central(0);
    central.extend(b.central(31));
    let overlapping = end(a.local(), &central, 2);
    assert_archive_refused(&overlapping, Zip, Limits::DEFAULT, "overlapping entries");

    let escape = zip(&[ZipEntry::link("l", "../evil")]);
    assert_link_refused(&escape, Zip, "l", "leads out of the archive");
}

// T11: caps

#[test]
fn the_entry_cap_counts_entries_and_the_folders_they_imply() {
    let three = Limits {
        entries: 3,
        bytes: MAX_UNPACKED_BYTES,
    };
    let files = |count: usize| {
        (0..count)
            .fold(Tar::default(), |tar, n| {
                tar.file(&format!("f{n}"), 0o644, b"x")
            })
            .gz()
    };
    Fixture::new().unpack(&files(3), TarGz, three).unwrap();
    assert_archive_refused(&files(4), TarGz, three, "more than 3 entries");

    // One file three folders deep is four entries.
    let deep = Tar::default().file("a/b/c/file", 0o644, b"x").gz();
    assert_archive_refused(&deep, TarGz, three, "more than 3 entries");

    let zipped: Vec<ZipEntry> = (0..4)
        .map(|n| ZipEntry::stored(&format!("f{n}"), 0o100644, b"x"))
        .collect();
    assert_archive_refused(&zip(&zipped), Zip, three, "more than 3 entries");
}

#[test]
fn the_size_cap_counts_the_bytes_produced() {
    let hundred = Limits {
        entries: MAX_ENTRIES,
        bytes: 100,
    };
    let exact = Tar::default()
        .file("a", 0o644, &[b'x'; 60])
        .file("b", 0o644, &[b'x'; 40])
        .gz();
    Fixture::new().unpack(&exact, TarGz, hundred).unwrap();

    let over = Tar::default()
        .file("a", 0o644, &[b'x'; 60])
        .file("b", 0o644, &[b'x'; 41])
        .gz();
    assert_archive_refused(&over, TarGz, hundred, "more than 100 bytes");

    // A tar extension header's data counts too.
    let pax = Tar::default()
        .pax(&[("comment", &[b'x'; 100][..])])
        .file("a", 0o644, b"x")
        .gz();
    assert_archive_refused(&pax, TarGz, hundred, "more than 100 bytes");

    let zipped = zip(&[ZipEntry::deflated("a", 0o100644, &[b'x'; 101])]);
    assert_archive_refused(&zipped, Zip, hundred, "more than 100 bytes");

    // An extension header over 64 KiB is refused whatever the size cap.
    let huge = Tar::default()
        .pax(&[("comment", &[b'x'; 70_000][..])])
        .file("a", 0o644, b"x")
        .gz();
    assert_archive_refused(&huge, TarGz, Limits::DEFAULT, "extension header");
}

#[test]
fn a_zip_entry_must_inflate_to_the_size_it_declares() {
    // The zip crate fails the read once the data passes the declared size.
    let mut big = ZipEntry::deflated("big", 0o100644, &[b'a'; 1000]);
    big.size = 10;
    assert_entry_refused(&zip(&[big]), Zip, "big", "cannot be read");

    // A stored entry with fewer bytes than it declares, and a matching CRC.
    let mut short = ZipEntry::stored("short", 0o100644, b"12345");
    short.size = 10;
    assert_entry_refused(
        &zip(&[short]),
        Zip,
        "short",
        "holds 5 bytes, but its header lists 10",
    );
}

// T12: modes and links

#[cfg(unix)]
#[test]
fn unix_modes_are_0755_or_0644_and_valid_relative_links_are_created() {
    use std::os::unix::fs::PermissionsExt;

    let mode = |path: &Path| fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777;

    let tar = Tar::default()
        .dir("bin/")
        .file("bin/tool", 0o4750, b"tool")
        .file("bin/plain", 0o2600, b"plain")
        .file("lib/all", 0o1777, b"all")
        .symlink("bin/alias", "tool")
        .symlink("lib/up", "../bin/tool")
        .gz();
    let fixture = unpacked(&tar, TarGz);
    let out = fixture.dest();
    for (path, expected) in [
        (".", 0o755),
        ("bin", 0o755),
        ("lib", 0o755),
        ("bin/tool", 0o755),
        ("bin/plain", 0o644),
        ("lib/all", 0o755),
    ] {
        assert_eq!(mode(&out.join(path)), expected, "{path}");
    }
    assert_eq!(
        fs::read_link(out.join("bin/alias")).unwrap(),
        Path::new("tool")
    );
    assert_eq!(
        fs::read_link(out.join("lib/up")).unwrap(),
        Path::new("../bin/tool")
    );
    assert_eq!(fs::read(out.join("lib/up")).unwrap(), b"tool");

    let zipped = zip(&[
        ZipEntry::stored("run", 0o104755, b"run"),
        ZipEntry::stored("doc", 0o100600, b"doc"),
        ZipEntry::link("alias", "run"),
    ]);
    let fixture = unpacked(&zipped, Zip);
    let out = fixture.dest();
    assert_eq!(mode(&out.join("run")), 0o755);
    assert_eq!(mode(&out.join("doc")), 0o644);
    assert_eq!(fs::read_link(out.join("alias")).unwrap(), Path::new("run"));
}

#[cfg(windows)]
#[test]
fn windows_refuses_an_archive_with_a_link() {
    let tar = Tar::default()
        .file("tool", 0o755, b"tool")
        .symlink("alias", "tool")
        .gz();
    assert_entry_refused(&tar, TarGz, "alias", "refused on Windows");

    let zipped = zip(&[
        ZipEntry::stored("tool", 0o100755, b"tool"),
        ZipEntry::link("alias", "tool"),
    ]);
    assert_entry_refused(&zipped, Zip, "alias", "refused on Windows");
}
