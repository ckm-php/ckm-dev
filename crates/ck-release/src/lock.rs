//! The tracked build inputs.
//!
//! - **`catalog/sources.lock.json`**, the [`Lock`]: one pinned upstream file
//!   per entry. Only `ck-release lock set` writes it, in its canonical form
//!   ([`Lock::to_canonical_bytes`]); [`Lock::load`] refuses any other bytes.
//! - **`catalog/php-lines.json`**, the [`Manifest`]: the PHP lines we build,
//!   the library stack of each, and which extensions are built in, shared and
//!   on, or shared and off. It holds no pins: a line's exact PHP version is
//!   its `php` entry in the lock, and a stack names a version *series* for a
//!   library whose two stacks need different versions.
//!
//! [`Inputs`] holds both and checks the rules that span them. A lock that
//! does not yet pin everything passes: the rule that a pin exists lives in
//! [`Inputs::php_entry`] and [`Inputs::stack_entries`], which the build calls.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use ck_core::catalog::schema::{MAX_ARTIFACT_BYTES, RuntimeName, RuntimeVersion, Sha256Digest};
use ck_core::extract::names::is_reserved_component;
use serde::{Deserialize, Serialize};
use url::{Host, Url};

use crate::{Error, Result, to_pretty_json};

/// The lock schema this tool reads and writes.
pub const LOCK_SCHEMA: u32 = 1;

/// The manifest schema this tool reads.
pub const MANIFEST_SCHEMA: u32 = 1;

/// The only hosts a pinned URL may name. Hosts compare exactly, never by
/// suffix. GitHub's redirect to its asset host is the fetcher's concern; the
/// lock records only the first URL. The five sources added 2026-11-28 are
/// approved for CI and pin refresh only: the app continues to download
/// runtime assets solely from verified GitHub Releases.
pub const APPROVED_HOSTS: &[&str] = &[
    "www.php.net",
    "github.com",
    "download.gnome.org",
    "www.sqlite.org",
    "curl.se",
    "downloads.php.net",
    "git.almalinux.org",
    "repo.almalinux.org",
    "security.ubuntu.com",
    "archive.ubuntu.com",
];

/// The lock entry name of the PHP source itself.
pub const PHP: &str = "php";

/// File names inside `catalog/`.
pub const LOCK_FILE: &str = "sources.lock.json";
pub const MANIFEST_FILE: &str = "php-lines.json";

const MAX_LICENSE_BYTES: usize = 128;
const MAX_URL_BYTES: usize = 2048;
const MAX_FILE_NAME_BYTES: usize = 128;
const MAX_NUMBER_DIGITS: usize = 4;
const MAX_EXTENSION_BYTES: usize = 32;

/// `catalog/sources.lock.json` in the repository this tool was built from.
/// The tool never ships, so a compile-time path is safe; `--lock` overrides it.
pub fn default_lock_path() -> PathBuf {
    catalog_dir().join(LOCK_FILE)
}

/// `catalog/php-lines.json`; `--manifest` overrides it.
pub fn default_manifest_path() -> PathBuf {
    catalog_dir().join(MANIFEST_FILE)
}

fn catalog_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("catalog")
}

/// What `refresh-pins.sh` checked upstream besides the SHA256 pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum SignatureKind {
    /// A detached OpenPGP signature, checked with `gpgv` against the pinned
    /// keyring and fingerprint allow-list.
    Gpg,
    /// A minisign signature against a pinned public key.
    Minisig,
    /// Only a checksum file published upstream next to the download.
    Sha256File,
    /// Nothing upstream to check: the pin is the only check.
    None,
}

impl SignatureKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gpg => "gpg",
            Self::Minisig => "minisig",
            Self::Sha256File => "sha256-file",
            Self::None => "none",
        }
    }
}

/// `catalog/sources.lock.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lock {
    pub schema: u32,
    pub entries: Vec<LockEntry>,
}

/// One pinned upstream file. Fields serialize in this order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockEntry {
    pub name: RuntimeName,
    pub version: RuntimeVersion,
    pub url: String,
    pub sha256: Sha256Digest,
    pub size: u64,
    /// SPDX licence expression of the component.
    pub license: String,
    pub signature: SignatureKind,
}

