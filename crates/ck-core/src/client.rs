//! The client side, used by `ck` (and later the desktop app).
//!
//! `connect` and `shutdown` never start a supervisor; `ensure` does.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde::de::DeserializeOwned;
use tokio::time::{Instant, sleep, timeout};

use crate::error::{Error, Result};
use crate::ipc::endpoint::Endpoint;
use crate::ipc::protocol::{Empty, HelloReply, Request, Response, ServiceStatus, StatusReply};
use crate::ipc::transport::{Connection, handshake};
use crate::ipc::{MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES};
use crate::layout;
use crate::paths::{DATA_VAR, HOME_VAR, Paths, Profile};

/// The arguments `ensure` passes to `current_exe()` to start the supervisor.
/// The CLI parses them as its hidden `supervisor run --detached` command.
pub const SUPERVISOR_RUN_ARGS: [&str; 3] = ["supervisor", "run", "--detached"];

/// How long `ensure` waits for a new supervisor to answer.
pub const SPAWN_WAIT: Duration = Duration::from_secs(10);

/// How long connecting and the `hello` exchange may take.
const CONNECT_DEADLINE: Duration = Duration::from_secs(5);

/// How long one request may take: longer than a start or a stop with the
/// default timeouts (15 s ready; 10 s stop plus 5 s after a kill).
const CALL_DEADLINE: Duration = Duration::from_secs(60);

/// How long `shutdown` waits for its reply, which comes only after every
/// service has stopped, one after the other.
const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(120);

/// How long `shutdown` waits, after the reply, for the endpoint to go away.
const EXIT_WAIT: Duration = Duration::from_secs(15);

/// How often `ensure` and `shutdown` look again.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// How many lines of `supervisor.log` a failed start reports.
const LOG_TAIL_LINES: usize = 20;

/// The most of `supervisor.log` read for those lines.
const LOG_TAIL_BYTES: u64 = 16 * 1024;

/// A connection to the supervisor that passed the peer check and `hello`.
#[derive(Debug)]
pub struct Client {
    /// `None` after a request failed on the way: a late reply could still be
    /// in flight, so the connection is never used again.
    connection: Option<Connection>,
    hello: HelloReply,
}

/// Connects to the running supervisor.
///
/// Gives [`Error::SupervisorUnavailable`](crate::Error::SupervisorUnavailable)
/// when none is running (including when the data folder does not exist yet).
pub async fn connect(paths: &Paths) -> Result<Client> {
    within(
        CONNECT_DEADLINE,
        "the supervisor to answer",
        open_client(paths),
    )
    .await
}

/// [`connect`] without its deadline.
async fn open_client(paths: &Paths) -> Result<Client> {
    let endpoint = Endpoint::locate(&paths.data)?.ok_or_else(not_running)?;
    let mut connection = Connection::open(&endpoint).await?;
    let hello = handshake(&mut connection, &endpoint).await?;
    Ok(Client {
        connection: Some(connection),
        hello,
    })
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
    match connect(paths).await {
        Err(Error::SupervisorUnavailable { .. }) => {}
        connected => return connected,
    }
    layout::ensure(paths)?;
    let child = spawn_supervisor(paths)?;
    wait_for_supervisor(paths, child).await
}

/// Asks the supervisor to stop every service and exit, then waits until it
/// has gone. `Ok(false)` when none was running.
///
/// The framing, `hello` and `shutdown` never change, so this also stops a
/// supervisor that speaks another protocol version, as long as it serves the
/// same data folder.
pub async fn shutdown(paths: &Paths) -> Result<bool> {
    let Some(endpoint) = Endpoint::locate(&paths.data)? else {
        return Ok(false);
    };
    let opened = within(
        CONNECT_DEADLINE,
        "the supervisor to answer",
        Connection::open(&endpoint),
    )
    .await;
    let mut connection = match opened {
        Ok(connection) => connection,
        Err(Error::SupervisorUnavailable { .. }) => return Ok(false),
        Err(error) => return Err(error),
    };

    // The data folder is checked before the protocol, so a version mismatch
    // still means our supervisor, which understands `shutdown`.
    let greeted = within(
        CONNECT_DEADLINE,
        "the supervisor to answer",
        handshake(&mut connection, &endpoint),
    )
    .await;
    match greeted {
        Ok(_) | Err(Error::VersionMismatch { .. }) => {}
        Err(error) => return Err(error),
    }

    let answer = within(
        SHUTDOWN_DEADLINE,
        "the supervisor to stop its services",
        exchange::<Empty>(&mut connection, &Request::Shutdown {}),
    )
    .await;
    // The client closes first; the supervisor waits for this before it lets
    // the connection go.
    drop(connection);
    answer?.into_result()?;
    wait_until_gone(&endpoint).await?;
    Ok(true)
}

