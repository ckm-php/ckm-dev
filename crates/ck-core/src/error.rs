//! The one error type of `ck-core`, the stable error codes it maps to, and the
//! `{code, message}` body that carries them over IPC and in `--json` output.

use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Result with [`Error`] as the default error.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Every failure `ck-core` reports. [`Error::code`] gives its stable code.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A `CK_DEV_*` test-profile variable has a value that is not allowed (D21).
    #[error("{var} {reason}")]
    BadOverride {
        var: &'static str,
        reason: &'static str,
    },

    /// The OS did not report a folder that the layout needs, such as "home".
    #[error("cannot find this user's {0} folder")]
    NoUserFolder(&'static str),

    /// A file-system call on `path` failed.
    #[error("cannot {op} {}: {source}", path.display())]
    Io {
        op: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// An OS call that has no path failed.
    #[error("cannot {op}: {source}")]
    Os {
        op: &'static str,
        #[source]
        source: io::Error,
    },

    /// A folder or file that must belong only to this user does not.
    #[error("{} is not private: {reason}", path.display())]
    NotPrivate { path: PathBuf, reason: String },

    /// A JSON file in the data folder could not be read as its schema.
    #[error("{} is not valid: {source}", path.display())]
    BadFile {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    /// An IPC frame announced more bytes than the limit for its direction.
    #[error("a message of {len} bytes is over the {limit}-byte limit")]
    FrameTooLarge { len: u32, limit: u32 },

    /// The connection closed after part of a frame.
    #[error("the connection closed in the middle of a message")]
    FrameTruncated,

    /// An IPC frame is not valid JSON for the expected message.
    #[error("malformed message: {0}")]
    Malformed(#[source] serde_json::Error),

    /// Waiting for something took longer than its limit.
    #[error("timed out waiting for {0}")]
    Timeout(&'static str),

    /// The other end of an IPC connection runs as a different user.
    #[error("the other end of the connection runs as a different user")]
    PeerRejected,

    /// No supervisor is reachable, or one could not be started.
    #[error("the supervisor is not available: {detail}")]
    SupervisorUnavailable { detail: String },

    /// The supervisor speaks another protocol version.
    #[error(
        "the running supervisor speaks protocol {theirs}, but this command speaks {ours}; \
         stop the supervisor and try again"
    )]
    VersionMismatch { ours: u32, theirs: u32 },

    /// The supervisor at our endpoint serves a different data folder.
    #[error(
        "the supervisor at this endpoint serves {}, not {}",
        actual.display(),
        expected.display()
    )]
    WrongSupervisor { expected: PathBuf, actual: PathBuf },

    /// The supervisor answered with an error; `code` is its code.
    #[error("{message}")]
    Remote { code: ErrorCode, message: String },

    /// A service id that cannot be used as a file name.
    #[error(
        "'{0}' is not a valid service id (1 to 32 of a-z, 0-9 and '-', not starting with '-', \
         and not a Windows device name such as 'con' or 'com1')"
    )]
    InvalidServiceId(String),

    /// A service definition that breaks a rule, such as a relative program path.
    #[error("service {service}: {reason}")]
    InvalidSpec { service: String, reason: String },

    /// No registered service has this id.
    #[error("unknown service '{0}'")]
    UnknownService(String),

    /// The service's port is taken. Ports are never reassigned.
    #[error("{service} cannot start: port {port} is already in use")]
    PortInUse { service: String, port: u16 },

    /// The service did not become ready, or exited while starting.
    #[error("{service} failed to start: {reason}")]
    StartFailed { service: String, reason: String },

    /// The service is still running after its stop timeout.
    #[error("{service} did not stop: {reason}")]
    StopFailed { service: String, reason: String },

    /// Another start or stop of the same service is in progress.
    #[error("{service} is busy with another start or stop; try again")]
    Busy { service: String },

    /// The process at a recorded pid is not the one the record describes.
    #[error("process {pid} is not the recorded process: {reason}")]
    IdentityMismatch { pid: u32, reason: String },

    /// A catalog that breaks an envelope or schema rule: its size, format,
    /// an unknown field or a field's value.
    #[error("the catalog is not valid: {reason}")]
    CatalogInvalid { reason: String },

    /// A catalog that no trusted key signed, including when no key is trusted.
    #[error("the catalog's signature is not trusted: {reason}")]
    CatalogUntrusted { reason: String },

    /// The catalog's `expires` time, in Unix seconds, has passed.
    #[error("the catalog expired at {expires} (Unix time); refresh it before installing")]
    CatalogExpired { expires: u64 },

    /// A catalog older than the newest one accepted, or than the app's floor.
    #[error("catalog sequence {offered} is older than {current}, the newest this app accepts")]
    CatalogOutdated { offered: u64, current: u64 },

    /// The accepted catalog's sequence, with different contents.
    #[error("catalog sequence {sequence} was already accepted with different contents")]
    CatalogConflict { sequence: u64 },

    /// The catalog has no artifact for this runtime version on this computer.
    #[error("the catalog has no {name} {version} for {platform}")]
    NotInCatalog {
        name: String,
        version: String,
        platform: String,
    },

    /// A URL that the transport policy refuses. `url` holds the scheme, host
    /// and path only, because redirect URLs carry signed tokens.
    #[error("{url} is not allowed: {reason}")]
    UrlRefused { url: String, reason: String },

    /// A request that failed, timed out or answered other than 200. `url` as
    /// in [`Error::UrlRefused`].
    #[error("the download of {url} failed: {reason}")]
    DownloadFailed { url: String, reason: String },

    /// A download that is not the pinned size: another `Content-Length`, a
    /// body cut short, or more bytes than pinned.
    #[error("{url} is not the {expected} bytes the catalog pins: {detail}")]
    SizeMismatch {
        url: String,
        expected: u64,
        detail: String,
    },

    /// A download whose SHA-256 is not the pinned one.
    #[error("{url} has SHA-256 {actual}, but the catalog pins {expected}")]
    ChecksumMismatch {
        url: String,
        expected: String,
        actual: String,
    },

    /// An archive that cannot be read, or that breaks a whole-archive rule
    /// such as the entry or size cap.
    #[error("the archive is refused: {reason}")]
    ArchiveRefused { reason: String },

    /// An archive entry that breaks a name, type or link rule, which refuses
    /// the whole archive. The name prints escaped: it comes from the archive.
    #[error("the archive is refused: entry {entry:?} {reason}")]
    ArchiveEntryRefused { entry: String, reason: String },

    /// Another install or uninstall of this runtime version holds its lock.
    #[error("{name} {version} is busy with another install or uninstall; try again")]
    RuntimeBusy { name: String, version: String },

    /// No folder exists for this runtime version.
    #[error("{name} {version} is not installed")]
    NotInstalled { name: String, version: String },

    /// Any other failure, with its context in the message.
    #[error("{0}")]
    Internal(String),
}

