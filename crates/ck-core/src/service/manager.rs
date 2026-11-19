//! Starts, stops and watches service processes.
//!
//! **Start:**
//! 1. Port check: bind `127.0.0.1:port` and drop it; on macOS and Windows also
//!    try a connect. Busy gives `port_in_use`; the port is never changed.
//! 2. Spawn with an argument list and a cleared environment plus a short
//!    allow-list, in its own process group (Unix) or a kill-on-close Job
//!    (Windows). If the Job cannot be assigned, the child is terminated and the
//!    start fails. Output goes to the service's log.
//! 3. Write the identity record.
//! 4. Readiness: a TCP connect every [`READY_POLL_INTERVAL`] while the child is
//!    alive, up to the spec's `ready_timeout`.
//!
//! **Stop:**
//! 1. The graceful step from the spec.
//! 2. Wait `stop_timeout`.
//! 3. Still alive and protected: `stop_failed`, and it stays `stopping`.
//! 4. Otherwise kill the group (Unix) or terminate the Job (Windows), then wait
//!    up to [`KILL_WAIT`].
//!
//! One operation per service at a time; a second one gets `busy`. `status`
//! reads a snapshot and never waits.
//!
//! **Watching.** A task owns each spawned child and notices its exit: a
//! `running` service becomes `failed`, and a `stopping` one `stopped`. Adopted
//! processes are checked every [`ADOPTED_CHECK_INTERVAL`] instead. A start that
//! fails after the spawn ends the child with the stop sequence, so a protected
//! service that will not stop stays `stopping` with its record. No state that
//! allows a new start (`stopped`, `failed`) ever has a live process.
//!
//! The manager expects `layout::ensure` to have created the data folders.

use std::collections::BTreeSet;
use std::path::PathBuf;
#[cfg(unix)]
use std::sync::Weak;
use std::sync::{Arc, Mutex as StdMutex, MutexGuard, PoisonError};
use std::time::Duration;

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::sync::{Mutex, OwnedMutexGuard};
use tokio::task::JoinHandle;

#[cfg(unix)]
use ck_sys::unix::Pid;

use crate::error::{Error, Result};
use crate::ipc::protocol::ServiceStatus;
use crate::paths::Paths;
#[cfg(unix)]
use crate::service::identity;
use crate::service::identity::ProcessIdentity;
use crate::service::process::{self, Handle, Readiness, Spawned};
use crate::service::record::{self, RECORD_SCHEMA, ScannedRecord, ServiceRecord};
use crate::service::spec::{ServiceId, ServiceSpec};
use crate::service::state::ServiceState;

/// How often readiness is tried while a service starts.
pub const READY_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// How long to wait for exit after a force-kill.
pub const KILL_WAIT: Duration = Duration::from_secs(5);

/// How often adopted processes are checked for exit. The timer runs only while
/// adopted services exist.
pub const ADOPTED_CHECK_INTERVAL: Duration = Duration::from_secs(2);

/// What [`Manager::adopt`] did with one record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordOutcome {
    /// The process matched its record and now counts as running (Unix).
    Adopted(ServiceId),
    /// The file was deleted and nothing was signalled.
    Discarded { file: PathBuf, reason: String },
}

/// Owns every service. It is `Send + Sync`, shared between connections
/// behind an `Arc`.
#[derive(Debug)]
pub struct Manager {
    paths: Arc<Paths>,
    /// In the order of the specs; `stop_all` walks it backwards.
    services: Vec<Arc<Service>>,
}

impl Manager {
    /// Registers `specs`, each validated, with unique ids. Every service
    /// starts `stopped`.
    pub fn new(paths: Paths, specs: Vec<ServiceSpec>) -> Result<Self> {
        let mut ids = BTreeSet::new();
        let mut services = Vec::with_capacity(specs.len());
        for spec in specs {
            spec.validate()?;
            if !ids.insert(spec.id.clone()) {
                return Err(Error::InvalidSpec {
                    service: spec.id.to_string(),
                    reason: "it is registered twice".to_owned(),
                });
            }
            services.push(Arc::new(Service::new(spec)));
        }
        Ok(Self {
            paths: Arc::new(paths),
            services,
        })
    }

