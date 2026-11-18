//! Safe wrappers over the operating-system calls ckm-dev needs.
//!
//! This is the only crate where `unsafe` is allowed (D22). Each function that
//! uses it carries its own `#[allow(unsafe_code)]` and a `SAFETY:` comment on
//! every block; every other crate keeps `unsafe_code = "forbid"`.

#[cfg(unix)]
pub mod unix;

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(windows)]
pub mod windows;
