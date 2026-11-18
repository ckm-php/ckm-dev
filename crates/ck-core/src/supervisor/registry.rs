//! Which services the supervisor manages.
//!
//! Release builds register none until M3. Debug builds read
//! `state/dev-services.json`, and only in an isolated profile, so tests can run
//! the dummy service. The IPC never carries a program or arguments.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::paths::Paths;
use crate::service::spec::{ServiceId, ServiceSpec, StopSignal};

/// `state/dev-services.json`.
///
/// ```json
/// {"services": [{"id": "dummy", "program": "/abs/ck-test-service",
///   "args": ["--port", "47001"], "port": 47001, "stop": {"signal": "term"},
///   "protected": false, "ready_timeout_ms": 5000, "stop_timeout_ms": 2000}]}
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DevServicesFile {
    pub services: Vec<DevService>,
}

/// One entry of [`DevServicesFile`]. Every field is required.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DevService {
    pub id: ServiceId,
    /// Absolute path of the program.
    pub program: PathBuf,
    pub args: Vec<String>,
    pub port: u16,
    pub stop: DevStop,
    pub protected: bool,
    pub ready_timeout_ms: u64,
    pub stop_timeout_ms: u64,
}

/// `{"signal": "term"}` (Unix only) or `{"command": {"program": …, "args": […]}}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DevStop {
    Signal(StopSignal),
    Command(DevStopCommand),
}

/// The `command` form of [`DevStop`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DevStopCommand {
    /// Absolute path of the program.
    pub program: PathBuf,
    pub args: Vec<String>,
}

/// The services to register.
///
/// Empty in a release build, and in a debug build outside an isolated profile.
/// In a debug build with an isolated profile it reads `dev-services.json`
/// (a missing file means none) and validates each spec.
pub fn load(paths: &Paths) -> Result<Vec<ServiceSpec>> {
    let _ = paths;
    todo!()
}
