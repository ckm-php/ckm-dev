//! Unix process and user calls: the effective uid, sessions, and signals.

use std::fmt;
use std::io;

/// A process id that is safe to pass to `kill(2)` and `killpg(2)`.
///
/// `kill` gives special meaning to some values, so `Pid` refuses them:
/// 0 signals the caller's own process group, 1 is init, and anything above
/// `i32::MAX` becomes a negative `pid_t` (-1 signals every process the user
/// owns; other negative values signal a whole group).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Pid(u32);

impl Pid {
    /// Checks `raw` and wraps it. Refuses 0, 1 and values above `i32::MAX`.
    ///
    /// Safety contract: pure; every later FFI call relies on this check.
    pub fn new(raw: u32) -> Result<Self, InvalidPid> {
        match libc::pid_t::try_from(raw) {
            Ok(pid) if pid > 1 => Ok(Self(raw)),
            _ => Err(InvalidPid(raw)),
        }
    }

    /// The raw process id.
    pub fn get(self) -> u32 {
        self.0
    }

    /// The id as the `pid_t` that libc takes.
    pub(crate) fn as_raw(self) -> libc::pid_t {
        // Lossless: `new` keeps the value in `2..=i32::MAX`.
        self.0.cast_signed()
    }
}

impl TryFrom<u32> for Pid {
    type Error = InvalidPid;

    fn try_from(raw: u32) -> Result<Self, Self::Error> {
        Self::new(raw)
    }
}

impl fmt::Display for Pid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A value that [`Pid::new`] refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidPid(pub u32);

impl fmt::Display for InvalidPid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} is not a process id that may be signalled", self.0)
    }
}

impl std::error::Error for InvalidPid {}

/// The signals ckm-dev sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// `SIGTERM`: ask the process to exit.
    Term,
    /// `SIGQUIT`: graceful exit for servers that use it (Nginx, PHP-FPM).
    Quit,
    /// `SIGINT`: interrupt.
    Int,
    /// `SIGKILL`: cannot be caught. Never sent to a protected service.
    Kill,
}

impl Signal {
    fn number(self) -> libc::c_int {
        match self {
            Self::Term => libc::SIGTERM,
            Self::Quit => libc::SIGQUIT,
            Self::Int => libc::SIGINT,
            Self::Kill => libc::SIGKILL,
        }
    }
}

/// Whether a signal reached a process.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// The kernel accepted the signal.
    Sent,
    /// No such process or group exists (`ESRCH`).
    NoSuchProcess,
}

/// The effective user id of this process.
///
/// Safety contract: wraps `geteuid`, which takes no arguments, touches no
/// memory and cannot fail.
#[allow(unsafe_code, reason = "std has no call for the effective uid")]
pub fn effective_uid() -> u32 {
    // SAFETY: `geteuid` takes no arguments, touches no memory and cannot fail.
    unsafe { libc::geteuid() }
}

/// Makes this process the leader of a new session and process group, detached
/// from any controlling terminal (`setsid`).
///
/// Fails with `EPERM` when this process already leads a process group, which
/// is the case for a command typed in a shell; call it only in a child spawned
/// without its own group.
///
/// Safety contract: wraps `setsid`, which takes no arguments and touches no
/// memory; the error comes from `errno`.
#[allow(unsafe_code, reason = "std's `CommandExt::setsid` is unstable")]
pub fn new_session() -> io::Result<()> {
    // SAFETY: `setsid` takes no arguments and touches no memory.
    let session = unsafe { libc::setsid() };
    if session == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Sends `signal` to the single process `pid` (`kill(pid, sig)`).
///
/// The caller must have checked that `pid` is still the process it means:
/// an owned child that has not been reaped, or an adopted process whose
/// identity was re-checked immediately before.
///
/// Safety contract: wraps `kill`, which touches no memory. [`Pid`] guarantees a
/// positive `pid_t` other than 1, so the call can never reach a group, every
/// process of the user, or init.
#[allow(
    unsafe_code,
    reason = "std can only send SIGKILL, through `Child::kill`"
)]
pub fn signal_process(pid: Pid, signal: Signal) -> io::Result<Delivery> {
    let (target, number) = (pid.as_raw(), signal.number());
    // SAFETY: `kill` touches no memory, and `Pid` makes `target` a single
    // positive pid other than init.
    let result = unsafe { libc::kill(target, number) };
    delivery(result)
}

/// Sends `signal` to the process group whose leader is `pid` (`killpg`).
///
/// Services are spawned as leaders of their own group, so this reaches the
/// service and every child it started. The same identity rule as
/// [`signal_process`] applies to the leader.
///
/// Safety contract: wraps `killpg`, which touches no memory. [`Pid`] excludes
/// 0, which would signal the caller's own group, and 1.
#[allow(unsafe_code, reason = "std has no call that signals a process group")]
pub fn signal_group(pid: Pid, signal: Signal) -> io::Result<Delivery> {
    let (group, number) = (pid.as_raw(), signal.number());
    // SAFETY: `killpg` touches no memory, and `Pid` keeps `group` away from 0
    // (the caller's own group) and 1.
    let result = unsafe { libc::killpg(group, number) };
    delivery(result)
}

/// Maps the return value of `kill` or `killpg`; `ESRCH` means the target is gone.
fn delivery(result: libc::c_int) -> io::Result<Delivery> {
    if result == 0 {
        return Ok(Delivery::Sent);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(Delivery::NoSuchProcess)
    } else {
        Err(error)
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::process::Command;

    use super::*;

    #[test]
    fn pid_refuses_values_that_kill_treats_specially() {
        let too_large = u32::try_from(i32::MAX).unwrap() + 1;
        for raw in [0, 1, too_large, u32::MAX] {
            assert_eq!(Pid::new(raw), Err(InvalidPid(raw)));
        }
        let own = std::process::id();
        assert_eq!(Pid::new(own).map(Pid::get), Ok(own));
    }

    #[test]
    fn signals_reach_a_live_child_and_report_a_reaped_one_as_gone() {
        // A zombie still accepts signals, so "gone" is checked only after `wait`.
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = Pid::new(child.id()).unwrap();
        assert_eq!(signal_process(pid, Signal::Term).unwrap(), Delivery::Sent);
        assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGTERM));
        assert_eq!(
            signal_process(pid, Signal::Term).unwrap(),
            Delivery::NoSuchProcess
        );

        let mut leader = Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let group = Pid::new(leader.id()).unwrap();
        assert_eq!(signal_group(group, Signal::Term).unwrap(), Delivery::Sent);
        assert_eq!(leader.wait().unwrap().signal(), Some(libc::SIGTERM));
        assert_eq!(
            signal_group(group, Signal::Term).unwrap(),
            Delivery::NoSuchProcess
        );
    }
}
