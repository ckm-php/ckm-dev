//! Which catalog was accepted last (`state/catalog-state.json`), and the lock
//! that lets one accept at a time compare and write it (`state/catalog.lock`).

use std::cmp::Ordering;
use std::fs::{self, File, TryLockError};
use std::io;
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::catalog::schema::Sha256Digest;
use crate::error::{Error, Result};
use crate::layout;
use crate::paths::Paths;

/// How long [`CatalogLock::acquire`] retries before giving up.
pub const LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// The pause between two tries of the lock.
const LOCK_RETRY: Duration = Duration::from_millis(20);

/// `state/catalog-state.json`: `{"sequence": 7, "payload_sha256": "<64 hex>"}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogState {
    pub sequence: u64,
    /// The SHA-256 of the decoded payload bytes.
    pub payload_sha256: Sha256Digest,
}

/// Whether an offered catalog replaces the stored one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Write the catalog and the new state.
    Store,
    /// The same catalog as the stored one: keep the state. `accept` repairs
    /// the cached catalog, but never replaces one with a higher sequence.
    Unchanged,
}

/// Compares an offered catalog with the stored state:
///
/// | Stored | Offered | Result |
/// |---|---|---|
/// | any | sequence below `min_sequence` | [`Error::CatalogOutdated`] |
/// | none (first use) | sequence at least `min_sequence` | [`Decision::Store`] |
/// | any | lower sequence | [`Error::CatalogOutdated`] |
/// | any | same sequence and payload SHA-256 | [`Decision::Unchanged`] |
/// | any | same sequence, other payload | [`Error::CatalogConflict`] |
/// | any | higher sequence | [`Decision::Store`] |
///
/// The floor holds after first use too: an app whose floor was raised above
/// the stored sequence accepts nothing below the floor.
///
/// A state file that cannot be read never gets here: [`read`] fails instead.
pub fn decide(
    stored: Option<&CatalogState>,
    offered: &CatalogState,
    min_sequence: u64,
) -> Result<Decision> {
    let newest = stored.map_or(min_sequence, |stored| stored.sequence.max(min_sequence));
    let outdated = || Error::CatalogOutdated {
        offered: offered.sequence,
        current: newest,
    };
    if offered.sequence < min_sequence {
        return Err(outdated());
    }
    let Some(stored) = stored else {
        return Ok(Decision::Store);
    };
    match offered.sequence.cmp(&stored.sequence) {
        Ordering::Less => Err(outdated()),
        Ordering::Equal if offered.payload_sha256 == stored.payload_sha256 => {
            Ok(Decision::Unchanged)
        }
        Ordering::Equal => Err(Error::CatalogConflict {
            sequence: stored.sequence,
        }),
        Ordering::Greater => Ok(Decision::Store),
    }
}

/// Reads the state. `Ok(None)` only when the file does not exist; a file that
/// cannot be read or parsed is an error ([`Error::Io`] or
/// [`Error::BadFile`]), never first use.
pub fn read(paths: &Paths) -> Result<Option<CatalogState>> {
    let path = paths.catalog_state_file();
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::io("read", path, e)),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|source| Error::BadFile { path, source })
}

/// Replaces the state with `layout::write_atomic`.
pub fn write(paths: &Paths, state: &CatalogState) -> Result<()> {
    let json = serde_json::to_vec_pretty(state)
        .map_err(|error| Error::Internal(format!("cannot encode the catalog state: {error}")))?;
    layout::write_atomic(&paths.catalog_state_file(), &json)
}

/// Holds `state/catalog.lock` until dropped. The file stays empty.
#[derive(Debug)]
pub struct CatalogLock {
    _file: File,
}

impl CatalogLock {
    /// Opens (creating 0600) `state/catalog.lock` and retries `try_lock`
    /// until it succeeds, or gives [`Error::Timeout`] after [`LOCK_TIMEOUT`].
    pub fn acquire(paths: &Paths) -> Result<Self> {
        let path = paths.catalog_lock_file();
        // Creating needs write access, and Windows locks only handles opened
        // for reading or writing. Nothing is ever written.
        let file = layout::private_file_options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| Error::io("open", &path, e))?;
        let start = Instant::now();
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self { _file: file }),
                Err(TryLockError::WouldBlock) if start.elapsed() < LOCK_TIMEOUT => {
                    thread::sleep(LOCK_RETRY);
                }
                Err(TryLockError::WouldBlock) => return Err(Error::Timeout("the catalog lock")),
                Err(TryLockError::Error(e)) => return Err(Error::io("lock", &path, e)),
            }
        }
    }
}
