//! `lumepeer-core` — session state machine, consent, permissions, license and
//! audit. This crate is the TCB of the application (design doc §4): it is the
//! only component allowed to authorize anything, and neither the UI nor the
//! guest may bypass it (§2.3).
//!
//! Everything not explicitly permitted is forbidden (§2.1). No panics on
//! untrusted input: parsing and limit violations return [`error::CoreError`]
//! (§2.4).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod audit;
pub mod consent;
pub mod constants;
pub mod error;
pub mod license;
pub mod protocol;
pub mod session;

/// Peer identity: the Iroh endpoint public key. Long-term identity material
/// itself lives only in the OS keystore (§7, §11.2).
pub type NodeId = iroh_base::PublicKey;

pub use error::{CoreError, Result};
