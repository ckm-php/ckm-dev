//! Services: their definitions, states, processes and identity records.
//!
//! The manager runs inside the supervisor and owns every service process. It
//! does not log; it returns errors and outcomes for the supervisor to log.

pub mod identity;
pub mod manager;
mod process;
pub mod record;
pub mod spec;
pub mod state;

pub use manager::Manager;
pub use spec::{ServiceId, ServiceSpec, StopMethod, StopSignal};
pub use state::ServiceState;