impl Client {
    /// The supervisor's reply to `hello`.
    pub fn hello(&self) -> &HelloReply {
        &self.hello
    }

    pub async fn status(&mut self) -> Result<StatusReply> {
        self.call(&Request::Status {}).await
    }

    pub async fn start(&mut self, service: &str) -> Result<ServiceStatus> {
        self.call(&Request::Start {
            service: service.to_owned(),
        })
        .await
    }

    pub async fn stop(&mut self, service: &str) -> Result<ServiceStatus> {
        self.call(&Request::Stop {
            service: service.to_owned(),
        })
        .await
    }

    /// Sends `request` and reads its reply within [`CALL_DEADLINE`]. An error
    /// reply becomes [`Error::Remote`](crate::Error::Remote).
    async fn call<T: DeserializeOwned>(&mut self, request: &Request) -> Result<T> {
        let connection = self
            .connection
            .as_mut()
            .ok_or_else(|| Error::SupervisorUnavailable {
                detail: "an earlier request on this connection failed".to_owned(),
            })?;
        let answer = within(
            CALL_DEADLINE,
            "the supervisor to answer",
            exchange::<T>(connection, request),
        )
        .await;
        match answer {
            Ok(response) => response.into_result().map_err(Error::from),
            Err(error) => {
                self.connection = None;
                Err(error)
            }
        }
    }
}

/// One request and its reply.
async fn exchange<T: DeserializeOwned>(
    connection: &mut Connection,
    request: &Request,
) -> Result<Response<T>> {
    connection.send(request, MAX_REQUEST_BYTES).await?;
    connection
        .recv::<Response<T>>(MAX_RESPONSE_BYTES)
        .await?
        .ok_or_else(|| Error::SupervisorUnavailable {
            detail: "it closed the connection without answering".to_owned(),
        })
}

/// Runs `future`, or gives [`Error::Timeout`](crate::Error::Timeout) after
/// `limit`.
async fn within<T>(
    limit: Duration,
    what: &'static str,
    future: impl Future<Output = Result<T>>,
) -> Result<T> {
    timeout(limit, future)
        .await
        .map_err(|_| Error::Timeout(what))?
}

fn not_running() -> Error {
    Error::SupervisorUnavailable {
        detail: "it is not running".to_owned(),
    }
}

/// Retries the connection every [`POLL_INTERVAL`] for up to [`SPAWN_WAIT`].
///
/// A child that exits 0 lost the lock to another supervisor (two `ensure`
/// calls at once), so the wait goes on for that one. Any other exit ends it
/// early.
async fn wait_for_supervisor(paths: &Paths, mut child: Child) -> Result<Client> {
    let deadline = Instant::now() + SPAWN_WAIT;
    let mut lost_the_lock = false;
    loop {
        match connect(paths).await {
            Err(Error::SupervisorUnavailable { .. }) => {}
            connected => return connected,
        }
        if !lost_the_lock {
            match child.try_wait() {
                Ok(None) => {}
                Ok(Some(status)) if status.success() => lost_the_lock = true,
                Ok(Some(status)) => {
                    return Err(not_started(paths, &format!("it exited with {status}")));
                }
                Err(e) => return Err(Error::os("check on the new supervisor", e)),
            }
        }
        if Instant::now() >= deadline {
            let reason = format!("it did not answer within {} s", SPAWN_WAIT.as_secs());
            return Err(not_started(paths, &reason));
        }
        sleep(POLL_INTERVAL).await;
    }
}

/// The error for a supervisor that did not come up, with the end of its log.
fn not_started(paths: &Paths, reason: &str) -> Error {
    let log = paths.supervisor_log();
    let tail = log_tail(&log);
    let tail = if tail.is_empty() {
        "(nothing)".to_owned()
    } else {
        tail
    };
    Error::SupervisorUnavailable {
        detail: format!(
            "it did not start: {reason}. The end of {}:\n{tail}",
            log.display()
        ),
    }
}

/// The last [`LOG_TAIL_LINES`] lines of the log, read from at most its last
/// [`LOG_TAIL_BYTES`]. Empty when the log cannot be read.
fn log_tail(path: &Path) -> String {
    let Ok(mut file) = File::open(path) else {
        return String::new();
    };
    let len = file.metadata().map(|meta| meta.len()).unwrap_or(0);
    if file
        .seek(SeekFrom::Start(len.saturating_sub(LOG_TAIL_BYTES)))
        .is_err()
    {
        return String::new();
    }
    let mut bytes = Vec::new();
    if file.take(LOG_TAIL_BYTES).read_to_end(&mut bytes).is_err() {
        return String::new();
    }
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().collect();
    let skip = lines.len().saturating_sub(LOG_TAIL_LINES);
    lines[skip..].join("\n")
}

