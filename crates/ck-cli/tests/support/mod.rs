//! Shared helpers for `ck`'s integration tests: an isolated test profile, the
//! dummy service's definition, free ports and processes.
//!
//! Every `ck` command runs with `CK_DEV_HOME` set on that command only,
//! pointing at the profile's own temporary folder. Before anything is created
//! or started, the profile checks that `ck` is a debug build in an isolated
//! profile whose data folder lies inside that folder.

mod process;
mod temp;

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read};
use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, PoisonError, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use ck_core::ipc::endpoint::Endpoint;
use ck_core::ipc::protocol::ServiceStatus;
use ck_core::layout;
use ck_core::paths::{DATA_VAR, HOME_VAR, Profile};
use ck_core::service::{ServiceId, StopSignal};
use ck_core::supervisor::registry::{DevService, DevServicesFile, DevStop, DevStopCommand};
use ck_core::{ErrorBody, ErrorCode, Paths};
use serde::de::DeserializeOwned;
use serde_json::Value;

pub use self::process::Proc;
use self::temp::{TempRoot, is_inside};

/// The longest any one `ck` command may run before it is killed.
const CK_DEADLINE: Duration = Duration::from_secs(30);

/// How long `ck`'s output pipes may stay open after it exits.
const PIPE_WAIT: Duration = Duration::from_secs(5);

/// How long a supervisor that a test runs may take to answer.
const SUPERVISOR_WAIT: Duration = Duration::from_secs(15);

const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Held across every spawn and every listener bind in the tests. Where the OS
/// has no atomic close-on-exec for pipes and sockets (macOS), a child spawned
/// by one test could otherwise inherit another test's pipe or listener.
static SPAWN_LOCK: Mutex<()> = Mutex::new(());

/// The binaries the tests run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Program {
    Ck,
    TestService,
}

impl Program {
    /// The absolute path cargo built it at.
    pub fn path(self) -> &'static Path {
        Path::new(match self {
            Self::Ck => env!("CARGO_BIN_EXE_ck"),
            Self::TestService => env!("CARGO_BIN_EXE_ck-test-service"),
        })
    }
}

/// Spawns `command` under [`SPAWN_LOCK`].
pub fn spawn(command: &mut Command) -> io::Result<Child> {
    let _guard = SPAWN_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    command.spawn()
}

/// A listener on a free loopback port, bound under [`SPAWN_LOCK`].
pub fn hold_port() -> TcpListener {
    let _guard = SPAWN_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind a free loopback port")
}

/// `N` different loopback ports that were free a moment ago. The listeners are
/// held together, so the OS cannot hand out one port twice.
pub fn free_ports<const N: usize>() -> [u16; N] {
    let listeners: [TcpListener; N] = std::array::from_fn(|_| hold_port());
    listeners.map(|listener| listener.local_addr().expect("read a free port").port())
}

/// The dummy service `id` on `port`, stopped by `SIGTERM` on Unix and by its
/// own `--stop` command on Windows, which has no signals. Tests add flags to
/// `args` and change the other fields as they need.
pub fn dummy(id: &str, port: u16) -> DevService {
    let program = Program::TestService.path().to_path_buf();
    let port_arg = port.to_string();
    let stop = if cfg!(windows) {
        DevStop::Command(DevStopCommand {
            program: program.clone(),
            args: vec!["--stop".to_owned(), "--port".to_owned(), port_arg.clone()],
        })
    } else {
        DevStop::Signal(StopSignal::Term)
    };
    DevService {
        id: ServiceId::new(id).unwrap_or_else(|error| panic!("{error}")),
        program,
        args: vec!["--port".to_owned(), port_arg],
        port,
        stop,
        protected: false,
        ready_timeout_ms: 10_000,
        stop_timeout_ms: 5_000,
    }
}

/// Polls `done` until it holds; panics after `limit`.
pub fn wait_until(limit: Duration, what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !done() {
        assert!(
            Instant::now() < deadline,
            "gave up after {limit:?} waiting for {what}"
        );
        thread::sleep(POLL_INTERVAL);
    }
}

/// What one `ck` command did.
pub struct CkOutput {
    args: String,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl fmt::Display for CkOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "`ck {}` exited with {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.args, self.code, self.stdout, self.stderr
        )
    }
}

impl CkOutput {
    /// The one JSON document on stdout, with `schema` 1.
    fn document(&self) -> Result<Value, String> {
        let document: Value = serde_json::from_str(&self.stdout)
            .map_err(|error| format!("expected one JSON document ({error}) from {self}"))?;
        if document["schema"].as_u64() != Some(1) {
            return Err(format!("expected schema 1 from {self}"));
        }
        Ok(document)
    }

