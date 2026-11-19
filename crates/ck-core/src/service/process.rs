//! One service process: the port check, the spawn, the task that watches an
//! owned child, signals, readiness and the stop sequence.
//!
//! An owned child is held by its own watcher task, and every signal to it goes
//! through that task, which first checks with `try_wait` that the child has not
//! been reaped: once reaped, its pid may belong to another process. An adopted
//! process (Unix) is not our child, so its identity is checked again
//! immediately before every signal.

use std::ffi::OsString;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{self, Instant};

#[cfg(unix)]
use ck_sys::unix::{self, Delivery, Pid, Signal};
#[cfg(windows)]
use ck_sys::windows::{CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, Job};

use crate::error::{Error, Result};
use crate::layout;
use crate::paths::Paths;
#[cfg(unix)]
use crate::service::identity::{self, ProcessIdentity};
use crate::service::manager::{KILL_WAIT, READY_POLL_INTERVAL};
use crate::service::spec::{ServiceSpec, StopMethod, StopSignal};

/// How long one loopback connect may take, in the port check and readiness.
/// A connect to a closed port can take seconds on Windows.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(200);

/// How often an adopted process is checked while a stop waits for it.
#[cfg(unix)]
const EXIT_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// The variables a service inherits from the supervisor; everything else is
/// cleared.
#[cfg(unix)]
const INHERITED_VARS: [&str; 2] = ["PATH", "TMPDIR"];
#[cfg(windows)]
const INHERITED_VARS: [&str; 4] = ["PATH", "TEMP", "TMP", "SystemRoot"];

/// Set to the profile's home folder, so the services of a test profile stay
/// inside it.
#[cfg(unix)]
const OS_HOME_VAR: &str = "HOME";
#[cfg(windows)]
const OS_HOME_VAR: &str = "USERPROFILE";

/// Fails with `port_in_use` when something already holds `127.0.0.1:port`.
/// The port is never changed.
pub(super) async fn check_port(spec: &ServiceSpec) -> Result<()> {
    let address = loopback(spec.port);
    let in_use = || Error::PortInUse {
        service: spec.id.to_string(),
        port: spec.port,
    };
    match std::net::TcpListener::bind(address) {
        Ok(listener) => drop(listener),
        Err(error) if is_port_taken(&error) => return Err(in_use()),
        Err(error) => {
            return Err(Error::StartFailed {
                service: spec.id.to_string(),
                reason: format!("cannot check port {}: {error}", spec.port),
            });
        }
    }
    // Here a bind to 127.0.0.1 can succeed beside a listener on 0.0.0.0.
    #[cfg(any(target_os = "macos", windows))]
    if connects(address).await {
        return Err(in_use());
    }
    Ok(())
}

/// Windows reports a port that another socket holds exclusively, or that the
/// system reserves, as access denied.
fn is_port_taken(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::AddrInUse
        || (cfg!(windows) && error.kind() == std::io::ErrorKind::PermissionDenied)
}

fn loopback(port: u16) -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, port))
}

async fn connects(address: SocketAddr) -> bool {
    matches!(
        time::timeout(CONNECT_TIMEOUT, TcpStream::connect(address)).await,
        Ok(Ok(_))
    )
}

/// Gives `command` the service environment, no input, and the service's log
/// as its output.
fn prepare(command: &mut Command, spec: &ServiceSpec, paths: &Paths) -> Result<()> {
    let log_path = paths.service_log(&spec.id);
    let log = layout::open_log(&log_path)?;
    let log_for_errors = log
        .try_clone()
        .map_err(|error| Error::io("open", &log_path, error))?;
    command
        .env_clear()
        .envs(
            INHERITED_VARS
                .iter()
                .filter_map(|name| std::env::var_os(name).map(|value| (name, value))),
        )
        .env(OS_HOME_VAR, &paths.home)
        .envs(
            spec.env
                .iter()
                .map(|(name, value)| (name.as_os_str(), value.as_os_str())),
        )
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(log_for_errors);
    Ok(())
}

/// A service child that no task watches yet. Until one does, nothing can
/// reap it, so `pid` is still its own.
#[derive(Debug)]
pub(super) struct Spawned {
    child: Child,
    pid: u32,
    #[cfg(windows)]
    job: Job,
}

