//! This machine's own clipboard (design doc §9.2; ADR 0030, ADR 0047).
//!
//! One worker, used the same way whichever role a session gives this node:
//! a host reads its own clipboard for a guest holding `clipboard_read`, and
//! a guest reads its own clipboard to offer a host it has an open view onto
//! (docs/bugs/10-clipboard-auto.md #1) — `network.rs::refresh_clipboard_watch`
//! is what decides, each time, whether either reason currently applies.
//!
//! Two decisions are baked into this module's shape.
//!
//! **The clipboard is read and written from Rust, never from the webview.**
//! Tauri has a clipboard plugin, and using it would have been one capability
//! line — but a capability line is exactly what it would have been: a
//! standing grant handing the untrusted presentation layer (§2.3) a live
//! handle on this machine's own clipboard, valid whether or not any session
//! exists. The webview asks the actor to sync; only the actor touches the
//! real clipboard, and only while a session or a view justifies it.
//!
//! **Everything OS-facing happens on one dedicated thread.** Reading a
//! clipboard is not a cheap in-process lookup: on X11 it is a round trip to
//! whichever client currently owns the selection, and that client can be
//! slow, wedged or gone. Running that on the actor loop would put an
//! unrelated application's responsiveness in the path of a revoke, which is
//! the failure ADR 0027 was written about. So the actor sends jobs to this
//! thread and receives changes back as events, and never blocks on either.
//!
//! The [`OsClipboard`] seam exists for the same reason [`crate::view::
//! HostMedia`] does: CI has no display, `arboard` needs one, and a test that
//! cannot run headless is a test that stops running.
//!
//! **File lists are a separate capability from text, read on this same
//! thread** (docs/bugs/14-clipboard-files.md #1; ADR 0047). `arboard` has no
//! notion of a file list at all, so [`OsClipboard::read_file_paths`] and
//! [`OsClipboard::write_file_paths`] go straight to a platform-specific
//! implementation per target, each chosen to add no new dependency *version*
//! to the tree — only a direct edge onto one `arboard` itself already pulls
//! in transitively on that platform (see the `platform` module below and the
//! ADR for the full comparison). Reading a file list is exactly the same kind
//! of round trip reading text is, so it is never done from the actor loop
//! either.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use lumepeer_core::constants::CLIPBOARD_POLL_INTERVAL_MS;

/// Why one clipboard operation did not happen.
///
/// Never fatal to a session: a clipboard is shared mutable state owned by
/// whatever else the user is running, and losing one round of sync to a
/// browser that held the selection too long is not a reason to tear anything
/// down (§18: degrade, do not fail).
#[derive(Debug)]
pub enum ClipboardError {
    /// This build or this machine has no clipboard at all (headless CI, a
    /// container, a platform `arboard` does not cover).
    Unavailable,
    /// The OS refused the operation, usually because another process holds
    /// the clipboard right now.
    Refused(String),
}

impl std::fmt::Display for ClipboardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable => write!(f, "no clipboard on this machine"),
            Self::Refused(why) => write!(f, "the clipboard refused the operation: {why}"),
        }
    }
}

impl std::error::Error for ClipboardError {}

/// The host machine's text clipboard, narrowed to what §9.2 exchanges.
///
/// Text only, on purpose: images and file lists are outside §9.2 for v1, and
/// an interface that cannot express them cannot accidentally start carrying
/// them.
pub trait OsClipboard {
    /// The clipboard's current text, or `None` when it holds something else,
    /// nothing, or could not be read at all.
    ///
    /// A failed read is not distinguishable from an empty clipboard here, and
    /// deliberately so: the only caller is a poll loop whose response to both
    /// is "nothing to send this round".
    fn read_text(&mut self) -> Option<String>;

    /// Places `text` on the clipboard.
    ///
    /// # Errors
    /// [`ClipboardError`] when the platform has no clipboard or refused the
    /// write; the caller logs and moves on.
    fn write_text(&mut self, text: &str) -> Result<(), ClipboardError>;

    /// The clipboard's current list of file paths (docs/bugs/
    /// 14-clipboard-files.md #1), or `None` when it holds something else,
    /// nothing, or could not be read.
    ///
    /// Bounded by `CLIPBOARD_FILE_LIST_MAX_ENTRIES` and
    /// `CLIPBOARD_FILE_PATH_MAX_BYTES` before this returns: a clipboard's file
    /// list is untrusted input — another local application's, not this
    /// user's own typed text — exactly the allocation-DoS shape §9.1 already
    /// guards against on the wire, applied here to what a foreign process on
    /// this same machine can hand the clipboard.
    fn read_file_paths(&mut self) -> Option<Vec<PathBuf>>;

