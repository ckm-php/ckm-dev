//! `ck-test-service`: a dummy service for `ck`'s integration tests.
//!
//! Debug builds only: a release build exits 2 before reading its arguments.
//! It is never shipped.
//!
//! - `--port N`: listen on `127.0.0.1:N` until stopped. A connection that
//!   sends `stop\n` stops it with code 0; a connection that sends nothing (a
//!   readiness probe) is ignored. `SIGTERM` and `SIGINT` stop it too (Unix).
//! - `--ready-delay-ms N`: wait before listening, to test readiness.
//! - `--exit-after-ms N`: exit with code 3 after N ms from its start, unasked.
//! - `--ignore-stop`: ignore `stop\n`, `SIGTERM` and `SIGINT`, printing
//!   `ignored stop (<how>)` on stdout, which is the service log.
//! - `--stop --port N`: send `stop\n` to the service on port N, then exit. This
//!   is the stop command on Windows, which has no signals.

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr};
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::time;

/// What a connection sends to ask for a stop.
const STOP_MESSAGE: &[u8] = b"stop\n";

/// The exit code of `--exit-after-ms`.
const EXIT_UNASKED: u8 = 3;

/// How long `--stop` may take to connect and to see the connection closed,
/// and how long a connection may take to send its message.
const IO_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Parser)]
#[command(name = "ck-test-service")]
struct Args {
    /// Port on 127.0.0.1 to listen on, or with --stop, to send `stop` to.
    #[arg(long)]
    port: u16,
    /// Wait this long before listening.
    #[arg(long, default_value_t = 0)]
    ready_delay_ms: u64,
    /// Exit with code 3 this long after starting, unasked.
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
    match request_stop(port) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("ck-test-service: cannot send stop to port {port}: {error}");
            ExitCode::FAILURE
        }
    }
}

fn request_stop(port: u16) -> io::Result<()> {
    let mut stream = std::net::TcpStream::connect_timeout(&loopback(port), IO_TIMEOUT)?;
    stream.write_all(STOP_MESSAGE)?;
    stream.shutdown(Shutdown::Write)?;
    // Wait until the service has read the message and closed the connection,
    // so the message is not lost when this process exits. How the connection
    // ends does not matter.
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    let _ = stream.read_to_end(&mut Vec::new());
    Ok(())
}

/// Listens until stopped.
fn serve(
    port: u16,
    ready_delay_ms: u64,
    exit_after_ms: Option<u64>,
    ignore_stop: bool,
) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("ck-test-service: cannot start: {error}");
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(run(
        port,
        Duration::from_millis(ready_delay_ms),
        exit_after_ms.map(Duration::from_millis),
        ignore_stop,
    ))
}

async fn run(
    port: u16,
    ready_delay: Duration,
    exit_after: Option<Duration>,
    ignore_stop: bool,
) -> ExitCode {
    let (stop_sender, mut stop_requests) = mpsc::unbounded_channel();
    // First, so that a stop signal never meets its default action, which
    // would end the process at once.
    if let Err(error) = watch_signals(&stop_sender) {
        eprintln!("ck-test-service: cannot watch signals: {error}");
        return ExitCode::FAILURE;
    }
    // Counted from the start, through the ready delay.
    let unasked_exit = exit_timer(exit_after);
    tokio::pin!(unasked_exit);

    let ready = time::sleep(ready_delay);
    tokio::pin!(ready);
    loop {
        tokio::select! {
            () = &mut unasked_exit => return exit_unasked(),
            Some(source) = stop_requests.recv() => {
                if let Some(code) = on_stop(source, ignore_stop) {
                    return code;
                }
            }
            () = &mut ready => break,
        }
    }

    let listener = match TcpListener::bind(loopback(port)).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("ck-test-service: cannot listen on port {port}: {error}");
            return ExitCode::FAILURE;
        }
    };
    println!("listening on 127.0.0.1:{port}");
    loop {
        tokio::select! {
            () = &mut unasked_exit => return exit_unasked(),
            Some(source) = stop_requests.recv() => {
                if let Some(code) = on_stop(source, ignore_stop) {
                    return code;
                }
            }
            accepted = listener.accept() => {
                if let Ok((stream, _)) = accepted {
                    tokio::spawn(read_message(stream, stop_sender.clone()));
                }
            }
        }
    }
}

/// Completes after `after`, or never.
async fn exit_timer(after: Option<Duration>) {
    match after {
        Some(after) => time::sleep(after).await,
        None => std::future::pending().await,
    }
}

fn exit_unasked() -> ExitCode {
    println!("exiting unasked");
    ExitCode::from(EXIT_UNASKED)
}

/// The exit code for a stop request, or `None` when `--ignore-stop` ignores it.
fn on_stop(source: &str, ignore_stop: bool) -> Option<ExitCode> {
    if ignore_stop {
        println!("ignored stop ({source})");
        None
    } else {
        println!("stopping ({source})");
        Some(ExitCode::SUCCESS)
    }
}

/// Reads what one connection sends. `stop\n` asks for a stop; anything else,
/// including nothing at all (a readiness probe), is ignored.
async fn read_message(stream: TcpStream, stop: UnboundedSender<&'static str>) {
    let mut message = Vec::new();
    let mut reader = stream.take(STOP_MESSAGE.len() as u64);
    let read = reader.read_to_end(&mut message);
    if matches!(time::timeout(IO_TIMEOUT, read).await, Ok(Ok(_))) && message == STOP_MESSAGE {
        let _ = stop.send("stop message");
    }
}

/// Turns `SIGTERM` and `SIGINT` into stop requests. Registering a signal
/// replaces its default action for the life of the process.
#[cfg(unix)]
fn watch_signals(stop: &UnboundedSender<&'static str>) -> io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    for (kind, name) in [
        (SignalKind::terminate(), "SIGTERM"),
        (SignalKind::interrupt(), "SIGINT"),
    ] {
        let mut signals = signal(kind)?;
        let stop = stop.clone();
        tokio::spawn(async move {
            while signals.recv().await.is_some() {
                if stop.send(name).is_err() {
                    break;
                }
            }
        });
    }
    Ok(())
}

/// Windows has no signals: the stop message is its graceful stop.
#[cfg(windows)]
fn watch_signals(_stop: &UnboundedSender<&'static str>) -> io::Result<()> {
    Ok(())
}

fn loopback(port: u16) -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, port))
}
