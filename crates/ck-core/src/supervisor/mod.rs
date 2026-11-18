//! The background supervisor: one per user and data folder, owning every
//! service (D19).
//!
//! It runs as the hidden `ck supervisor run`, started on demand by
//! `client::ensure`, until `ck supervisor stop`, logout or shutdown. It has no
//! idle exit and does not start at login.

use crate::error::Result;
use crate::paths::Paths;

pub mod lock;
pub mod registry;

/// Runs the supervisor for `paths` until it is shut down. Call it inside a
/// current-thread tokio runtime with I/O and time enabled.
///
/// - **Start order:** layout, lock, endpoint, signal handlers, registry,
///   adoption, serve.
/// - **Single instance:** when another supervisor holds the lock, it returns
///   `Ok(())` at once and quietly.
/// - **`detached`:** set when `client::ensure` spawned it. On Unix it first
///   calls `ck_sys::unix::new_session()`; a `ck supervisor run` typed in a
///   shell already leads its group, so it does not.
/// - **Shutdown** (a `shutdown` request, `SIGTERM` or `SIGINT`): stop accepting,
///   stop every service, remove the socket, return. When a protected service
///   will not stop, the shutdown fails and the supervisor keeps running.
/// - **Logs:** plain UTC lines on stderr, which `client::ensure` points at
///   `logs/supervisor.log`.
pub async fn run(paths: Paths, detached: bool) -> Result<()> {
    let _ = (paths, detached);
    todo!()
}
