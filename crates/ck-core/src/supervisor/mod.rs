//! The background supervisor: one per user and data folder, owning every
//! service (D19).
//!
//! It runs as the hidden `ck supervisor run`, started on demand by
//! `client::ensure`, until `ck supervisor stop`, logout or shutdown. It has no
//! idle exit and does not start at login.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::Serialize;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};
use tokio::time::timeout;

use crate::error::{Error, ErrorBody, ErrorCode, Result};
use crate::ipc::endpoint::Endpoint;
use crate::ipc::protocol::{
    Empty, HelloReply, PROTOCOL, Request, Response, ServiceStatus, StatusReply,
};
use crate::ipc::transport::{Connection, Listener};
use crate::ipc::{
    FIRST_FRAME_TIMEOUT, IDLE_TIMEOUT, MAX_CONNECTIONS, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES,
};
use crate::layout;
use crate::paths::{BuildKind, Paths};
use crate::service::Manager;
use crate::service::manager::RecordOutcome;

use self::lock::InstanceLock;

pub mod lock;
mod log;
pub mod registry;

/// How long the loop pauses after a failed accept, so that a lasting failure
/// cannot keep the one thread busy.
const ACCEPT_RETRY: Duration = Duration::from_millis(100);

/// How long the supervisor waits, once it has stopped listening, for open
/// connections to close. Clients close first, after reading their reply.
const DRAIN_WAIT: Duration = Duration::from_secs(2);

/// How much of a client's `hello` name is kept for the log.
const CLIENT_NAME_CHARS: usize = 64;

/// Runs the supervisor for `paths` until it is shut down. Call it inside a
/// current-thread tokio runtime with I/O and time enabled.
///
/// - **Start order:** layout, lock, endpoint, signal handlers, registry,
///   adoption, serve.
/// - **Single instance:** when another supervisor holds the lock, it returns
///   `Ok(())` at once and quietly.
/// - **Detaching:** when `client::ensure` spawned it, the caller first runs
///   [`detach`], before any runtime exists.
/// - **Shutdown** (a `shutdown` request, `SIGTERM` or `SIGINT`): stop accepting,
///   stop every service, remove the socket, return. When a protected service
///   will not stop, the shutdown fails and the supervisor keeps running.
/// - **Logs:** plain UTC lines on stderr, which `client::ensure` points at
///   `logs/supervisor.log`.
pub async fn run(paths: Paths) -> Result<()> {
    layout::ensure(&paths)?;
    let Some(lock) = InstanceLock::try_acquire(&paths)? else {
        return Ok(());
    };
    let endpoint = Endpoint::locate(&paths.data)?.ok_or_else(|| {
        Error::Internal(format!(
            "the data folder {} disappeared while the supervisor started",
            paths.data.display()
        ))
    })?;
    let mut listener = Listener::bind(&endpoint, &lock)?;
    let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));

    let served = serve(&mut listener, &permits, &endpoint, paths).await;

    // Only the lock holder may remove the socket, so the socket goes first.
    // The lock is released right after, so a new supervisor can start while
    // the last replies (including the one to `shutdown`) are read.
    let closed = listener.close();
    drop(lock);
    if served.is_ok() {
        log::info("stopped listening; every service is stopped");
    }
    drain(&permits).await;
    served.and(closed)
}

/// Startup after the bind (signal handlers, registry, adoption), then the
/// accept loop. Returns once a shutdown has stopped every service.
async fn serve(
    listener: &mut Listener,
    permits: &Arc<Semaphore>,
    endpoint: &Endpoint,
    paths: Paths,
) -> Result<()> {
    let mut signals = sys::Signals::install()?;
    let specs = registry::load(&paths)?;
    let manager = Manager::new(paths, specs)?;
    for outcome in manager.adopt().await? {
        log_adoption(&outcome);
    }

    let (shutdowns, mut requests) = mpsc::channel(MAX_CONNECTIONS);
    let shared = Arc::new(Shared {
        manager,
        hello: hello_reply(endpoint),
        stopping: AtomicBool::new(false),
        shutdowns,
    });
    log::info(format_args!(
        "started: version {}, pid {}, {:?} build, data folder {}, listening on {endpoint}, \
         {} services registered",
        shared.hello.version,
        shared.hello.pid,
        shared.hello.build,
        endpoint.data.display(),
        shared.manager.status().len(),
    ));

    loop {
        let event = tokio::select! {
            accepted = listener.accept() => Event::Accepted(accepted),
            signal = signals.recv() => Event::Shutdown {
                cause: format!("{signal} received"),
                answer: None,
            },
            Some(request) = requests.recv() => Event::Shutdown {
                cause: format!("{} asked", request.client),
                answer: Some(request.answer),
            },
        };
        match event {
            Event::Accepted(accepted) => admit(accepted, permits, &shared).await,
            Event::Shutdown { cause, answer } => {
                if shut_down(&shared, &cause, answer).await {
                    // Every service is stopped, so queued and later requests
                    // already have their answer.
                    requests.close();
                    while let Ok(request) = requests.try_recv() {
                        let _ = request.answer.send(Ok(()));
                    }
                    return Ok(());
                }
            }
        }
    }
}