    /// Reads the identity records once, at supervisor start.
    ///
    /// On Unix, a record whose id is registered and whose identity matches the
    /// live process is adopted. Every other record is deleted and nothing is
    /// signalled. On Windows every record is deleted.
    ///
    /// On Unix it also starts the liveness check of the adopted processes,
    /// which ends once none is left or the manager is dropped.
    pub async fn adopt(&self) -> Result<Vec<RecordOutcome>> {
        if self
            .services
            .iter()
            .any(|service| service.lock().process.is_some())
        {
            return Err(Error::Internal(
                "records are adopted only before any service starts".to_owned(),
            ));
        }
        let outcomes = record::scan(&self.paths)?
            .into_iter()
            .map(|scanned| self.adopt_one(scanned))
            .collect();
        #[cfg(unix)]
        self.watch_adopted();
        Ok(outcomes)
    }

    /// A snapshot of every service. Never waits for an operation.
    pub fn status(&self) -> Vec<ServiceStatus> {
        self.services
            .iter()
            .map(|service| service.lock().status.clone())
            .collect()
    }

    /// Starts `service`, or reports it as it is when it already runs.
    pub async fn start(&self, service: &str) -> Result<ServiceStatus> {
        let service = Arc::clone(self.find(service)?);
        let op = service.try_begin()?;
        join_op(tokio::spawn(start_service(
            service,
            Arc::clone(&self.paths),
            op,
        )))
        .await
    }

    /// Stops `service`, or reports it as it is when it is already stopped.
    pub async fn stop(&self, service: &str) -> Result<ServiceStatus> {
        let service = Arc::clone(self.find(service)?);
        let op = service.try_begin()?;
        join_op(tokio::spawn(stop_service(
            service,
            Arc::clone(&self.paths),
            op,
        )))
        .await
    }

