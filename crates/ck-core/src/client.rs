//! The client side, used by `ck` (and later the desktop app).
//!
//! `connect` and `shutdown` never start a supervisor; `ensure` does.

use std::time::Duration;

use crate::error::Result;
use crate::ipc::protocol::{HelloReply, ServiceStatus, StatusReply};
use crate::paths::Paths;

/// The arguments `ensure` passes to `current_exe()` to start the supervisor.
/// The CLI parses them as its hidden `supervisor run --detached` command.
pub const SUPERVISOR_RUN_ARGS: [&str; 3] = ["supervisor", "run", "--detached"];

/// How long `ensure` waits for a new supervisor to answer.
pub const SPAWN_WAIT: Duration = Duration::from_secs(10);

/// A connection to the supervisor that passed the peer check and `hello`.
#[derive(Debug)]
pub struct Client {
    _stub: (),
}

/// Connects to the running supervisor.
///
/// Gives [`Error::SupervisorUnavailable`](crate::Error::SupervisorUnavailable)
/// when none is running (including when the data folder does not exist yet).
pub async fn connect(paths: &Paths) -> Result<Client> {
    let _ = paths;
    todo!()
}

/// Connects, starting the supervisor first when none is running.
///
/// It creates the layout, then spawns `current_exe()` with
/// [`SUPERVISOR_RUN_ARGS`]: stdin null, stdout and stderr appended to
/// `logs/supervisor.log`, detached (on Windows: no window, a new process group,
/// breakaway from the caller's Job when allowed, and std handles made
/// non-inheritable first). It then retries the connection for up to
/// [`SPAWN_WAIT`]; if the child exits first, the error carries the end of
/// the log.
pub async fn ensure(paths: &Paths) -> Result<Client> {
    let _ = paths;
    todo!()
}

/// Asks the supervisor to stop every service and exit, then waits until it
/// has gone. `Ok(false)` when none was running.
///
/// The framing, `hello` and `shutdown` never change, so this also stops a
/// supervisor that speaks another protocol version, as long as it serves the
/// same data folder.
pub async fn shutdown(paths: &Paths) -> Result<bool> {
    let _ = paths;
    todo!()
}

impl Client {
    /// The supervisor's reply to `hello`.
    pub fn hello(&self) -> &HelloReply {
        todo!()
    }

    pub async fn status(&mut self) -> Result<StatusReply> {
        todo!()
    }

    pub async fn start(&mut self, service: &str) -> Result<ServiceStatus> {
        let _ = service;
        todo!()
    }

    pub async fn stop(&mut self, service: &str) -> Result<ServiceStatus> {
        let _ = service;
        todo!()
    }
}
