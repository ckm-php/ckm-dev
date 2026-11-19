//! What a service is: how to start it, where it listens, and how to stop it.

use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// How long a start waits for the port to accept connections, by default.
pub const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(15);

/// How long a stop waits after the graceful step, by default.
pub const DEFAULT_STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// A service id: `[a-z0-9][a-z0-9-]{0,31}`, and not a Windows device name
/// (`con`, `prn`, `aux`, `nul`, `com0`–`com9`, `lpt0`–`lpt9`).
///
/// It becomes a file name (`run/services/<id>.json`, `logs/services/<id>.log`),
/// so every constructor checks it, including deserialization. Device names are
/// refused on every OS, so a data folder stays portable.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ServiceId(String);

impl ServiceId {
    /// Checks `id`; [`Error::InvalidServiceId`] when it breaks the pattern.
    pub fn new(id: impl Into<String>) -> Result<Self> {
        let id = id.into();
        if is_valid_id(&id) {
            Ok(Self(id))
        } else {
            Err(Error::InvalidServiceId(id))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// `[a-z0-9][a-z0-9-]{0,31}`, and not a device name. Every allowed byte is
/// ASCII, so any other character fails the byte checks.
fn is_valid_id(id: &str) -> bool {
    let Some((first, rest)) = id.as_bytes().split_first() else {
        return false;
    };
    rest.len() <= 31
        && (first.is_ascii_lowercase() || first.is_ascii_digit())
        && rest
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
        && !is_windows_device_name(id)
}

/// Windows reserves these names for devices, so they are refused on every OS to
/// keep record and log file names portable. An id is already lower case, so
/// the comparison needs no case folding.
fn is_windows_device_name(id: &str) -> bool {
    match id.as_bytes() {
        b"con" | b"prn" | b"aux" | b"nul" => true,
        [b'c', b'o', b'm', digit] | [b'l', b'p', b't', digit] => digit.is_ascii_digit(),
        _ => false,
    }
}

impl TryFrom<String> for ServiceId {
    type Error = Error;

    fn try_from(id: String) -> Result<Self> {
        Self::new(id)
    }
}

impl From<ServiceId> for String {
    fn from(id: ServiceId) -> Self {
        id.0
    }
}

impl fmt::Display for ServiceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// One service the supervisor can run.
///
/// The IPC never carries any of this: requests name a registered `id` only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceSpec {
    pub id: ServiceId,
    /// Absolute path of the program. Never looked up on `PATH`.
    pub program: PathBuf,
    /// Passed as a list, never through a shell.
    pub args: Vec<OsString>,
    /// Added after the cleared environment's short allow-list.
    pub env: Vec<(OsString, OsString)>,
    /// The loopback port the service listens on. Checked free before each
    /// start, never changed.
    pub port: u16,
    pub stop: StopMethod,
    /// A protected service is never force-killed: a stop that times out
    /// returns `stop_failed` and leaves it running.
    pub protected: bool,
    pub ready_timeout: Duration,
    pub stop_timeout: Duration,
}

impl ServiceSpec {
    /// Checks the rules a spec must meet before the manager accepts it.
    ///
    /// The programs (service and stop command) are absolute, the port is not 0,
    /// and on Windows the stop method is a command, because Windows has no
    /// signals. Both timeouts are above 0. A broken rule gives
    /// [`Error::InvalidSpec`].
    pub fn validate(&self) -> Result<()> {
        let reason = if !self.program.is_absolute() {
            format!(
                "the program {} is not an absolute path",
                self.program.display()
            )
        } else if self.port == 0 {
            "the port is 0".to_owned()
        } else if self.ready_timeout.is_zero() {
            "the ready timeout is 0".to_owned()
        } else if self.stop_timeout.is_zero() {
            "the stop timeout is 0".to_owned()
        } else {
            match &self.stop {
                StopMethod::Signal(_) if cfg!(windows) => {
                    "Windows has no signals, so it needs a stop command".to_owned()
                }
                StopMethod::Command { program, .. } if !program.is_absolute() => format!(
                    "the stop command {} is not an absolute path",
                    program.display()
                ),
                StopMethod::Signal(_) | StopMethod::Command { .. } => return Ok(()),
            }
        };
        Err(Error::InvalidSpec {
            service: self.id.to_string(),
            reason,
        })
    }
}

/// The graceful step of a stop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopMethod {
    /// Send this signal to the process-group leader. Unix only.
    Signal(StopSignal),
    /// Run this command and wait for the service to exit, as `nginx -s quit`
    /// will be. Any OS.
    Command {
        /// Absolute path, never looked up on `PATH`.
        program: PathBuf,
        args: Vec<OsString>,
    },
}

/// A signal that asks a service to exit. `SIGKILL` is not one: force-killing
/// is the manager's last step, never the graceful one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopSignal {
    Term,
    Quit,
    Int,
}

#[cfg(unix)]
impl From<StopSignal> for ck_sys::unix::Signal {
    fn from(signal: StopSignal) -> Self {
        match signal {
            StopSignal::Term => Self::Term,
            StopSignal::Quit => Self::Quit,
            StopSignal::Int => Self::Int,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_ids_follow_the_file_name_pattern() {
        let longest = "a".repeat(32);
        let accepted = [
            "a",
            "0",
            "web",
            "php-8-2",
            "a-",
            longest.as_str(),
            // Only the exact device names are refused.
            "com10",
            "console",
            "nul-1",
            "auxiliary",
        ];
        for id in accepted {
            assert!(ServiceId::new(id).is_ok(), "should accept {id:?}");
        }

        let too_long = "a".repeat(33);
        let numbered_devices = ["com", "lpt"]
            .into_iter()
            .flat_map(|device| (0..=9).map(move |n| format!("{device}{n}")));
        let devices = ["con", "prn", "aux", "nul"]
            .into_iter()
            .map(str::to_owned)
            .chain(numbered_devices);
        let rejected = [
            "",
            "-a",
            "Web",
            "a_b",
            "a.b",
            "a/b",
            "a\\b",
            "..",
            "a b",
            "caf\u{e9}",
            too_long.as_str(),
        ]
        .into_iter()
        .map(str::to_owned)
        .chain(devices);
        for id in rejected {
            assert!(
                matches!(ServiceId::new(id.as_str()), Err(Error::InvalidServiceId(_))),
                "should reject {id:?}"
            );
        }

        assert!(serde_json::from_str::<ServiceId>(r#""../x""#).is_err());
        assert!(serde_json::from_str::<ServiceId>(r#""nul""#).is_err());
    }
}
