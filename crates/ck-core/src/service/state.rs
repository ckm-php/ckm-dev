//! The service state machine.
//!
//! ```text
//! stopped -> starting -> running -> stopping -> stopped
//! starting -> failed     the process exited, or readiness timed out
//! running  -> failed     the process exited unexpectedly
//! failed   -> starting
//! ```
//!
//! A failure before the spawn leaves the service `stopped` with `last_error`.
//! A protected service whose stop timed out stays `stopping`. Nothing restarts
//! automatically.

use serde::{Deserialize, Serialize};

/// Where a service is in its life cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceState {
    Stopped,
    Starting,
    Running,
    Stopping,
    Failed,
}