impl LockEntry {
    /// The URL's last path segment: the name the fetcher saves the file as.
    /// A safe file name once [`LockEntry::validate`] has passed, which every
    /// entry of a parsed [`Lock`] has.
    pub fn file_name(&self) -> &str {
        last_segment(&self.url)
    }

    /// The URL, size and licence rules of one entry.
    pub fn validate(&self) -> Result<()> {
        let what = self.label();
        let wrap = |reason: String| Error::Invalid(format!("{what}: {reason}"));
        check_url(&self.url).map_err(wrap)?;
        check_size(self.size).map_err(wrap)?;
        check_license(&self.license).map_err(wrap)?;
        Ok(())
    }

    /// One line of `lock export`: eight fields separated by one tab, ending
    /// in a newline. No field is empty or holds a tab or newline.
    ///
    /// `name  version  url  sha256  size  license  signature  file`
    pub fn export_line(&self) -> String {
        format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            self.name,
            self.version,
            self.url,
            self.sha256,
            self.size,
            self.license,
            self.signature.as_str(),
            self.file_name()
        )
    }

    fn label(&self) -> String {
        format!("lock entry {} {}", self.name, self.version)
    }

    fn same_pin(&self, other: &Self) -> bool {
        self.name == other.name
            && self
                .version
                .as_str()
                .eq_ignore_ascii_case(other.version.as_str())
    }
}

impl Lock {
    /// An empty lock.
    pub fn new() -> Self {
        Self {
            schema: LOCK_SCHEMA,
            entries: Vec::new(),
        }
    }

    /// Parses and checks a lock: the schema, each entry, and no two entries
    /// for one name and version (versions compare ignoring case).
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let lock: Self = serde_json::from_slice(bytes)
            .map_err(|error| Error::Invalid(format!("the lock: {error}")))?;
        lock.validate()?;
        Ok(lock)
    }

    pub fn validate(&self) -> Result<()> {
        if self.schema != LOCK_SCHEMA {
            return Err(Error::Invalid(format!(
                "lock schema {} is not {LOCK_SCHEMA}",
                self.schema
            )));
        }
        let mut seen = HashSet::new();
        for entry in &self.entries {
            entry.validate()?;
            if !seen.insert((
                entry.name.as_str(),
                entry.version.as_str().to_ascii_lowercase(),
            )) {
                return Err(Error::Invalid(format!(
                    "{} is pinned more than once (versions compare ignoring case)",
                    entry.label()
                )));
            }
        }
        Ok(())
    }

    /// Reads a lock file. Its bytes must be exactly
    /// [`Lock::to_canonical_bytes`], so every change goes through `lock set`.
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = fs::read(path).map_err(|e| Error::io("read", path, e))?;
        let lock = Self::parse(&bytes)
            .map_err(|error| Error::Invalid(format!("{}: {error}", path.display())))?;
        if bytes != lock.to_canonical_bytes()? {
            return Err(Error::Invalid(format!(
                "{} is not in the form `ck-release lock set` writes (sorted entries, \
                 two-space JSON, one trailing newline)",
                path.display()
            )));
        }
        Ok(lock)
    }

    /// Entries sorted by name, then version; two-space JSON; a trailing
    /// newline.
    pub fn to_canonical_bytes(&self) -> Result<Vec<u8>> {
        let mut sorted = self.clone();
        sorted.sort();
        to_pretty_json(&sorted)
    }

    /// Adds `entry`, or replaces the entry with its name and version.
    pub fn set(&mut self, entry: LockEntry) {
        self.entries.retain(|old| !old.same_pin(&entry));
        self.entries.push(entry);
        self.sort();
    }

    /// Checks the lock, then replaces `path` atomically with its canonical
    /// bytes.
    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        write_atomic(path, &self.to_canonical_bytes()?)
    }

    fn sort(&mut self) {
        self.entries.sort_by(|a, b| {
            (a.name.as_str(), a.version.as_str()).cmp(&(b.name.as_str(), b.version.as_str()))
        });
    }
}

impl Default for Lock {
    fn default() -> Self {
        Self::new()
    }
}

/// A library stack: every library a group of PHP lines links, built once
/// per OS. Everything that links libssl must use the same libssl.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    clap::ValueEnum,
)]
#[serde(rename_all = "lowercase")]
pub enum Stack {
    /// OpenSSL 1.1.1, for PHP 7.4 and 8.0.
    Legacy,
    /// OpenSSL 3, for PHP 8.1 and later.
    Current,
}

