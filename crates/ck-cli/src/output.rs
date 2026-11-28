//! The `--json` documents, the text output, and how errors are printed.
//!
//! Every document is one line of JSON on stdout: `{"schema":1,"ok":true,…}`
//! with the body's fields beside `schema` and `ok`, or
//! `{"schema":1,"ok":false,"error":{"code":…,"message":…}}`. Without `--json`,
//! errors go to stderr as `ck: <message>`.

use std::io::{self, Write as _};
use std::path::PathBuf;

use ck_core::branding;
#[cfg(feature = "net")]
use ck_core::install::{InstallOutcome, InstalledRuntime};
use ck_core::ipc::protocol::ServiceStatus;
use ck_core::paths::{BuildKind, Profile};
use ck_core::service::ServiceState;
use ck_core::{Error, ErrorBody, Result};
use serde::Serialize;

/// The version of every `--json` document.
pub const SCHEMA: u32 = 1;

/// A table cell for a value a service does not have, such as the pid of a
/// stopped service.
const EMPTY_CELL: &str = "-";

/// One `--json` document.
#[derive(Serialize)]
pub struct Document<T> {
    pub schema: u32,
    pub ok: bool,
    #[serde(flatten)]
    pub body: T,
}

/// Body of a failure: `"error": {"code", "message"}`.
#[derive(Serialize)]
pub struct Failure {
    pub error: ErrorBody,
}

/// Body of `ck paths --json`.
#[derive(Serialize)]
pub struct PathsBody {
    pub build: BuildKind,
    pub profile: Profile,
    pub home: PathBuf,
    pub data: PathBuf,
    /// `null` until the data folder exists, because the endpoint comes from
    /// its canonical path.
    pub endpoint: Option<String>,
}

/// Body of `ck status --json`.
#[derive(Serialize)]
pub struct StatusBody {
    /// `null` when no supervisor is running; `services` is then empty.
    pub supervisor: Option<SupervisorBody>,
    pub services: Vec<ServiceStatus>,
}

/// The running supervisor, from its `hello` reply.
#[derive(Serialize)]
pub struct SupervisorBody {
    pub pid: u32,
    pub version: String,
}

/// Body of `ck start --json` and `ck stop --json`.
#[derive(Serialize)]
pub struct ServiceBody {
    pub service: ServiceStatus,
}

/// Body of `ck supervisor stop --json`.
#[derive(Serialize)]
pub struct SupervisorStopBody {
    /// False when no supervisor was running.
    pub was_running: bool,
}

/// Body of `ck install --json`.
#[cfg(feature = "net")]
#[derive(Serialize)]
pub struct InstallBody {
    pub name: String,
    pub version: String,
    pub outcome: &'static str,
    /// A lossy string, so an unusual path cannot fail serialization after the
    /// install has already succeeded.
    pub path: String,
}

#[cfg(feature = "net")]
impl InstallBody {
    /// The command's validated names and the install result.
    pub fn new(name: &str, version: &str, outcome: &InstallOutcome) -> Self {
        let (outcome, path) = match outcome {
            InstallOutcome::Installed(path) => ("installed", path),
            InstallOutcome::AlreadyPresent(path) => ("already-present", path),
        };
        Self {
            name: name.to_owned(),
            version: version.to_owned(),
            outcome,
            path: path.to_string_lossy().into_owned(),
        }
    }
}

/// One installed runtime in `ck installed --json`.
#[cfg(feature = "net")]
#[derive(Serialize)]
pub struct RuntimeBody {
    pub name: String,
    pub version: String,
}

#[cfg(feature = "net")]
impl From<&InstalledRuntime> for RuntimeBody {
    fn from(runtime: &InstalledRuntime) -> Self {
        Self {
            name: runtime.name.to_string(),
            version: runtime.version.to_string(),
        }
    }
}

/// Body of `ck installed --json`.
#[cfg(feature = "net")]
#[derive(Serialize)]
pub struct InstalledBody {
    pub runtimes: Vec<RuntimeBody>,
}

/// Prints `body` as a successful JSON document on stdout.
pub fn print_ok<T: Serialize>(body: T) -> Result<()> {
    let document = Document {
        schema: SCHEMA,
        ok: true,
        body,
    };
    let text = serde_json::to_string(&document)
        .map_err(|e| Error::Internal(format!("cannot write the JSON output: {e}")))?;
    print_line(&text)
}

/// Writes `text` and a newline to stdout. A closed pipe gives an error rather
/// than the panic that `println!` would give.
pub fn print_line(text: &str) -> Result<()> {
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "{text}")
        .and_then(|()| stdout.flush())
        .map_err(|e| Error::os("write to stdout", e))
}

