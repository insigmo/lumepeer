//! Lumepeer's privileged helper service, as a library (ADR 0043, ADR 0049).
//!
//! The binary in `main.rs` is the service itself; this library is what the
//! desktop client links so both ends of the endpoint agree on the two bytes
//! that cross it without either copying the other's constants.
//!
//! Nothing privileged lives here. [`client`] opens a pipe and writes two
//! bytes; every capability is on the far side, in the service. [`frame`] and
//! [`host_role`] are the exceptions to "no unsafe on this side" (ADR 0049,
//! ADR 0085): a shared-memory mapping and a named mutex have no safe
//! standard-library wrapper, the same way becoming a Windows service or
//! creating a DACL'd pipe does not on the service's own side. Neither of them
//! is a capability — one reads bytes the privileged side published, the other
//! asks the kernel a question about who is hosting.
//!
//! [`log`] is here for a duller reason: this project ships two services now
//! (ADR 0085), both of them started by the service control manager and
//! therefore both of them with no stdout to write to. Where the file goes and
//! what bounds it are the same question twice, so it is answered once.

#[cfg(target_os = "windows")]
pub mod agent_channel;
#[cfg(target_os = "windows")]
pub mod agent_launch;
pub mod agent_protocol;
pub mod client;
#[cfg(target_os = "windows")]
pub mod frame;
#[cfg(target_os = "windows")]
pub mod host_role;
pub mod log;
#[cfg(target_os = "windows")]
pub mod logon_screen;
#[cfg(target_os = "windows")]
pub mod machine_store;
pub mod protocol;
pub mod session_change;

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

/// The single argument that starts the desktop application as this machine's
/// session agent (ADR 0085).
///
/// Lives here, in the crate both sides link, for the same reason
/// [`SECURE_DESKTOP_WORKER_ARG`] does: the privileged side that builds the
/// command line and the process that checks its own arguments read one
/// constant, so they cannot drift. The agent is the desktop binary rather
/// than this one — it needs capture, encode and a window for the session
/// indicator, none of which belong anywhere near a `LocalSystem` process
/// (ADR 0085 §1).
pub const SESSION_AGENT_ARG: &str = "--session-agent";

/// The single argument that re-executes this binary as the logon-screen
/// worker (ADR 0088 §1).
///
/// The session-0 host launches a copy of this binary with exactly this
/// argument into the console session's `Winsta0\Winlogon` desktop, where it
/// lives for as long as that screen is being served: it attaches to the same
/// channel a session agent uses, publishes frames of the logon screen on a
/// tick, and performs the input the host forwards to it.
///
/// Distinct from [`SECURE_DESKTOP_WORKER_ARG`], which is the same desktop and
/// a different job — one frame for a UAC prompt an ordinary client cannot see,
/// and then gone (ADR 0056). Keeping them apart is what keeps that worker's
/// "exists only for the one capture" property true while this one exists for a
/// session.
pub const LOGON_SCREEN_WORKER_ARG: &str = "--logon-screen-worker";

/// The argument that re-executes this binary as the secure-desktop *input*
/// worker (ADR 0057), followed by four bounded integers `kind logical x y`.
///
/// The mirror of [`SECURE_DESKTOP_WORKER_ARG`]: where that worker reads one
/// frame off `Winsta0\Winlogon`, this one performs one input event on it and
/// exits with the outcome as its exit code. The parameters travel as plain
/// integers on the command line — never a peer string — because
/// `secure_desktop_launch` builds the line itself from values the service has
/// already validated, and `main.rs` re-parses them under the same bounds
/// (ADR 0057 §3).
pub const SECURE_DESKTOP_INPUT_WORKER_ARG: &str = "--secure-desktop-input-worker";
