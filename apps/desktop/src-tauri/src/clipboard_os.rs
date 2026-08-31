//! This machine's own clipboard (design doc §9.2; ADR 0030, ADR 0046).
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
}

/// The actor's handle on the clipboard thread.
///
/// Dropping it ends the thread: the job channel closes and the loop returns.
#[derive(Debug)]
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
    }
}
