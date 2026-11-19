//! Local IPC between `ck` (or the desktop app) and the supervisor (D19).
//!
//! A Unix socket (0600 in a 0700 folder) or a named pipe whose DACL allows
//! only the current user. Both ends check that the peer is the current user.
//! There is no TCP control port.

use std::time::Duration;

pub mod endpoint;
pub mod frame;
pub mod protocol;
pub mod transport;

/// Largest request body a server accepts.
pub const MAX_REQUEST_BYTES: u32 = 64 * 1024;

/// Largest response body a client accepts.
pub const MAX_RESPONSE_BYTES: u32 = 1024 * 1024;

/// Connections the supervisor serves at once; connections over the limit are
/// dropped.
pub const MAX_CONNECTIONS: usize = 32;

/// How long a new connection may take to send its first frame.
pub const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(2);

/// How long a connection may stay silent between requests.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
