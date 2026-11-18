//! Process identity: what tells one process from a later one with the same pid.
//!
//! - **Linux:** the start time from `/proc/<pid>/stat` (clock ticks since boot),
//!   the `/proc/<pid>/exe` link, and the boot id from
//!   `/proc/sys/kernel/random/boot_id`. Read with safe std code.
//! - **macOS:** `ck_sys::macos::process_start_time` (microseconds since the
//!   epoch) and `process_exe`. No boot id.
//! - **Windows:** not used. Services live and die with their Job, are never
//!   adopted, and are signalled only through Job handles.
//!
//! An adopted process is re-checked immediately before every signal. A
//! PID-reuse window of microseconds remains; pidfd is a later hardening.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[cfg(unix)]
use ck_sys::unix::Pid;

#[cfg(unix)]
use crate::error::{Error, Result};

/// The identity of one process, as seen at spawn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessIdentity {
    /// Opaque platform start time; compared for equality only.
    pub start_time: u64,
    pub exe: PathBuf,
    /// Linux only; `None` elsewhere.
    pub boot_id: Option<String>,
}

/// The identity of the live process `pid`; `Ok(None)` when it does not exist.
///
/// A process that has exited but is not yet reaped (a zombie) counts as gone
/// on Linux.
#[cfg(unix)]
pub fn read(pid: Pid) -> Result<Option<ProcessIdentity>> {
    os::read(pid)
}

/// Checks that `pid` is still the process `expected` describes. Call it
/// immediately before signalling an adopted process.
///
/// A missing process or any difference gives
/// [`Error::IdentityMismatch`](crate::Error::IdentityMismatch), and the caller
/// must not signal.
#[cfg(unix)]
pub fn verify(pid: Pid, expected: &ProcessIdentity) -> Result<()> {
    let mismatch = |reason: &str| Error::IdentityMismatch {
        pid: pid.get(),
        reason: reason.to_owned(),
    };
    let actual = match read(pid) {
        Ok(Some(actual)) => actual,
        Ok(None) => return Err(mismatch("it is no longer running")),
        // Every process we spawn runs as this user, and another user's
        // process hides its details from us, so it cannot be ours.
        Err(error) if is_permission_denied(&error) => {
            return Err(mismatch("it belongs to another user"));
        }
        Err(error) => return Err(error),
    };
    if actual.boot_id != expected.boot_id {
        return Err(mismatch("the computer has restarted since it was recorded"));
    }
    if actual.start_time != expected.start_time {
        return Err(mismatch("it started at a different time"));
    }
    if actual.exe != expected.exe {
        return Err(mismatch("it runs a different program"));
    }
    Ok(())
}

#[cfg(unix)]
fn is_permission_denied(error: &Error) -> bool {
    match error {
        Error::Io { source, .. } | Error::Os { source, .. } => {
            source.kind() == std::io::ErrorKind::PermissionDenied
        }
        _ => false,
    }
}

#[cfg(target_os = "linux")]
mod os {
    use std::fs;
    use std::io;
    use std::path::Path;

    use ck_sys::unix::Pid;

    use super::ProcessIdentity;
    use crate::error::{Error, Result};

    const BOOT_ID: &str = "/proc/sys/kernel/random/boot_id";

    pub(super) fn read(pid: Pid) -> Result<Option<ProcessIdentity>> {
        let dir = Path::new("/proc").join(pid.get().to_string());
        let Some(start_time) = start_ticks(&dir)? else {
            return Ok(None);
        };
        let exe_link = dir.join("exe");
        let exe = match fs::read_link(&exe_link) {
            Ok(exe) => exe,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(Error::io("read", exe_link, error)),
        };
        // A second look at the start time: if the process exited and its pid
        // was reused between the two reads, the executable is not its own.
        if start_ticks(&dir)? != Some(start_time) {
            return Ok(None);
        }
        let boot_id =
            fs::read_to_string(BOOT_ID).map_err(|error| Error::io("read", BOOT_ID, error))?;
        Ok(Some(ProcessIdentity {
            start_time,
            exe,
            boot_id: Some(boot_id.trim().to_owned()),
        }))
    }

    /// The start time in clock ticks since boot; `None` when the process is
    /// gone or a zombie.
    fn start_ticks(dir: &Path) -> Result<Option<u64>> {
        let path = dir.join("stat");
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(Error::io("read", path, error)),
        };
        let stat = parse_stat(&bytes).ok_or_else(|| {
            Error::Internal(format!("{} has an unexpected format", path.display()))
        })?;
        // A zombie has exited and only waits to be reaped.
        Ok((!matches!(stat.state, 'Z' | 'X')).then_some(stat.start_ticks))
    }

    #[derive(Debug, PartialEq, Eq)]
    struct Stat {
        state: char,
        start_ticks: u64,
    }

    /// Parses `/proc/<pid>/stat` (`proc_pid_stat(5)`). The command name
    /// (field 2) may hold spaces, `)` and bytes that are not UTF-8, so parsing
    /// starts after the last `)`: field 3 is the state, field 22 the start time.
    fn parse_stat(stat: &[u8]) -> Option<Stat> {
        let close = stat.iter().rposition(|&byte| byte == b')')?;
        let rest = std::str::from_utf8(stat.get(close + 1..)?).ok()?;
        let mut fields = rest.split_ascii_whitespace();
        let state = fields.next()?.chars().next()?;
        // Skip fields 4 to 21.
        let start_ticks = fields.nth(18)?.parse().ok()?;
        Some(Stat { state, start_ticks })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn stat_is_parsed_after_the_last_parenthesis() {
            let stat = b"4242 (a) (b) S 1 4242 4242 0 -1 4194304 120 0 0 0 1 0 0 0 20 0 1 0 \
                98765 2265088 200 18446744073709551615 1 1 0 0 0 0 0 0 0 0 0 0 17 2 0 0\n";
            assert_eq!(
                parse_stat(stat),
                Some(Stat {
                    state: 'S',
                    start_ticks: 98765
                })
            );

            assert_eq!(parse_stat(b"4242 (a) (b) S 1 4242"), None);
            assert_eq!(parse_stat(b"4242 no parenthesis S 1"), None);
        }
    }
}

#[cfg(target_os = "macos")]
mod os {
    use ck_sys::macos;
    use ck_sys::unix::Pid;

    use super::ProcessIdentity;
    use crate::error::{Error, Result};

    pub(super) fn read(pid: Pid) -> Result<Option<ProcessIdentity>> {
        let Some(start_time) = started_at(pid)? else {
            return Ok(None);
        };
        let exe = macos::process_exe(pid)
            .map_err(|error| Error::os("read the executable of a process", error))?;
        let Some(exe) = exe else {
            return Ok(None);
        };
        // A second look at the start time: if the process exited and its pid
        // was reused between the two reads, the executable is not its own.
        if started_at(pid)? != Some(start_time) {
            return Ok(None);
        }
        Ok(Some(ProcessIdentity {
            start_time,
            exe,
            boot_id: None,
        }))
    }

    fn started_at(pid: Pid) -> Result<Option<u64>> {
        macos::process_start_time(pid)
            .map_err(|error| Error::os("read the start time of a process", error))
    }
}

/// Other Unix systems are not targets (D17): nothing is ever adopted there.
#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
mod os {
    use ck_sys::unix::Pid;

    use super::ProcessIdentity;
    use crate::error::{Error, Result};

    pub(super) fn read(_pid: Pid) -> Result<Option<ProcessIdentity>> {
        Err(Error::Internal(
            "process identity is not supported on this OS".to_owned(),
        ))
    }
}