/// Polls until nothing answers at `endpoint`, for up to [`EXIT_WAIT`].
///
/// Only "not running" means gone. Any other result means it is still there:
/// on Windows, for example, the pipe is busy while its last connections close.
async fn wait_until_gone(endpoint: &Endpoint) -> Result<()> {
    let deadline = Instant::now() + EXIT_WAIT;
    loop {
        let probe = within(
            CONNECT_DEADLINE,
            "the supervisor to answer",
            Connection::open(endpoint),
        )
        .await;
        if let Err(Error::SupervisorUnavailable { .. }) = probe {
            return Ok(());
        }
        drop(probe);
        if Instant::now() >= deadline {
            return Err(Error::Timeout("the supervisor to exit"));
        }
        sleep(POLL_INTERVAL).await;
    }
}

/// Spawns `current_exe()` as a detached supervisor and does not wait for it.
/// Dropping the `Child` later does not kill it.
fn spawn_supervisor(paths: &Paths) -> Result<Child> {
    let exe = std::env::current_exe().map_err(|e| Error::os("find this program's path", e))?;
    sys::spawn_detached(&exe, paths)
}

/// The supervisor command, with fresh log handles each time it is built.
///
/// - It runs in the data folder, so it never holds the caller's folder open
///   (Windows cannot delete a folder that a process runs in).
/// - It gets the test-profile variables that give `paths`, so it serves the
///   same data folder even when `paths` did not come from this environment.
fn supervisor_command(exe: &Path, paths: &Paths) -> Result<Command> {
    let log_path = paths.supervisor_log();
    let log = layout::open_log(&log_path)?;
    let log_for_errors = log
        .try_clone()
        .map_err(|e| Error::io("open", &log_path, e))?;
    let mut command = Command::new(exe);
    command
        .args(SUPERVISOR_RUN_ARGS)
        .current_dir(&paths.data)
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(log_for_errors);
    match paths.profile {
        Profile::Isolated => command
            .env(HOME_VAR, &paths.home)
            .env(DATA_VAR, &paths.data),
        Profile::Standard => command.env_remove(HOME_VAR).env_remove(DATA_VAR),
    };
    Ok(command)
}

#[cfg(unix)]
use self::unix as sys;
#[cfg(windows)]
use self::windows as sys;

#[cfg(unix)]
mod unix {
    use std::path::Path;
    use std::process::Child;

    use super::supervisor_command;
    use crate::error::{Error, Result};
    use crate::paths::Paths;

    /// No process group of its own: the child calls `setsid` itself
    /// (`--detached`), which a group leader could not.
    pub(super) fn spawn_detached(exe: &Path, paths: &Paths) -> Result<Child> {
        supervisor_command(exe, paths)?
            .spawn()
            .map_err(|e| Error::io("run", exe, e))
    }
}

#[cfg(windows)]
mod windows {
    use std::io;
    use std::os::windows::process::CommandExt;
    use std::path::Path;
    use std::process::Child;

    use ck_sys::windows::{
        CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW,
        stop_std_handle_inheritance,
    };

    use super::supervisor_command;
    use crate::error::{Error, Result};
    use crate::paths::Paths;

    /// No console window, a new process group (so the caller's Ctrl+C does
    /// not reach it), and out of the caller's Job when the Job allows it.
    /// The std handles are made non-inheritable first, so the supervisor
    /// never holds a caller's pipe open (`$x = ck start …` would hang).
    pub(super) fn spawn_detached(exe: &Path, paths: &Paths) -> Result<Child> {
        stop_std_handle_inheritance()
            .map_err(|e| Error::os("make the standard handles non-inheritable", e))?;
        let detached = CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP;
        let spawned = match spawn_with_flags(exe, paths, detached | CREATE_BREAKAWAY_FROM_JOB)? {
            // The caller's Job forbids breakaway (access denied): the
            // supervisor then stays in that Job.
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                spawn_with_flags(exe, paths, detached)?
            }
            other => other,
        };
        spawned.map_err(|e| Error::io("run", exe, e))
    }

    /// One attempt, with a fresh command and fresh log handles. The outer
    /// error is building the command; the inner one is the spawn.
    fn spawn_with_flags(exe: &Path, paths: &Paths, flags: u32) -> Result<io::Result<Child>> {
        Ok(supervisor_command(exe, paths)?
            .creation_flags(flags)
            .spawn())
    }
}