    /// Places `paths` on the clipboard as a file list, so a paste in the
    /// user's own file manager actually produces them (docs/bugs/
    /// 14-clipboard-files.md #3).
    ///
    /// # Errors
    /// [`ClipboardError`] when the platform has no clipboard or refused the
    /// write.
    fn write_file_paths(&mut self, paths: &[PathBuf]) -> Result<(), ClipboardError>;
}

/// Builds an [`OsClipboard`] on the thread that will own it.
///
/// A factory rather than a value because the platform handle behind
/// `arboard` belongs to the thread that opened it — on X11 it owns a
/// connection that serves selection requests for as long as this process is
/// the selection owner. Constructing it here would mean moving it across a
/// thread boundary for no reason.
pub type ClipboardFactory = Box<dyn FnOnce() -> Box<dyn OsClipboard> + Send + 'static>;

/// `arboard` over the platform clipboard.
///
/// The handle is opened lazily and re-opened after a failure: on Linux the
/// connection can be lost with the display server, and a permanently dead
/// handle would silently disable the clipboard for the rest of the process's
/// life.
#[derive(Default)]
struct ArboardClipboard {
    inner: Option<arboard::Clipboard>,
}

impl std::fmt::Debug for ArboardClipboard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArboardClipboard")
            .field("open", &self.inner.is_some())
            .finish()
    }
}

impl ArboardClipboard {
    fn handle(&mut self) -> Result<&mut arboard::Clipboard, ClipboardError> {
        if self.inner.is_none() {
            match arboard::Clipboard::new() {
                Ok(clipboard) => self.inner = Some(clipboard),
                Err(error) => {
                    tracing::debug!(%error, "no clipboard available on this machine");
                    return Err(ClipboardError::Unavailable);
                }
            }
        }
        self.inner.as_mut().ok_or(ClipboardError::Unavailable)
    }
}

impl OsClipboard for ArboardClipboard {
    fn read_text(&mut self) -> Option<String> {
        // Content never reaches a log line, here or anywhere else (§15): the
        // clipboard is the single most likely place for a password to be.
        match self.handle().and_then(|c| {
            c.get_text()
                .map_err(|e| ClipboardError::Refused(e.to_string()))
        }) {
            Ok(text) => Some(text),
            Err(error) => {
                tracing::debug!(%error, "clipboard read skipped this round");
                self.inner = None;
                None
            }
        }
    }

    fn write_text(&mut self, text: &str) -> Result<(), ClipboardError> {
        let result = self.handle().and_then(|c| {
            c.set_text(text.to_owned())
                .map_err(|e| ClipboardError::Refused(e.to_string()))
        });
        if result.is_err() {
            self.inner = None;
        }
        result
    }

    fn read_file_paths(&mut self) -> Option<Vec<PathBuf>> {
        platform::read_file_paths()
    }

    fn write_file_paths(&mut self, paths: &[PathBuf]) -> Result<(), ClipboardError> {
        platform::write_file_paths(paths)
    }
}

/// Platform-specific file-list access (docs/bugs/14-clipboard-files.md #1).
///
/// `arboard` covers none of these three formats at all — it is text (and,
/// behind a feature this workspace does not enable, images) only. Rather than
/// add a general-purpose clipboard crate on top of the one already in the
/// tree, each arm below calls the lowest-level crate that already exists at
/// the version `arboard`'s own backend pulls in on that platform, so nothing
/// here adds a *new* dependency version to `Cargo.lock` — only a direct edge
/// onto one this process already links transitively. See ADR 0047 for the
/// full comparison against a general-purpose alternative.
#[cfg(target_os = "windows")]
mod platform {
    use std::num::NonZeroUsize;
    use std::path::PathBuf;

    use clipboard_win::formats::{CF_HDROP, FileList};
    use lumepeer_core::constants::{
        CLIPBOARD_FILE_LIST_MAX_ENTRIES, CLIPBOARD_FILE_PATH_MAX_BYTES,
    };

    use super::ClipboardError;

    /// A generous ceiling on the whole `CF_HDROP` block, checked before
    /// `clipboard-win` walks it: `DROPFILES`'s own header plus
    /// `CLIPBOARD_FILE_LIST_MAX_ENTRIES` UTF-16 paths of
    /// `CLIPBOARD_FILE_PATH_MAX_BYTES` bytes each, doubled for UTF-16 and
    /// given headroom for the header and the terminating nulls.
    fn max_hdrop_bytes() -> usize {
        CLIPBOARD_FILE_LIST_MAX_ENTRIES * CLIPBOARD_FILE_PATH_MAX_BYTES * 2 + 4096
    }

