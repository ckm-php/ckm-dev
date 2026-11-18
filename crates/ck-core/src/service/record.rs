//! Identity records: `run/services/<id>.json`, one per running service.
//!
//! A record is written atomically right after the spawn and deleted after the
//! service stops. A new supervisor reads them to adopt services that outlived
//! the old one (Unix); on Windows the services died with the Job, so every
//! record is deleted.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::layout;
use crate::paths::Paths;
use crate::service::identity::ProcessIdentity;
use crate::service::spec::ServiceId;

/// The record format version.
pub const RECORD_SCHEMA: u32 = 1;

/// The contents of `run/services/<id>.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceRecord {
    /// [`RECORD_SCHEMA`]; any other value is discarded.
    pub schema: u32,
    pub service: ServiceId,
    pub port: u16,
    pub pid: u32,
    /// The identity seen at spawn. Always set on Unix; `None` on Windows,
    /// which never adopts. A Unix record without it is never adopted.
    pub identity: Option<ProcessIdentity>,
}

/// One file found in `run/services/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScannedRecord {
    pub file: PathBuf,
    /// `None` when the file is unreadable, not valid JSON for
    /// [`ServiceRecord`], or of another schema.
    pub record: Option<ServiceRecord>,
}

/// Writes the record for `record.service` atomically, 0600.
pub fn write(paths: &Paths, record: &ServiceRecord) -> Result<()> {
    let json = serde_json::to_vec_pretty(record).map_err(|error| {
        Error::Internal(format!(
            "cannot encode the record of {}: {error}",
            record.service
        ))
    })?;
    layout::write_atomic(&paths.record_file(&record.service), &json)
}

/// Deletes the record for `id`. A missing file is not an error.
pub fn remove(paths: &Paths, id: &ServiceId) -> Result<()> {
    delete(&paths.record_file(id))
}

/// Lists every file in `run/services/`, parsed where possible.
///
/// Folders are skipped. A file is a valid record only when it is a regular
/// file (links are never followed) named `<service>.json` after the record
/// inside it; anything else, such as a temporary file left by an interrupted
/// write, comes back with `record: None` so that adoption deletes it.
pub fn scan(paths: &Paths) -> Result<Vec<ScannedRecord>> {
    let dir = paths.records_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(Error::io("read", dir, error)),
    };
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| Error::io("read", &dir, error))?;
        let file_type = entry.file_type().ok();
        if file_type.is_some_and(|file_type| file_type.is_dir()) {
            continue;
        }
        let file = entry.path();
        let record = if file_type.is_some_and(|file_type| file_type.is_file()) {
            parse(&file)
        } else {
            None
        };
        found.push(ScannedRecord { file, record });
    }
    found.sort_by(|a, b| a.file.cmp(&b.file));
    Ok(found)
}

/// Deletes one file in `run/services/`. A missing file is not an error.
pub(super) fn delete(file: &Path) -> Result<()> {
    match fs::remove_file(file) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Error::io("delete", file, error)),
    }
}

fn parse(file: &Path) -> Option<ServiceRecord> {
    let record: ServiceRecord = serde_json::from_slice(&fs::read(file).ok()?).ok()?;
    let own_name = format!("{}.json", record.service);
    (record.schema == RECORD_SCHEMA && file.file_name() == Some(OsStr::new(&own_name)))
        .then_some(record)
}