impl Error {
    /// A file-system error on `path`, for `map_err`.
    pub fn io(op: &'static str, path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            op,
            path: path.into(),
            source,
        }
    }

    /// An OS error with no path, for `map_err`.
    pub fn os(op: &'static str, source: io::Error) -> Self {
        Self::Os { op, source }
    }

    /// The stable code for this error, sent over IPC and mapped to an exit code.
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::SupervisorUnavailable { .. } => ErrorCode::SupervisorUnavailable,
            Self::UnknownService(_) => ErrorCode::UnknownService,
            Self::PortInUse { .. } => ErrorCode::PortInUse,
            Self::StartFailed { .. } => ErrorCode::StartFailed,
            Self::StopFailed { .. } => ErrorCode::StopFailed,
            Self::Busy { .. } => ErrorCode::Busy,
            Self::VersionMismatch { .. } => ErrorCode::VersionMismatch,
            Self::Remote { code, .. } => *code,
            Self::FrameTooLarge { .. } | Self::FrameTruncated | Self::Malformed(_) => {
                ErrorCode::BadRequest
            }
            Self::BadOverride { .. }
            | Self::NoUserFolder(_)
            | Self::Io { .. }
            | Self::Os { .. }
            | Self::NotPrivate { .. }
            | Self::BadFile { .. }
            | Self::Timeout(_)
            | Self::PeerRejected
            | Self::WrongSupervisor { .. }
            | Self::InvalidServiceId(_)
            | Self::InvalidSpec { .. }
            | Self::IdentityMismatch { .. }
            | Self::CatalogInvalid { .. }
            | Self::CatalogUntrusted { .. }
            | Self::CatalogExpired { .. }
            | Self::CatalogOutdated { .. }
            | Self::CatalogConflict { .. }
            | Self::NotInCatalog { .. }
            | Self::UrlRefused { .. }
            | Self::DownloadFailed { .. }
            | Self::SizeMismatch { .. }
            | Self::ChecksumMismatch { .. }
            | Self::ArchiveRefused { .. }
            | Self::ArchiveEntryRefused { .. }
            | Self::RuntimeBusy { .. }
            | Self::NotInstalled { .. }
            | Self::Internal(_) => ErrorCode::Internal,
        }
    }
}

/// Stable error codes: the `code` field over IPC and in `--json` output.
///
/// Exit code 0 means success and 2 is a usage error from argument parsing;
/// every other exit code comes from [`ErrorCode::exit_code`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Any failure without its own code. Exit 1.
    Internal,
    /// A request the supervisor could not read. Exit 1.
    BadRequest,
    /// Exit 3.
    SupervisorUnavailable,
    /// Exit 4.
    UnknownService,
    /// Exit 5.
    PortInUse,
    /// Exit 6.
    StartFailed,
    /// Exit 7.
    StopFailed,
    /// Exit 8.
    Busy,
    /// Exit 9.
    VersionMismatch,
}

impl ErrorCode {
    /// The process exit code for this error.
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::Internal | Self::BadRequest => 1,
            Self::SupervisorUnavailable => 3,
            Self::UnknownService => 4,
            Self::PortInUse => 5,
            Self::StartFailed => 6,
            Self::StopFailed => 7,
            Self::Busy => 8,
            Self::VersionMismatch => 9,
        }
    }
}

/// `{"code": …, "message": …}`: an error as the supervisor sends it and as
/// `--json` output prints it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorBody {
    pub code: ErrorCode,
    pub message: String,
}

impl From<&Error> for ErrorBody {
    fn from(error: &Error) -> Self {
        Self {
            code: error.code(),
            message: error.to_string(),
        }
    }
}

impl From<ErrorBody> for Error {
    fn from(body: ErrorBody) -> Self {
        Self::Remote {
            code: body.code,
            message: body.message,
        }
    }
}
