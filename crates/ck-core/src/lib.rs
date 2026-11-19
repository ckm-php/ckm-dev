//! Core library shared by the `ck` command and the desktop app.

pub mod branding;
pub mod catalog;
pub mod client;
pub mod download;
pub mod error;
pub mod extract;
pub mod install;
pub mod ipc;
pub mod layout;
pub mod paths;
pub mod service;
pub mod supervisor;

#[cfg(test)]
mod test_support;

pub use error::{Error, ErrorBody, ErrorCode, Result};
pub use paths::Paths;