    /// `CF_HDROP` via `clipboard-win`, the exact crate (and pinned version)
    /// `arboard`'s own Windows text backend already depends on.
    ///
    /// The byte-length check runs before `clipboard-win` allocates a single
    /// `PathBuf`: an absurd `CF_HDROP` block is untrusted input from whichever
    /// other local process currently owns the clipboard, so it is bounded the
    /// same way an oversized wire frame is (§9.1). The per-path length check
    /// afterwards cannot happen any earlier — `clipboard-win`'s `FileList`
    /// getter has no lower-level entry point that yields one path at a time —
    /// so it is the second, narrower gate rather than the only one.
    pub fn read_file_paths() -> Option<Vec<PathBuf>> {
        let byte_len = clipboard_win::raw::size(CF_HDROP).map_or(0, NonZeroUsize::get);
        if byte_len == 0 {
            // No `CF_HDROP` format on the clipboard at all: not files, and
            // not an error either.
            return None;
        }
        if byte_len > max_hdrop_bytes() {
            tracing::warn!(byte_len, "refusing an oversized clipboard file list");
            return None;
        }
        let mut paths = match clipboard_win::get_clipboard::<Vec<PathBuf>, _>(FileList) {
            Ok(paths) => paths,
            Err(error) => {
                tracing::debug!(%error, "clipboard file list read skipped this round");
                return None;
            }
        };
        if paths.len() > CLIPBOARD_FILE_LIST_MAX_ENTRIES {
            tracing::warn!(
                count = paths.len(),
                "refusing a clipboard file list past the entry limit"
            );
            return None;
        }
        paths.retain(|path| path.as_os_str().len() <= CLIPBOARD_FILE_PATH_MAX_BYTES);
        if paths.is_empty() { None } else { Some(paths) }
    }

    pub fn write_file_paths(paths: &[PathBuf]) -> Result<(), ClipboardError> {
        let strings: Vec<String> = paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        // `set_clipboard`'s generic `Setter` bound needs a `Sized` value type,
        // which a slice is not; `raw::set_file_list` is the same operation
        // one level down, over an explicitly held clipboard guard instead.
        let _clip = clipboard_win::Clipboard::new_attempts(10)
            .map_err(|error| ClipboardError::Refused(error.to_string()))?;
        clipboard_win::raw::set_file_list(&strings)
            .map_err(|error| ClipboardError::Refused(error.to_string()))
    }
}

/// macOS: `NSPasteboardTypeFileURL` via `NSPasteboard`, through `objc2-app-
/// kit` and `objc2-foundation` — the same crates (and pinned versions)
/// `arboard`'s own macOS text backend already depends on.
#[cfg(target_os = "macos")]
mod platform {
    use std::path::PathBuf;

    use lumepeer_core::constants::{
        CLIPBOARD_FILE_LIST_MAX_ENTRIES, CLIPBOARD_FILE_PATH_MAX_BYTES,
    };
    use objc2_app_kit::NSPasteboard;
    use objc2_foundation::{NSArray, NSString, NSURL};

    use super::ClipboardError;

    /// `NSPasteboardTypeFileURL`'s value, built directly rather than read from
    /// objc2's generated `extern "C"` static: `apps/desktop` forbids (not
    /// merely denies) `unsafe_code` crate-wide, and unlike `crates/media`'s
    /// `deny`, `forbid` cannot be locally overridden. `NSPasteboardType` is
    /// itself just an `NSString` (the UTI), so building this one is safe.
    fn file_url_type() -> objc2::rc::Retained<objc2_app_kit::NSPasteboardType> {
        NSString::from_str("public.file-url")
    }

    pub fn read_file_paths() -> Option<Vec<PathBuf>> {
        let pasteboard = NSPasteboard::generalPasteboard();
        let items = pasteboard.pasteboardItems()?;
        // Checked before this side allocates a single `PathBuf`: the
        // pasteboard is untrusted input from whichever other local
        // application last owned it (§9.1's allocation bound, applied here).
        if items.count() > CLIPBOARD_FILE_LIST_MAX_ENTRIES {
            tracing::warn!(
                count = items.count(),
                "refusing a clipboard file list past the entry limit"
            );
            return None;
        }
        let file_url_type = file_url_type();
        let mut paths = Vec::new();
        for item in &items {
            let Some(value) = item.stringForType(&file_url_type) else {
                continue;
            };
            let url_string = value.to_string();
            if url_string.len() > CLIPBOARD_FILE_PATH_MAX_BYTES {
                continue;
            }
            if let Some(path) = file_url_to_path(&url_string) {
                paths.push(path);
            }
        }
        if paths.is_empty() { None } else { Some(paths) }
    }