impl Stack {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Legacy => "legacy",
            Self::Current => "current",
        }
    }
}

/// `catalog/php-lines.json`. Hand-edited; formatted by prettier.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema: u32,
    /// For each stack, the version series of each library that differs
    /// between stacks. A library not named here has one pin, shared by every
    /// stack.
    pub stacks: BTreeMap<Stack, BTreeMap<RuntimeName, String>>,
    pub lines: Vec<Line>,
}

/// One PHP line we build.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Line {
    /// The minor line, such as `8.5`.
    pub line: String,
    pub stack: Stack,
    /// Platform IDs with build coverage for this line.
    pub platforms: Vec<String>,
    /// When true, the build planner allows smoke runs but not clean candidates.
    pub ci_only: bool,
    /// The `N` in `8.5.12-ckN`. Raised for a rebuild of the same PHP patch.
    pub build_revision: u32,
    /// The catalog licence: PHP's own. Library licences ship in the archive.
    pub license: String,
    pub extensions: Extensions,
}

/// The extension classes of one line, by the lower-case names configure
/// and the `.so` files use.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Extensions {
    /// Compiled into the binary.
    pub builtin: Vec<String>,
    /// Shared, and on in the generated ini.
    pub default: Vec<String>,
    /// Shared, and off until the user turns it on.
    pub optional: Vec<String>,
}

impl Manifest {
    /// Parses and checks a manifest.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let manifest: Self = serde_json::from_slice(bytes)
            .map_err(|error| Error::Invalid(format!("the manifest: {error}")))?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let bytes = fs::read(path).map_err(|e| Error::io("read", path, e))?;
        Self::parse(&bytes).map_err(|error| Error::Invalid(format!("{}: {error}", path.display())))
    }

    /// The schema; each series; and for each line its syntax, its stack, a
    /// build revision of 1 or more, its licence, and extension names that are
    /// valid and in one class only.
    pub fn validate(&self) -> Result<()> {
        if self.schema != MANIFEST_SCHEMA {
            return Err(Error::Invalid(format!(
                "manifest schema {} is not {MANIFEST_SCHEMA}",
                self.schema
            )));
        }
        for (stack, series) in &self.stacks {
            for (name, prefix) in series {
                if name.as_str() == PHP {
                    return Err(Error::Invalid(format!(
                        "stack {}: php is chosen by line, not by stack",
                        stack.as_str()
                    )));
                }
                if RuntimeVersion::new(prefix.clone()).is_err() {
                    return Err(Error::Invalid(format!(
                        "stack {}: {prefix:?} is not a version series for {name}",
                        stack.as_str()
                    )));
                }
            }
        }
        let mut lines = HashSet::new();
        for line in &self.lines {
            let what = format!("PHP line {:?}", line.line);
            let wrap = |reason: String| Error::Invalid(format!("{what}: {reason}"));
            if !is_php_line(&line.line) {
                return Err(wrap("is not a minor line such as 8.5".to_owned()));
            }
            if !lines.insert(line.line.as_str()) {
                return Err(wrap("is listed more than once".to_owned()));
            }
            if !self.stacks.contains_key(&line.stack) {
                return Err(wrap(format!(
                    "stack {} is not in \"stacks\"",
                    line.stack.as_str()
                )));
            }
            if line.platforms.is_empty()
                || line.platforms.iter().collect::<HashSet<_>>().len() != line.platforms.len()
            {
                return Err(wrap("platforms must be nonempty and unique".to_owned()));
            }
            if line.build_revision == 0 {
                return Err(wrap("build_revision starts at 1".to_owned()));
            }
            check_license(&line.license).map_err(wrap)?;
            let mut names = HashSet::new();
            let all = line
                .extensions
                .builtin
                .iter()
                .chain(&line.extensions.default)
                .chain(&line.extensions.optional);
            for name in all {
                if !is_extension_name(name) {
                    return Err(wrap(format!(
                        "{name:?} is not an extension name (1 to {MAX_EXTENSION_BYTES} of a-z, \
                         0-9 and '_', starting with a letter)"
                    )));
                }
                if !names.insert(name.as_str()) {
                    return Err(wrap(format!("extension {name} is listed more than once")));
                }
            }
        }
        Ok(())
    }

    pub fn line(&self, line: &str) -> Option<&Line> {
        self.lines.iter().find(|candidate| candidate.line == line)
    }
}

