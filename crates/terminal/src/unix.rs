//! The Unix pseudo-terminal: `openpty`, a controlling terminal, and a shell
//! that dies with its handles (ADR 0079).
//!
//! `portable-pty` carries the parts that have no safe binding — the `openpty`
//! pair, `setsid`/`TIOCSCTTY` on the child, the `TIOCSWINSZ` resize and
//! `waitpid` — which is why this file has no `unsafe` in it.
//!
//! The privilege rule of ADR 0079 needs no work here in the ordinary case: a
//! Lumepeer running on Linux or macOS *is* the interactive user's process, so
//! a child of it already is. The exception is the one case where saying so
//! would be false — a client running as `root` — and that is refused rather
//! than guessed at, because "drop to whom" is a decision nobody made and a
//! wrong guess hands a guest a root shell.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::{ShellError, ShellSize};

/// Shell to run when the environment names none.
///
/// POSIX guarantees `/bin/sh` exists; `$SHELL` is what the person at the
/// machine actually uses, and is preferred for that reason.
const FALLBACK_SHELL: &str = "/bin/sh";

/// The host's own shell, with no arguments (ADR 0079).
pub(crate) fn host_shell() -> OsString {
    match std::env::var_os("SHELL") {
        Some(shell) if !shell.is_empty() => shell,
        _ => OsString::from(FALLBACK_SHELL),
    }
}

/// Whether this process may hand its own privileges to a shell.
///
/// The whole of ADR 0079 decision 2 on Unix: a client that is the interactive
/// user may, and a client that is `root` may not, because the shell would then
/// be a root shell nobody granted.
fn may_lend_privileges() -> bool {
    !nix::unistd::geteuid().is_root()
}

/// Everything a running shell owns, dropped as one.
///
/// The kill lives in this type's `Drop` rather than in [`Shell`]'s, because
/// what has to be true is "when the last handle to this shell is gone, so is
/// the shell" — and the handle that outlives the others is the control half
/// the actor keeps, not the [`Shell`] value it was split out of. A session
/// that ends by its connection dropping never gets to call anything, and this
/// is what makes that case leave nothing behind (ADR 0079).
struct Inner {
    /// The master side, kept alive so the resize ioctl has something to
    /// address; dropping it closes the pty.
    master: Mutex<Box<dyn MasterPty + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    /// The shell's own pid, captured before the child was handed to the
    /// reaper thread. `None` on a platform that does not report one.
    ///
    /// Test builds only. Nothing on the shipped path needs it — a shell is
    /// killed through its killer, not by number — and the orphan check of
    /// ADR 0079 has no other way to ask whether the process is really gone.
    #[cfg(test)]
    pid: Option<u32>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        kill_now(&self.killer);
    }
}

/// Signals the shell, ignoring a shell that has already gone.
///
/// Not an error: a revoke and an `exit` race every time, and neither of them
/// is wrong.
fn kill_now(killer: &Mutex<Box<dyn ChildKiller + Send + Sync>>) {
    if let Ok(mut killer) = killer.lock() {
        let _ = killer.kill();
    }
}

pub(crate) struct Shell {
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
    control: ShellControl,
}

impl std::fmt::Debug for Shell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shell").finish_non_exhaustive()
    }
}

/// Resize and kill, usable while another thread is blocked reading output.
#[derive(Clone)]
pub(crate) struct ShellControl {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for ShellControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShellControl").finish_non_exhaustive()
    }
}

impl Shell {
    pub(crate) fn spawn(size: ShellSize) -> Result<Self, ShellError> {
        if !may_lend_privileges() {
            tracing::warn!(
                "refusing a remote shell: this process runs as root and there is no \
                 interactive user to drop to (ADR 0079)"
            );
            return Err(ShellError::CannotDropPrivileges);
        }

        let pair = native_pty_system()
            .openpty(pty_size(size))
            .map_err(|error| {
                tracing::warn!(%error, "no pseudo-terminal could be opened");
                ShellError::Unavailable
            })?;

        // No arguments, and the program is this machine's own (ADR 0079).
        // `CommandBuilder::new` inherits this process's environment, which is
        // the interactive user's, so the shell starts where they would.
        let mut child = pair
            .slave
            .spawn_command(CommandBuilder::new(host_shell()))
            .map_err(|error| {
                tracing::warn!(%error, "the host's shell could not be started");
                ShellError::Unavailable
            })?;
        // The slave fd is the child's now. Keeping a copy would hold the pty
        // open after the shell exits, so the reader below would block for ever
        // instead of seeing EOF — which is one of the two ways an orphan is
        // born.
        drop(pair.slave);

        let killer = child.clone_killer();
        #[cfg(test)]
        let pid = child.process_id();
        // The other way an orphan is born: `std::process::Child` does not reap
        // on drop, so a shell nobody waits on becomes a zombie that still
        // answers to its pid. One thread per shell, blocked in `wait`, ends
        // the moment the shell does.
        std::thread::spawn(move || {
            let _ = child.wait();
        });

        let reader = pair.master.try_clone_reader().map_err(|error| {
            tracing::warn!(%error, "the shell's output could not be read");
            ShellError::Unavailable
        })?;
        let writer = pair.master.take_writer().map_err(|error| {
            tracing::warn!(%error, "the shell's input could not be written");
            ShellError::Unavailable
        })?;

        Ok(Self {
            reader,
            writer,
            control: ShellControl {
                inner: Arc::new(Inner {
                    master: Mutex::new(pair.master),
                    killer: Mutex::new(killer),
                    #[cfg(test)]
                    pid,
                }),
            },
        })
    }

