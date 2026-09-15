//! Taking this machine down at a guest's request (§4.1; ADR 0084).
//!
//! The last step of the reboot path, and the only one that touches the
//! operating system. Everything that decides *whether* it happens is upstream:
//! `crates/core` holds the `reboot` grant, and `network.rs` holds the warning
//! window the person at this machine can stop it inside. By the time anything
//! here runs, both have already said yes.
//!
//! Three rules shape it:
//!
//! - **The system's own path, not ours.** `shutdown.exe` on Windows,
//!   `systemctl` (falling back to `shutdown`) on Linux, `shutdown` on macOS.
//!   Every one of them runs the machine's real shutdown sequence — other
//!   sessions are warned, services stop in order, filesystems are flushed —
//!   and every one of them refuses when the caller has no right to it. Writing
//!   our own would mean reimplementing all of that badly and losing the
//!   refusal.
//! - **A refusal is an answer, not a silence.** A command that exits non-zero
//!   comes back as an `Err` carrying what it said, so the host's log and audit
//!   trail record "this was refused" rather than "this was requested and then
//!   nothing happened". §18.
//! - **Nothing is interpolated.** Every argument here is a literal chosen by
//!   `mode`. There is no path, no user string and no peer input anywhere in
//!   this file, which is what keeps a privileged subprocess from becoming a
//!   quoting bug (the same rule `service_control::elevate` states).
//!
//! Blocking, and deliberately so: the caller runs it on a blocking task, and
//! on success it never returns at all.

use lumepeer_core::protocol::RebootMode;

/// Asks this machine's operating system to restart or shut down.
///
/// # Errors
/// A description of what the system refused — most plausibly for want of the
/// rights to do it — never a panic. On success this usually does not return
/// in any meaningful sense: the process is torn down with everything else.
pub fn go_down(mode: RebootMode) -> Result<(), String> {
    platform::go_down(mode)
}

#[cfg(target_os = "windows")]
mod platform {
    use std::os::windows::process::CommandExt as _;
    use std::process::Command;

    use lumepeer_core::protocol::RebootMode;

    /// `CREATE_NO_WINDOW`, for the same reason `service_control` uses it: a
    /// GUI app must not flash a console at whoever is sitting here.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    /// `/t 0` rather than the default 30-second delay: the wait a person gets
    /// is `REBOOT_WARNING_SECS`, shown in this app's own window with a button
    /// on it, and stacking a second countdown they cannot reach the cancel for
    /// would only make the total unpredictable.
    ///
    /// `/f` is deliberately **absent**. Forcing applications closed discards
    /// unsaved work belonging to the person at this machine, who did not ask
    /// for any of this — a shutdown an open document can veto is the correct
    /// failure here, and it is a failure this side reports.
    pub fn go_down(mode: RebootMode) -> Result<(), String> {
        let flag = match mode {
            RebootMode::Reboot => "/r",
            RebootMode::Shutdown => "/s",
        };
        let output = Command::new("shutdown.exe")
            .args([flag, "/t", "0"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map_err(|error| format!("cannot run the shutdown command: {error}"))?;
        if output.status.success() {
            return Ok(());
        }
        Err(refusal(&output))
    }

    /// What the command said, trimmed to one line, or its exit code when it
    /// said nothing.
    fn refusal(output: &std::process::Output) -> String {
        let said = String::from_utf8_lossy(&output.stderr);
        let line = said.lines().map(str::trim).find(|line| !line.is_empty());
        line.map_or_else(
            || format!("the shutdown command failed with {}", output.status),
            ToOwned::to_owned,
        )
    }
}

#[cfg(not(target_os = "windows"))]
mod platform {
    use std::process::Command;

    use lumepeer_core::protocol::RebootMode;

    /// `systemctl` first, `shutdown` second — and on macOS only the second,
    /// which is why the list is built rather than hardcoded to one command.
    ///
    /// A machine running systemd is told through systemd, because `shutdown`
    /// there is a compatibility shim over the same call and the shim is not
    /// present on every image. A machine without it — macOS, a container, a
    /// non-systemd distribution — falls through to the SysV-era command that
    /// has always existed. "Not installed" is not a refusal, so the fallback
    /// is tried on any failure of the first, and what is reported is what the
    /// *last* one said.
    pub fn commands(mode: RebootMode) -> Vec<(&'static str, Vec<&'static str>)> {
        let mut candidates = Vec::new();
        if cfg!(target_os = "linux") {
            candidates.push((
                "systemctl",
                vec![match mode {
                    RebootMode::Reboot => "reboot",
                    RebootMode::Shutdown => "poweroff",
                }],
            ));
        }
        candidates.push((
            "shutdown",
            match mode {
                RebootMode::Reboot => vec!["-r", "now"],
                RebootMode::Shutdown => vec!["-h", "now"],
            },
        ));
        candidates
    }

    pub fn go_down(mode: RebootMode) -> Result<(), String> {
        let mut last = "no shutdown command could be run at all".to_owned();
        for (program, args) in commands(mode) {
            match Command::new(program).args(&args).output() {
                Ok(output) if output.status.success() => return Ok(()),
                Ok(output) => last = refusal(program, &output),
                Err(error) => last = format!("cannot run {program}: {error}"),
            }
        }
        Err(last)
    }

    /// What the command said, trimmed to one line, or its exit status when it
    /// said nothing. `polkit` and `shutdown` both explain a refused
    /// shutdown on stderr, and that sentence is worth more to an operator than
    /// an exit code.
    fn refusal(program: &str, output: &std::process::Output) -> String {
        let said = String::from_utf8_lossy(&output.stderr);
        let line = said.lines().map(str::trim).find(|line| !line.is_empty());
        line.map_or_else(
            || format!("{program} failed with {}", output.status),
            ToOwned::to_owned,
        )
    }
}

#[cfg(test)]
mod tests {
    #[cfg(not(target_os = "windows"))]
    use super::platform;
    #[cfg(not(target_os = "windows"))]
    use lumepeer_core::protocol::RebootMode;

    /// The one thing about this module that can be checked without taking the
    /// machine down: which commands it would run, and that a restart and a
    /// shutdown never collapse into the same one.
    ///
    /// Nothing here executes anything. `go_down` itself is deliberately
    /// untested: a test that passed would leave no machine to report it.
    #[cfg(not(target_os = "windows"))]
    #[test]
    fn a_restart_and_a_shutdown_are_different_commands_everywhere() {
        let restart = platform::commands(RebootMode::Reboot);
        let off = platform::commands(RebootMode::Shutdown);
        assert_eq!(restart.len(), off.len());
        assert!(!restart.is_empty());
        for (a, b) in restart.iter().zip(off.iter()) {
            assert_eq!(a.0, b.0, "the same program answers both");
            assert_ne!(a.1, b.1, "{} would do the same thing for both modes", a.0);
        }
    }
}