/// The lock and the manifest, checked together.
#[derive(Debug, Clone)]
pub struct Inputs {
    pub lock: Lock,
    pub manifest: Manifest,
}

impl Inputs {
    /// Checks the rules that span both files:
    ///
    /// - each `php` entry is a version `X.Y.Z` whose line `X.Y` is in the
    ///   manifest, and each line has at most one;
    /// - for each stack, each library has at most one entry it could use:
    ///   in the stack's series when the stack names one, otherwise any.
    pub fn new(lock: Lock, manifest: Manifest) -> Result<Self> {
        let inputs = Self { lock, manifest };
        inputs.check()?;
        Ok(inputs)
    }

    /// [`Lock::load`] and [`Manifest::load`], then [`Inputs::new`].
    pub fn load(lock: &Path, manifest: &Path) -> Result<Self> {
        Self::new(Lock::load(lock)?, Manifest::load(manifest)?)
    }

    /// Sets a pin and checks the cross-file rules. PHP has one source pin
    /// per minor line; a patch refresh replaces that line's previous pin.
    /// Other pins still use [`Lock::set`]'s exact-version rule.
    pub fn set(&mut self, entry: LockEntry) -> Result<()> {
        entry.validate()?;
        let mut lock = self.lock.clone();
        if entry.name.as_str() == PHP
            && let Some(line) = php_line_of(entry.version.as_str())
        {
            lock.entries.retain(|old| {
                old.name.as_str() != PHP || php_line_of(old.version.as_str()) != Some(line)
            });
        }
        lock.set(entry);
        lock.validate()?;
        let candidate = Self {
            lock,
            manifest: self.manifest.clone(),
        };
        candidate.check()?;
        *self = candidate;
        Ok(())
    }

    fn check(&self) -> Result<()> {
        let mut php_lines = HashSet::new();
        for entry in self.php_entries() {
            let Some(line) = php_line_of(entry.version.as_str()) else {
                return Err(Error::Invalid(format!(
                    "{}: a PHP pin is a version X.Y.Z",
                    entry.label()
                )));
            };
            if self.manifest.line(line).is_none() {
                return Err(Error::Invalid(format!(
                    "{}: line {line} is not in {MANIFEST_FILE}",
                    entry.label()
                )));
            }
            if !php_lines.insert(line) {
                return Err(Error::Invalid(format!(
                    "line {line} has more than one php pin"
                )));
            }
        }
        for stack in self.manifest.stacks.keys() {
            self.resolve_stack(*stack, false)?;
        }
        Ok(())
    }

    /// The `php` entry of `line`; an error when the line is unknown or not
    /// pinned yet.
    pub fn php_entry(&self, line: &str) -> Result<&LockEntry> {
        if self.manifest.line(line).is_none() {
            return Err(Error::Invalid(format!(
                "line {line:?} is not in {MANIFEST_FILE}"
            )));
        }
        self.php_entries()
            .find(|entry| php_line_of(entry.version.as_str()) == Some(line))
            .ok_or_else(|| Error::Invalid(format!("line {line} has no php pin in {LOCK_FILE}")))
    }

    /// Every library entry `stack` builds, one per name, sorted by name.
    /// An error when a library the stack names a series for has no pin in
    /// that series.
    pub fn stack_entries(&self, stack: Stack) -> Result<Vec<&LockEntry>> {
        self.resolve_stack(stack, true)
    }