    /// Stops every service, for shutdown. Fails with `stop_failed` when a
    /// protected service will not stop; the others are still stopped.
    pub async fn stop_all(&self) -> Result<()> {
        let mut first_error = None;
        for service in self.services.iter().rev() {
            // Shutdown waits for an operation in progress instead of failing
            // with `busy`.
            let op = Arc::clone(&service.op).lock_owned().await;
            let task = tokio::spawn(stop_service(
                Arc::clone(service),
                Arc::clone(&self.paths),
                op,
            ));
            if let Err(error) = join_op(task).await
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    fn find(&self, service: &str) -> Result<&Arc<Service>> {
        self.services
            .iter()
            .find(|candidate| candidate.spec.id.as_str() == service)
            .ok_or_else(|| Error::UnknownService(service.to_owned()))
    }

    fn adopt_one(&self, scanned: ScannedRecord) -> RecordOutcome {
        match self.try_adopt(scanned.record) {
            Ok(id) => RecordOutcome::Adopted(id),
            Err(reason) => discard(scanned.file, reason),
        }
    }

    /// Adopts the process a record describes, or says why not. Nothing is
    /// signalled either way.
    #[cfg(unix)]
    fn try_adopt(&self, record: Option<ServiceRecord>) -> std::result::Result<ServiceId, String> {
        let record = record.ok_or("it is not a valid record")?;
        let service = self
            .services
            .iter()
            .find(|service| service.spec.id == record.service)
            .ok_or("its service is not registered")?;
        if record.port != service.spec.port {
            return Err(format!(
                "it was recorded with port {}, but the service now uses port {}",
                record.port, service.spec.port
            ));
        }
        let identity = record.identity.ok_or("it has no process identity")?;
        let pid = Pid::new(record.pid).map_err(|error| error.to_string())?;
        identity::verify(pid, &identity).map_err(|error| error.to_string())?;
        service.lock().adopt(pid, identity);
        Ok(record.service)
    }

    /// Services on Windows end with the supervisor's Job, so every record is
    /// stale.
    #[cfg(windows)]
    fn try_adopt(&self, _record: Option<ServiceRecord>) -> std::result::Result<ServiceId, String> {
        Err("services do not outlive the supervisor on Windows".to_owned())
    }

    /// Checks the adopted processes every [`ADOPTED_CHECK_INTERVAL`] until
    /// none is left. It holds the services weakly, so it also ends with the
    /// manager.
    #[cfg(unix)]
    fn watch_adopted(&self) {
        let mut watched: Vec<Weak<Service>> = self
            .services
            .iter()
            .filter(|service| service.lock().adopted().is_some())
            .map(Arc::downgrade)
            .collect();
        if watched.is_empty() {
            return;
        }
        let paths = Arc::downgrade(&self.paths);
        tokio::spawn(async move {
            while !watched.is_empty() {
                tokio::time::sleep(ADOPTED_CHECK_INTERVAL).await;
                let Some(live_paths) = paths.upgrade() else {
                    return;
                };
                watched.retain(|service| {
                    service
                        .upgrade()
                        .is_some_and(|service| service.check_adopted(&live_paths))
                });
            }
        });
    }
}

/// Deletes a record file that was not adopted. Nothing is signalled.
fn discard(file: PathBuf, reason: String) -> RecordOutcome {
    let reason = match record::delete(&file) {
        Ok(()) => reason,
        Err(error) => format!("{reason}; deleting it failed: {error}"),
    };
    RecordOutcome::Discarded { file, reason }
}

/// One registered service.
#[derive(Debug)]
struct Service {
    spec: ServiceSpec,
    /// Held by the one start or stop in progress; a failed `try_lock` means
    /// `busy`.
    op: Arc<Mutex<()>>,
    /// The status snapshot and the current process. Every transition is made
    /// under this lock, and it is never held across an `.await`.
    inner: StdMutex<Inner>,
}

impl Service {
    fn new(spec: ServiceSpec) -> Self {
        let status = ServiceStatus {
            id: spec.id.clone(),
            state: ServiceState::Stopped,
            pid: None,
            port: spec.port,
            since: None,
            adopted: false,
            protected: spec.protected,
            last_error: None,
        };
        Self {
            spec,
            op: Arc::new(Mutex::new(())),
            inner: StdMutex::new(Inner {
                status,
                process: None,
                generations: 0,
            }),
        }
    }

    /// Every transition completes inside one critical section, so even a
    /// poisoned lock holds a consistent snapshot.
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn try_begin(&self) -> Result<OwnedMutexGuard<()>> {
        Arc::clone(&self.op)
            .try_lock_owned()
            .map_err(|_| Error::Busy {
                service: self.spec.id.to_string(),
            })
    }

    /// Called by the watcher task when the owned child `generation` exits.
    fn child_exited(&self, paths: &Paths, generation: u64, description: &str) {
        let mut inner = self.lock();
        if !inner.is_current(generation) {
            return;
        }
        if inner.status.state == ServiceState::Starting {
            // The start in progress sees this and ends the transition itself.
            if let Some(process) = inner.process.as_mut() {
                process.exited = Some(description.to_owned());
            }
        } else {
            inner.process_gone(paths, format!("it exited unexpectedly ({description})"));
        }
    }

    /// One liveness check of the adopted process. False once there is nothing
    /// left to watch.
    #[cfg(unix)]
    fn check_adopted(&self, paths: &Paths) -> bool {
        let Some((generation, pid, identity)) = self.lock().adopted() else {
            return false;
        };
        if !matches!(
            identity::verify(pid, &identity),
            Err(Error::IdentityMismatch { .. })
        ) {
            return true;
        }
        let mut inner = self.lock();
        if inner.is_current(generation) {
            inner.process_gone(paths, "it is no longer running".to_owned());
        }
        false
    }
}

#[derive(Debug)]
struct Inner {
    status: ServiceStatus,
    /// `Some` while a process may be alive, which is only in `starting`,
    /// `running` or `stopping`.
    process: Option<Process>,
    /// The last generation handed out.
    generations: u64,
}

#[derive(Debug)]
struct Process {
    /// Tells this process from a later one of the same service.
    generation: u64,
    handle: Handle,
    /// How an owned child exited while it was `starting`. The start in
    /// progress ends the transition.
    exited: Option<String>,
}

impl Inner {
    fn enter(&mut self, state: ServiceState) {
        self.status.state = state;
        self.status.since = now();
    }

    fn is_current(&self, generation: u64) -> bool {
        self.process
            .as_ref()
            .is_some_and(|process| process.generation == generation)
    }

    fn next_generation(&mut self) -> u64 {
        self.generations += 1;
        self.generations
    }

    fn install(&mut self, generation: u64, handle: Handle, pid: u32) {
        self.process = Some(Process {
            generation,
            handle,
            exited: None,
        });
        self.status.pid = Some(pid);
    }

    #[cfg(unix)]
    fn adopt(&mut self, pid: Pid, identity: ProcessIdentity) {
        let generation = self.next_generation();
        self.install(generation, Handle::Adopted { pid, identity }, pid.get());
        self.status.adopted = true;
        self.enter(ServiceState::Running);
    }

    /// The generation, pid and identity of the adopted process, if that is
    /// what runs.
    #[cfg(unix)]
    fn adopted(&self) -> Option<(u64, Pid, ProcessIdentity)> {
        match &self.process {
            Some(Process {
                generation,
                handle: Handle::Adopted { pid, identity },
                ..
            }) => Some((*generation, *pid, identity.clone())),
            _ => None,
        }
    }

    /// The current process has gone, and no operation is ending it: a
    /// `stopping` service is now `stopped`, any other `failed`.
    fn process_gone(&mut self, paths: &Paths, error: String) {
        if self.status.state == ServiceState::Stopping {
            self.finish(paths, ServiceState::Stopped, None);
        } else {
            self.finish(paths, ServiceState::Failed, Some(error));
        }
    }

    /// Ends the current process's entry: its record is deleted, and the
    /// service is `state` with no pid. `error` replaces `last_error` when set.
    fn finish(&mut self, paths: &Paths, state: ServiceState, error: Option<String>) {
        // A record that cannot be deleted is harmless: it no longer matches a
        // live process, so the next adoption deletes it.
        let _ = record::remove(paths, &self.status.id);
        self.process = None;
        self.status.pid = None;
        self.status.adopted = false;
        if error.is_some() {
            self.status.last_error = error;
        }
        self.enter(state);
    }
}

/// The current time in RFC 3339 UTC, for `since`.
fn now() -> Option<String> {
    OffsetDateTime::now_utc().format(&Rfc3339).ok()
}

fn start_failed(spec: &ServiceSpec, reason: String) -> Error {
    Error::StartFailed {
        service: spec.id.to_string(),
        reason,
    }
}

/// Waits for an operation's task. Operations run in their own task, so a
/// caller that gives up (a closed connection) cannot leave a service halfway
/// through a transition.
async fn join_op(task: JoinHandle<Result<ServiceStatus>>) -> Result<ServiceStatus> {
    task.await
        .map_err(|error| Error::Internal(format!("a service operation ended early: {error}")))?
}

async fn start_service(
    service: Arc<Service>,
    paths: Arc<Paths>,
    _op: OwnedMutexGuard<()>,
) -> Result<ServiceStatus> {
    let spec = &service.spec;
    {
        let mut inner = service.lock();
        match inner.status.state {
            ServiceState::Running => return Ok(inner.status.clone()),
            ServiceState::Starting | ServiceState::Stopping => {
                return Err(start_failed(
                    spec,
                    "an earlier stop has not finished; stop it again first".to_owned(),
                ));
            }
            ServiceState::Stopped | ServiceState::Failed => inner.enter(ServiceState::Starting),
        }
    }

    let mut spawned = match launch(spec, &paths).await {
        Ok(spawned) => spawned,
        Err(error) => {
            // Nothing was left running, so the service is `stopped`.
            let mut inner = service.lock();
            inner.status.last_error = Some(error.to_string());
            inner.enter(ServiceState::Stopped);
            return Err(error);
        }
    };
    if let Err(reason) = spawned.confine().await {
        return fail_start(&service, &paths, reason);
    }

    let pid = spawned.pid();
    // Read before the watcher takes the child: until then nothing can reap
    // it, so `pid` is still its own.
    let identity = identity_of(pid);
    let link = {
        let mut inner = service.lock();
        let generation = inner.next_generation();
        let watcher = Arc::clone(&service);
        let watcher_paths = Arc::clone(&paths);
        // The exit callback takes this lock, so it runs only after the
        // process is installed below.
        let link = process::start_watcher(spawned, move |description| {
            watcher.child_exited(&watcher_paths, generation, description);
        });
        inner.install(generation, Handle::Owned(link.clone()), pid);
        link
    };
    let handle = Handle::Owned(link.clone());

    let recorded = identity.and_then(|identity| {
        let record = ServiceRecord {
            schema: RECORD_SCHEMA,
            service: spec.id.clone(),
            port: spec.port,
            pid,
            identity,
        };
        record::write(&paths, &record).map_err(|error| format!("cannot write its record: {error}"))
    });
    if let Err(reason) = recorded {
        return abort_start(&service, &paths, &handle, reason).await;
    }

    match process::wait_ready(spec, &link).await {
        Readiness::Ready => promote(&service, &paths),
        Readiness::Exited(description) => fail_start(
            &service,
            &paths,
            exited_while_starting(spec, &paths, &description),
        ),
        Readiness::TimedOut => {
            let reason = format!(
                "it did not accept connections on port {} within {:?}",
                spec.port, spec.ready_timeout
            );
            abort_start(&service, &paths, &handle, reason).await
        }
    }
}

/// The steps up to and including the spawn. On an error no process is left
/// running.
async fn launch(spec: &ServiceSpec, paths: &Paths) -> Result<Spawned> {
    spec.validate()?;
    process::check_port(spec).await?;
    process::spawn(spec, paths)
}

/// The identity to record for a child that nothing has reaped yet.
#[cfg(unix)]
fn identity_of(pid: u32) -> std::result::Result<Option<ProcessIdentity>, String> {
    let pid = Pid::new(pid).map_err(|error| error.to_string())?;
    match identity::read(pid) {
        Ok(Some(identity)) => Ok(Some(identity)),
        Ok(None) => Err("it exited as soon as it started".to_owned()),
        Err(error) => Err(format!("cannot read its process identity: {error}")),
    }
}

/// Windows never adopts, so its records carry no identity.
#[cfg(windows)]
fn identity_of(_pid: u32) -> std::result::Result<Option<ProcessIdentity>, String> {
    Ok(None)
}

fn exited_while_starting(spec: &ServiceSpec, paths: &Paths, description: &str) -> String {
    format!(
        "it exited while starting ({description}); see {}",
        paths.service_log(&spec.id).display()
    )
}

/// Readiness passed: `running`, unless the child exited meanwhile.
fn promote(service: &Service, paths: &Paths) -> Result<ServiceStatus> {
    let mut inner = service.lock();
    let exited = match inner.process.as_ref() {
        Some(process) => process.exited.clone(),
        None => Some("its process has gone".to_owned()),
    };
    match exited {
        None => {
            inner.status.last_error = None;
            inner.enter(ServiceState::Running);
            Ok(inner.status.clone())
        }
        Some(description) => {
            let reason = exited_while_starting(&service.spec, paths, &description);
            inner.finish(paths, ServiceState::Failed, Some(reason.clone()));
            Err(start_failed(&service.spec, reason))
        }
    }
}

/// The start failed and no process of it is left: `failed`.
fn fail_start(service: &Service, paths: &Paths, reason: String) -> Result<ServiceStatus> {
    service
        .lock()
        .finish(paths, ServiceState::Failed, Some(reason.clone()));
    Err(start_failed(&service.spec, reason))
}

/// The start failed after the spawn: the child is stopped as a stop would do
/// it. If it is protected and will not stop, it keeps its record and the
/// service stays `stopping`.
async fn abort_start(
    service: &Service,
    paths: &Paths,
    handle: &Handle,
    reason: String,
) -> Result<ServiceStatus> {
    let halted = process::halt(&service.spec, paths, handle).await;
    let mut inner = service.lock();
    let alive = inner
        .process
        .as_ref()
        .is_some_and(|process| process.exited.is_none());
    match halted {
        Err(still_running) if alive => {
            let reason = format!("{reason}; {still_running}");
            inner.status.last_error = Some(reason.clone());
            inner.enter(ServiceState::Stopping);
            Err(start_failed(&service.spec, reason))
        }
        _ => {
            inner.finish(paths, ServiceState::Failed, Some(reason.clone()));
            Err(start_failed(&service.spec, reason))
        }
    }
}

async fn stop_service(
    service: Arc<Service>,
    paths: Arc<Paths>,
    _op: OwnedMutexGuard<()>,
) -> Result<ServiceStatus> {
    let (generation, handle) = {
        let mut inner = service.lock();
        let Some(process) = &inner.process else {
            return Ok(inner.status.clone());
        };
        let current = (process.generation, process.handle.clone());
        inner.enter(ServiceState::Stopping);
        current
    };
    let halted = process::halt(&service.spec, &paths, &handle).await;
    let mut inner = service.lock();
    match halted {
        // Still running (protected, so never killed, or the kill did not
        // work): it stays `stopping` with its record.
        Err(reason) if inner.is_current(generation) => {
            inner.status.last_error = Some(reason.clone());
            Err(Error::StopFailed {
                service: service.spec.id.to_string(),
                reason,
            })
        }
        _ => {
            // An owned child's watcher has usually finished this already.
            if inner.is_current(generation) {
                inner.finish(&paths, ServiceState::Stopped, None);
            }
            Ok(inner.status.clone())
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::process::{Child, Command};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::paths::Profile;
    use crate::service::spec::{
        DEFAULT_READY_TIMEOUT, DEFAULT_STOP_TIMEOUT, StopMethod, StopSignal,
    };

    /// Removes the test's folder, even when an assertion fails.
    struct TempDir(PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Kills and reaps the test's own child, even when an assertion fails.
    struct OwnChild(Child);

    impl Drop for OwnChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[tokio::test]
    async fn a_record_with_another_start_time_is_deleted_and_never_signalled() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = TempDir(
            std::env::temp_dir().join(format!("ck-core-t6-{}-{nanos}", std::process::id())),
        );
        let paths = Paths {
            home: root.0.clone(),
            data: root.0.join("data"),
            profile: Profile::Isolated,
        };
        std::fs::create_dir_all(paths.records_dir()).unwrap();

        let mut child = {
            let _spawns = crate::test_support::spawn_guard();
            OwnChild(Command::new("sleep").arg("30").spawn().unwrap())
        };
        let pid = Pid::new(child.0.id()).unwrap();
        let real = identity::read(pid).unwrap().expect("the child runs");
        identity::verify(pid, &real).unwrap();

        let forged = ProcessIdentity {
            start_time: real.start_time.wrapping_add(1),
            ..real
        };
        assert!(matches!(
            identity::verify(pid, &forged),
            Err(Error::IdentityMismatch { .. })
        ));

        // Registered with the same id and port, so only the identity can
        // decide.
        let id = ServiceId::new("sleeper").unwrap();
        let port = 47_999;
        record::write(
            &paths,
            &ServiceRecord {
                schema: RECORD_SCHEMA,
                service: id.clone(),
                port,
                pid: pid.get(),
                identity: Some(forged),
            },
        )
        .unwrap();
        let spec = ServiceSpec {
            id: id.clone(),
            program: PathBuf::from("/bin/sleep"),
            args: Vec::new(),
            env: Vec::new(),
            port,
            stop: StopMethod::Signal(StopSignal::Term),
            protected: false,
            ready_timeout: DEFAULT_READY_TIMEOUT,
            stop_timeout: DEFAULT_STOP_TIMEOUT,
        };
        let manager = Manager::new(paths.clone(), vec![spec]).unwrap();

        let outcomes = manager.adopt().await.unwrap();

        assert!(
            matches!(
                outcomes.as_slice(),
                [RecordOutcome::Discarded { file, reason }]
                    if *file == paths.record_file(&id) && reason.contains("not the recorded process")
            ),
            "{outcomes:?}"
        );
        assert!(!paths.record_file(&id).exists());
        let status = manager.status();
        assert_eq!(status[0].state, ServiceState::Stopped);
        assert_eq!(status[0].pid, None);
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "the test's child must not have been signalled"
        );
    }
}
