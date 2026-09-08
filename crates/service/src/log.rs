//! Where the helper service's own diagnostics go.
//!
//! Everything else in this crate is deliberately mute towards its caller: the
//! pipe answers `ok` or `refused` and never says why, because a privileged
//! process that explains itself to an unprivileged one is an oracle (ADR
//! 0043, `protocol.rs`). That rule is about the *wire*. It was never meant to
//! mean nobody can find out what happened, and for a long time that is what it
//! amounted to: the service called `tracing_subscriber::fmt()`, which writes
//! to stdout, and a process started by the service control manager has no
//! stdout at all. Every `tracing::warn!` in this crate — the session-binding
//! refusal, the worker that would not spawn, the frame that did not fit —
//! went nowhere. When the secure desktop stopped working on a real machine
//! there was no way to tell which of the six possible reasons it was, from
//! either side.
//!
//! So: one append-only file, next to nothing else. `LocalSystem` has no
//! `AppData` worth writing to and the workers are separate short-lived
//! processes in a different session, so the path is a fixed machine-wide one
//! under `%ProgramData%` that every one of them derives the same way rather
//! than being told.
//!
//! Failing to log never fails the service. A directory that cannot be created
//! or a file that cannot be opened leaves tracing on the stdout it had before,
//! and the service goes on doing its job.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;

/// Directory the log file lives in, under `%ProgramData%`.
const DIRECTORY: &str = r"Lumepeer\logs";

/// The one file this crate writes. Not dated and not rotated by day: the
/// service is a background thing that logs a handful of lines per session, and
/// a folder of mostly-empty files would be worse to read than one file.
const FILE_NAME: &str = "lumepeer-service.log";

/// Size at which the next process to open the file starts it over, in bytes.
///
/// The cheapest bound that still cannot grow without limit. Truncating on open
/// rather than rolling means the newest lines are always in the same file,
/// which is the one thing somebody debugging this actually needs.
const MAX_BYTES: u64 = 4 * 1024 * 1024;

/// Installs the subscriber. Returns the file being written, if any, so the
/// caller can say where it went.
pub fn init() -> Option<PathBuf> {
    let filter = || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    if let Some(file) = FileLog::open() {
        let path = file.path.clone();
        tracing_subscriber::fmt()
            .with_env_filter(filter())
            .with_ansi(false)
            .with_writer(file)
            .init();
        return Some(path);
    }

    // No writable location. Useless under the service control manager, which
    // supplies no stdout, but it keeps a `--console` run in a shell working
    // exactly as it did before.
    tracing_subscriber::fmt().with_env_filter(filter()).init();
    None
}

/// The open file, shared by every clone the subscriber makes of it.
#[derive(Debug, Clone)]
struct FileLog {
    file: Arc<Mutex<File>>,
    path: PathBuf,
}

impl FileLog {
    /// Creates the directory if needed and opens the file for appending,
    /// starting it over if what is already there is past [`MAX_BYTES`].
    fn open() -> Option<Self> {
        let path = directory().join(FILE_NAME);
        std::fs::create_dir_all(path.parent()?).ok()?;
        let oversized = std::fs::metadata(&path).is_ok_and(|meta| meta.len() > MAX_BYTES);
        let file = OpenOptions::new()
            .create(true)
            .append(!oversized)
            .write(oversized)
            .truncate(oversized)
            .open(&path)
            .ok()?;
        Some(Self {
            file: Arc::new(Mutex::new(file)),
            path,
        })
    }
}

impl<'a> MakeWriter<'a> for FileLog {
    type Writer = Handle;

    fn make_writer(&'a self) -> Self::Writer {
        Handle(Arc::clone(&self.file))
    }
}

/// One writer handed to the subscriber for a single event.
#[derive(Debug)]
struct Handle(Arc<Mutex<File>>);

impl Write for Handle {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // A poisoned lock means another thread panicked mid-write. A log file
        // is not state anything depends on; keep writing rather than turning
        // it into a second failure.
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .flush()
    }
}

/// The machine-wide directory the file lives in.
///
/// `%ProgramData%` is what `LocalSystem` and the console-session worker both
/// resolve to the same place, unlike anything under a user profile. The
/// literal fallback is for the case where the variable is missing from a
/// service's environment, which it should not be.
fn directory() -> PathBuf {
    std::env::var_os("ProgramData")
        .map_or_else(|| PathBuf::from(r"C:\ProgramData"), PathBuf::from)
        .join(DIRECTORY)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The path is machine-wide, not per-user: the service runs as
    /// `LocalSystem` in session 0 and its workers run in the console session,
    /// and both have to write the same file for the log to tell one story.
    #[test]
    fn the_directory_is_under_program_data() {
        let dir = directory();
        assert!(dir.ends_with(DIRECTORY), "unexpected directory: {dir:?}");
        assert!(!dir.to_string_lossy().contains("AppData"));
    }
}
