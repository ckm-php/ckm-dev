//! macOS process identity through libproc (`proc_pidinfo`, `proc_pidpath`).
//!
//! Linux reads the same facts from `/proc` with safe std code in `ck-core`.

use std::ffi::OsStr;
use std::io;
use std::mem;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use crate::unix::Pid;

/// The start time of `pid`, in microseconds since the Unix epoch.
///
/// `Ok(None)` when no such process exists. Together with the executable path
/// it tells one process from a later one that reuses its pid.
///
/// Safety contract: calls `proc_pidinfo(PROC_PIDTBSDINFO)` with a zeroed
/// `proc_bsdinfo` owned by this function and its exact size, and reads the
/// struct only when the call reports that it filled all of it.
#[allow(unsafe_code, reason = "libproc has no safe wrapper in std or libc")]
pub fn process_start_time(pid: Pid) -> io::Result<Option<u64>> {
    // SAFETY: `proc_bsdinfo` holds only integers and integer arrays, for which
    // all-zero bytes are a valid value.
    let mut info: libc::proc_bsdinfo = unsafe { mem::zeroed() };
    let size =
        libc::c_int::try_from(mem::size_of::<libc::proc_bsdinfo>()).map_err(io::Error::other)?;
    let buffer = (&raw mut info).cast::<libc::c_void>();
    // SAFETY: `buffer` points at `info`, which this function owns and which is
    // exactly `size` bytes long.
    let written =
        unsafe { libc::proc_pidinfo(pid.as_raw(), libc::PROC_PIDTBSDINFO, 0, buffer, size) };
    if written <= 0 {
        return gone_or_error();
    }
    if written != size {
        return Err(io::Error::other(format!(
            "proc_pidinfo returned {written} of {size} bytes for pid {pid}"
        )));
    }
    let micros = info
        .pbi_start_tvsec
        .checked_mul(1_000_000)
        .and_then(|seconds| seconds.checked_add(info.pbi_start_tvusec))
        .ok_or_else(|| io::Error::other(format!("start time of pid {pid} is out of range")))?;
    Ok(Some(micros))
}

/// The absolute path of the executable that `pid` runs.
///
/// `Ok(None)` when no such process exists.
///
/// Safety contract: calls `proc_pidpath` with a buffer of
/// `PROC_PIDPATHINFO_MAXSIZE` bytes owned by this function, and reads only the
/// length the call returns.
#[allow(unsafe_code, reason = "libproc has no safe wrapper in std or libc")]
pub fn process_exe(pid: Pid) -> io::Result<Option<PathBuf>> {
    let capacity = u32::try_from(libc::PROC_PIDPATHINFO_MAXSIZE).map_err(io::Error::other)?;
    let mut path = vec![0_u8; usize::try_from(capacity).map_err(io::Error::other)?];
    let buffer = path.as_mut_ptr().cast::<libc::c_void>();
    // SAFETY: `buffer` points at `path`, which this function owns and which is
    // exactly `capacity` bytes long.
    let length = unsafe { libc::proc_pidpath(pid.as_raw(), buffer, capacity) };
    if length <= 0 {
        return gone_or_error();
    }
    let length = usize::try_from(length).map_err(io::Error::other)?;
    let Some(bytes) = path.get(..length) else {
        return Err(io::Error::other(format!(
            "proc_pidpath returned a length of {length} for pid {pid}"
        )));
    };
    Ok(Some(PathBuf::from(OsStr::from_bytes(bytes))))
}

/// Maps a failed libproc call: `ESRCH` means the process is gone.
fn gone_or_error<T>() -> io::Result<Option<T>> {
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(None)
    } else {
        Err(error)
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;
    use crate::unix::tests::Reap;
    use crate::unix::{Delivery, Signal, signal_process};

    #[test]
    fn identity_is_read_while_alive_and_none_once_reaped() {
        let mut child = Reap(Command::new("sleep").arg("30").spawn().unwrap());
        let pid = Pid::new(child.0.id()).unwrap();
        assert!(
            process_start_time(pid)
                .unwrap()
                .is_some_and(|micros| micros > 0)
        );
        let exe = process_exe(pid).unwrap().unwrap();
        assert!(exe.is_absolute() && exe.ends_with("sleep"), "{exe:?}");

        assert_eq!(signal_process(pid, Signal::Term).unwrap(), Delivery::Sent);
        assert!(!child.0.wait().unwrap().success());
        assert_eq!(process_start_time(pid).unwrap(), None);
        assert_eq!(process_exe(pid).unwrap(), None);
    }
}