/// What the accept loop woke up for.
enum Event {
    Accepted(Result<Connection>),
    Shutdown {
        cause: String,
        /// The connection waiting for the outcome, for a `shutdown` request.
        answer: Option<oneshot::Sender<Result<()>>>,
    },
}

/// A `shutdown` request, passed from its connection to the accept loop.
struct ShutdownRequest {
    client: String,
    answer: oneshot::Sender<Result<()>>,
}

/// What every connection shares.
struct Shared {
    manager: Manager,
    hello: HelloReply,
    /// Set while a shutdown stops the services; `start` is refused meanwhile,
    /// so a shutdown cannot leave a service running behind it.
    stopping: AtomicBool,
    shutdowns: mpsc::Sender<ShutdownRequest>,
}

/// Stops every service. True when all stopped; otherwise the supervisor
/// stays up and serves again, and `answer` gets the error.
async fn shut_down(
    shared: &Shared,
    cause: &str,
    answer: Option<oneshot::Sender<Result<()>>>,
) -> bool {
    // The accept loop does not run while the services stop: no new
    // connections are taken, and the open ones are still served.
    log::info(format_args!("shutting down: {cause}"));
    shared.stopping.store(true, Ordering::SeqCst);
    let stopped = shared.manager.stop_all().await;
    let done = stopped.is_ok();
    if let Err(error) = &stopped {
        log::error(format_args!(
            "the shutdown failed, so the supervisor keeps running: {error}"
        ));
        shared.stopping.store(false, Ordering::SeqCst);
    }
    if let Some(answer) = answer {
        let _ = answer.send(stopped);
    }
    done
}

/// Hands a new connection to its own task, within [`MAX_CONNECTIONS`].
async fn admit(accepted: Result<Connection>, permits: &Arc<Semaphore>, shared: &Arc<Shared>) {
    match accepted {
        Ok(connection) => match Arc::clone(permits).try_acquire_owned() {
            Ok(permit) => {
                tokio::spawn(converse(connection, Arc::clone(shared), permit));
            }
            // Dropping the connection closes it, unanswered.
            Err(_) => log::warn(format_args!(
                "refused a connection: {MAX_CONNECTIONS} are already open"
            )),
        },
        Err(Error::PeerRejected) => log::warn("refused a connection from another user"),
        Err(error) => {
            log::error(error);
            tokio::time::sleep(ACCEPT_RETRY).await;
        }
    }
}

/// Serves one connection until the client closes it or goes quiet.
///
/// The first frame must come within [`FIRST_FRAME_TIMEOUT`], later ones within
/// [`IDLE_TIMEOUT`]. The client closes first, so the server reads until EOF
/// and never closes a connection whose reply may still be unread.
async fn converse(mut connection: Connection, shared: Arc<Shared>, _permit: OwnedSemaphorePermit) {
    let mut client = "a client".to_owned();
    let mut wait = FIRST_FRAME_TIMEOUT;
    loop {
        let received = timeout(wait, connection.recv::<Request>(MAX_REQUEST_BYTES)).await;
        let request = match received {
            Ok(Ok(Some(request))) => request,
            // Closed, as clients do after their last reply, or silent too long.
            Ok(Ok(None)) | Err(_) => return,
            Ok(Err(error)) => {
                refuse(&mut connection, &client, &error).await;
                return;
            }
        };
        wait = IDLE_TIMEOUT;
        if let Request::Hello { client: name, .. } = &request {
            client = name.chars().take(CLIENT_NAME_CHARS).collect();
        }
        let response = dispatch(request, &shared, &client).await;
        if let Err(error) = reply(&mut connection, &response).await {
            log::warn(format_args!("cannot answer {client}: {error}"));
            return;
        }
    }
}

