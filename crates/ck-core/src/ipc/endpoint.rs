//! Where the supervisor for a data folder listens.
//!
//! The endpoint is a pure function of the canonical data folder and the user,
//! so the supervisor and every client derive the same one.
//!
//! - **Unix:** `<data>/run/ck.sock` when that path is at most
//!   [`MAX_SOCKET_PATH_BYTES`] bytes. Otherwise
//!   `/tmp/ck-dev-<uid>-<24 hex of SHA-256>/ck.sock`; the server creates that
//!   folder 0700 and checks it with `lstat` (a real directory, owned by the
//!   user, mode 0700). The base is fixed at `/tmp`, never read from the
//!   environment.
//! - **Windows:** `\\.\pipe\ck-dev-<SID>-<32 hex of SHA-256 of the lower-cased
//!   path>`.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::branding::DATA_DIR_NAME;
use crate::error::{Error, Result};

/// Longest socket path used inside the data folder (macOS allows 103 bytes,
/// Linux 107).
#[cfg(unix)]
pub const MAX_SOCKET_PATH_BYTES: usize = 100;

/// Base of the fallback socket folder for long data paths.
#[cfg(unix)]
pub const FALLBACK_BASE: &str = "/tmp";

/// The socket's file name, inside `run/` or the fallback folder.
#[cfg(unix)]
const SOCKET_FILE: &str = "ck.sock";

/// The supervisor's address for one data folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// The canonical data folder: the hash input, and what `hello` must report.
    pub data: PathBuf,
    /// The socket path (Unix) or the pipe path `\\.\pipe\…` (Windows).
    pub address: PathBuf,
    /// Unix only: the private `/tmp` folder that holds the socket when the
    /// data path is too long. Always `None` on Windows.
    pub fallback_dir: Option<PathBuf>,
}

impl Endpoint {
    /// The endpoint for a canonical data folder and a user id. Pure.
    #[cfg(unix)]
    pub fn derive(canonical_data: PathBuf, uid: u32) -> Self {
        use std::os::unix::ffi::OsStrExt;

        use crate::layout::RUN_DIR;

        let in_data = canonical_data.join(RUN_DIR).join(SOCKET_FILE);
        if in_data.as_os_str().as_bytes().len() <= MAX_SOCKET_PATH_BYTES {
            return Self {
                data: canonical_data,
                address: in_data,
                fallback_dir: None,
            };
        }

        let hash = sha256_hex_prefix(canonical_data.as_os_str().as_bytes(), 12);
        let dir = Path::new(FALLBACK_BASE).join(format!("{DATA_DIR_NAME}-{uid}-{hash}"));
        Self {
            data: canonical_data,
            address: dir.join(SOCKET_FILE),
            fallback_dir: Some(dir),
        }
    }

    /// The endpoint for a canonical data folder and a user SID string. Pure.
    #[cfg(windows)]
    pub fn derive(canonical_data: PathBuf, sid: &str) -> Self {
        // Windows paths ignore case, so two spellings of one folder must name
        // one pipe. The hash input is the lower-cased path as UTF-8.
        let lowered = canonical_data.to_string_lossy().to_lowercase();
        let hash = sha256_hex_prefix(lowered.as_bytes(), 16);
        Self {
            data: canonical_data,
            address: PathBuf::from(format!(r"\\.\pipe\{DATA_DIR_NAME}-{sid}-{hash}")),
            fallback_dir: None,
        }
    }

    /// The endpoint for the current user and the data folder `data`.
    ///
    /// `Ok(None)` when the data folder does not exist yet, which means no
    /// supervisor is running. Creates nothing.
    pub fn locate(data: &Path) -> Result<Option<Self>> {
        let canonical = match std::fs::canonicalize(data) {
            Ok(canonical) => canonical,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Error::io("resolve", data, e)),
        };
        sys::for_current_user(canonical).map(Some)
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.address.display().fmt(f)
    }
}

