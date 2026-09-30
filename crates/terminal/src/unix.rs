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

use std::ffi::{OsStr, OsString};
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

/// The character set a shell is given when this process has none (ADR 0130).
///
/// Only the character handling: messages, dates and sorting stay whatever the
/// host already had. Every macOS has `en_US.UTF-8`; `C.UTF-8` is the one a
/// Linux is surest to have, built into glibc since 2.35 and packaged by
/// Debian long before.
#[cfg(target_os = "macos")]
const UTF8_CTYPE: &str = "en_US.UTF-8";
#[cfg(not(target_os = "macos"))]
const UTF8_CTYPE: &str = "C.UTF-8";

/// The `LC_CTYPE` a shell needs on top of the environment it inherits, if any
/// (ADR 0130).
///
/// A Lumepeer started from the Dock, Finder or a launch agent carries no
/// `LANG` at all — Terminal.app sets one for its own shells, nothing sets one
/// for ours — so the shell ran in the C locale, took each byte of a Cyrillic
/// letter for a character of its own and drew the half it could not print as
/// `<0084>`. What was typed was fine; the shell could not read it.
///
/// Locale variables resolve `LC_ALL`, then `LC_CTYPE`, then `LANG`, an empty
/// one counting as unset. A UTF-8 answer is left alone. A set `LC_ALL` is too:
/// it overrides every category, so it was put there on purpose by whoever
/// owns the machine, and it is theirs to change.
fn ctype_override(
    lc_all: Option<&OsStr>,
    lc_ctype: Option<&OsStr>,
    lang: Option<&OsStr>,
) -> Option<&'static str> {
    fn set(value: Option<&OsStr>) -> Option<&OsStr> {
        value.filter(|value| !value.is_empty())
    }
    if set(lc_all).is_some() {
        return None;
    }
    match set(lc_ctype).or(set(lang)) {
        Some(locale) if is_utf8(locale) => None,
        _ => Some(UTF8_CTYPE),
    }
}

/// Whether a locale name says UTF-8, in any of the spellings in use
/// (`UTF-8`, `utf8`, with or without a language in front).
fn is_utf8(locale: &OsStr) -> bool {
    let locale = locale.to_string_lossy().to_ascii_lowercase();
    locale.contains("utf-8") || locale.contains("utf8")
}

/// The terminal the shell is told it is drawn on (ADR 0132).
///
/// Every byte the shell writes is drawn by `xterm.js` in the guest's window,
/// so that is the terminal, whatever started this process. A Lumepeer started
/// from the Dock, a desktop menu or a launch agent has no `TERM` at all, and
/// `portable-pty` adds none: `git`, `grep`, `ls` and Debian's prompt then take
/// the terminal for a dumb one and print no colour. One started from another
/// terminal carries that terminal's `TERM`, which names the wrong emulator.
/// `xterm-256color` is what `xterm.js` implements, and its terminfo entry is on
/// every macOS and in Debian's `ncurses-base`.
const TERM: &str = "xterm-256color";