    /// `file:///a/b%20c` -> `/a/b c`. Percent-decodes the path component of a
    /// `file://` URL; anything that is not that scheme is refused rather than
    /// guessed at.
    fn file_url_to_path(url: &str) -> Option<PathBuf> {
        let path = url.strip_prefix("file://")?;
        let decoded = percent_encoding::percent_decode_str(path)
            .decode_utf8()
            .ok()?;
        Some(PathBuf::from(decoded.into_owned()))
    }

    pub fn write_file_paths(paths: &[PathBuf]) -> Result<(), ClipboardError> {
        let pasteboard = NSPasteboard::generalPasteboard();
        pasteboard.clearContents();
        let urls: Vec<objc2::rc::Retained<NSURL>> = paths
            .iter()
            .map(|path| NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy())))
            .collect();
        let objects: Vec<
            objc2::rc::Retained<
                objc2::runtime::ProtocolObject<dyn objc2_app_kit::NSPasteboardWriting>,
            >,
        > = urls
            .iter()
            .map(|url| {
                objc2::runtime::ProtocolObject::<dyn objc2_app_kit::NSPasteboardWriting>::from_ref(
                    &**url,
                )
                .into()
            })
            .collect();
        let array = NSArray::from_retained_slice(&objects);
        if pasteboard.writeObjects(&array) {
            Ok(())
        } else {
            Err(ClipboardError::Refused(
                "NSPasteboard refused the file list".to_owned(),
            ))
        }
    }
}

/// Linux (X11, and Wayland through XWayland): `text/uri-list` on the
/// `CLIPBOARD` selection, through `x11-clipboard` — a small, purpose-built
/// crate over `x11rb` at the same major version `arboard`'s own Linux text
/// backend already depends on, so this adds no new *version* of `x11rb` to
/// the tree.
///
/// The same limitation `arboard`'s own text backend already has on Linux:
/// there is no native Wayland clipboard protocol implementation here, only
/// the X11 selection mechanism, which a pure-Wayland compositor with no
/// `XWayland` compatibility layer does not serve. That is an existing gap this
/// task does not widen.
#[cfg(target_os = "linux")]
mod platform {
    use std::path::PathBuf;
    use std::time::Duration;

    use lumepeer_core::constants::{
        CLIPBOARD_FILE_LIST_MAX_ENTRIES, CLIPBOARD_FILE_PATH_MAX_BYTES,
    };
    use percent_encoding::AsciiSet;

    use super::ClipboardError;

    /// How long a read waits for the current selection owner to answer.
    ///
    /// A clipboard read is a round trip to another process (this module's own
    /// header, and ADR 0027): a selection owner that is slow or wedged must
    /// cost this poll one round, not hang the dedicated clipboard thread.
    const SELECTION_TIMEOUT: Duration = Duration::from_millis(200);

    /// Characters a `file://` URI's path component must not carry literally
    /// (RFC 3986 `pchar`, minus the separators this code needs to keep
    /// readable: `/ - . _ ~`).
    const PATH_ENCODE_SET: &AsciiSet = &percent_encoding::NON_ALPHANUMERIC
        .remove(b'/')
        .remove(b'-')
        .remove(b'.')
        .remove(b'_')
        .remove(b'~');

    fn context() -> Option<x11_clipboard::Clipboard> {
        match x11_clipboard::Clipboard::new() {
            Ok(clipboard) => Some(clipboard),
            Err(error) => {
                tracing::debug!(%error, "no X11 display for the clipboard file-list seam");
                None
            }
        }
    }

