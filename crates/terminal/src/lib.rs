//! `lumepeer-terminal` — the host's side of a remote terminal (ADR 0079).
//!
//! One job: start a shell behind a pseudo-terminal **as the machine's
//! interactive user**, hand back its two byte streams, and let the caller
//! resize and kill it. Everything above that — whether a guest may ask for one
//! at all, how many, and what happens to the bytes — belongs to
//! `lumepeer-core` and to the actor that owns the session (§2.3).
//!
//! ## Why this is a crate and not a module
//!
//! Spawning processes and dropping tokens is not `lumepeer-media`'s subject,
//! and `apps/desktop/src-tauri` carries no `unsafe` at all by policy — it
//! delegates every such need to a compiled crate (`fs4`, `keyring`,
//! `lumepeer-service`). `GetShellWindow`/`DuplicateTokenEx`/
//! `CreateProcessAsUserW` and the `ConPTY` handles have no safe bindings, so
//! they live here. It also means `cargo test -p lumepeer-terminal` runs
//! without the Tauri build script, the `requireAdministrator` manifest or the
//! sidecar binaries in the way.
//!
//! ## The one invariant
//!
//! **The shell gets the desktop user's privileges, never this process's.**
//! On Windows the client runs elevated (ADR 0057), so the drop is explicit:
//! the token comes from the process behind `GetShellWindow()`, which is
//! Explorer running unelevated as the person at the machine. On Unix the
//! client already *is* that person — except when it is `root`, which is the
//! one case where the sentence would be false, and is refused for the same
//! reason. Where the drop cannot be made, [`ShellError::CannotDropPrivileges`]
//! comes back and **no process is created**. There is no fallback: an
//! administrator shell by accident is the outcome this crate exists to
//! prevent.
//!
//! ## What never happens here
//!
//! The guest does not name the program. The shell is `$SHELL` or `%COMSPEC%`
//! with a fixed fallback and no arguments, because "may start a shell" and
//! "may start any program with any arguments" are not the same permission and
//! only the first one was granted.
//!
//! Nothing that crosses these streams is logged, kept or written anywhere. A
//! terminal is a person talking to a machine, and §15's audit log is
//! deliberately pseudonymous and deliberately exportable (ADR 0041); a
//! transcript would make it neither. The `tracing` calls in here carry
//! lifecycle facts only — a shell started, a shell ended — and never a byte of
//! what was typed.

#![warn(missing_docs)]

use std::io::{Read, Write};

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
use unix as platform;
#[cfg(windows)]
use windows as platform;

/// Size of a terminal, in character cells.
///
/// Already bounded by the time it reaches here: `MessageEnvelope::decode`
/// refuses a zero or over-`TERMINAL_COLS_MAX`/`TERMINAL_ROWS_MAX` geometry at
/// the parse boundary (§9.1), so this type carries a number somebody's window
/// actually has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShellSize {
    /// Width in character cells.
    pub cols: u16,
    /// Height in character cells.
    pub rows: u16,
}

/// Why a shell could not be started (§18; ADR 0079).
///
/// Deliberately only two, and they map one-to-one onto the wire's
/// `TerminalRefusal::CannotDropPrivileges` and `TerminalRefusal::Unavailable`:
/// the caller has no third thing to say to a guest, and a richer error here
/// would only tempt somebody to forward the detail — which is a fact about the
/// host's own machine (§15).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellError {
    /// The shell could not be run as this machine's interactive user, so it
    /// was not run at all.
    ///
    /// On Windows: the elevated client found no unelevated desktop session to
    /// take a token from, or the spawn with that token was refused. On Unix:
    /// the client is running as `root`, and "drop to whom" is a decision
    /// nobody made.
    CannotDropPrivileges,
    /// There is no shell to run, or the pseudo-terminal could not be created.
    Unavailable,
}