    pub(crate) fn into_parts(self) -> (Box<dyn Read + Send>, Box<dyn Write + Send>, ShellControl) {
        (self.reader, self.writer, self.control)
    }

    pub(crate) fn control(&self) -> ShellControl {
        self.control.clone()
    }
}

impl ShellControl {
    pub(crate) fn resize(&self, size: ShellSize) -> Result<(), ShellError> {
        let master = self
            .inner
            .master
            .lock()
            .map_err(|_| ShellError::Unavailable)?;
        master
            .resize(pty_size(size))
            .map_err(|_| ShellError::Unavailable)
    }

    pub(crate) fn kill(&self) {
        kill_now(&self.inner.killer);
    }
}

const fn pty_size(size: ShellSize) -> PtySize {
    PtySize {
        rows: size.rows,
        cols: size.cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// The shell is the host's own, taken from the host's own environment and
    /// never from anything a guest sent (ADR 0079).
    #[test]
    fn the_shell_comes_from_the_hosts_environment_with_a_posix_fallback() {
        // Deliberately not `set_var`: it is process-global and would race the
        // other tests in this binary. What is checkable without it is the
        // invariant that matters — a shell is always named, and it is one of
        // the only two things this function is allowed to name.
        let shell = host_shell();
        assert!(!shell.is_empty());
        match std::env::var_os("SHELL") {
            Some(from_env) if !from_env.is_empty() => assert_eq!(shell, from_env),
            _ => assert_eq!(shell, OsString::from(FALLBACK_SHELL)),
        }
    }

    /// A shell really starts, really echoes, and is really gone afterwards.
    ///
    /// The orphan check ADR 0079 asks for: once the control half kills it, its
    /// pid must name nothing — not a live process and not a zombie nobody
    /// reaped. A session ending while a shell keeps running on somebody's
    /// machine is the failure this guards.
    #[test]
    fn a_shell_runs_echoes_and_leaves_nothing_behind() {
        if nix::unistd::geteuid().is_root() {
            // Refusing is the correct behaviour for this case and has its own
            // test; there is no shell here to inspect.
            return;
        }
        let shell = Shell::spawn(ShellSize { cols: 80, rows: 24 }).expect("a shell");
        let pid = shell.control.inner.pid.expect("a pid");
        assert!(alive(pid), "the shell was not running after spawn");

        let (mut reader, mut writer, control) = shell.into_parts();
        writer.write_all(b"echo lumepeer-marker\n").unwrap();
        writer.flush().unwrap();

        // A pty echoes the input as well as the output, so the marker appears
        // twice; either occurrence proves the round trip. Reading stops at EOF
        // so a shell that died instead of answering fails the assert rather
        // than hanging the suite.
        let mut seen = Vec::new();
        let mut buffer = [0u8; 1024];
        while !contains(&seen, b"lumepeer-marker") {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => seen.extend_from_slice(&buffer[..read]),
            }
        }
        assert!(
            contains(&seen, b"lumepeer-marker"),
            "the shell never echoed anything back"
        );

        control.kill();
        drop(control);
        assert!(
            gone(pid),
            "the shell outlived the session that asked for it"
        );
    }

    /// Dropping every handle kills the shell too: a session that ends by its
    /// connection dropping never gets to call `kill`, and must still leave
    /// nothing behind (ADR 0079).
    #[test]
    fn dropping_the_last_handle_kills_the_shell() {
        if nix::unistd::geteuid().is_root() {
            return;
        }
        let shell = Shell::spawn(ShellSize { cols: 80, rows: 24 }).expect("a shell");
        let pid = shell.control.inner.pid.expect("a pid");
        assert!(alive(pid));

        // A second handle is not enough to keep it alive once both are gone,
        // and *is* enough while one remains — which is what makes the control
        // half safe to clone into the actor.
        let control = shell.control();
        drop(shell);
        assert!(alive(pid), "one live handle should still hold the shell");
        drop(control);
        assert!(gone(pid), "a shell with no handles left was still running");
    }

    /// A resize reaches the kernel, which is what makes `stty size` inside the
    /// shell agree with the guest's window.
    #[test]
    fn a_resize_reaches_the_kernel() {
        if nix::unistd::geteuid().is_root() {
            return;
        }
        let shell = Shell::spawn(ShellSize { cols: 80, rows: 24 }).expect("a shell");
        let control = shell.control();
        control
            .resize(ShellSize {
                cols: 120,
                rows: 40,
            })
            .expect("a resize");
        let size = {
            let master = control.inner.master.lock().unwrap();
            master.get_size().expect("a size")
        };
        assert_eq!((size.cols, size.rows), (120, 40));
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    /// Whether a pid still names a process this session could signal.
    ///
    /// Signal 0 is the portable "does this exist" probe. A zombie still
    /// answers to it, which is the point: the reaper thread is what makes
    /// this go false, and a build without one would pass a liveness check
    /// while leaving a process table entry behind.
    fn alive(pid: u32) -> bool {
        let Ok(pid) = i32::try_from(pid) else {
            return false;
        };
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
    }

    /// [`alive`] going false within a couple of seconds. A kill is a signal,
    /// and the shell needs a moment to act on it.
    fn gone(pid: u32) -> bool {
        for _ in 0..100 {
            if !alive(pid) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        false
    }
}