    pub fn read_file_paths() -> Option<Vec<PathBuf>> {
        let clipboard = context()?;
        let uri_list = clipboard.getter.get_atom("text/uri-list").ok()?;
        let property = clipboard.getter.atoms.property;
        let raw = clipboard
            .load(
                clipboard.getter.atoms.clipboard,
                uri_list,
                property,
                SELECTION_TIMEOUT,
            )
            .ok()?;
        // Checked before this side allocates a single `PathBuf`: the
        // selection owner is untrusted input from whichever other local
        // application currently holds it (§9.1's allocation bound, applied
        // here). A generous ceiling on the whole payload, mirroring the
        // Windows `CF_HDROP` byte-length pre-check.
        let max_bytes = CLIPBOARD_FILE_LIST_MAX_ENTRIES * (CLIPBOARD_FILE_PATH_MAX_BYTES + 16);
        if raw.len() > max_bytes {
            tracing::warn!(
                byte_len = raw.len(),
                "refusing an oversized clipboard file list"
            );
            return None;
        }
        let text = String::from_utf8_lossy(&raw);
        let mut paths = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            // RFC 2483 `text/uri-list`: blank lines and `#`-comments are not
            // entries.
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line.len() > CLIPBOARD_FILE_PATH_MAX_BYTES {
                continue;
            }
            if let Some(path) = uri_to_path(line) {
                paths.push(path);
            }
            if paths.len() >= CLIPBOARD_FILE_LIST_MAX_ENTRIES {
                break;
            }
        }
        if paths.is_empty() { None } else { Some(paths) }
    }

    fn uri_to_path(uri: &str) -> Option<PathBuf> {
        let path = uri.strip_prefix("file://")?;
        let decoded = percent_encoding::percent_decode_str(path)
            .decode_utf8()
            .ok()?;
        Some(PathBuf::from(decoded.into_owned()))
    }

    pub fn write_file_paths(paths: &[PathBuf]) -> Result<(), ClipboardError> {
        let clipboard = context().ok_or(ClipboardError::Unavailable)?;
        let uri_list = clipboard
            .getter
            .get_atom("text/uri-list")
            .map_err(|error| ClipboardError::Refused(error.to_string()))?;
        let mut body = String::new();
        for path in paths {
            let path_text = path.to_string_lossy();
            let encoded = percent_encoding::utf8_percent_encode(&path_text, PATH_ENCODE_SET);
            body.push_str("file://");
            for chunk in encoded {
                body.push_str(chunk);
            }
            body.push_str("\r\n");
        }
        clipboard
            .store(
                clipboard.getter.atoms.clipboard,
                uri_list,
                body.into_bytes(),
            )
            .map_err(|error| ClipboardError::Refused(error.to_string()))
    }
}

/// Any other target this workspace does not ship a desktop client for: no
/// file-list support, exactly like the text path degrades when `arboard`
/// itself has no backend.
#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
mod platform {
    use std::path::PathBuf;

    use super::ClipboardError;

    pub fn read_file_paths() -> Option<Vec<PathBuf>> {
        None
    }

    pub fn write_file_paths(_paths: &[PathBuf]) -> Result<(), ClipboardError> {
        Err(ClipboardError::Unavailable)
    }
}

/// A machine with no clipboard: every read is empty and every write fails.
///
/// This is the seam CI runs on. It is not a silent success — a write reports
/// [`ClipboardError::Unavailable`], so a test that believed it had synced
/// learns otherwise.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct NoClipboard;

#[cfg(test)]
impl OsClipboard for NoClipboard {
    fn read_text(&mut self) -> Option<String> {
        None
    }

    fn write_text(&mut self, _text: &str) -> Result<(), ClipboardError> {
        Err(ClipboardError::Unavailable)
    }

    fn read_file_paths(&mut self) -> Option<Vec<PathBuf>> {
        None
    }

    fn write_file_paths(&mut self, _paths: &[PathBuf]) -> Result<(), ClipboardError> {
        Err(ClipboardError::Unavailable)
    }
}

/// The platform clipboard of this machine.
#[must_use]
pub fn platform_clipboard() -> ClipboardFactory {
    Box::new(|| Box::new(ArboardClipboard::default()))
}

/// A clipboard that does nothing, for tests and headless runners.
#[cfg(test)]
#[must_use]
pub fn no_clipboard() -> ClipboardFactory {
    Box::new(|| Box::new(NoClipboard))
}

/// One request from the actor to the clipboard thread.
enum ClipboardJob {
    /// Put this text on the local clipboard (a peer's payload, already
    /// validated and authorized).
    Write(String),
    /// Put this file list on the local clipboard — a completed receive's
    /// paths, so pasting them actually works (docs/bugs/
    /// 14-clipboard-files.md #3).
    WriteFiles(Vec<PathBuf>),
    /// Read the local clipboard's current file list, if it has one
    /// (docs/bugs/14-clipboard-files.md #1). Answered on a oneshot rather
    /// than through `on_change`: this is an on-demand request tied to one
    /// IPC call, not a continuous watch like text's.
    ReadFiles(tokio::sync::oneshot::Sender<Option<Vec<PathBuf>>>),
}

/// The actor's handle on the clipboard thread.
///
/// Dropping every clone of it ends the thread: the job channel closes and
/// the loop returns. `Clone` exists so a task spawned off the actor loop —
/// reading the local clipboard's file list, which is a round trip like any
/// other clipboard read (docs/bugs/14-clipboard-files.md #1) — can hold its
/// own handle without borrowing the actor.
#[derive(Debug, Clone)]
pub struct ClipboardWorker {
    jobs: std::sync::mpsc::Sender<ClipboardJob>,
    /// Whether any live session currently permits reading this machine's
    /// clipboard. `false` means the loop does not read it at all — not that
    /// it reads and discards (§8.1: no capture without a viewer, and the
    /// clipboard is no different).
    watching: Arc<AtomicBool>,
}