/// Spawns the service with an argument list, never a shell, as the leader of
/// its own process group (Unix) or with no window (Windows). On Windows the
/// Job is created first, so that only [`Spawned::confine`] can fail after the
/// spawn.
pub(super) fn spawn(spec: &ServiceSpec, paths: &Paths) -> Result<Spawned> {
    let failed = |reason: String| Error::StartFailed {
        service: spec.id.to_string(),
        reason,
    };
    #[cfg(windows)]
    let job = Job::kill_on_close()
        .map_err(|error| failed(format!("cannot create its Job Object: {error}")))?;
    let mut command = Command::new(&spec.program);
    command.args(&spec.args);
    prepare(&mut command, spec, paths)?;
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    let child = command
        .spawn()
        .map_err(|error| failed(format!("cannot run {}: {error}", spec.program.display())))?;
    let pid = child
        .id()
        .ok_or_else(|| failed("it exited before it could be watched".to_owned()))?;
    Ok(Spawned {
        child,
        pid,
        #[cfg(windows)]
        job,
    })
}

impl Spawned {
    pub(super) fn pid(&self) -> u32 {
        self.pid
    }

    /// Puts the child in its kill-on-close Job. No service runs outside a
    /// Job, so when that fails the child is ended through its own handle (the
    /// only one that reaches a process outside every Job) and the start fails.
    #[cfg(windows)]
    pub(super) async fn confine(&mut self) -> std::result::Result<(), String> {
        let Err(error) = self.job.assign(&self.child) else {
            return Ok(());
        };
        let _ = self.child.start_kill();
        let _ = time::timeout(KILL_WAIT, self.child.wait()).await;
        Err(format!("cannot put it in a Job Object: {error}"))
    }

    /// Nothing to do on Unix: the child has led its own process group since
    /// the spawn.
    #[cfg(unix)]
    pub(super) async fn confine(&mut self) -> std::result::Result<(), String> {
        Ok(())
    }

    /// Runs `action` unless the child has already exited.
    fn act(&mut self, action: Action) -> Result<Reached> {
        if self.has_exited()? {
            return Ok(Reached::Exited);
        }
        self.deliver(action)
    }

    /// Whether the child has exited. It reaps the child, and is the check
    /// before every signal: an exited but unreaped child still holds its pid.
    fn has_exited(&mut self) -> Result<bool> {
        self.child
            .try_wait()
            .map(|status| status.is_some())
            .map_err(|error| Error::os("check a service process", error))
    }

    #[cfg(unix)]
    fn deliver(&mut self, action: Action) -> Result<Reached> {
        let pid = Pid::new(self.pid).map_err(|error| Error::Internal(error.to_string()))?;
        let sent = send_signal(pid, action);
        // macOS can refuse to signal a group whose members have all exited;
        // an exit that happened meanwhile is not a failure.
        if sent.is_err() && self.has_exited()? {
            return Ok(Reached::Exited);
        }
        sent
    }

    #[cfg(windows)]
    fn deliver(&mut self, action: Action) -> Result<Reached> {
        match action {
            Action::Kill => self
                .job
                .terminate(1)
                .map_err(|error| Error::os("terminate the Job Object of a service", error))?,
        }
        Ok(Reached::Sent)
    }
}

/// What can be done to a service process.
#[derive(Debug, Clone, Copy)]
enum Action {
    /// Send this signal to the process-group leader (Unix).
    #[cfg(unix)]
    Signal(StopSignal),
    /// Kill the whole process group (Unix) or terminate the Job (Windows).
    Kill,
}

/// Whether an action reached the process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reached {
    Sent,
    /// The process had already gone, so nothing was sent.
    Exited,
}

#[cfg(unix)]
fn send_signal(pid: Pid, action: Action) -> Result<Reached> {
    let delivery = match action {
        Action::Signal(stop) => unix::signal_process(pid, stop.into()),
        Action::Kill => unix::signal_group(pid, Signal::Kill),
    }
    .map_err(|error| Error::os("signal a service process", error))?;
    Ok(match delivery {
        Delivery::Sent => Reached::Sent,
        Delivery::NoSuchProcess => Reached::Exited,
    })
}

#[derive(Debug)]
struct ActionRequest {
    action: Action,
    reply: oneshot::Sender<Result<Reached>>,
}