/// A request that could not be read. When the client can be told why (a
/// frame over the limit or malformed JSON) it gets an error reply; then the
/// connection closes, because after an unread body the stream is out of step.
async fn refuse(connection: &mut Connection, client: &str, error: &Error) {
    log::warn(format_args!("dropped a connection from {client}: {error}"));
    if matches!(error, Error::FrameTooLarge { .. } | Error::Malformed(_)) {
        let response: Response<Empty> = Response::Err(ErrorBody::from(error));
        let _ = reply(connection, &response).await;
    }
}

/// Sends a reply within [`IDLE_TIMEOUT`], so a client that stops reading
/// cannot hold its connection forever. A reply over the size limit is
/// replaced by an error.
async fn reply<T: Serialize>(connection: &mut Connection, response: &Response<T>) -> Result<()> {
    let sent = send_within(connection, response).await;
    match sent {
        Err(Error::FrameTooLarge { len, limit }) => {
            let too_large: Response<Empty> = Response::Err(ErrorBody {
                code: ErrorCode::Internal,
                message: format!("the reply of {len} bytes is over the {limit}-byte limit"),
            });
            send_within(connection, &too_large).await
        }
        other => other,
    }
}

async fn send_within<T: Serialize>(connection: &mut Connection, message: &T) -> Result<()> {
    timeout(IDLE_TIMEOUT, connection.send(message, MAX_RESPONSE_BYTES))
        .await
        .map_err(|_| Error::Timeout("the client to read its reply"))?
}

/// The `data` of a reply, whichever request it answers.
#[derive(Serialize)]
#[serde(untagged)]
enum Data {
    Hello(HelloReply),
    Status(StatusReply),
    Service(ServiceStatus),
    Empty(Empty),
}

async fn dispatch(request: Request, shared: &Shared, client: &str) -> Response<Data> {
    let result = match request {
        // Answered whatever protocol the client speaks: the client decides,
        // and may still send `shutdown`, which never changes.
        Request::Hello { protocol, .. } => {
            if protocol != PROTOCOL {
                log::warn(format_args!(
                    "{client} speaks protocol {protocol}, this supervisor {PROTOCOL}"
                ));
            }
            Ok(Data::Hello(shared.hello.clone()))
        }
        Request::Status {} => Ok(Data::Status(StatusReply {
            services: shared.manager.status(),
        })),
        Request::Start { service } => shared.start(&service, client).await.map(Data::Service),
        Request::Stop { service } => shared.stop(&service, client).await.map(Data::Service),
        Request::Shutdown {} => shared
            .shutdown(client)
            .await
            .map(|()| Data::Empty(Empty {})),
    };
    match result {
        Ok(data) => Response::Ok(data),
        Err(error) => Response::Err(ErrorBody::from(&error)),
    }
}

impl Shared {
    async fn start(&self, service: &str, client: &str) -> Result<ServiceStatus> {
        // There is no `.await` between this check and the manager taking the
        // service's operation lock, and the runtime has one thread. So a
        // shutdown either refuses this start, or its `stop_all` waits for it
        // and then stops the service.
        if self.stopping.load(Ordering::SeqCst) {
            return Err(Error::SupervisorUnavailable {
                detail: "it is shutting down".to_owned(),
            });
        }
        log::info(format_args!("{client} asked to start {service}"));
        let result = self.manager.start(service).await;
        log_outcome("start", service, &result);
        result
    }

    async fn stop(&self, service: &str, client: &str) -> Result<ServiceStatus> {
        log::info(format_args!("{client} asked to stop {service}"));
        let result = self.manager.stop(service).await;
        log_outcome("stop", service, &result);
        result
    }

    /// Asks the accept loop to shut down, and waits for the outcome.
    async fn shutdown(&self, client: &str) -> Result<()> {
        let (answer, outcome) = oneshot::channel();
        let request = ShutdownRequest {
            client: client.to_owned(),
            answer,
        };
        if self.shutdowns.send(request).await.is_err() {
            // The queue closes only after every service has stopped.
            return Ok(());
        }
        outcome.await.unwrap_or_else(|_| {
            Err(Error::Internal(
                "the shutdown ended without an outcome".to_owned(),
            ))
        })
    }
}

fn hello_reply(endpoint: &Endpoint) -> HelloReply {
    HelloReply {
        protocol: PROTOCOL,
        version: env!("CARGO_PKG_VERSION").to_owned(),
        pid: std::process::id(),
        build: BuildKind::current(),
        data_dir: endpoint.data.clone(),
    }
}

