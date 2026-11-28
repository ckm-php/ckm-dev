//! One handler per command. Each prints its own success output (text, or one
//! `output::Document` with `--json`); `main` prints errors and sets the exit
//! code.
//!
//! - `status`, `paths` and `supervisor stop` never start the supervisor.
//! - `start` and `stop` start it through `client::ensure`, and are idempotent.
//! - Text status: `Supervisor: running (pid N)` or `Supervisor: not running`,
//!   then a table with the columns `SERVICE STATE PID PORT SINCE`. `status`
//!   with no supervisor is not an error.

use ck_core::ipc::endpoint::Endpoint;
use ck_core::ipc::protocol::ServiceStatus;
use ck_core::paths::BuildKind;
use ck_core::{ErrorCode, Paths, Result, client, supervisor};

use crate::output::{self, PathsBody, ServiceBody, StatusBody, SupervisorBody, SupervisorStopBody};

#[cfg(feature = "net")]
use std::time::SystemTime;

#[cfg(feature = "net")]
use crate::output::{InstallBody, InstalledBody, RuntimeBody};
#[cfg(feature = "net")]
use ck_core::catalog::trust::{CATALOG_URL, Trust};
#[cfg(feature = "net")]
use ck_core::download::transport::Transport;
#[cfg(feature = "net")]
use ck_core::layout;
#[cfg(feature = "net")]
use ck_core::runtime::RuntimeService;

/// `ck status [--json]`
pub async fn status(paths: &Paths, json: bool) -> Result<()> {
    let body = match client::connect(paths).await {
        Ok(mut client) => {
            let hello = client.hello();
            let supervisor = SupervisorBody {
                pid: hello.pid,
                version: hello.version.clone(),
            };
            let reply = client.status().await?;
            StatusBody {
                supervisor: Some(supervisor),
                services: reply.services,
            }
        }
        Err(error) if error.code() == ErrorCode::SupervisorUnavailable => StatusBody {
            supervisor: None,
            services: Vec::new(),
        },
        Err(error) => return Err(error),
    };

    if json {
        return output::print_ok(body);
    }
    let headline = match &body.supervisor {
        Some(supervisor) => format!("Supervisor: running (pid {})", supervisor.pid),
        None => "Supervisor: not running".to_owned(),
    };
    output::print_line(&headline)?;
    output::print_line(&output::status_table(&body.services))
}

/// `ck start <service> [--json]`
pub async fn start(paths: &Paths, service: &str, json: bool) -> Result<()> {
    let mut client = client::ensure(paths).await?;
    let status = client.start(service).await?;
    print_service(status, json)
}

/// `ck stop <service> [--json]`
pub async fn stop(paths: &Paths, service: &str, json: bool) -> Result<()> {
    let mut client = client::ensure(paths).await?;
    let status = client.stop(service).await?;
    print_service(status, json)
}

/// The result of `start` or `stop`: the service's state after it.
fn print_service(service: ServiceStatus, json: bool) -> Result<()> {
    if json {
        output::print_ok(ServiceBody { service })
    } else {
        output::print_line(&output::service_line(&service))
    }
}

/// `ck supervisor stop [--json]`. Not an error when none is running.
pub async fn supervisor_stop(paths: &Paths, json: bool) -> Result<()> {
    let was_running = client::shutdown(paths).await?;
    if json {
        return output::print_ok(SupervisorStopBody { was_running });
    }
    output::print_line(if was_running {
        "Supervisor: stopped"
    } else {
        "Supervisor: not running"
    })
}

/// `ck supervisor run [--detached]`, hidden: `ck_core::supervisor::run`.
/// `main` has already detached it when asked.
pub async fn supervisor_run(paths: Paths) -> Result<()> {
    supervisor::run(paths).await
}

/// `ck paths [--json]`. Creates nothing, so it is safe with a release binary.
pub fn paths(paths: Paths, json: bool) -> Result<()> {
    let endpoint = Endpoint::locate(&paths.data)?.map(|endpoint| endpoint.to_string());
    let body = PathsBody {
        build: BuildKind::current(),
        profile: paths.profile,
        home: paths.home,
        data: paths.data,
        endpoint,
    };

    if json {
        return output::print_ok(body);
    }
    let endpoint = body
        .endpoint
        .as_deref()
        .unwrap_or("none (the data folder does not exist yet)");
    let lines = [
        format!("Build:    {}", output::build_name(body.build)),
        format!("Profile:  {}", output::profile_name(body.profile)),
        format!("Home:     {}", body.home.display()),
        format!("Data:     {}", body.data.display()),
        format!("Endpoint: {endpoint}"),
    ];
    output::print_line(&lines.join("\n"))
}

/// `ck install <name> <version> [--json]`. Uses only the compiled trust and
/// catalog URL; no key, path or URL can be supplied by the caller.
#[cfg(feature = "net")]
pub fn install(
    paths: &Paths,
    transport: &dyn Transport,
    name: &str,
    version: &str,
    json: bool,
) -> Result<()> {
    let trust = Trust::release()?;
    let url = CATALOG_URL.ok_or_else(|| {
        ck_core::Error::Internal("no catalog URL is compiled into this build".to_owned())
    })?;
    let service = RuntimeService::new(paths, &trust, transport, url)?;
    let outcome = service.install(name, version, SystemTime::now)?;
    if json {
        return output::print_ok(InstallBody::new(name, version, &outcome));
    }
    output::print_line(&output::install_line(&InstallBody::new(
        name, version, &outcome,
    )))
}

/// `ck installed [--json]`. Reads the install layout only: no catalog, no
/// network and no trust check.
#[cfg(feature = "net")]
pub fn installed(paths: &Paths, json: bool) -> Result<()> {
    layout::ensure(paths)?;
    let found = ck_core::install::installed(paths)?;
    let body = InstalledBody {
        runtimes: found.iter().map(RuntimeBody::from).collect(),
    };
    if json {
        return output::print_ok(body);
    }
    output::print_line(&output::installed_lines(&body))
}