/// Prints `error` as a JSON document on stdout, or as `ck: <message>` on
/// stderr. When stdout cannot take the document, the message goes to stderr.
pub fn print_error(error: &Error, json: bool) {
    if json {
        let document = Document {
            schema: SCHEMA,
            ok: false,
            body: Failure {
                error: ErrorBody::from(error),
            },
        };
        if let Ok(text) = serde_json::to_string(&document)
            && print_line(&text).is_ok()
        {
            return;
        }
    }
    // Nothing is left to report a failure to, so it is ignored.
    let _ = writeln!(io::stderr().lock(), "{}: {error}", branding::COMMAND);
}

/// The name `--json` uses for a build kind.
pub const fn build_name(build: BuildKind) -> &'static str {
    match build {
        BuildKind::Debug => "debug",
        BuildKind::Release => "release",
    }
}

/// The name `--json` uses for a profile.
pub const fn profile_name(profile: Profile) -> &'static str {
    match profile {
        Profile::Standard => "standard",
        Profile::Isolated => "isolated",
    }
}

/// The name `--json` uses for a service state.
pub const fn state_name(state: ServiceState) -> &'static str {
    match state {
        ServiceState::Stopped => "stopped",
        ServiceState::Starting => "starting",
        ServiceState::Running => "running",
        ServiceState::Stopping => "stopping",
        ServiceState::Failed => "failed",
    }
}

/// One service after `ck start` or `ck stop`, such as
/// `web: running (pid 4242, port 8900)` or `web: stopped (port 8900)`.
pub fn service_line(service: &ServiceStatus) -> String {
    let state = state_name(service.state);
    match service.pid {
        Some(pid) => format!("{}: {state} (pid {pid}, port {})", service.id, service.port),
        None => format!("{}: {state} (port {})", service.id, service.port),
    }
}

/// The one text line `ck install` prints.
#[cfg(feature = "net")]
pub fn install_line(body: &InstallBody) -> String {
    format!(
        "{} {}: {} at {}",
        body.name, body.version, body.outcome, body.path
    )
}

/// The text `ck installed` prints: one `name version` line each, or a note
/// when nothing is installed.
#[cfg(feature = "net")]
pub fn installed_lines(body: &InstalledBody) -> String {
    if body.runtimes.is_empty() {
        return "No runtimes installed.".to_owned();
    }
    body.runtimes
        .iter()
        .map(|runtime| format!("{} {}", runtime.name, runtime.version))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The `ck status` table: a header row, then one row per service. Columns
/// are left-aligned and two spaces apart, and no line ends in spaces.
pub fn status_table(services: &[ServiceStatus]) -> String {
    let header = ["SERVICE", "STATE", "PID", "PORT", "SINCE"].map(String::from);
    let rows: Vec<[String; 5]> = std::iter::once(header)
        .chain(services.iter().map(|service| {
            [
                service.id.to_string(),
                state_name(service.state).to_owned(),
                service
                    .pid
                    .map_or_else(|| EMPTY_CELL.to_owned(), |pid| pid.to_string()),
                service.port.to_string(),
                service
                    .since
                    .clone()
                    .unwrap_or_else(|| EMPTY_CELL.to_owned()),
            ]
        }))
        .collect();

    // `format!` pads by `char`, so widths are counted the same way.
    let mut widths = [0; 5];
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = cell.chars().count().max(*width);
        }
    }

    rows.iter()
        .map(|row| {
            let cells: Vec<String> = row
                .iter()
                .zip(widths)
                .map(|(cell, width)| format!("{cell:<width$}"))
                .collect();
            cells.join("  ").trim_end().to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(all(test, feature = "net"))]
mod tests {
    use serde_json::Value;

    use super::*;

    #[test]
    fn install_and_installed_bodies_are_one_json_document() {
        let path = PathBuf::from("runtimes/php/8.5.13-ck1");
        let install = InstallBody::new(
            "php",
            "8.5.13-ck1",
            &InstallOutcome::Installed(path.clone()),
        );
        let document = Document {
            schema: SCHEMA,
            ok: true,
            body: &install,
        };
        let text = serde_json::to_string(&document).unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["schema"], 1);
        assert_eq!(value["ok"], true);
        assert_eq!(value["name"], "php");
        assert_eq!(value["version"], "8.5.13-ck1");
        assert_eq!(value["outcome"], "installed");
        assert_eq!(value["path"], path.to_str().unwrap());
        assert_eq!(
            install_line(&install),
            format!("php 8.5.13-ck1: installed at {}", path.display())
        );

        let installed = InstalledBody {
            runtimes: vec![RuntimeBody {
                name: "php".to_owned(),
                version: "8.5.13-ck1".to_owned(),
            }],
        };
        let document = Document {
            schema: SCHEMA,
            ok: true,
            body: &installed,
        };
        let text = serde_json::to_string(&document).unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["runtimes"].as_array().map(Vec::len), Some(1));
        assert_eq!(installed_lines(&installed), "php 8.5.13-ck1");
    }
}