fn log_adoption(outcome: &RecordOutcome) {
    match outcome {
        RecordOutcome::Adopted(id) => {
            log::info(format_args!(
                "adopted {id}, still running from an earlier supervisor"
            ));
        }
        RecordOutcome::Discarded { file, reason } => {
            log::warn(format_args!("discarded {}: {reason}", file.display()));
        }
    }
}

fn log_outcome(action: &str, service: &str, result: &Result<ServiceStatus>) {
    match result {
        Ok(status) => log::info(format_args!(
            "{action} {service}: {:?}, pid {:?}",
            status.state, status.pid
        )),
        Err(error) => log::warn(format_args!("{action} {service}: {error}")),
    }
}

/// Leaves the caller's session and drops what the caller handed down, for a
/// supervisor that `client::ensure` spawned (`ck supervisor run --detached`).
///
/// On Unix it calls `setsid` (which fails for a process that already leads a
/// group, as one typed in a shell does), then closes every inherited
/// descriptor above standard error, such as a `flock` the caller holds. Call
/// it first in `main`, while the process has one thread and owns no other
/// descriptor. On Windows `ensure` already detached it through its creation
/// flags, so there is nothing to do.
pub fn detach() -> Result<()> {
    sys::detach()
}

/// Waits, up to [`DRAIN_WAIT`], for every connection task to end. Those still
/// open are cancelled when the caller drops the runtime.
async fn drain(permits: &Semaphore) {
    let all = u32::try_from(MAX_CONNECTIONS).unwrap_or(u32::MAX);
    if timeout(DRAIN_WAIT, permits.acquire_many(all))
        .await
        .is_err()
    {
        log::warn("closed connections that were still open");
    }
}

#[cfg(unix)]
use self::unix as sys;
#[cfg(windows)]
use self::windows as sys;

#[cfg(unix)]
mod unix {
    use tokio::signal::unix::{Signal, SignalKind, signal};

    use crate::error::{Error, Result};

    /// `ensure` spawned this process without a process group of its own, so
    /// `setsid` can make it a session leader.
    pub(super) fn detach() -> Result<()> {
        ck_sys::unix::new_session()
            .map_err(|e| Error::os("leave the caller's session (setsid)", e))?;
        ck_sys::unix::close_inherited_fds()
            .map_err(|e| Error::os("close the descriptors inherited from the caller", e))
    }

    /// `SIGTERM` and `SIGINT`, each a request to shut down.
    pub(super) struct Signals {
        terminate: Signal,
        interrupt: Signal,
    }

    impl Signals {
        pub(super) fn install() -> Result<Self> {
            Ok(Self {
                terminate: signal(SignalKind::terminate())
                    .map_err(|e| Error::os("handle SIGTERM", e))?,
                interrupt: signal(SignalKind::interrupt())
                    .map_err(|e| Error::os("handle SIGINT", e))?,
            })
        }

        /// Waits for the next one and names it. Cancel safe.
        pub(super) async fn recv(&mut self) -> &'static str {
            tokio::select! {
                _ = self.terminate.recv() => "SIGTERM",
                _ = self.interrupt.recv() => "SIGINT",
            }
        }
    }
}

#[cfg(windows)]
mod windows {
    use tokio::signal::windows::{CtrlBreak, CtrlC, CtrlClose, ctrl_break, ctrl_c, ctrl_close};

    use crate::error::{Error, Result};

    /// Nothing to do: `ensure` spawned this process already detached, through
    /// its creation flags.
    pub(super) fn detach() -> Result<()> {
        Ok(())
    }

    /// Ctrl+C, Ctrl+Break and closing the console, each a request to shut down.
    pub(super) struct Signals {
        ctrl_c: CtrlC,
        ctrl_break: CtrlBreak,
        ctrl_close: CtrlClose,
    }

    impl Signals {
        pub(super) fn install() -> Result<Self> {
            Ok(Self {
                ctrl_c: ctrl_c().map_err(|e| Error::os("handle Ctrl+C", e))?,
                ctrl_break: ctrl_break().map_err(|e| Error::os("handle Ctrl+Break", e))?,
                ctrl_close: ctrl_close().map_err(|e| Error::os("handle closing the console", e))?,
            })
        }

        /// Waits for the next one and names it. Cancel safe.
        pub(super) async fn recv(&mut self) -> &'static str {
            tokio::select! {
                _ = self.ctrl_c.recv() => "Ctrl+C",
                _ = self.ctrl_break.recv() => "Ctrl+Break",
                _ = self.ctrl_close.recv() => "console close",
            }
        }
    }
}
