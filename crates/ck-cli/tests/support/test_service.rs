//! `ck-test-service`: a dummy service for `ck`'s integration tests.
//!
//! Debug builds only: a release build exits 2 before reading its arguments.
//! It is never shipped.
//!
//! - `--port N`: listen on `127.0.0.1:N` until stopped. A connection that
//!   sends `stop\n` stops it; a connection that sends nothing (a readiness
//!   probe) is ignored. `SIGTERM` and `SIGINT` stop it too.
//! - `--ready-delay-ms N`: wait before listening, to test readiness.
//! - `--exit-after-ms N`: exit with code 1 after N ms, unasked.
//! - `--ignore-stop`: ignore `stop\n`, `SIGTERM` and `SIGINT`.
//! - `--stop --port N`: send `stop\n` to the service on port N, then exit. This
//!   is the stop command on Windows, which has no signals.

use std::process::ExitCode;

use clap::Parser;

#[derive(Parser)]
#[command(name = "ck-test-service")]
struct Args {
    /// Port on 127.0.0.1 to listen on, or with --stop, to send `stop` to.
    #[arg(long)]
    port: u16,
    /// Wait this long before listening.
    #[arg(long, default_value_t = 0)]
    ready_delay_ms: u64,
    /// Exit with code 1 after this long, unasked.
    #[arg(long)]
    exit_after_ms: Option<u64>,
    /// Ignore the stop message, SIGTERM and SIGINT.
    #[arg(long)]
    ignore_stop: bool,
    /// Send `stop` to the service on --port, then exit.
    #[arg(long)]
    stop: bool,
}

fn main() -> ExitCode {
    if !cfg!(debug_assertions) {
        eprintln!("ck-test-service: a test tool for debug builds only");
        return ExitCode::from(2);
    }

    let Args {
        port,
        ready_delay_ms,
        exit_after_ms,
        ignore_stop,
        stop,
    } = Args::parse();
    if stop {
        send_stop(port)
    } else {
        serve(port, ready_delay_ms, exit_after_ms, ignore_stop)
    }
}

/// `--stop --port N`.
fn send_stop(port: u16) -> ExitCode {
    let _ = port;
    todo!()
}

/// Listens until stopped.
fn serve(
    port: u16,
    ready_delay_ms: u64,
    exit_after_ms: Option<u64>,
    ignore_stop: bool,
) -> ExitCode {
    let _ = (port, ready_delay_ms, exit_after_ms, ignore_stop);
    todo!()
}
