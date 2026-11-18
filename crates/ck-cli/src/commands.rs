//! One handler per command. Each prints its own success output (text, or one
//! `output::Document` with `--json`); `main` prints errors and sets the exit
//! code.
//!
//! - `status`, `paths` and `supervisor stop` never start the supervisor.
//! - `start` and `stop` start it through `client::ensure`, and are idempotent.
//! - Text status: `Supervisor: running (pid N)` or `Supervisor: not running`,
//!   then a table with the columns `SERVICE STATE PID PORT SINCE`. `status`
//!   with no supervisor is not an error.

use ck_core::Result;

/// `ck status [--json]`
pub fn status(json: bool) -> Result<()> {
    let _ = json;
    todo!()
}

/// `ck start <service> [--json]`
pub fn start(service: &str, json: bool) -> Result<()> {
    let _ = (service, json);
    todo!()
}

/// `ck stop <service> [--json]`
pub fn stop(service: &str, json: bool) -> Result<()> {
    let _ = (service, json);
    todo!()
}

/// `ck supervisor stop [--json]`. Not an error when none is running.
pub fn supervisor_stop(json: bool) -> Result<()> {
    let _ = json;
    todo!()
}

/// `ck supervisor run [--detached]`, hidden: `ck_core::supervisor::run`.
pub fn supervisor_run(detached: bool) -> Result<()> {
    let _ = detached;
    todo!()
}

/// `ck paths [--json]`. Creates nothing, so it is safe with a release binary.
pub fn paths(json: bool) -> Result<()> {
    let _ = json;
    todo!()
}
