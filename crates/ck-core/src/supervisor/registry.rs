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
/// (a missing file means none) and validates each spec. A file that is not
/// valid JSON for [`DevServicesFile`] is an error.
pub fn load(paths: &Paths) -> Result<Vec<ServiceSpec>> {
    dev::load(paths)
}

/// The loader exists only in debug builds, so a release binary cannot read
/// programs from the data folder at all.
#[cfg(debug_assertions)]
mod dev {
    use std::ffi::OsString;
    use std::fs;
    use std::io;
    use std::time::Duration;

    use super::{DevService, DevServicesFile, DevStop};
    use crate::error::{Error, Result};
    use crate::paths::{Paths, Profile};
    use crate::service::spec::{ServiceSpec, StopMethod};

    pub(super) fn load(paths: &Paths) -> Result<Vec<ServiceSpec>> {
        if paths.profile != Profile::Isolated {
            return Ok(Vec::new());
        }
        let path = paths.dev_services_file();
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(Error::io("read", path, e)),
        };
        let file: DevServicesFile =
            serde_json::from_slice(&bytes).map_err(|source| Error::BadFile { path, source })?;
        file.services
            .into_iter()
            .map(|service| {
                let spec = into_spec(service);
                spec.validate()?;
                Ok(spec)
            })
            .collect()
    }

    /// The file has no `env`: test services get only the allow-list.
    fn into_spec(service: DevService) -> ServiceSpec {
        let stop = match service.stop {
            DevStop::Signal(signal) => StopMethod::Signal(signal),
            DevStop::Command(command) => StopMethod::Command {
                program: command.program,
                args: command.args.into_iter().map(OsString::from).collect(),
            },
        };
        ServiceSpec {
            id: service.id,
            program: service.program,
            args: service.args.into_iter().map(OsString::from).collect(),
            env: Vec::new(),
            port: service.port,
            stop,
            protected: service.protected,
            ready_timeout: Duration::from_millis(service.ready_timeout_ms),
            stop_timeout: Duration::from_millis(service.stop_timeout_ms),
        }
    }
}

/// Release builds register nothing until M3.
#[cfg(not(debug_assertions))]
mod dev {
    use crate::error::Result;
    use crate::paths::Paths;
    use crate::service::spec::ServiceSpec;

    pub(super) fn load(_paths: &Paths) -> Result<Vec<ServiceSpec>> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::paths::Profile;
    use crate::test_support::TempDir;

    #[test]
    fn only_an_isolated_profile_reads_dev_services() {
        let root = TempDir::new("registry");
        let data = root.path().join("data");
        // Absolute on every OS, and a stop command passes `validate` on
        // Windows too, so only the profile check can make the list empty.
        let program = root.path().join("service");
        let file = DevServicesFile {
            services: vec![DevService {
                id: ServiceId::new("dummy").unwrap(),
                program: program.clone(),
                args: vec!["--port".to_owned(), "47001".to_owned()],
                port: 47_001,
                stop: DevStop::Command(DevStopCommand {
                    program,
                    args: vec!["--stop".to_owned()],
                }),
                protected: false,
                ready_timeout_ms: 5000,
                stop_timeout_ms: 2000,
            }],
        };
        let standard = Paths {
            home: root.path().to_path_buf(),
            data,
            profile: Profile::Standard,
        };
        fs::create_dir_all(standard.state_dir()).unwrap();
        fs::write(
            standard.dev_services_file(),
            serde_json::to_vec(&file).unwrap(),
        )
        .unwrap();

        assert!(load(&standard).unwrap().is_empty());

        let isolated = Paths {
            profile: Profile::Isolated,
            ..standard
        };
        let loaded = load(&isolated).unwrap();
        if cfg!(debug_assertions) {
            assert_eq!(loaded.len(), 1);
            assert_eq!(loaded[0].id.as_str(), "dummy");
        } else {
            assert!(loaded.is_empty());
        }
    }
}
