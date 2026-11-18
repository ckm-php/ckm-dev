//! The `--json` documents, and how errors are printed.
//!
//! Every document is one line of JSON on stdout: `{"schema":1,"ok":true,…}`
//! with the body's fields beside `schema` and `ok`, or
//! `{"schema":1,"ok":false,"error":{"code":…,"message":…}}`. Without `--json`,
//! errors go to stderr as `ck: <message>`.

use std::path::PathBuf;

use ck_core::branding;
use ck_core::ipc::protocol::ServiceStatus;
use ck_core::paths::{BuildKind, Profile};
use ck_core::{Error, ErrorBody};
use serde::Serialize;

/// The version of every `--json` document.
pub const SCHEMA: u32 = 1;

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
#[expect(dead_code, reason = "built once the paths handler is implemented")]
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
#[expect(dead_code, reason = "built once the status handler is implemented")]
#[derive(Serialize)]
pub struct StatusBody {
    /// `null` when no supervisor is running; `services` is then empty.
    pub supervisor: Option<SupervisorBody>,
    pub services: Vec<ServiceStatus>,
}

/// The running supervisor, from its `hello` reply.
#[expect(dead_code, reason = "built once the status handler is implemented")]
#[derive(Serialize)]
pub struct SupervisorBody {
    pub pid: u32,
    pub version: String,
}

/// Body of `ck start --json` and `ck stop --json`.
#[expect(
    dead_code,
    reason = "built once the start and stop handlers are implemented"
)]
#[derive(Serialize)]
pub struct ServiceBody {
    pub service: ServiceStatus,
}

/// Body of `ck supervisor stop --json`.
#[expect(
    dead_code,
    reason = "built once the supervisor stop handler is implemented"
)]
#[derive(Serialize)]
pub struct SupervisorStopBody {
    /// False when no supervisor was running.
    pub was_running: bool,
}

/// Prints `error` as a JSON document on stdout, or as `ck: <message>` on stderr.
pub fn print_error(error: &Error, json: bool) {
    if json {
        let document = Document {
            schema: SCHEMA,
            ok: false,
            body: Failure {
                error: ErrorBody::from(error),
            },
        };
        match serde_json::to_string(&document) {
            Ok(text) => println!("{text}"),
            Err(_) => eprintln!("{}: {error}", branding::COMMAND),
        }
    } else {
        eprintln!("{}: {error}", branding::COMMAND);
    }
}
