//! The `ck` command.
//!
//! Exit codes: 0 ok, 2 usage (from clap), and the rest from
//! `ck_core::ErrorCode::exit_code` (1 other, 3 supervisor unavailable,
//! 4 unknown service, 5 port in use, 6 start failed, 7 stop failed, 8 busy,
//! 9 version mismatch).

use std::process::ExitCode;

use ck_core::{Error, Paths, Result, branding};
use clap::{Args, Parser, Subcommand};

mod commands;
mod output;

/// Manage the local development environment.
#[derive(Parser)]
#[command(name = branding::COMMAND, version, arg_required_else_help = true)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show the supervisor and every service.
    Status(JsonFlag),
    /// Start a service, starting the supervisor first if needed.
    Start {
        service: String,
        #[command(flatten)]
        flag: JsonFlag,
    },
    /// Stop a service, starting the supervisor first if needed.
    Stop {
        service: String,
        #[command(flatten)]
        flag: JsonFlag,
    },
    /// Manage the background supervisor.
    Supervisor {
        #[command(subcommand)]
        command: SupervisorCommand,
    },
    /// Show the build, profile, folders and IPC endpoint. Creates nothing.
    Paths(JsonFlag),
}

#[derive(Subcommand)]
enum SupervisorCommand {
    /// Stop every service, then the supervisor.
    Stop(JsonFlag),
    /// Run the supervisor in this process.
    #[command(hide = true)]
    Run {
        /// Started by `ck start` or `ck stop`: leave the caller's session.
        #[arg(long)]
        detached: bool,
    },
}

#[derive(Args, Clone, Copy)]
struct JsonFlag {
    /// Print one JSON document on stdout instead of text.
    #[arg(long)]
    json: bool,
}

impl Command {
    /// Whether this command reports in JSON, errors included.
    fn json(&self) -> bool {
        match self {
            Self::Status(flag) | Self::Paths(flag) => flag.json,
            Self::Start { flag, .. } | Self::Stop { flag, .. } => flag.json,
            Self::Supervisor { command } => match command {
                SupervisorCommand::Stop(flag) => flag.json,
                SupervisorCommand::Run { .. } => false,
            },
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let json = cli.command.json();
    match run(cli.command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            output::print_error(&error, json);
            ExitCode::from(error.code().exit_code())
        }
    }
}

/// Runs `command` on a current-thread runtime, which the supervisor and the
/// client both expect.
fn run(command: Command) -> Result<()> {
    let paths = Paths::current()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::os("start the async runtime", e))?;
    runtime.block_on(dispatch(command, paths))
}

async fn dispatch(command: Command, paths: Paths) -> Result<()> {
    match command {
        Command::Status(flag) => commands::status(&paths, flag.json).await,
        Command::Start { service, flag } => commands::start(&paths, &service, flag.json).await,
        Command::Stop { service, flag } => commands::stop(&paths, &service, flag.json).await,
        Command::Supervisor { command } => match command {
            SupervisorCommand::Stop(flag) => commands::supervisor_stop(&paths, flag.json).await,
            SupervisorCommand::Run { detached } => commands::supervisor_run(paths, detached).await,
        },
        Command::Paths(flag) => commands::paths(paths, flag.json),
    }
}