    /// The document of a command that exited 0 with `"ok": true`.
    fn success(&self) -> Result<Value, String> {
        let document = self.document()?;
        if self.code != Some(0) || document["ok"].as_bool() != Some(true) {
            return Err(format!("expected success from {self}"));
        }
        Ok(document)
    }
}

/// `ck status --json`.
#[derive(Debug)]
pub struct Status {
    /// The supervisor's pid, when one runs.
    pub supervisor: Option<u32>,
    pub services: Vec<ServiceStatus>,
}

impl Status {
    pub fn service(&self, id: &str) -> ServiceStatus {
        self.services
            .iter()
            .find(|service| service.id.as_str() == id)
            .cloned()
            .unwrap_or_else(|| panic!("no service {id} in {self:?}"))
    }
}

/// A temporary isolated profile with its own data folder and supervisor.
///
/// Dropping it cleans up, in this order: `ck supervisor stop`, then the
/// supervisor child the test runs, then any service still running (by pid,
/// after its identity is checked again), then the profile and its `/tmp`
/// socket folder.
pub struct TestProfile {
    paths: Paths,
    /// The private `/tmp` folder that holds the socket when the data path is
    /// too long (Unix).
    fallback_dir: Option<PathBuf>,
    /// A supervisor this test runs as its own child.
    supervisor: Option<Child>,
    /// Where each such supervisor wrote its output.
    supervisor_outputs: Vec<PathBuf>,
    /// Every process the test saw, by identity, for the drop guard. Windows
    /// reuses pids quickly and its records hold no identity, so there the drop
    /// guard asks the supervisor again instead.
    #[cfg(unix)]
    seen: Mutex<Vec<Proc>>,
    /// Last, so that it is removed after everything else is dropped.
    root: TempRoot,
}