/// The way to an owned child: its watcher task, and its exit once known.
#[derive(Debug, Clone)]
pub(super) struct ChildLink {
    actions: mpsc::Sender<ActionRequest>,
    /// A description of the exit status, once the child has exited.
    exit: watch::Receiver<Option<String>>,
}

/// Starts the task that owns and watches `spawned` from now on.
///
/// When the child exits, the task first calls `on_exit` with a description of
/// the exit status, then publishes it to every [`ChildLink`]. A child is never
/// killed when its task or the manager goes away: on Unix it can be adopted
/// later, and on Windows its Job ends it.
pub(super) fn start_watcher(
    spawned: Spawned,
    on_exit: impl FnOnce(&str) + Send + 'static,
) -> ChildLink {
    let (actions, requests) = mpsc::channel(1);
    let (exit_sender, exit) = watch::channel(None);
    tokio::spawn(run_watcher(spawned, requests, exit_sender, on_exit));
    ChildLink { actions, exit }
}

async fn run_watcher(
    mut spawned: Spawned,
    mut requests: mpsc::Receiver<ActionRequest>,
    exit: watch::Sender<Option<String>>,
    on_exit: impl FnOnce(&str) + Send + 'static,
) {
    let status = loop {
        tokio::select! {
            status = spawned.child.wait() => break status,
            Some(request) = requests.recv() => {
                // The asker may have stopped waiting; the action stands anyway.
                let _ = request.reply.send(spawned.act(request.action));
            }
        }
    };
    let description = match status {
        Ok(status) => status.to_string(),
        Err(error) => format!("its exit status could not be read: {error}"),
    };
    // On Windows this closes the Job, which ends anything left inside it.
    drop(spawned);
    on_exit(&description);
    exit.send_replace(Some(description));
}

impl ChildLink {
    async fn act(&self, action: Action) -> Result<Reached> {
        let (reply, answer) = oneshot::channel();
        if self
            .actions
            .send(ActionRequest { action, reply })
            .await
            .is_err()
        {
            // The watcher has finished: the child exited and was reaped.
            return Ok(Reached::Exited);
        }
        // The watcher drops a request without an answer only once the child
        // has exited.
        answer.await.unwrap_or(Ok(Reached::Exited))
    }

    async fn wait_exit(&self, limit: Duration) -> bool {
        let mut exit = self.exit.clone();
        matches!(
            time::timeout(limit, exit.wait_for(Option::is_some)).await,
            Ok(Ok(_))
        )
    }
}

/// A service process as the manager holds it.
#[derive(Debug, Clone)]
pub(super) enum Handle {
    /// A child this supervisor spawned.
    Owned(ChildLink),
    /// A process adopted from an earlier supervisor (Unix).
    #[cfg(unix)]
    Adopted { pid: Pid, identity: ProcessIdentity },
}

impl Handle {
    async fn act(&self, action: Action) -> Result<Reached> {
        match self {
            Self::Owned(link) => link.act(action).await,
            #[cfg(unix)]
            Self::Adopted { pid, identity } => {
                if adopted_has_exited(*pid, identity)? {
                    return Ok(Reached::Exited);
                }
                let sent = send_signal(*pid, action);
                // As for an owned child: an exit meanwhile is not a failure.
                if sent.is_err() && adopted_has_exited(*pid, identity)? {
                    return Ok(Reached::Exited);
                }
                sent
            }
        }
    }

    /// Waits up to `limit` for the process to exit; true once it has.
    async fn wait_exit(&self, limit: Duration) -> bool {
        match self {
            Self::Owned(link) => link.wait_exit(limit).await,
            #[cfg(unix)]
            Self::Adopted { pid, identity } => {
                let deadline = Instant::now().checked_add(limit);
                loop {
                    if adopted_has_exited(*pid, identity).unwrap_or(false) {
                        return true;
                    }
                    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                        return false;
                    }
                    time::sleep(EXIT_POLL_INTERVAL).await;
                }
            }
        }
    }
}