    /// What the build needs to know about `line`.
    pub fn line_info(&self, line: &str) -> Result<LineInfo<'_>> {
        let php = self.php_entry(line)?;
        let line = self
            .manifest
            .line(line)
            .ok_or_else(|| Error::Invalid(format!("line {line:?} is not in {MANIFEST_FILE}")))?;
        let version = RuntimeVersion::new(format!("{}-ck{}", php.version, line.build_revision))?;
        Ok(LineInfo { line, php, version })
    }

    fn php_entries(&self) -> impl Iterator<Item = &LockEntry> {
        self.lock
            .entries
            .iter()
            .filter(|entry| entry.name.as_str() == PHP)
    }

    /// With `require_all` false, a library with no entry in its series is
    /// skipped, so a lock can be pinned one entry at a time.
    fn resolve_stack(&self, stack: Stack, require_all: bool) -> Result<Vec<&LockEntry>> {
        let empty = BTreeMap::new();
        let series = self.manifest.stacks.get(&stack).unwrap_or(&empty);
        let names: BTreeSet<&RuntimeName> = self
            .lock
            .entries
            .iter()
            .filter(|entry| entry.name.as_str() != PHP)
            .map(|entry| &entry.name)
            .collect();
        let mut chosen = Vec::new();
        for name in &names {
            let prefix = series.get(*name);
            let candidates: Vec<&LockEntry> = self
                .lock
                .entries
                .iter()
                .filter(|entry| &entry.name == *name)
                .filter(|entry| {
                    prefix.is_none_or(|prefix| in_series(entry.version.as_str(), prefix))
                })
                .collect();
            match candidates.as_slice() {
                [entry] => chosen.push(*entry),
                [] if !require_all => {}
                [] => {
                    return Err(Error::Invalid(format!(
                        "stack {}: no pin of {name} in series {:?}",
                        stack.as_str(),
                        prefix.map_or("", String::as_str)
                    )));
                }
                _ => {
                    return Err(Error::Invalid(format!(
                        "stack {}: more than one pin of {name} fits; name a series for it in \
                         {MANIFEST_FILE}",
                        stack.as_str()
                    )));
                }
            }
        }
        if require_all && let Some(name) = series.keys().find(|name| !names.contains(name)) {
            return Err(Error::Invalid(format!(
                "stack {}: {name} has no pin in {LOCK_FILE}",
                stack.as_str()
            )));
        }
        Ok(chosen)
    }
}

/// One PHP line resolved against the lock: the output of `lock line`.
#[derive(Debug, Clone)]
pub struct LineInfo<'a> {
    pub line: &'a Line,
    pub php: &'a LockEntry,
    /// The release version, `<php version>-ck<build revision>`.
    pub version: RuntimeVersion,
}

impl LineInfo<'_> {
    /// `key<TAB>value` lines in a fixed order; lists are separated by one
    /// space and may be empty:
    ///
    /// ```text
    /// line      8.5
    /// php       8.5.12
    /// version   8.5.12-ck1
    /// revision  1
    /// stack     current
    /// license   PHP-3.01
    /// builtin   ctype date ...
    /// default   curl fileinfo ...
    /// optional  ftp shmop ...
    /// ```
    pub fn export(&self) -> String {
        let extensions = &self.line.extensions;
        let mut text = String::new();
        let mut field = |key: &str, value: &str| {
            let _ = writeln!(text, "{key}\t{value}");
        };
        field("line", &self.line.line);
        field("php", self.php.version.as_str());
        field("version", self.version.as_str());
        field("revision", &self.line.build_revision.to_string());
        field("stack", self.line.stack.as_str());
        field("license", &self.line.license);
        field("builtin", &extensions.builtin.join(" "));
        field("default", &extensions.default.join(" "));
        field("optional", &extensions.optional.join(" "));
        text
    }
}

/// 1 to 128 bytes with no control characters: `ck-core`'s catalog rule.
pub(crate) fn check_license(license: &str) -> Result<(), String> {
    if license.is_empty()
        || license.len() > MAX_LICENSE_BYTES
        || license.chars().any(char::is_control)
    {
        return Err(format!(
            "the licence is not 1 to {MAX_LICENSE_BYTES} bytes without control characters"
        ));
    }
    Ok(())
}

/// 1 byte to `ck-core`'s artifact limit.
pub(crate) fn check_size(size: u64) -> Result<(), String> {
    if !(1..=MAX_ARTIFACT_BYTES).contains(&size) {
        return Err(format!(
            "size {size} is not 1 to {MAX_ARTIFACT_BYTES} bytes"
        ));
    }
    Ok(())
}

/// HTTPS to an approved host with no userinfo, port, query or fragment,
/// written exactly as the URL parser writes it, ending in a safe file name.
fn check_url(text: &str) -> Result<(), String> {
    if text.len() > MAX_URL_BYTES {
        return Err(format!("the URL is over {MAX_URL_BYTES} bytes"));
    }
    let url = Url::parse(text).map_err(|error| format!("the URL does not parse: {error}"))?;
    if url.as_str() != text {
        return Err(format!(
            "the URL must be written in its normal form, {:?}",
            url.as_str()
        ));
    }
    if url.scheme() != "https" {
        return Err("the URL must use https".to_owned());
    }
    if !url.username().is_empty() || url.password().is_some() || url.port().is_some() {
        return Err("the URL must have no user, password or port".to_owned());
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err("the URL must have no query or fragment".to_owned());
    }
    match url.host() {
        Some(Host::Domain(host)) if APPROVED_HOSTS.contains(&host) => {}
        _ => {
            return Err(format!(
                "the URL's host is not one of {}",
                APPROVED_HOSTS.join(", ")
            ));
        }
    }
    check_file_name(last_segment(text))
}