impl TestProfile {
    /// A new profile whose supervisor will run `services`. Nothing runs yet.
    pub fn new(services: Vec<DevService>) -> Self {
        if !cfg!(debug_assertions) {
            panic!("a test profile needs a debug build: a release build ignores CK_DEV_HOME");
        }
        let root = TempRoot::create();

        // Before anything is created: a debug `ck`, an isolated profile, and a
        // data folder inside the temporary folder.
        let output =
            run_ck(root.path(), &["paths", "--json"]).unwrap_or_else(|error| panic!("{error}"));
        let document = output.success().unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(document["build"].as_str(), Some("debug"), "{output}");
        assert_eq!(document["profile"].as_str(), Some("isolated"), "{output}");
        let path_of = |field: &str| {
            PathBuf::from(
                document[field]
                    .as_str()
                    .unwrap_or_else(|| panic!("no {field} path from {output}")),
            )
        };
        let (home, data) = (path_of("home"), path_of("data"));
        assert_eq!(home, root.path(), "{output}");
        assert!(
            is_inside(&data, root.path()),
            "the data folder must be inside {}: {output}",
            root.path().display()
        );

        let paths = Paths {
            home,
            data,
            profile: Profile::Isolated,
        };
        // The same private layout the supervisor creates, including the
        // protected DACL on Windows, which is only set on a new data folder.
        layout::ensure(&paths).unwrap_or_else(|error| panic!("cannot create the profile: {error}"));
        let file = serde_json::to_vec_pretty(&DevServicesFile { services })
            .expect("encode dev-services.json");
        layout::write_atomic(&paths.dev_services_file(), &file)
            .unwrap_or_else(|error| panic!("cannot write dev-services.json: {error}"));
        let fallback_dir = Endpoint::locate(&paths.data)
            .unwrap_or_else(|error| panic!("cannot locate the endpoint: {error}"))
            .and_then(|endpoint| endpoint.fallback_dir);

        Self {
            paths,
            fallback_dir,
            supervisor: None,
            supervisor_outputs: Vec::new(),
            #[cfg(unix)]
            seen: Mutex::new(Vec::new()),
            root,
        }
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    /// Runs `ck` in this profile with captured output and a deadline.
    pub fn ck(&self, args: &[&str]) -> CkOutput {
        run_ck(self.root.path(), args).unwrap_or_else(|error| panic!("{error}"))
    }

    /// Runs a `--json` command that must succeed, and returns its document.
    pub fn ok(&self, args: &[&str]) -> Value {
        self.ck(args)
            .success()
            .unwrap_or_else(|error| panic!("{error}"))
    }

    /// Runs a `--json` command that must fail with `code` and its exit code,
    /// and returns the error.
    pub fn fails(&self, args: &[&str], code: ErrorCode) -> ErrorBody {
        let output = self.ck(args);
        assert_eq!(output.code, Some(i32::from(code.exit_code())), "{output}");
        let document = output.document().unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(document["ok"].as_bool(), Some(false), "{output}");
        let error: ErrorBody =
            parse(&document["error"]).unwrap_or_else(|error| panic!("{error}: {output}"));
        assert_eq!(error.code, code, "{output}");
        error
    }

    /// `ck status --json`. Never starts a supervisor.
    pub fn status(&self) -> Status {
        let status = self.read_status().unwrap_or_else(|error| panic!("{error}"));
        self.remember(&status);
        status
    }

    /// `ck start <id> --json`, which must succeed.
    pub fn start(&self, id: &str) -> ServiceStatus {
        self.service_command("start", id)
    }

    /// `ck stop <id> --json`, which must succeed.
    pub fn stop(&self, id: &str) -> ServiceStatus {
        self.service_command("stop", id)
    }

    /// The live process `pid`, which runs `program`.
    pub fn process(&self, pid: u32, program: Program) -> Proc {
        Proc::observe(pid, program).unwrap_or_else(|| panic!("process {pid} is not running"))
    }

    /// Runs `ck supervisor run` as this test's child, with its output in a
    /// file, and waits until it answers.
    pub fn run_supervisor(&mut self) {
        assert!(self.supervisor.is_none(), "a supervisor child already runs");
        let output = self
            .root
            .path()
            .join(format!("supervisor-{}.out", self.supervisor_outputs.len()));
        let file = File::create(&output)
            .unwrap_or_else(|error| panic!("cannot create {}: {error}", output.display()));
        let file_for_errors = file.try_clone().expect("share the output file");
        self.supervisor_outputs.push(output);
        let mut command = ck_command(self.root.path(), &["supervisor", "run"]);
        command
            .stdin(Stdio::null())
            .stdout(file)
            .stderr(file_for_errors);
        let child = spawn(&mut command)
            .unwrap_or_else(|error| panic!("cannot run the supervisor: {error}"));
        let pid = child.id();
        self.supervisor = Some(child);

        let deadline = Instant::now() + SUPERVISOR_WAIT;
        while self.status().supervisor != Some(pid) {
            let exited = self
                .supervisor
                .as_mut()
                .and_then(|child| child.try_wait().ok().flatten());
            if let Some(exit) = exited {
                panic!("the supervisor exited ({exit}) before it answered");
            }
            assert!(
                Instant::now() < deadline,
                "the supervisor did not answer within {SUPERVISOR_WAIT:?}"
            );
            thread::sleep(POLL_INTERVAL);
        }
    }

    /// Kills the supervisor this test runs, as a crash would: `SIGKILL` on
    /// Unix, `TerminateProcess` on Windows. Its services are not asked to stop.
    pub fn kill_supervisor(&mut self) {
        let mut child = self.supervisor.take().expect("the test runs a supervisor");
        child.kill().expect("kill the supervisor");
        child.wait().expect("reap the supervisor");
    }

    fn service_command(&self, command: &str, id: &str) -> ServiceStatus {
        let document = self.ok(&[command, id, "--json"]);
        let service: ServiceStatus =
            parse(&document["service"]).unwrap_or_else(|error| panic!("{error}"));
        if let Some(pid) = service.pid {
            self.remember_pid(pid, Program::TestService);
        }
        service
    }

    /// `ck status --json` without panicking, for the drop guard too.
    fn read_status(&self) -> Result<Status, String> {
        let document = run_ck(self.root.path(), &["status", "--json"])?.success()?;
        let supervisor = match &document["supervisor"] {
            Value::Null => None,
            supervisor => Some(parse(&supervisor["pid"])?),
        };
        let services = parse(&document["services"])?;
        Ok(Status {
            supervisor,
            services,
        })
    }

    fn remember(&self, status: &Status) {
        if let Some(pid) = status.supervisor {
            self.remember_pid(pid, Program::Ck);
        }
        for pid in status.services.iter().filter_map(|service| service.pid) {
            self.remember_pid(pid, Program::TestService);
        }
    }

    /// Notes a process for the drop guard, by its identity now.
    #[cfg(unix)]
    fn remember_pid(&self, pid: u32, program: Program) {
        let Some(process) = Proc::observe(pid, program) else {
            return;
        };
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        if !seen.contains(&process) {
            seen.push(process);
        }
    }

    /// Windows keeps no list: see `seen`.
    #[cfg(windows)]
    fn remember_pid(&self, _pid: u32, _program: Program) {}

    /// Kills every process the test saw and every service with a record, each
    /// only while its pid still shows the identity seen or recorded.
    #[cfg(unix)]
    fn kill_leftovers(&mut self) {
        use ck_core::service::record;

        let recorded = record::scan(&self.paths)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|scanned| scanned.record)
            .filter_map(|record| Proc::from_record(&record));
        let seen = self.seen.get_mut().unwrap_or_else(PoisonError::into_inner);
        for process in seen.iter().cloned().chain(recorded) {
            process.kill();
        }
    }

