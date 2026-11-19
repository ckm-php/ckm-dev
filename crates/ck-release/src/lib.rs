//! The release tool of the PHP build pipeline. It never ships.
//!
//! - [`lock`]: the tracked build inputs, `catalog/sources.lock.json` (the
//!   pins) and `catalog/php-lines.json` (the PHP lines we build). This crate
//!   is their only reader and the only writer of the lock.
//! - [`pack`]: packs a relocated runtime folder into the archive users unpack,
//!   and writes its [`Descriptor`].
//! - [`payload`]: turns descriptors into an unsigned catalog payload.
//!
//! `build/php/README.md` freezes the formats and the CLI.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use ck_core::catalog::schema::{
    Arch, ArchiveFormat, Os, Platform, RuntimeName, RuntimeVersion, Sha256Digest,
};
use serde::{Deserialize, Serialize};

pub mod lock;
pub mod pack;
pub mod payload;

/// Result with [`Error`] as the default error.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Every failure of the release tool.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A file-system call on `path` failed.
    #[error("cannot {op} {}: {source}", path.display())]
    Io {
        op: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// A tracked file, a descriptor or an argument breaks a rule.
    #[error("{0}")]
    Invalid(String),

    #[error(transparent)]
    Core(#[from] ck_core::Error),
}

impl Error {
    pub(crate) fn io(op: &'static str, path: &Path, source: io::Error) -> Self {
        Self::Io {
            op,
            path: path.to_owned(),
            source,
        }
    }
}

/// The descriptor schema this tool writes and reads.
pub const DESCRIPTOR_SCHEMA: u32 = 1;

/// What `pack` writes next to each archive and `payload build` reads: one
/// archive of one runtime version for one OS and CPU.
///
/// ```json
/// {"schema": 1, "name": "php", "version": "8.5.12-ck1", "license": "PHP-3.01",
///  "os": "linux", "cpu": "x86_64", "file": "php-8.5.12-ck1-linux-x86_64.tar.gz",
///  "sha256": "<64 lower-case hex>", "size": 31457280, "format": "tar.gz"}
/// ```
///
/// `os`, `cpu` and `format` take `ck-core`'s catalog values; `cpu` becomes
/// the catalog's `arch`. `file` is always [`asset_file_name`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Descriptor {
    pub schema: u32,
    pub name: RuntimeName,
    pub version: RuntimeVersion,
    /// The SPDX licence expression for the catalog entry.
    pub license: String,
    pub os: Os,
    pub cpu: Arch,
    /// The release asset's file name, and the archive's name on disk.
    pub file: String,
    pub sha256: Sha256Digest,
    /// Exact size in bytes.
    pub size: u64,
    pub format: ArchiveFormat,
}

impl Descriptor {
    pub fn platform(&self) -> Platform {
        Platform {
            os: self.os,
            arch: self.cpu,
        }
    }

    /// Parses and checks one descriptor.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let descriptor: Self = serde_json::from_slice(bytes)
            .map_err(|error| Error::Invalid(format!("the descriptor: {error}")))?;
        descriptor.validate()?;
        Ok(descriptor)
    }

    /// The schema, the licence, the size, the format, and the file name.
    pub fn validate(&self) -> Result<()> {
        let what = format!("descriptor {} {}", self.name, self.version);
        if self.schema != DESCRIPTOR_SCHEMA {
            return Err(Error::Invalid(format!(
                "{what}: schema {} is not {DESCRIPTOR_SCHEMA}",
                self.schema
            )));
        }
        lock::check_license(&self.license)
            .map_err(|reason| Error::Invalid(format!("{what}: {reason}")))?;
        lock::check_size(self.size)
            .map_err(|reason| Error::Invalid(format!("{what}: {reason}")))?;
        if self.format != ArchiveFormat::TarGz {
            return Err(Error::Invalid(format!("{what}: the format must be tar.gz")));
        }
        let expected = asset_file_name(&self.name, &self.version, self.platform());
        if self.file != expected {
            return Err(Error::Invalid(format!(
                "{what}: the file is {:?}, not {expected:?}",
                self.file
            )));
        }
        Ok(())
    }

    /// Reads and checks a descriptor file.
    pub fn read(path: &Path) -> Result<Self> {
        let bytes = fs::read(path).map_err(|e| Error::io("read", path, e))?;
        Self::parse(&bytes).map_err(|error| Error::Invalid(format!("{}: {error}", path.display())))
    }

    /// Writes the descriptor as pretty JSON with a trailing newline.
    pub fn write(&self, path: &Path) -> Result<()> {
        self.validate()?;
        lock::write_atomic(path, &to_pretty_json(self)?)
    }
}

/// `<name>-<version>-<os>-<cpu>.tar.gz`, for example
/// `php-8.5.12-ck1-linux-x86_64.tar.gz`: unique within a release.
pub fn asset_file_name(name: &RuntimeName, version: &RuntimeVersion, platform: Platform) -> String {
    format!(
        "{name}-{version}-{}-{}.tar.gz",
        platform.os.as_str(),
        platform.arch.as_str()
    )
}

/// Reads an OS as `ck-core` spells it: `linux`, `macos` or `windows`.
pub fn parse_os(text: &str) -> Result<Os> {
    [Os::Linux, Os::Macos, Os::Windows]
        .into_iter()
        .find(|os| os.as_str() == text)
        .ok_or_else(|| Error::Invalid(format!("{text:?} is not an OS (linux, macos, windows)")))
}

/// Reads a CPU as `ck-core` spells it: `x86_64` or `aarch64`.
pub fn parse_cpu(text: &str) -> Result<Arch> {
    [Arch::X86_64, Arch::Aarch64]
        .into_iter()
        .find(|cpu| cpu.as_str() == text)
        .ok_or_else(|| Error::Invalid(format!("{text:?} is not a CPU (x86_64, aarch64)")))
}

/// Pretty JSON with two-space indents and a trailing newline: the form of
/// every file this tool writes.
pub(crate) fn to_pretty_json<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| Error::Invalid(format!("cannot serialize: {error}")))?;
    bytes.push(b'\n');
    Ok(bytes)
}
