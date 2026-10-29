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
pub mod access_settings;
#[cfg(target_os = "windows")]
pub mod agent_channel;
#[cfg(target_os = "windows")]
pub mod agent_launch;
pub mod agent_protocol;
pub mod client;
#[cfg(target_os = "windows")]
pub mod client_launch;
#[cfg(target_os = "windows")]
pub mod consent_dialog;
#[cfg(target_os = "windows")]
pub mod desktop_input_channel;
#[cfg(target_os = "windows")]
pub mod frame;
#[cfg(target_os = "windows")]
pub mod host_channel;
pub mod host_channel_protocol;
#[cfg(target_os = "windows")]
pub mod host_role;
pub mod log;
pub mod logon_enroll;
#[cfg(target_os = "windows")]
pub mod logon_screen;
pub mod logon_seat;
#[cfg(target_os = "windows")]
pub mod machine_store;
#[cfg(target_os = "windows")]
pub mod program_data;
pub mod protocol;
pub mod session_change;
#[cfg(target_os = "windows")]
pub mod stay_awake;

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

/// The single argument that starts the desktop application as this machine's
/// `LocalSystem` desktop injector (ADR 0114).
///
/// The service launches a copy of the desktop binary with exactly this argument
/// into the console session's `Winsta0\Default` desktop, keeping its own
/// `LocalSystem` token rather than dropping to the signed-in user the way the
/// session agent does (`agent_launch`). It lives for as long as the console
/// session does: it opens the desktop injector channel
/// ([`desktop_input_channel`]) and performs the input the service forwards to
/// it, through the very same `WindowsInjector` the host would run in-process.
///
/// The injector is the desktop binary, not this one, for the reason the session
/// agent is (ADR 0085 §1): the injection logic — scan codes, chords, the
/// grab/relative pointer state machine — lives in `crates/media`, which this
/// privileged binary deliberately does not link. Keeping one implementation and
/// launching it is how a host avoids two injectors kept in step by review.
///
/// Distinct from [`SESSION_AGENT_ARG`], which is the same desktop and the
/// signed-in user's token; the two are launched, checked and torn down apart so
/// "the injector is `LocalSystem`, the agent is not" stays a fact a reader can
/// see rather than a flag they must trace (ADR 0114 §1).
pub const SYSTEM_INPUT_WORKER_ARG: &str = "--system-input-worker";

/// The single argument that starts the desktop application as this machine's
/// host on the logon screen (ADR 0126).
///
/// The service launches the desktop binary with exactly this argument into the
/// console session's `Winsta0\Winlogon` desktop, as `LocalSystem`, whenever
/// nobody is signed in there. It hosts with the identity and the device
/// password of the account that turned the feature on, so a guest reaches the
/// same saved host before anybody signs in as after; it is stopped the moment
/// somebody does, and the ordinary client takes over.
pub const LOGON_HOST_ARG: &str = "--logon-host";

/// Exit code of a logon host that found the feature off, the owner's store
/// empty, or no device password to admit anybody with (ADR 0126).
///
/// Not a failure to relaunch after: nothing about it changes until somebody
/// signs in and changes a setting, so the service waits for the console to
/// change rather than starting the same refusal again on every tick.
pub const LOGON_HOST_EXIT_NOT_ENABLED: u32 = 3;

/// Exit code of a logon host that found the host role already held — most
/// likely by a client still running in a disconnected session after a fast
/// user switch (ADR 0085 §4, ADR 0126). Waited out like
/// [`LOGON_HOST_EXIT_NOT_ENABLED`], for the same reason.
pub const LOGON_HOST_EXIT_ROLE_TAKEN: u32 = 4;

/// Exit code of a logon host that had a guest when it was asked to leave
/// (ADR 0164).
///
/// That guest has just signed the machine in and is dialing again; the service
/// reads this as "start the owner's client", so the dial has somebody to reach
/// whether or not autostart is on.
pub const LOGON_HOST_EXIT_HANDED_OVER: u32 = 5;

/// The single argument that starts the desktop application as this machine's
/// host in the signed-in console session (ADR 0169 §1).
///
/// [`LOGON_HOST_ARG`]'s counterpart for the other side of a sign-in: the
/// service launches the desktop binary with exactly this argument into the
/// console session's `Winsta0\Default` desktop, as `LocalSystem`, while
/// somebody is signed in there. It hosts with that person's stores, the ones
/// their elevated client kept (ADR 0123), so every guest reaches the identity
/// it saved; and it injects from where the ADR 0114 injector stands, so
/// elevated windows and a `VMware` guest take its input.
pub const SESSION_HOST_ARG: &str = "--session-host";

/// Exit code of a session host on a machine where ADR 0169 is not turned on
/// yet: no `session-host-enabled` marker under `%ProgramData%\Lumepeer`
/// (`program_data::session_host_enabled`).
///
/// Waited out like [`LOGON_HOST_EXIT_NOT_ENABLED`]: the elevated client hosts
/// as before, and starting the same refusal on every tick would only fill the
/// log.
pub const SESSION_HOST_EXIT_NOT_ENABLED: u32 = 6;

/// Exit code of a session host whose signed-in user has no store to host
/// with: they never opened Lumepeer, so there is no identity any guest saved
/// (ADR 0169 §1). Waited out like [`SESSION_HOST_EXIT_NOT_ENABLED`].
pub const SESSION_HOST_EXIT_NO_STORE: u32 = 7;

/// Exit code of a session host that found the host role held for longer than
/// it waits for a leaving logon host to let it go — an elevated client
/// started before ADR 0169 was turned on, most likely. Waited out like the two
/// above.
pub const SESSION_HOST_EXIT_ROLE_TAKEN: u32 = 8;

/// Exit code of a session host that closed its endpoint (ADR 0169 §1): no
/// client attached, "accept connections after sign-in" off, no session left.
///
/// Not a refusal: the service starts it again at once, and the new process
/// listens on the client's channel without binding an endpoint until a
/// client attaches or the setting is turned on. Leaving is how this host
/// closes the endpoint, because a process that never bound one is the only
/// kind sure to answer nothing.
pub const SESSION_HOST_EXIT_CLOSED: u32 = 9;

/// The argument that starts the desktop application as the access-settings
/// window only, followed by the SID of the account whose policy it edits
/// (ADR 0169 §5).
///
/// The client starts itself again with it through UAC
/// (`access_settings::open_elevated`): an elevated process with no hosting, no
/// tray, no network and no guest window, that edits the device password, the
/// second factor, the unattended role, the logon screen and the trusted
/// devices where only administrators write, and then tells the session host.
pub const ACCESS_SETTINGS_ARG: &str = "--access-settings";
