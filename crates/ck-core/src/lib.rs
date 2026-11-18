//! Core library shared by the `ck` command and the desktop app.

pub mod branding;
pub mod client;
pub mod error;
pub mod ipc;
pub mod layout;
pub mod paths;
pub mod service;
pub mod supervisor;

pub use error::{Error, ErrorBody, ErrorCode, Result};
pub use paths::Paths;