    /// Kills only what a supervisor reports right now, after an image check:
    /// Windows reuses pids quickly. The services of a supervisor that has gone
    /// ended with its Jobs. A detached supervisor that neither stops nor
    /// answers is left behind.
    #[cfg(windows)]
    fn kill_leftovers(&mut self) {
        let Ok(status) = self.read_status() else {
            return;
        };
        let services = status
            .services
            .iter()
            .filter_map(|service| service.pid)
            .map(|pid| (pid, Program::TestService));
        let supervisor = status.supervisor.map(|pid| (pid, Program::Ck));
        for (pid, program) in services.chain(supervisor) {
            if let Some(process) = Proc::observe(pid, program) {
                process.kill();
            }
        }
    }

    /// Prints the supervisor's and the services' logs, for a failing test.
    fn print_logs(&self) {
        let mut files = vec![self.paths.supervisor_log()];
        files.extend(self.supervisor_outputs.iter().cloned());
        if let Ok(entries) = fs::read_dir(self.paths.logs_dir().join("services")) {
            files.extend(entries.flatten().map(|entry| entry.path()));
        }
        for file in files {
            if let Ok(text) = fs::read_to_string(&file) {
                eprintln!("--- {} ---\n{text}", file.display());
            }
        }
    }
}

impl Drop for TestProfile {
    fn drop(&mut self) {
        if thread::panicking() {
            self.print_logs();
        }
        // What runs now, before anything is stopped. Errors are ignored: no
        // supervisor may be running.
        if let Ok(status) = self.read_status() {
            self.remember(&status);
        }
        // 1. Stop every service, then the supervisor.
        let _ = run_ck(self.root.path(), &["supervisor", "stop"]);
        // 2. The supervisor this test runs, if it did not stop.
        if let Some(mut child) = self.supervisor.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        // 3. Any service still running, by pid.
        self.kill_leftovers();
        // 4. The socket folder in `/tmp`; `root` then removes the profile.
        if let Some(dir) = &self.fallback_dir {
            let _ = fs::remove_dir_all(dir);
        }
    }
}

/// `ck` with the profile's home set on this command only, and without any
/// `CK_DEV_DATA` from the test's own environment.
fn ck_command(home: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(Program::Ck.path());
    command.args(args).env(HOME_VAR, home).env_remove(DATA_VAR);
    command
}

/// Runs `ck` with piped output. It fails when `ck` outlives [`CK_DEADLINE`]
/// (it is then killed), or when its output pipes stay open after it exits,
/// which means a process it started inherited them.
fn run_ck(home: &Path, args: &[&str]) -> Result<CkOutput, String> {
    let shown = args.join(" ");
    let mut command = ck_command(home, args);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child =
        spawn(&mut command).map_err(|error| format!("cannot run `ck {shown}`: {error}"))?;
    let stdout = read_in_background(child.stdout.take());
    let stderr = read_in_background(child.stderr.take());

    let deadline = Instant::now() + CK_DEADLINE;
    let timed_out = loop {
        match child.try_wait() {
            Ok(None) if Instant::now() < deadline => thread::sleep(POLL_INTERVAL),
            Ok(None) => break true,
            Ok(Some(_)) | Err(_) => break false,
        }
    };
    if timed_out {
        let _ = child.kill();
    }
    let status = child
        .wait()
        .map_err(|error| format!("cannot wait for `ck {shown}`: {error}"))?;
    if timed_out {
        return Err(format!(
            "`ck {shown}` did not finish within {CK_DEADLINE:?}"
        ));
    }

    let still_open = || {
        format!(
            "`ck {shown}` exited ({status}), but its output stayed open: a process it started \
             inherited it"
        )
    };
    let stdout = stdout.recv_timeout(PIPE_WAIT).map_err(|_| still_open())?;
    let stderr = stderr.recv_timeout(PIPE_WAIT).map_err(|_| still_open())?;
    Ok(CkOutput {
        args: shown,
        code: status.code(),
        stdout,
        stderr,
    })
}

/// Reads a pipe to its end on another thread. The text arrives once every
/// process holding the pipe has closed it.
fn read_in_background(pipe: Option<impl Read + Send + 'static>) -> mpsc::Receiver<String> {
    let (sender, receiver) = mpsc::channel();
    if let Some(mut pipe) = pipe {
        thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            let _ = sender.send(String::from_utf8_lossy(&bytes).into_owned());
        });
    }
    receiver
}

fn parse<T: DeserializeOwned>(value: &Value) -> Result<T, String> {
    serde_json::from_value(value.clone())
        .map_err(|error| format!("unexpected JSON {value}: {error}"))
}
