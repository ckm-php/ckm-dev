//! The supervisor and `ck status`, `start` and `stop`, driven by the dummy
//! service in an isolated profile on free ports.
//!
//! Debug builds only: a release build ignores the test-profile variables, so
//! these tests would reach the user's real data folder.
#![cfg(debug_assertions)]

mod support;

use std::fs;
use std::net::{Ipv4Addr, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use ck_core::ErrorCode;
use ck_core::service::{ServiceId, ServiceState};

use support::{Proc, Program, TestProfile, dummy, free_ports, hold_port, wait_until};

/// How long a process may take to go after it was stopped or killed.
const EXIT_WAIT: Duration = Duration::from_secs(10);

#[test]
fn start_spawns_one_supervisor_and_supervisor_stop_ends_everything() {
    let [port_one, port_two] = free_ports();
    let profile = TestProfile::new(vec![dummy("one", port_one), dummy("two", port_two)]);

    assert_eq!(
        profile.status().supervisor,
        None,
        "status never starts the supervisor"
    );

    // Both race to start the supervisor, with their output piped: `ck` fails
    // the test if a supervisor it started kept the pipes open. They start two
    // services, because a second start of one service may get `busy`.
    let (one, two) = thread::scope(|scope| {
        let one = scope.spawn(|| profile.start("one"));
        let two = scope.spawn(|| profile.start("two"));
        (one.join(), two.join())
    });
    let (one, two) = (one.expect("start one"), two.expect("start two"));
    assert_eq!(one.state, ServiceState::Running, "{one:?}");
    assert_eq!(two.state, ServiceState::Running, "{two:?}");

    // A supervisor reports only the services it started, so both in one
    // reply means one supervisor.
    let status = profile.status();
    let supervisor = status.supervisor.expect("a supervisor runs");
    for started in [&one, &two] {
        let now = status.service(started.id.as_str());
        assert_eq!((now.state, now.pid), (ServiceState::Running, started.pid));
    }
    let supervisor = profile.process(supervisor, Program::Ck);
    let two = profile.process(
        two.pid.expect("a running service has a pid"),
        Program::TestService,
    );

    let stopped = profile.stop("one");
    assert_eq!((stopped.state, stopped.pid), (ServiceState::Stopped, None));

    // Before `supervisor stop`, which it would otherwise start again.
    profile.fails(&["start", "unknown", "--json"], ErrorCode::UnknownService);

    let stopped = profile.ok(&["supervisor", "stop", "--json"]);
    assert_eq!(stopped["was_running"].as_bool(), Some(true), "{stopped}");
    wait_until(EXIT_WAIT, "the supervisor and its service to exit", || {
        !supervisor.is_alive() && !two.is_alive()
    });
    assert_eq!(profile.status().supervisor, None);
}

#[test]
fn a_busy_port_fails_the_start_and_is_never_reassigned() {
    let listener = hold_port();
    let port = listener.local_addr().expect("read the held port").port();
    let mut profile = TestProfile::new(vec![dummy("svc", port)]);
    profile.run_supervisor();

    let error = profile.fails(&["start", "svc", "--json"], ErrorCode::PortInUse);
    assert!(
        error.message.contains(&port.to_string()),
        "the message names the port: {error:?}"
    );
    let service = profile.status().service("svc");
    assert_eq!(
        (service.state, service.pid, service.port),
        (ServiceState::Stopped, None, port)
    );

    // The test's listener still holds the port and accepts.
    let _client =
        TcpStream::connect((Ipv4Addr::LOCALHOST, port)).expect("connect to the test's listener");
    listener
        .accept()
        .expect("the test's listener still accepts");
}

/// Starts `svc` under a supervisor the test runs, then kills that supervisor
/// as a crash would.
fn crash_supervisor_with_a_running_service() -> (TestProfile, Proc) {
    let [port] = free_ports();
    let mut profile = TestProfile::new(vec![dummy("svc", port)]);
    profile.run_supervisor();
    let started = profile.start("svc");
    assert_eq!(started.state, ServiceState::Running, "{started:?}");
    let service = profile.process(
        started.pid.expect("a running service has a pid"),
        Program::TestService,
    );
    profile.kill_supervisor();
    (profile, service)
}

#[cfg(unix)]
#[test]
fn a_new_supervisor_adopts_a_service_that_outlived_the_old_one() {
    let (mut profile, service) = crash_supervisor_with_a_running_service();
    assert!(
        service.is_alive(),
        "the service must outlive the supervisor"
    );

    profile.run_supervisor();
    let adopted = profile.status().service("svc");
    assert_eq!(
        (adopted.state, adopted.pid, adopted.adopted),
        (ServiceState::Running, Some(service.pid()), true)
    );

    let stopped = profile.stop("svc");
    assert_eq!(stopped.state, ServiceState::Stopped, "{stopped:?}");
    wait_until(EXIT_WAIT, "the adopted service to exit", || {
        !service.is_alive()
    });
}

#[cfg(windows)]
#[test]
fn services_end_with_a_crashed_supervisor_and_their_records_are_deleted() {
    let (mut profile, service) = crash_supervisor_with_a_running_service();
    wait_until(EXIT_WAIT, "the service to end with its Job", || {
        !service.is_alive()
    });
    let record = profile
        .paths()
        .record_file(&ServiceId::new("svc").expect("a valid id"));
    assert!(record.exists(), "the crashed supervisor left its record");

    profile.run_supervisor();
    assert!(
        !record.exists(),
        "the new supervisor deletes the stale record"
    );
    let service = profile.status().service("svc");
    assert_eq!(
        (service.state, service.pid, service.adopted),
        (ServiceState::Stopped, None, false)
    );
}

#[cfg(unix)]
#[test]
fn a_forged_record_is_discarded_and_its_process_never_signalled() {
    use std::process::{Child, Command};

    use ck_core::service::identity::{self, ProcessIdentity};
    use ck_core::service::record::{self, RECORD_SCHEMA, ServiceRecord};
    use ck_sys::unix::Pid;

    /// The test's own child, killed and reaped even when an assertion fails.
    struct OwnChild(Child);

    impl Drop for OwnChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    let [port] = free_ports();
    let mut profile = TestProfile::new(vec![dummy("svc", port)]);
    let mut sleeper =
        OwnChild(support::spawn(Command::new("sleep").arg("300")).expect("run sleep"));
    let pid = Pid::new(sleeper.0.id()).expect("a child's pid");
    let real = identity::read(pid)
        .expect("read the child's identity")
        .expect("the child runs");

    // Same id and port as the registered service, so only the start time
    // tells the process apart.
    let id = ServiceId::new("svc").expect("a valid id");
    let forged = ServiceRecord {
        schema: RECORD_SCHEMA,
        service: id.clone(),
        port,
        pid: pid.get(),
        identity: Some(ProcessIdentity {
            start_time: real.start_time.wrapping_add(1),
            ..real
        }),
    };
    record::write(profile.paths(), &forged).expect("write the forged record");

    profile.run_supervisor();

    assert!(
        !profile.paths().record_file(&id).exists(),
        "the forged record is deleted"
    );
    let service = profile.status().service("svc");
    assert_eq!(
        (service.state, service.pid, service.adopted),
        (ServiceState::Stopped, None, false)
    );
    assert!(
        sleeper.0.try_wait().expect("check the child").is_none(),
        "the test's own child was signalled"
    );
}

#[test]
fn a_service_that_ignores_stop_is_killed_unless_it_is_protected() {
    // Long enough for the Windows stop command to start and deliver its
    // message inside the grace time.
    const GRACE_MS: u64 = 2_000;
    let [plain_port, guarded_port] = free_ports();
    let mut plain = dummy("plain", plain_port);
    let mut guarded = dummy("guarded", guarded_port);
    for service in [&mut plain, &mut guarded] {
        service.args.push("--ignore-stop".to_owned());
        service.stop_timeout_ms = GRACE_MS;
    }
    guarded.protected = true;
    let mut profile = TestProfile::new(vec![plain, guarded]);
    profile.run_supervisor();

    // An ordinary service is killed once the grace time has passed.
    let started = profile.start("plain");
    let plain = profile.process(
        started.pid.expect("a running service has a pid"),
        Program::TestService,
    );
    let asked = Instant::now();
    let stopped = profile.stop("plain");
    assert!(
        asked.elapsed() >= Duration::from_millis(GRACE_MS),
        "it was killed before the grace time"
    );
    assert_eq!(stopped.state, ServiceState::Stopped, "{stopped:?}");
    wait_until(EXIT_WAIT, "the killed service to exit", || {
        !plain.is_alive()
    });
    // The graceful step reached it (a signal on Unix, the stop command on
    // Windows) and was ignored.
    let log_file = profile
        .paths()
        .service_log(&ServiceId::new("plain").expect("a valid id"));
    let log = fs::read_to_string(log_file).expect("read the service log");
    assert!(log.contains("ignored stop"), "{log}");

    // A protected service is never killed.
    let started = profile.start("guarded");
    let guarded = profile.process(
        started.pid.expect("a running service has a pid"),
        Program::TestService,
    );
    let error = profile.fails(&["stop", "guarded", "--json"], ErrorCode::StopFailed);
    assert!(
        guarded.is_alive(),
        "a protected service was killed: {error:?}"
    );
    let service = profile.status().service("guarded");
    assert_eq!(
        (service.state, service.pid),
        (ServiceState::Stopping, Some(guarded.pid()))
    );

    // Ended here by pid, so that the drop guard's `supervisor stop` can finish.
    guarded.kill();
    wait_until(EXIT_WAIT, "the protected service to exit", || {
        !guarded.is_alive()
    });
}

#[test]
fn a_service_that_exits_while_starting_fails_the_start() {
    let [port] = free_ports();
    let mut service = dummy("svc", port);
    // It exits long before it would listen, so readiness can only see the
    // exit.
    service
        .args
        .extend(["--ready-delay-ms", "5000", "--exit-after-ms", "100"].map(str::to_owned));
    let mut profile = TestProfile::new(vec![service]);
    profile.run_supervisor();

    profile.fails(&["start", "svc", "--json"], ErrorCode::StartFailed);
    let service = profile.status().service("svc");
    assert_eq!((service.state, service.pid), (ServiceState::Failed, None));
}