impl ClipboardWorker {
    /// Asks the clipboard thread to place `text` on the local clipboard.
    ///
    /// Fire and forget: the answer that matters (did the peer's paste land?)
    /// is one the user sees in their own application, and a failed write is
    /// logged by the thread that attempted it.
    pub fn write(&self, text: String) {
        if self.jobs.send(ClipboardJob::Write(text)).is_err() {
            tracing::debug!("the clipboard thread is gone; dropping an inbound payload");
        }
    }

    /// Asks the clipboard thread to place `paths` on the local clipboard as
    /// a file list (docs/bugs/14-clipboard-files.md #3).
    ///
    /// Fire and forget, for the same reason [`Self::write`] is: a failed
    /// write is logged by the thread that attempted it, and there is no
    /// caller waiting on the outcome.
    pub fn write_files(&self, paths: Vec<PathBuf>) {
        if self.jobs.send(ClipboardJob::WriteFiles(paths)).is_err() {
            tracing::debug!("the clipboard thread is gone; dropping a file-list payload");
        }
    }

    /// Reads this machine's own clipboard file list, on the dedicated
    /// thread, exactly once (docs/bugs/14-clipboard-files.md #1).
    ///
    /// Unlike text, file lists are not polled continuously: this answers one
    /// on-demand request (a user action offering the local clipboard's
    /// files to a peer), so `None` covers both "not files" and "the
    /// clipboard thread is gone" alike — the caller's response to either is
    /// the same, offering nothing.
    pub async fn read_files(&self) -> Option<Vec<PathBuf>> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        if self.jobs.send(ClipboardJob::ReadFiles(reply)).is_err() {
            return None;
        }
        rx.await.ok().flatten()
    }

    /// Turns polling on or off.
    ///
    /// Called whenever the set of sessions that may read this machine's
    /// clipboard changes — a grant moving, a session ending, a view opening
    /// or closing.
    pub fn set_watching(&self, on: bool) {
        let was = self.watching.swap(on, Ordering::Relaxed);
        if was != on {
            tracing::info!(watching = on, "clipboard watch");
        }
    }
}

/// Starts the clipboard thread.
///
/// `on_change` receives the clipboard's new text every time the poll sees it
/// change while watching is on. It is a bounded channel on purpose: if the
/// actor is far enough behind that it has filled up, the right answer is to
/// skip this round and re-detect the same content next time, not to queue
/// clipboard history.
#[must_use]
pub fn spawn(
    make: ClipboardFactory,
    on_change: tokio::sync::mpsc::Sender<String>,
) -> ClipboardWorker {
    let (jobs_tx, jobs_rx) = std::sync::mpsc::channel();
    let watching = Arc::new(AtomicBool::new(false));
    let thread_watching = Arc::clone(&watching);

    // A plain OS thread, not `spawn_blocking`: this one blocks by design and
    // lives as long as the actor, which is exactly what the blocking pool is
    // not for.
    std::thread::Builder::new()
        .name("clipboard".to_owned())
        .spawn(move || run(make(), &jobs_rx, &thread_watching, &on_change))
        .map_or_else(
            |error| tracing::warn!(%error, "no clipboard thread: clipboard sync is off"),
            |_handle| (),
        );

    ClipboardWorker {
        jobs: jobs_tx,
        watching,
    }
}