/// The text after the URL's last `/`.
fn last_segment(url: &str) -> &str {
    url.rsplit('/').next().unwrap_or_default()
}

/// `[A-Za-z0-9][A-Za-z0-9._+-]*`, at most 128 bytes, with no `..` and no
/// Windows device name: safe as a file name on every OS and in a shell.
pub(crate) fn check_file_name(name: &str) -> Result<(), String> {
    let Some((first, rest)) = name.as_bytes().split_first() else {
        return Err("the file name is empty".to_owned());
    };
    let valid = name.len() <= MAX_FILE_NAME_BYTES
        && first.is_ascii_alphanumeric()
        && rest
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'+' | b'-'))
        && !name.contains("..")
        && !is_reserved_component(name);
    if valid {
        Ok(())
    } else {
        Err(format!(
            "{name:?} is not a safe file name (1 to {MAX_FILE_NAME_BYTES} of A-Z, a-z, 0-9 and \
             '._+-', starting with a letter or digit, with no '..')"
        ))
    }
}

/// `version` is `series`, or starts with it followed by anything but a
/// digit: `3.5.9` and `3.5` fit `3.5`, `1.1.1w` fits `1.1.1`, `3.50.1` does
/// not fit `3.5`.
fn in_series(version: &str, series: &str) -> bool {
    match version.strip_prefix(series) {
        Some(rest) => !rest.as_bytes().first().is_some_and(u8::is_ascii_digit),
        None => false,
    }
}

/// A number with 1 to 4 digits and no leading zero.
fn is_number(text: &str) -> bool {
    (1..=MAX_NUMBER_DIGITS).contains(&text.len())
        && text.bytes().all(|byte| byte.is_ascii_digit())
        && (text == "0" || !text.starts_with('0'))
}

/// `X.Y`, such as `8.5`.
fn is_php_line(text: &str) -> bool {
    text.split_once('.')
        .is_some_and(|(major, minor)| is_number(major) && is_number(minor))
}

/// The line `X.Y` of a PHP version `X.Y.Z`.
fn php_line_of(version: &str) -> Option<&str> {
    let (line, patch) = version.rsplit_once('.')?;
    (is_php_line(line) && is_number(patch)).then_some(line)
}

fn is_extension_name(name: &str) -> bool {
    let Some((first, rest)) = name.as_bytes().split_first() else {
        return false;
    };
    name.len() <= MAX_EXTENSION_BYTES
        && first.is_ascii_lowercase()
        && rest
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'_')
}

