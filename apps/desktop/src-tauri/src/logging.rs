//! Tracing setup: human-readable on stdout in development, structured JSON to
//! a rotating file in a release build (§16.1).
//!
//! The file half is not a nicety. A release binary is built with
//! `windows_subsystem = "windows"`, so its stdout is attached to nothing at
//! all: every line the app logged about why a session failed went into a void,
//! and the only way to see any of it was to relaunch the installed client from
//! a shell that supplied a redirected handle. Rotation follows
//! `LOG_ROTATION_DAYS` and `LOG_ROTATION_MAX_MIB` (§14, §16.1).
//!
//! Failing to log is never allowed to fail the app: if the directory cannot be
//! created or the file cannot be opened, tracing falls back to stdout and the
//! app starts anyway (§24.5).

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lumepeer_core::constants::{LOG_ROTATION_DAYS, LOG_ROTATION_MAX_MIB};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;

use crate::config::Settings;

/// Prefix every log file this app writes shares.
const FILE_PREFIX: &str = "lumepeer-";
/// Extension every log file this app writes shares.
const FILE_SUFFIX: &str = ".log";

/// Installs the tracing subscriber and returns the file being written, if any.
///
/// The caller logs that path itself: the subscriber does not exist yet while
/// this runs, so nothing said here would be recorded.
pub fn init(settings: &Settings) -> Option<PathBuf> {
    let filter = || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    if cfg!(debug_assertions) {
        tracing_subscriber::fmt().with_env_filter(filter()).init();
        return None;
    }

    if let Some(file) = settings.log_dir().and_then(|dir| FileLog::open(&dir).ok()) {
        let path = file.path();
        tracing_subscriber::fmt()
            .json()
            .with_env_filter(filter())
            .with_writer(file)
            .init();
        return Some(path);
    }

    // No writable directory: JSON on stdout, exactly as before. Useless in a
    // windowed build, but it is the honest fallback and it keeps a
    // console-launched or redirected run working.
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter())
        .init();
    None
}

/// Handle a `tracing` subscriber writes through. Cloning it is cheap; every
/// clone writes to the same rotating file.
#[derive(Debug, Clone)]
pub struct FileLog {
    inner: Arc<Mutex<Rotating>>,
    path: PathBuf,
}

impl FileLog {
    /// Creates `dir` if needed, prunes expired files and opens a fresh one.
    ///
    /// # Errors
    /// Any I/O error from creating the directory or the file. The caller is
    /// expected to fall back to stdout rather than treat this as fatal.
    pub fn open(dir: &Path) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        prune(
            dir,
            Duration::from_secs(u64::from(LOG_ROTATION_DAYS) * 86_400),
        );
        let rotating = Rotating::open(dir.to_path_buf())?;
        let path = rotating.path.clone();
        Ok(Self {
            inner: Arc::new(Mutex::new(rotating)),
            path,
        })
    }

    /// Path of the file opened at startup.
    #[must_use]
    pub fn path(&self) -> PathBuf {
        self.path.clone()
    }
}

impl<'a> MakeWriter<'a> for FileLog {
    type Writer = Handle;

    fn make_writer(&'a self) -> Self::Writer {
        Handle(Arc::clone(&self.inner))
    }
}

/// One writer handed to the subscriber for the duration of a single event.
#[derive(Debug)]
pub struct Handle(Arc<Mutex<Rotating>>);

impl Write for Handle {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // A poisoned lock means some other thread panicked mid-write. The log
        // file is not session state; keep writing rather than poisoning the
        // whole app over it.
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

/// The open file plus what is needed to decide when to start the next one.
#[derive(Debug)]
struct Rotating {
    dir: PathBuf,
    path: PathBuf,
    file: File,
    written: u64,
    day: i64,
}

impl Rotating {
    fn open(dir: PathBuf) -> io::Result<Self> {
        let now = SystemTime::now();
        let path = dir.join(file_name(now));
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let written = file.metadata().map_or(0, |meta| meta.len());
        Ok(Self {
            dir,
            path,
            file,
            written,
            day: days_since_epoch(now),
        })
    }