/// Whether an adopted process has gone: its pid no longer shows the recorded
/// identity. An error means it could not be checked, and nothing may be sent.
#[cfg(unix)]
fn adopted_has_exited(pid: Pid, identity: &ProcessIdentity) -> Result<bool> {
    match identity::verify(pid, identity) {
        Ok(()) => Ok(false),
        Err(Error::IdentityMismatch { .. }) => Ok(true),
        Err(error) => Err(error),
    }
}

/// The result of waiting for a new child to accept connections.
#[derive(Debug)]
pub(super) enum Readiness {
    Ready,
    /// The child exited; how.
    Exited(String),
    TimedOut,
}

/// Tries a loopback connect every [`READY_POLL_INTERVAL`] while the child is
/// alive, up to the spec's `ready_timeout`.
pub(super) async fn wait_ready(spec: &ServiceSpec, link: &ChildLink) -> Readiness {
    let mut exit = link.exit.clone();
    let deadline = Instant::now().checked_add(spec.ready_timeout);
    let address = loopback(spec.port);
    loop {
        let exited = exit.borrow_and_update().clone();
        if let Some(description) = exited {
            return Readiness::Exited(description);
        }
        if connects(address).await {
            return Readiness::Ready;
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Readiness::TimedOut;
        }
        tokio::select! {
            () = time::sleep(READY_POLL_INTERVAL) => {}
            changed = exit.changed() => {
                if changed.is_err() {
                    return Readiness::Exited("its watcher has stopped".to_owned());
                }
            }
        }
    }
}

/// The stop sequence (module docs of the manager, steps 1 to 4):
///
/// 1. The graceful step from the spec.
/// 2. Wait up to `stop_timeout`.
/// 3. Still alive and protected: give up. A protected service is never killed.
/// 4. Otherwise kill the group (Unix) or terminate the Job (Windows), and wait
///    up to [`KILL_WAIT`].
///
/// `Ok` once the process has gone; `Err` with the reason while it still runs.
pub(super) async fn halt(
    spec: &ServiceSpec,
    paths: &Paths,
    handle: &Handle,
) -> std::result::Result<(), String> {
    let requested = match &spec.stop {
        StopMethod::Signal(signal) => request_signal(handle, *signal).await,
        StopMethod::Command { program, args } => run_stop_command(spec, paths, program, args),
    };
    let reason = match requested {
        Ok(()) => {
            if handle.wait_exit(spec.stop_timeout).await {
                return Ok(());
            }
            format!(
                "it was still running {:?} after the stop request",
                spec.stop_timeout
            )
        }
        Err(reason) => reason,
    };
    if spec.protected {
        return Err(format!("{reason}; it is protected, so it is not killed"));
    }
    if let Err(error) = handle.act(Action::Kill).await {
        return Err(format!("{reason}, and killing it failed: {error}"));
    }
    if handle.wait_exit(KILL_WAIT).await {
        Ok(())
    } else {
        Err(format!(
            "{reason}, and it was still running {KILL_WAIT:?} after a kill"
        ))
    }
}

#[cfg(unix)]
async fn request_signal(handle: &Handle, signal: StopSignal) -> std::result::Result<(), String> {
    handle
        .act(Action::Signal(signal))
        .await
        .map(|_reached| ())
        .map_err(|error| format!("cannot send the stop signal: {error}"))
}

/// [`ServiceSpec::validate`] refuses a signal stop on Windows; this keeps the
/// refusal should one get here anyway.
#[cfg(windows)]
async fn request_signal(_handle: &Handle, _signal: StopSignal) -> std::result::Result<(), String> {
    Err("Windows has no signals, so it needs a stop command".to_owned())
}

/// Runs the stop command with the service's environment and log, and reaps it
/// in the background. It is a helper, not a service: one that still runs after
/// `stop_timeout` is killed.
fn run_stop_command(
    spec: &ServiceSpec,
    paths: &Paths,
    program: &Path,
    args: &[OsString],
) -> std::result::Result<(), String> {
    let mut command = Command::new(program);
    command.args(args).kill_on_drop(true);
    prepare(&mut command, spec, paths).map_err(|error| error.to_string())?;
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot run the stop command {}: {error}", program.display()))?;
    let limit = spec.stop_timeout;
    tokio::spawn(async move {
        // Dropped after the limit, it is killed (`kill_on_drop`) and reaped.
        let _ = time::timeout(limit, child.wait()).await;
    });
    Ok(())
}