/// Replaces `path`: writes `.<name>.<pid>.tmp` in the same folder, syncs
/// it, then renames it over `path`.
pub(crate) fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| Error::Invalid(format!("{} has no file name", path.display())))?;
    let mut temp_name = std::ffi::OsString::from(".");
    temp_name.push(name);
    temp_name.push(format!(".{}.tmp", std::process::id()));
    let temp = path.with_file_name(temp_name);
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|e| Error::io("create", &temp, e))?;
        file.write_all(contents)
            .and_then(|()| file.sync_all())
            .map_err(|e| Error::io("write", &temp, e))?;
        drop(file);
        fs::rename(&temp, path).map_err(|e| Error::io("replace", path, e))
    })();
    if result.is_err() {
        // Best effort: the first error is the one worth reporting.
        let _ = fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    const OPENSSL_URL: &str =
        "https://github.com/openssl/openssl/releases/download/openssl-3.5.9/openssl-3.5.9.tar.gz";

    fn entry(name: &str, version: &str, url: &str) -> Value {
        json!({
            "name": name,
            "version": version,
            "url": url,
            "sha256": "a".repeat(64),
            "size": 1,
            "license": "Apache-2.0",
            "signature": "gpg",
        })
    }

    #[test]
    fn lock_rules() {
        type Mutate = fn(&mut Value);
        let cases: &[(&str, Mutate, bool)] = &[
            ("valid", |_| {}, true),
            (
                "every signature kind",
                |l| l["entries"][0]["signature"] = json!("sha256-file"),
                true,
            ),
            (
                "http",
                |l| l["entries"][0]["url"] = json!(OPENSSL_URL.replacen("https", "http", 1)),
                false,
            ),
            (
                "host not approved",
                |l| l["entries"][0]["url"] = json!("https://example.com/a.tar.gz"),
                false,
            ),
            (
                "host suffix",
                |l| l["entries"][0]["url"] = json!("https://github.com.evil.com/a.tar.gz"),
                false,
            ),
            (
                "port",
                |l| l["entries"][0]["url"] = json!("https://github.com:8443/a.tar.gz"),
                false,
            ),
            (
                "userinfo",
                |l| l["entries"][0]["url"] = json!("https://u@github.com/a.tar.gz"),
                false,
            ),
            (
                "query",
                |l| l["entries"][0]["url"] = json!("https://github.com/a.tar.gz?x=1"),
                false,
            ),
            (
                "not normal form",
                |l| l["entries"][0]["url"] = json!("https://GitHub.com/a.tar.gz"),
                false,
            ),
            (
                "no file name",
                |l| l["entries"][0]["url"] = json!("https://github.com/a/"),
                false,
            ),
            (
                "encoded file name",
                |l| l["entries"][0]["url"] = json!("https://github.com/a%20b.tar.gz"),
                false,
            ),
            (
                "hash upper case",
                |l| l["entries"][0]["sha256"] = json!("A".repeat(64)),
                false,
            ),
            (
                "hash short",
                |l| l["entries"][0]["sha256"] = json!("a".repeat(63)),
                false,
            ),
            ("size zero", |l| l["entries"][0]["size"] = json!(0), false),
            (
                "licence empty",
                |l| l["entries"][0]["license"] = json!(""),
                false,
            ),
            (
                "licence too long",
                |l| l["entries"][0]["license"] = json!("x".repeat(129)),
                false,
            ),
            (
                "licence control",
                |l| l["entries"][0]["license"] = json!("MIT\tX"),
                false,
            ),
            (
                "bad name",
                |l| l["entries"][0]["name"] = json!("OpenSSL"),
                false,
            ),
            (
                "unknown signature",
                |l| l["entries"][0]["signature"] = json!("pgp"),
                false,
            ),
            (
                "unknown field",
                |l| l["entries"][0]["mirror"] = json!("x"),
                false,
            ),
            ("unknown top field", |l| l["extra"] = json!(1), false),
            ("schema", |l| l["schema"] = json!(2), false),
            (
                "duplicate",
                |l| {
                    let copy = l["entries"][0].clone();
                    l["entries"].as_array_mut().unwrap().push(copy);
                },
                false,
            ),
            (
                "duplicate ignoring case",
                |l| {
                    let mut copy = l["entries"][0].clone();
                    copy["version"] = json!("3.5.9A");
                    l["entries"][0]["version"] = json!("3.5.9a");
                    l["entries"].as_array_mut().unwrap().push(copy);
                },
                false,
            ),
        ];
        for (label, mutate, ok) in cases {
            let mut lock =
                json!({"schema": 1, "entries": [entry("openssl", "3.5.9", OPENSSL_URL)]});
            mutate(&mut lock);
            let result = Lock::parse(lock.to_string().as_bytes());
            assert_eq!(result.is_ok(), *ok, "{label}: {result:?}");
        }
    }

    #[test]
    fn every_approved_host_is_accepted_exactly() {
        for host in APPROVED_HOSTS {
            let url = format!("https://{host}/a.tar.gz");
            let lock = json!({"schema": 1, "entries": [entry("openssl", "3.5.9", &url)]});
            assert!(
                Lock::parse(lock.to_string().as_bytes()).is_ok(),
                "{host} must be accepted"
            );
            let suffix = format!("https://{host}.example.com/a.tar.gz");
            let lock = json!({"schema": 1, "entries": [entry("openssl", "3.5.9", &suffix)]});
            assert!(
                Lock::parse(lock.to_string().as_bytes()).is_err(),
                "a host suffix must still be refused: {suffix}"
            );
        }
    }

    /// Stacks pick their own series, an ambiguous library is refused, a
    /// line finds its php pin, and the export formats stay as frozen.
    #[test]
    fn stacks_lines_and_export() {
        let manifest = Manifest::parse(
            json!({
                "schema": 1,
                "stacks": {"legacy": {"openssl": "1.1.1"}, "current": {"openssl": "3.5"}},
                "lines": [{
                    "line": "8.5", "stack": "current", "build_revision": 2, "license": "PHP-3.01",
                    "platforms": ["linux-x86_64", "macos-aarch64"], "ci_only": false,
                    "extensions": {"builtin": ["ctype"], "default": ["curl", "intl"], "optional": []}
                }]
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap();
        let legacy_url = "https://github.com/openssl/openssl/releases/download/OpenSSL_1_1_1w/openssl-1.1.1w.tar.gz";
        let lock = |extra: Vec<Value>| {
            let mut entries = vec![
                entry("openssl", "3.5.9", OPENSSL_URL),
                entry("openssl", "1.1.1w", legacy_url),
                entry(
                    "php",
                    "8.5.12",
                    "https://www.php.net/distributions/php-8.5.12.tar.xz",
                ),
            ];
            entries.extend(extra);
            Lock::parse(
                json!({"schema": 1, "entries": entries})
                    .to_string()
                    .as_bytes(),
            )
            .unwrap()
        };

        let inputs = Inputs::new(lock(vec![]), manifest.clone()).unwrap();
        let legacy = inputs.stack_entries(Stack::Legacy).unwrap();
        assert_eq!(legacy.len(), 1);
        assert_eq!(legacy[0].version.as_str(), "1.1.1w");
        assert_eq!(
            inputs.stack_entries(Stack::Current).unwrap()[0].export_line(),
            format!(
                "openssl\t3.5.9\t{OPENSSL_URL}\t{}\t1\tApache-2.0\tgpg\topenssl-3.5.9.tar.gz\n",
                "a".repeat(64)
            )
        );
        assert_eq!(
            inputs.line_info("8.5").unwrap().export(),
            "line\t8.5\nphp\t8.5.12\nversion\t8.5.12-ck2\nrevision\t2\nstack\tcurrent\n\
             license\tPHP-3.01\nbuiltin\tctype\ndefault\tcurl intl\noptional\t\n"
        );
        assert!(inputs.line_info("7.4").is_err(), "unknown line");

        let two_libxml2 = vec![
            entry(
                "libxml2",
                "2.11.9",
                "https://download.gnome.org/sources/libxml2/2.11/libxml2-2.11.9.tar.xz",
            ),
            entry(
                "libxml2",
                "2.15.4",
                "https://download.gnome.org/sources/libxml2/2.15/libxml2-2.15.4.tar.xz",
            ),
        ];
        assert!(
            Inputs::new(lock(two_libxml2), manifest.clone()).is_err(),
            "ambiguous"
        );
        let php_without_line = vec![entry(
            "php",
            "7.4.33",
            "https://www.php.net/distributions/php-7.4.33.tar.xz",
        )];
        assert!(
            Inputs::new(lock(php_without_line), manifest).is_err(),
            "php line not in manifest"
        );
    }

    #[test]
    fn setting_a_php_patch_replaces_only_its_minor_line() {
        let mut inputs = Inputs::load(&default_lock_path(), &default_manifest_path()).unwrap();
        let other = inputs.php_entry("7.4").unwrap().clone();
        let count = inputs.lock.entries.len();
        let new: LockEntry = serde_json::from_value(entry(
            "php",
            "8.5.99",
            "https://www.php.net/distributions/php-8.5.99.tar.xz",
        ))
        .unwrap();
        inputs.set(new).unwrap();
        assert_eq!(inputs.php_entry("8.5").unwrap().version.as_str(), "8.5.99");
        assert_eq!(inputs.php_entry("7.4").unwrap(), &other);
        assert_eq!(inputs.lock.entries.len(), count);
    }

    #[test]
    fn tracked_files_load() {
        let inputs = Inputs::load(&default_lock_path(), &default_manifest_path()).unwrap();
        let lines: Vec<&str> = inputs
            .manifest
            .lines
            .iter()
            .map(|line| line.line.as_str())
            .collect();
        assert_eq!(lines, ["7.4", "8.2", "8.3", "8.4", "8.5"]);
    }
}