    /// Starts the next file when this one is full or the date has moved on.
    fn roll_if_due(&mut self, incoming: usize) -> io::Result<()> {
        let max_bytes = u64::from(LOG_ROTATION_MAX_MIB) * 1024 * 1024;
        let now = SystemTime::now();
        let today = days_since_epoch(now);
        let full = self
            .written
            .saturating_add(incoming.try_into().unwrap_or(u64::MAX))
            > max_bytes;
        if !full && today == self.day {
            return Ok(());
        }
        let retention = Duration::from_secs(u64::from(LOG_ROTATION_DAYS) * 86_400);
        prune(&self.dir, retention);
        let next = Self::open(self.dir.clone())?;
        // Only swap once the replacement is actually open: a failed roll must
        // leave the current file in place rather than lose the lines.
        *self = next;
        Ok(())
    }
}

impl Write for Rotating {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.roll_if_due(buf.len())?;
        let written = self.file.write(buf)?;
        self.written = self
            .written
            .saturating_add(written.try_into().unwrap_or(u64::MAX));
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// `lumepeer-YYYY-MM-DD-HHMMSS.log` in UTC.
///
/// The clock is only ever used to name and expire files, so a rollback shifts
/// which file a line lands in and nothing else.
fn file_name(now: SystemTime) -> String {
    let secs = now
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let (year, month, day) = civil_from_days(days_since_epoch(now));
    let seconds_today = secs % 86_400;
    let (hour, minute, second) = (
        seconds_today / 3_600,
        (seconds_today % 3_600) / 60,
        seconds_today % 60,
    );
    format!(
        "{FILE_PREFIX}{year:04}-{month:02}-{day:02}-{hour:02}{minute:02}{second:02}{FILE_SUFFIX}"
    )
}

/// Deletes this app's log files that are older than `retention`.
///
/// Every error is ignored on purpose: a log directory that cannot be tidied is
/// not a reason to refuse to log, let alone to refuse to start.
fn prune(dir: &Path, retention: Duration) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with(FILE_PREFIX) || !name.ends_with(FILE_SUFFIX) {
            continue;
        }
        let expired = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .and_then(|modified| {
                now.duration_since(modified)
                    .map_err(|_| io::Error::other("modified in the future"))
            })
            .is_ok_and(|age| age > retention);
        if expired {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Whole days between the Unix epoch and `now`.
fn days_since_epoch(now: SystemTime) -> i64 {
    now.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|since| i64::try_from(since.as_secs() / 86_400).ok())
        .unwrap_or(0)
}

/// Civil date from a day count since 1970-01-01, after Howard Hinnant's
/// `civil_from_days`. Pulling a date library in for a file name would be a
/// dependency the supply-chain job has to carry forever.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    // Both land in 1..=31 and 1..=12 by construction; the fallbacks exist so
    // this stays total rather than relying on that being true (§2.4).
    let day = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let month = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "a failed assumption must fail the test"
    )]

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lumepeer-log-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn the_epoch_and_a_known_date_round_trip() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 2026-08-24, the day this was written.
        assert_eq!(civil_from_days(20_689), (2026, 8, 24));
    }

    #[test]
    fn a_file_name_carries_the_date_and_the_prefix() {
        let name = file_name(UNIX_EPOCH);
        assert_eq!(name, "lumepeer-1970-01-01-000000.log");
    }

    #[test]
    fn opening_creates_the_directory_and_the_file_and_writes_land_in_it() {
        let dir = scratch("writes");
        let log = FileLog::open(&dir).expect("the log directory must be creatable");
        let mut handle = log.make_writer();
        handle.write_all(b"{\"level\":\"INFO\"}\n").unwrap();
        handle.flush().unwrap();
        let text = std::fs::read_to_string(log.path()).unwrap();
        assert!(
            text.contains("INFO"),
            "the line must reach the file: {text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pruning_keeps_fresh_files_and_leaves_foreign_ones_alone() {
        let dir = scratch("prune");
        std::fs::create_dir_all(&dir).unwrap();
        let ours = dir.join("lumepeer-2020-01-01-000000.log");
        let theirs = dir.join("something-else.txt");
        std::fs::write(&ours, b"old").unwrap();
        std::fs::write(&theirs, b"not ours").unwrap();
        // Both were written just now, so nothing is expired yet.
        prune(
            &dir,
            Duration::from_secs(u64::from(LOG_ROTATION_DAYS) * 86_400),
        );
        assert!(ours.exists());
        assert!(theirs.exists());
        // With no retention at all, only this app's files go.
        prune(&dir, Duration::ZERO);
        assert!(!ours.exists(), "an expired log file must be removed");
        assert!(
            theirs.exists(),
            "a file this app did not write must never be removed"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
