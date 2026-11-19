//! The catalog payload, schema 1 (D3, D6).
//!
//! ```json
//! {"schema": 1, "sequence": 7, "expires": 1790000000,
//!  "runtimes": [{"name": "php", "version": "8.4.3", "license": "PHP-3.01",
//!    "artifacts": [{"os": "linux", "arch": "x86_64",
//!      "url": "https://github.com/…/php-8.4.3-linux-x86_64.tar.gz",
//!      "sha256": "<64 lower-case hex>", "size": 31457280, "format": "tar.gz"}]}]}
//! ```
//!
//! Unknown fields are refused at every level. The newtypes and enums check
//! their own value when they are built or deserialized, so a name or version
//! that reaches a path is always valid; [`parse`] adds the rules that span
//! fields.

use std::collections::HashSet;
use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::download::fetch::parse_strict_url;
use crate::error::{Error, Result, escape_untrusted};
use crate::extract::names::is_reserved_component;
use crate::service::spec::ServiceId;

/// The payload schema this app reads.
pub const SCHEMA: u32 = 1;

/// The largest artifact a catalog may pin.
pub const MAX_ARTIFACT_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// The longest version, in bytes.
const MAX_VERSION_BYTES: usize = 64;

/// The longest licence expression, in bytes.
const MAX_LICENSE_BYTES: usize = 128;

/// The longest artifact URL, in bytes.
const MAX_URL_BYTES: usize = 2048;

/// A whole payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Payload {
    pub schema: u32,
    /// Higher in every catalog published after this one.
    pub sequence: u64,
    /// Unix seconds. Nothing installs from the catalog at or after this time.
    pub expires: u64,
    pub runtimes: Vec<Runtime>,
}

/// One version of one runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Runtime {
    pub name: RuntimeName,
    pub version: RuntimeVersion,
    /// The SPDX licence expression, shown to the user.
    pub license: String,
    pub artifacts: Vec<Artifact>,
}

/// The file to download for one OS and CPU.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub os: Os,
    pub arch: Arch,
    /// Checked by the transport policy before each request.
    pub url: String,
    pub sha256: Sha256Digest,
    /// Exact size in bytes: 1 to [`MAX_ARTIFACT_BYTES`].
    pub size: u64,
    pub format: ArchiveFormat,
}

/// Parses a payload whose signature has passed, then checks the rules that
/// span fields:
///
/// - `schema` is [`SCHEMA`];
/// - every `license` is 1 to 128 bytes with no control characters;
/// - every `url` is at most 2048 bytes and parses as strictly as the
///   downloader parses it;
/// - every `size` is 1 byte to [`MAX_ARTIFACT_BYTES`];
/// - no two artifacts share a name, version, OS and CPU. Versions compare
///   ignoring case, because on macOS and Windows `1.0A` and `1.0a` would
///   share one folder.
///
/// JSON that does not fit the types, or a broken rule, gives
/// [`Error::CatalogInvalid`].
pub fn parse(payload: &[u8]) -> Result<Payload> {
    let payload: Payload = serde_json::from_slice(payload).map_err(|error| {
        invalid(format!(
            "the payload: {}",
            escape_untrusted(&error.to_string())
        ))
    })?;
    if payload.schema != SCHEMA {
        return Err(invalid(format!(
            "payload schema {} is not supported; this app reads schema {SCHEMA}",
            payload.schema
        )));
    }
    check_artifacts(&payload.runtimes)?;
    Ok(payload)
}

/// Each licence, the URL and size of each artifact, and no two artifacts for
/// one name, version and platform.
fn check_artifacts(runtimes: &[Runtime]) -> Result<()> {
    let mut seen = HashSet::new();
    for runtime in runtimes {
        let license = &runtime.license;
        if license.is_empty()
            || license.len() > MAX_LICENSE_BYTES
            || license.chars().any(char::is_control)
        {
            return Err(invalid(format!(
                "{} {} has a licence that is not 1 to {MAX_LICENSE_BYTES} bytes without \
                 control characters",
                runtime.name, runtime.version
            )));
        }
        for artifact in &runtime.artifacts {
            let platform = Platform {
                os: artifact.os,
                arch: artifact.arch,
            };
            // The URL is never echoed: it may carry a token.
            if artifact.url.len() > MAX_URL_BYTES {
                return Err(invalid(format!(
                    "{} {} for {platform} has a URL over {MAX_URL_BYTES} bytes",
                    runtime.name, runtime.version
                )));
            }
            if let Err(error) = parse_strict_url(&artifact.url) {
                return Err(invalid(format!(
                    "{} {} for {platform} has a URL the downloader refuses: {error}",
                    runtime.name, runtime.version
                )));
            }
            if !(1..=MAX_ARTIFACT_BYTES).contains(&artifact.size) {
                return Err(invalid(format!(
                    "{} {} for {platform} has size {}, not 1 to {MAX_ARTIFACT_BYTES} bytes",
                    runtime.name, runtime.version, artifact.size
                )));
            }
            // Versions are ASCII, so ASCII lower case folds every case.
            let key = (
                runtime.name.as_str(),
                runtime.version.as_str().to_ascii_lowercase(),
                platform,
            );
            if !seen.insert(key) {
                return Err(invalid(format!(
                    "{} {} has more than one artifact for {platform} (versions compare \
                     ignoring case)",
                    runtime.name, runtime.version
                )));
            }
        }
    }
    Ok(())
}

fn invalid(reason: String) -> Error {
    Error::CatalogInvalid { reason }
}