/// The clipboard thread's loop: serve writes promptly, poll for changes on
/// the interval, and read nothing at all while watching is off.
fn run(
    mut clipboard: Box<dyn OsClipboard>,
    jobs: &std::sync::mpsc::Receiver<ClipboardJob>,
    watching: &AtomicBool,
    on_change: &tokio::sync::mpsc::Sender<String>,
) {
    let interval = Duration::from_millis(CLIPBOARD_POLL_INTERVAL_MS);
    // What the clipboard held the last time this loop looked, or last put
    // there itself. `None` while watching is off, so turning a grant on
    // starts from a fresh baseline rather than from a memory of what the user
    // copied while nobody was allowed to see it.
    let mut seen: Option<String> = None;
    let mut had_baseline = false;
    let mut next_poll = Instant::now() + interval;

    loop {
        let wait = next_poll.saturating_duration_since(Instant::now());
        match jobs.recv_timeout(wait) {
            Ok(ClipboardJob::Write(text)) => {
                match clipboard.write_text(&text) {
                    // Remembering what we just wrote is the outer half of the
                    // echo suppression: a payload this machine applied on a
                    // peer's behalf must not come back round as a local change
                    // and be sent straight back to that peer.
                    Ok(()) => seen = Some(text),
                    Err(error) => tracing::debug!(%error, "could not apply a peer's clipboard"),
                }
                continue;
            }
            Ok(ClipboardJob::WriteFiles(paths)) => {
                if let Err(error) = clipboard.write_file_paths(&paths) {
                    tracing::debug!(%error, "could not apply a clipboard file-list payload");
                }
                continue;
            }
            Ok(ClipboardJob::ReadFiles(reply)) => {
                let _ = reply.send(clipboard.read_file_paths());
                continue;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            // The actor dropped its handle: nothing left to serve.
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
        }

        next_poll = Instant::now() + interval;
        if !watching.load(Ordering::Relaxed) {
            seen = None;
            had_baseline = false;
            continue;
        }
        let Some(text) = clipboard.read_text() else {
            continue;
        };
        if !had_baseline {
            // First look after a grant turned on. Whatever is on the clipboard
            // predates the decision, so it is the starting point, not news.
            seen = Some(text);
            had_baseline = true;
            continue;
        }
        if seen.as_deref() == Some(text.as_str()) {
            continue;
        }
        // `seen` moves forward only once the change is actually queued: a
        // full channel means this round is lost, and the same content has to
        // still look new on the next one.
        if on_change.try_send(text.clone()).is_ok() {
            seen = Some(text);
        }
    }
}

/// An in-memory clipboard the tests of this crate drive directly.
///
/// Shared with the actor tests in [`crate::network`], which need to assert
/// something stronger than "nothing was sent": that with no grant live the
/// user's clipboard was never *read*. `reads` is what makes that assertion
/// possible.
#[cfg(test)]
pub mod testing {
    #![allow(clippy::unwrap_used, reason = "a failed assumption must fail the test")]

    use std::sync::{Arc, Mutex};

    use super::{ClipboardError, ClipboardFactory, OsClipboard};

    use std::path::PathBuf;

    /// What a test clipboard holds and what has been done to it.
    #[derive(Debug, Default)]
    pub struct TestClipboardState {
        /// Current contents, as a test set them or a write left them.
        pub text: Option<String>,
        /// Every text written to it, in order.
        pub writes: Vec<String>,
        /// How many times it has been read. Zero is a real assertion: §8.1
        /// says an ungranted session must not read the user's clipboard at
        /// all, not read it and discard the result.
        pub reads: usize,
        /// Current file list, as a test set it (docs/bugs/
        /// 14-clipboard-files.md #1).
        pub files: Option<Vec<PathBuf>>,
        /// Every file list written to it, in order (docs/bugs/
        /// 14-clipboard-files.md #3).
        pub file_writes: Vec<Vec<PathBuf>>,
    }

    /// A test clipboard's shared state.
    pub type SharedTestClipboard = Arc<Mutex<TestClipboardState>>;

    #[derive(Debug)]
    struct TestClipboard(SharedTestClipboard);

    impl OsClipboard for TestClipboard {
        fn read_text(&mut self) -> Option<String> {
            let mut state = self.0.lock().unwrap();
            state.reads += 1;
            state.text.clone()
        }

        fn write_text(&mut self, text: &str) -> Result<(), ClipboardError> {
            let mut state = self.0.lock().unwrap();
            state.text = Some(text.to_owned());
            state.writes.push(text.to_owned());
            Ok(())
        }

        fn read_file_paths(&mut self) -> Option<Vec<PathBuf>> {
            self.0.lock().unwrap().files.clone()
        }

        fn write_file_paths(&mut self, paths: &[PathBuf]) -> Result<(), ClipboardError> {
            let mut state = self.0.lock().unwrap();
            state.files = Some(paths.to_vec());
            state.file_writes.push(paths.to_vec());
            Ok(())
        }
    }

    /// A clipboard factory plus the state it will be backed by.
    #[must_use]
    pub fn test_clipboard() -> (ClipboardFactory, SharedTestClipboard) {
        let state: SharedTestClipboard = Arc::new(Mutex::new(TestClipboardState::default()));
        let handed = Arc::clone(&state);
        (Box::new(move || Box::new(TestClipboard(handed))), state)
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "a failed assumption must fail the test"
    )]

    use super::testing::test_clipboard as fake;
    use super::*;

    /// The gate of §8.1 applied to the clipboard: with no grant live, the
    /// loop must not read the user's clipboard at all — not read it and throw
    /// the result away.
    #[tokio::test(flavor = "multi_thread")]
    async fn nothing_is_read_while_the_watch_is_off() {
        let (factory, state) = fake();
        state.lock().unwrap().text = Some("a password, probably".to_owned());
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let worker = spawn(factory, tx);

        tokio::time::sleep(Duration::from_millis(CLIPBOARD_POLL_INTERVAL_MS * 3)).await;
        assert!(rx.try_recv().is_err(), "a change escaped without a grant");
        drop(worker);
    }

    /// Turning a grant on adopts whatever is already there as the baseline:
    /// the decision was "the guest may see what I copy", not "the guest may
    /// have what I copied before deciding".
    #[tokio::test(flavor = "multi_thread")]
    async fn the_first_look_after_a_grant_is_a_baseline_not_a_change() {
        let (factory, state) = fake();
        state.lock().unwrap().text = Some("older than the grant".to_owned());
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let worker = spawn(factory, tx);
        worker.set_watching(true);

        tokio::time::sleep(Duration::from_millis(CLIPBOARD_POLL_INTERVAL_MS * 3)).await;
        assert!(rx.try_recv().is_err());

        state.lock().unwrap().text = Some("copied after the grant".to_owned());
        let change = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(change, "copied after the grant");
        drop(worker);
    }

    /// A payload applied on a peer's behalf must not be re-detected as this
    /// machine's own local change and sent straight back (§9.2 loop
    /// suppression, outer half).
    #[tokio::test(flavor = "multi_thread")]
    async fn an_applied_payload_does_not_come_back_as_a_local_change() {
        let (factory, state) = fake();
        state.lock().unwrap().text = Some("start".to_owned());
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let worker = spawn(factory, tx);
        worker.set_watching(true);
        tokio::time::sleep(Duration::from_millis(CLIPBOARD_POLL_INTERVAL_MS * 2)).await;

        worker.write("from the peer".to_owned());
        tokio::time::sleep(Duration::from_millis(CLIPBOARD_POLL_INTERVAL_MS * 3)).await;
        assert!(rx.try_recv().is_err(), "the applied payload echoed back");
        assert_eq!(
            state.lock().unwrap().writes,
            vec!["from the peer".to_owned()]
        );
        drop(worker);
    }

    /// Turning the watch back off forgets the baseline, so a clipboard change
    /// made while nobody was allowed to look is not reported when the next
    /// grant arrives.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_change_made_while_unwatched_is_not_reported_later() {
        let (factory, state) = fake();
        state.lock().unwrap().text = Some("start".to_owned());
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let worker = spawn(factory, tx);
        worker.set_watching(true);
        tokio::time::sleep(Duration::from_millis(CLIPBOARD_POLL_INTERVAL_MS * 2)).await;

        worker.set_watching(false);
        tokio::time::sleep(Duration::from_millis(CLIPBOARD_POLL_INTERVAL_MS * 2)).await;
        state.lock().unwrap().text = Some("private".to_owned());
        tokio::time::sleep(Duration::from_millis(CLIPBOARD_POLL_INTERVAL_MS * 2)).await;

        worker.set_watching(true);
        tokio::time::sleep(Duration::from_millis(CLIPBOARD_POLL_INTERVAL_MS * 3)).await;
        assert!(rx.try_recv().is_err(), "a pre-grant clipboard was reported");
        drop(worker);
    }

    /// A machine with no clipboard degrades to doing nothing, and says so on
    /// the write path rather than reporting a success that did not happen.
    #[test]
    fn a_machine_without_a_clipboard_reports_it() {
        let mut clipboard = NoClipboard;
        assert!(clipboard.read_text().is_none());
        assert!(matches!(
            clipboard.write_text("x"),
            Err(ClipboardError::Unavailable)
        ));
        assert!(clipboard.read_file_paths().is_none());
        assert!(matches!(
            clipboard.write_file_paths(&[std::path::PathBuf::from("x")]),
            Err(ClipboardError::Unavailable)
        ));
    }

    /// docs/bugs/14-clipboard-files.md #1: a clipboard holding files reports
    /// them, distinctly from a clipboard holding text or nothing.
    #[test]
    fn a_file_list_is_read_through_the_same_seam_as_text() {
        let (factory, state) = fake();
        let paths = vec![
            std::path::PathBuf::from("/tmp/report.pdf"),
            std::path::PathBuf::from("/tmp/photo.png"),
        ];
        state.lock().unwrap().files = Some(paths.clone());
        let mut clipboard = factory();
        assert_eq!(clipboard.read_file_paths(), Some(paths));
    }

    /// docs/bugs/14-clipboard-files.md #3: a completed receive's paths are
    /// put on this machine's own clipboard so pasting them actually works.
    #[test]
    fn received_file_paths_are_written_back_to_the_clipboard() {
        let (factory, state) = fake();
        let mut clipboard = factory();
        let paths = vec![std::path::PathBuf::from("/tmp/inbox/report.pdf")];
        assert!(clipboard.write_file_paths(&paths).is_ok());
        assert_eq!(state.lock().unwrap().file_writes, vec![paths]);
    }
}