impl std::fmt::Display for ShellError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::CannotDropPrivileges => {
                "the shell could not be run as this machine's interactive user"
            }
            Self::Unavailable => "no shell could be started",
        };
        f.write_str(text)
    }
}

impl std::error::Error for ShellError {}

/// A running shell behind a pseudo-terminal.
///
/// Dropping it kills the process and releases the pseudo-terminal: an
/// orphaned shell outliving the session that asked for it is the failure this
/// type's `Drop` exists to make impossible (ADR 0079).
#[derive(Debug)]
pub struct Shell {
    inner: platform::Shell,
}

/// The half of a [`Shell`] that can be used from another thread.
///
/// Resizing and killing have to reach a shell whose reader thread is blocked
/// in `read`, which is why they are separable at all — the same reason
/// `ControlConnection` splits into halves.
#[derive(Debug, Clone)]
pub struct ShellControl {
    inner: platform::ShellControl,
}

impl Shell {
    /// Starts the host's own shell at `size`, as this machine's interactive
    /// user.
    ///
    /// # Errors
    /// [`ShellError::CannotDropPrivileges`] when the shell cannot be run as
    /// that user — in which case nothing was started — and
    /// [`ShellError::Unavailable`] when there is no shell or no
    /// pseudo-terminal to be had.
    pub fn spawn(size: ShellSize) -> Result<Self, ShellError> {
        let inner = platform::Shell::spawn(size)?;
        tracing::info!("a remote shell started");
        Ok(Self { inner })
    }

    /// Splits into the output stream, the input stream and the control half.
    ///
    /// The two streams are blocking and are meant for their own threads; the
    /// control half is what the actor keeps.
    #[must_use]
    pub fn into_parts(self) -> (Box<dyn Read + Send>, Box<dyn Write + Send>, ShellControl) {
        let (reader, writer, control) = self.inner.into_parts();
        (reader, writer, ShellControl { inner: control })
    }

    /// The control half, without consuming the shell.
    #[must_use]
    pub fn control(&self) -> ShellControl {
        ShellControl {
            inner: self.inner.control(),
        }
    }
}

impl ShellControl {
    /// Tells the shell its window changed size.
    ///
    /// A resize that the kernel refuses is dropped with a log line rather than
    /// propagated: the shell is still running and still usable at the old
    /// geometry, and there is nothing a guest could do about it (§18).
    pub fn resize(&self, size: ShellSize) {
        if let Err(error) = self.inner.resize(size) {
            tracing::debug!(%error, "a remote shell could not be resized");
        }
    }

    /// Kills the shell and releases the pseudo-terminal.
    ///
    /// Idempotent, and safe to call from any thread — including while another
    /// one is blocked reading the shell's output, which is exactly when a
    /// revoke arrives.
    pub fn kill(&self) {
        self.inner.kill();
    }
}

/// The shell this host runs, and the fixed fallback when the environment
/// names none (ADR 0079).
///
/// Read from **this** machine's environment, never from anything a guest
/// sent, and used with no arguments. A guest that could name the executable
/// would hold arbitrary process execution under another name.
#[must_use]
pub fn host_shell() -> std::ffi::OsString {
    platform::host_shell()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR 0079: the shell comes from the host's own environment and is never
    /// empty — a build that returned nothing here would spawn nothing and
    /// report `Unavailable` forever.
    #[test]
    fn the_host_names_its_own_shell_and_always_names_one() {
        let shell = host_shell();
        assert!(
            !shell.is_empty(),
            "a host with no shell in its environment still has a fallback"
        );
    }

    /// The two refusals a caller may see are distinguishable and say what
    /// they mean, because one of them is a boundary working correctly and the
    /// other is a malfunction (§18).
    #[test]
    fn the_two_refusals_are_distinct_and_readable() {
        assert_ne!(ShellError::CannotDropPrivileges, ShellError::Unavailable);
        assert!(
            ShellError::CannotDropPrivileges
                .to_string()
                .contains("interactive user")
        );
    }
}