/// The first `bytes` bytes of the SHA-256 of `input`, as lower-case hex.
fn sha256_hex_prefix(input: &[u8], bytes: usize) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";

    let digest = Sha256::digest(input);
    let mut hex = String::with_capacity(bytes * 2);
    for byte in digest.iter().take(bytes) {
        hex.push(char::from(DIGITS[usize::from(byte >> 4)]));
        hex.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    hex
}

#[cfg(unix)]
use self::unix as sys;
#[cfg(windows)]
use self::windows as sys;

#[cfg(unix)]
mod unix {
    use std::path::PathBuf;

    use super::Endpoint;
    use crate::error::Result;

    pub(super) fn for_current_user(canonical_data: PathBuf) -> Result<Endpoint> {
        Ok(Endpoint::derive(
            canonical_data,
            ck_sys::unix::effective_uid(),
        ))
    }
}

#[cfg(windows)]
mod windows {
    use std::path::PathBuf;

    use super::Endpoint;
    use crate::error::{Error, Result};

    pub(super) fn for_current_user(canonical_data: PathBuf) -> Result<Endpoint> {
        let sid = ck_sys::windows::current_user_sid()
            .map_err(|e| Error::os("read the current user's SID", e))?;
        Ok(Endpoint::derive(canonical_data, &sid))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// `/aaa…` of `len` bytes.
    fn data_of_len(len: usize, fill: &str) -> PathBuf {
        PathBuf::from(format!("/{}", fill.repeat(len - 1)))
    }

    #[test]
    fn the_socket_stays_in_run_up_to_the_limit() {
        // "/run/ck.sock" adds 12 bytes.
        let data = data_of_len(MAX_SOCKET_PATH_BYTES - 12, "a");
        let endpoint = Endpoint::derive(data.clone(), 1000);
        assert_eq!(endpoint.address, data.join("run").join("ck.sock"));
        assert_eq!(endpoint.address.as_os_str().len(), MAX_SOCKET_PATH_BYTES);
        assert_eq!(endpoint.fallback_dir, None);
        assert_eq!(endpoint.data, data);
    }

    #[test]
    fn a_long_data_path_takes_the_private_tmp_folder() {
        let data = data_of_len(MAX_SOCKET_PATH_BYTES - 11, "a");
        let endpoint = Endpoint::derive(data.clone(), 1000);
        assert_eq!(endpoint.data, data);

        let dir = endpoint.fallback_dir.clone().unwrap();
        assert_eq!(dir.parent(), Some(Path::new("/tmp")));
        assert_eq!(endpoint.address, dir.join("ck.sock"));
        let name = dir.file_name().unwrap().to_str().unwrap();
        let hash = name
            .strip_prefix(&format!("{DATA_DIR_NAME}-1000-"))
            .unwrap();
        assert_eq!(hash.len(), 24);
        assert!(hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));

        // The same folder and user give the same endpoint; another folder or
        // user gives another one.
        assert_eq!(Endpoint::derive(data.clone(), 1000), endpoint);
        assert_ne!(Endpoint::derive(data, 1001).address, endpoint.address);
        let other = data_of_len(MAX_SOCKET_PATH_BYTES - 11, "b");
        assert_ne!(Endpoint::derive(other, 1000).address, endpoint.address);
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn a_pipe_name_ignores_case_and_names_the_user() {
        let sid = "S-1-5-21-1-2-3-1001";
        let endpoint = Endpoint::derive(PathBuf::from(r"C:\Users\Dev\AppData\Local\ck-dev"), sid);
        let same = Endpoint::derive(PathBuf::from(r"c:\users\dev\appdata\local\CK-DEV"), sid);
        assert_eq!(endpoint.address, same.address);
        assert_eq!(endpoint.fallback_dir, None);

        let name = endpoint.address.to_str().unwrap();
        let hash = name
            .strip_prefix(&format!(r"\\.\pipe\{DATA_DIR_NAME}-{sid}-"))
            .unwrap();
        assert_eq!(hash.len(), 32);
    }
}
