//! Lumepeer's privileged helper service, as a library (ADR 0043, ADR 0049).
//!
//! The binary in `main.rs` is the service itself; this library is what the
//! desktop client links so both ends of the endpoint agree on the two bytes
//! that cross it without either copying the other's constants.
//!
//! Nothing privileged lives here. [`client`] opens a pipe and writes two
//! bytes; every capability is on the far side, in the service. [`frame`] is
//! the one exception to "no unsafe on this side" (ADR 0049): reading a
//! shared-memory mapping has no safe standard-library wrapper, the same way
//! becoming a Windows service or creating a DACL'd pipe does not on the
//! service's own side.

pub mod client;
#[cfg(target_os = "windows")]
pub mod frame;
pub mod protocol;

/// Name the service is registered under with the service control manager.
///
/// Shared so the installer, the status query and the service itself cannot
/// disagree about what to look for.
pub const SERVICE_NAME: &str = "LumepeerHelper";

/// The single argument that re-executes this binary as the secure-desktop
/// capture worker (ADR 0056).
///
/// The service launches a copy of itself with exactly this argument into the
/// console session's `Winsta0\Winlogon` desktop; the worker opens the shared
/// mapping, takes one GDI snapshot of that desktop, writes it and exits. Both
/// the launcher and `main.rs`'s argument check read this one constant so they
/// cannot drift.
pub const SECURE_DESKTOP_WORKER_ARG: &str = "--secure-desktop-worker";