/// A runtime name: `[a-z0-9][a-z0-9-]{0,31}` and not a Windows device name,
/// the same rule as a [`ServiceId`]. It becomes a folder name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RuntimeName(String);

impl RuntimeName {
    /// Checks `name`; [`Error::CatalogInvalid`] when it breaks the rule.
    pub fn new(name: impl Into<String>) -> Result<Self> {
        let name = name.into();
        match ServiceId::new(name.clone()) {
            Ok(_) => Ok(Self(name)),
            Err(_) => Err(Error::CatalogInvalid {
                reason: format!(
                    "{name:?} is not a runtime name (1 to 32 of a-z, 0-9 and '-', not starting \
                     with '-', and not a Windows device name)"
                ),
            }),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A runtime version: `[0-9A-Za-z][0-9A-Za-z.+_-]{0,63}`, with no `..`, no
/// trailing dot, and no Windows reserved name as its stem (`CON`, `nul.1`).
/// It becomes a folder name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RuntimeVersion(String);

impl RuntimeVersion {
    /// Checks `version`; [`Error::CatalogInvalid`] when it breaks the rule.
    pub fn new(version: impl Into<String>) -> Result<Self> {
        let version = version.into();
        if is_valid_version(&version) {
            Ok(Self(version))
        } else {
            Err(Error::CatalogInvalid {
                reason: format!(
                    "{version:?} is not a runtime version (1 to 64 of A-Z, a-z, 0-9 and \
                     '.+_-', starting with a letter or digit, with no '..', no trailing '.', \
                     and not a Windows device name)"
                ),
            })
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Every allowed byte is ASCII, so any other character fails the byte checks.
fn is_valid_version(version: &str) -> bool {
    let Some((first, rest)) = version.as_bytes().split_first() else {
        return false;
    };
    version.len() <= MAX_VERSION_BYTES
        && first.is_ascii_alphanumeric()
        && rest
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'+' | b'_' | b'-'))
        && !version.contains("..")
        && !version.ends_with('.')
        && !is_reserved_component(version)
}

/// A SHA-256 digest. In JSON it is 64 lower-case hex characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Sha256Digest([u8; 32]);

impl Sha256Digest {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The digest of `data`.
    pub fn of(data: &[u8]) -> Self {
        Self(Sha256::digest(data).into())
    }

    /// Reads 64 lower-case hex characters; [`Error::CatalogInvalid`] for
    /// anything else, upper case included.
    pub fn from_hex(hex: &str) -> Result<Self> {
        let invalid = || Error::CatalogInvalid {
            reason: format!("{hex:?} is not a SHA-256 digest (64 lower-case hex characters)"),
        };
        let (pairs, []) = hex.as_bytes().as_chunks::<2>() else {
            return Err(invalid());
        };
        if pairs.len() != 32 {
            return Err(invalid());
        }
        let mut bytes = [0; 32];
        for (byte, [high, low]) in bytes.iter_mut().zip(pairs) {
            let (Some(high), Some(low)) = (hex_value(*high), hex_value(*low)) else {
                return Err(invalid());
            };
            *byte = high << 4 | low;
        }
        Ok(Self(bytes))
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

fn hex_value(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        _ => None,
    }
}

impl fmt::Display for Sha256Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

/// The operating systems of the v1 computers (D17).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Os {
    Linux,
    Macos,
    Windows,
}

impl Os {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Linux => "linux",
            Self::Macos => "macos",
            Self::Windows => "windows",
        }
    }
}

/// The CPUs of the v1 computers, and Linux and Windows on ARM later (D17).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Arch {
    #[serde(rename = "x86_64")]
    X86_64,
    #[serde(rename = "aarch64")]
    Aarch64,
}

impl Arch {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64",
            Self::Aarch64 => "aarch64",
        }
    }
}

/// An OS and CPU: what an artifact is built for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Platform {
    pub os: Os,
    pub arch: Arch,
}

impl Platform {
    /// The computer this binary was built for; `None` on any other OS or CPU.
    pub const fn current() -> Option<Self> {
        let os = if cfg!(target_os = "linux") {
            Os::Linux
        } else if cfg!(target_os = "macos") {
            Os::Macos
        } else if cfg!(windows) {
            Os::Windows
        } else {
            return None;
        };
        let arch = if cfg!(target_arch = "x86_64") {
            Arch::X86_64
        } else if cfg!(target_arch = "aarch64") {
            Arch::Aarch64
        } else {
            return None;
        };
        Some(Self { os, arch })
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.os.as_str(), self.arch.as_str())
    }
}

/// How an artifact is packed. Every other format refuses the catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ArchiveFormat {
    #[serde(rename = "tar.gz")]
    TarGz,
    #[serde(rename = "zip")]
    Zip,
}

impl fmt::Display for RuntimeName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl fmt::Display for RuntimeVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl TryFrom<String> for RuntimeName {
    type Error = Error;

    fn try_from(name: String) -> Result<Self> {
        Self::new(name)
    }
}

impl TryFrom<String> for RuntimeVersion {
    type Error = Error;

    fn try_from(version: String) -> Result<Self> {
        Self::new(version)
    }
}

impl TryFrom<String> for Sha256Digest {
    type Error = Error;

    fn try_from(hex: String) -> Result<Self> {
        Self::from_hex(&hex)
    }
}

impl From<RuntimeName> for String {
    fn from(name: RuntimeName) -> Self {
        name.0
    }
}

impl From<RuntimeVersion> for String {
    fn from(version: RuntimeVersion) -> Self {
        version.0
    }
}

impl From<Sha256Digest> for String {
    fn from(digest: Sha256Digest) -> Self {
        digest.to_string()
    }
}
