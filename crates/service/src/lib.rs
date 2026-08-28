//! Lumepeer's privileged helper service, as a library (ADR 0043).
//!
//! The binary in `main.rs` is the service itself; this library is what the
//! desktop client links so both ends of the endpoint agree on the two bytes
//! that cross it without either copying the other's constants.
//!
//! Nothing privileged lives here. [`client`] opens a pipe and writes two
//! bytes; every capability is on the far side, in the service.

pub mod client;
pub mod protocol;

/// Name the service is registered under with the service control manager.
///
/// Shared so the installer, the status query and the service itself cannot
/// disagree about what to look for.
pub const SERVICE_NAME: &str = "LumepeerHelper";