/// The host's shell with the host's environment, plus the one variable
/// [`ctype_override`] says it lacks, and told what it is drawn on.
fn shell_command() -> CommandBuilder {
    // No arguments, and the program is this machine's own (ADR 0079).
    // `CommandBuilder::new` inherits this process's environment, which is
    // the interactive user's, so the shell starts where they would.
    let mut command = CommandBuilder::new(host_shell());
    if let Some(ctype) = ctype_override(
        std::env::var_os("LC_ALL").as_deref(),
        std::env::var_os("LC_CTYPE").as_deref(),
        std::env::var_os("LANG").as_deref(),
    ) {
        command.env("LC_CTYPE", ctype);
    }
    command.env("TERM", TERM);
    // `xterm.js` draws 24-bit colour, and terminfo has no standard way to say
    // so; this variable is where programs look for it.
    command.env("COLORTERM", "truecolor");
    command
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

        let mut child = pair.slave.spawn_command(shell_command()).map_err(|error| {
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

    /// Which environments get a UTF-8 `LC_CTYPE` added, and which are left as
    /// they are (ADR 0130).
    #[test]
    fn a_utf8_ctype_is_added_only_where_the_locale_has_none() {
        let os = |value: &'static str| Some(OsStr::new(value));

        // Nothing set at all: what a Mac app started from the Dock has.
        assert_eq!(ctype_override(None, None, None), Some(UTF8_CTYPE));
        assert_eq!(ctype_override(None, None, os("C")), Some(UTF8_CTYPE));
        assert_eq!(ctype_override(None, None, os("POSIX")), Some(UTF8_CTYPE));
        // Empty is unset, so the next variable down decides.
        assert_eq!(ctype_override(os(""), os(""), os("C")), Some(UTF8_CTYPE));
        assert_eq!(ctype_override(None, os(""), os("en_US.UTF-8")), None);
        // `LC_CTYPE` outranks `LANG`, in both directions.
        assert_eq!(
            ctype_override(None, os("C"), os("en_US.UTF-8")),
            Some(UTF8_CTYPE)
        );
        assert_eq!(ctype_override(None, os("UTF-8"), None), None);

        // Already UTF-8, however it is spelled.
        for locale in ["en_US.UTF-8", "ru_RU.utf8", "C.UTF-8", "de_DE.UTF-8@euro"] {
            assert_eq!(ctype_override(None, None, os(locale)), None, "{locale}");
        }

        // `LC_ALL` is the machine owner's word on every category.
        assert_eq!(ctype_override(os("C"), None, None), None);
        assert_eq!(ctype_override(os("ru_RU.UTF-8"), None, None), None);
    }

    /// The shell itself reports UTF-8, which is what makes a Cyrillic letter
    /// one character to it rather than two bytes drawn as `<0084>`
    /// (ADR 0130).
    ///
    /// Only proves the fix when the test runs with no UTF-8 locale of its
    /// own — `env -u LANG -u LC_ALL -u LC_CTYPE cargo test` — which is what a
    /// Lumepeer started from the Dock has; under a UTF-8 terminal it would
    /// pass without it.
    #[test]
    fn the_shell_speaks_utf8() {
        if nix::unistd::geteuid().is_root() {
            return;
        }
        let shell = Shell::spawn(ShellSize { cols: 80, rows: 24 }).expect("a shell");
        let (mut reader, mut writer, _control) = shell.into_parts();
        // The quotes split the marker so the terminal's echo of this line
        // never contains it; only the printed answer does. The `exit` is what
        // ends the read below even if `locale` answers nothing.
        writer
            .write_all(b"printf 'char''map=%s\\n' \"$(locale charmap)\"; exit\n")
            .unwrap();
        writer.flush().unwrap();

        let mut seen = Vec::new();
        let mut buffer = [0u8; 1024];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => seen.extend_from_slice(&buffer[..read]),
            }
        }
        let seen = String::from_utf8_lossy(&seen);
        let charmap = seen
            .split("charmap=")
            .nth(1)
            .and_then(|rest| rest.lines().next())
            .map(str::trim);
        assert_eq!(charmap, Some("UTF-8"), "the shell said: {seen}");
    }

    /// The shell is told it is drawn on `xterm.js`, whatever this process was
    /// started with — which is what makes `ls`, `git` and the prompt colour
    /// what they print (ADR 0132).
    ///
    /// Only proves the fix when the test runs without those values of its
    /// own — `TERM=dumb COLORTERM= cargo test` — which is what a Lumepeer
    /// started from the Dock has; under a 256-colour terminal `TERM` would
    /// pass without it.
    #[test]
    fn the_shell_knows_its_terminal_draws_colour() {
        if nix::unistd::geteuid().is_root() {
            return;
        }
        let shell = Shell::spawn(ShellSize { cols: 80, rows: 24 }).expect("a shell");
        let (mut reader, mut writer, _control) = shell.into_parts();
        // The quotes split the marker so the terminal's echo of this line
        // never contains it; only the printed answer does. The `exit` is what
        // ends the read below.
        writer
            .write_all(b"printf 'term''=%s/%s\\n' \"$TERM\" \"$COLORTERM\"; exit\n")
            .unwrap();
        writer.flush().unwrap();

        let mut seen = Vec::new();
        let mut buffer = [0u8; 1024];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => seen.extend_from_slice(&buffer[..read]),
            }
        }
        let seen = String::from_utf8_lossy(&seen);
        let answer = seen
            .split("term=")
            .nth(1)
            .and_then(|rest| rest.lines().next())
            .map(str::trim);
        assert_eq!(
            answer,
            Some("xterm-256color/truecolor"),
            "the shell said: {seen}"
        );
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
