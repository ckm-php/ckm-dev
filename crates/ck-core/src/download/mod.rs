//! Downloads (D3): which URLs may be requested ([`policy`]), the HTTP call
//! as a trait ([`transport`]), and fetching to a checked file ([`fetch`],
//! [`verified`]).
//!
//! Everything is synchronous; async callers use `spawn_blocking`. There are
//! no retries, no resumed downloads and no cache.

pub mod fetch;
pub mod policy;
pub mod transport;
pub mod verified;
