//! The release tool of the PHP build pipeline. It never ships.
//!
//! - [`lock`]: the tracked build inputs, `catalog/sources.lock.json` (the
//!   pins) and `catalog/php-lines.json` (the PHP lines we build). This crate
//!   is their only reader and the only writer of the lock.
//! - [`pack`]: packs a relocated runtime folder into the archive users unpack,
//!   and writes its [`Descriptor`].
//! - [`payload`]: measures the downloaded release files and turns them into
//!   the unsigned catalog payload, and reads a payload file back.
//! - [`assets`]: checks a payload against the downloaded release files.
//! - [`key`]: reads the owner's signing key (Linux and macOS only, never in
//!   CI) and reports what it is.
//! - [`sign`]: signs a payload into an envelope (D25), and verifies one with
//!   the compiled keys.
//!
//! `build/php/README.md` freezes the formats and the CLI. Every failure is an
//! [`Error`]: `Io` for a file-system call, `Core` for an error `ck-core`
//! returns, and `Invalid` for every rule this tool adds, refusals included.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use ck_core::catalog::schema::{
    Arch, ArchiveFormat, Os, Platform, RuntimeName, RuntimeVersion, Sha256Digest,
};
use serde::{Deserialize, Serialize};

pub mod assets;
pub mod key;
pub mod lock;
pub mod pack;
pub mod payload;
pub mod sign;

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

/// What `payload build` reads next to each archive: one runtime version for
/// one OS and CPU. `pack` produces descriptors for Unix archives.
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
        check_archive_format(self.platform(), self.format)
            .map_err(|reason| Error::Invalid(format!("{what}: {reason}")))?;
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

/// `<name>-<version>-<os>-<cpu>.tar.gz` on Unix or `.zip` on Windows.
/// This only builds a name; it does not validate platform eligibility.
pub fn asset_file_name(name: &RuntimeName, version: &RuntimeVersion, platform: Platform) -> String {
    let suffix = archive_suffix(platform_archive_format(platform));
    format!(
        "{name}-{version}-{}-{}{suffix}",
        platform.os.as_str(),
        platform.arch.as_str()
    )
}

pub(crate) const fn archive_suffix(format: ArchiveFormat) -> &'static str {
    match format {
        ArchiveFormat::TarGz => ".tar.gz",
        ArchiveFormat::Zip => ".zip",
    }
}

const fn platform_archive_format(platform: Platform) -> ArchiveFormat {
    match platform.os {
        Os::Linux | Os::Macos => ArchiveFormat::TarGz,
        Os::Windows => ArchiveFormat::Zip,
    }
}

/// Release-tool rules only; the app's catalog schema and trust are unchanged.
pub(crate) fn check_archive_format(
    platform: Platform,
    format: ArchiveFormat,
) -> Result<(), String> {
    if platform.os == Os::Windows && platform.arch != Arch::X86_64 {
        return Err("Windows release assets require x86_64".to_owned());
    }
    let expected = platform_archive_format(platform);
    if format != expected {
        let suffix = archive_suffix(expected);
        return Err(format!(
            "the format for {platform} must be {}",
            &suffix[1..]
        ));
    }
    Ok(())
}

/// The release download folder every artifact URL sits under, from
/// `ck-core`'s compiled catalog URL:
/// `https://github.com/ckm-php/ckm-dev/releases/download/`.
pub fn release_prefix() -> Result<&'static str> {
    ck_core::catalog::trust::release_download_prefix().ok_or_else(|| {
        Error::Invalid("this build has no catalog URL, so no release prefix".to_owned())
    })
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

/// Helpers shared by the unit tests of every module.
#[cfg(test)]
pub(crate) mod test_support {
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use std::process;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// A new folder under the system temp folder, removed on drop.
    pub(crate) struct TempDir(pub(crate) PathBuf);

    impl TempDir {
        pub(crate) fn new() -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |since| since.subsec_nanos());
            let path = env::temp_dir().join(format!(
                "ck-release-{}-{nanos}-{}",
                process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}
