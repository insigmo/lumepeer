//! Arming and releasing the guest's keyboard grab (ADR 0090).
//!
//! `lumepeer-guestkeys` knows *how* to take `Win+D` and `Alt+Tab` away from
//! this machine; this decides *when*, which is a question about windows and
//! therefore belongs here rather than in that crate (the same split
//! `view_windows.rs` makes for `ViewWindows`).
//!
//! The rule is one line long, and every part of it matters:
//!
//! > A grab is live while a view window that holds `input` is focused, and
//! > the operator has not released it.
//!
//! - **Focused**, because a grab is global: `WH_KEYBOARD_LL` sees every
//!   keystroke on the desktop, so a grab that outlived the window's focus
//!   would send the operator's own `Win+E` to someone else's machine.
//! - **Holds `input`**, because a view-only session has nothing to do with a
//!   keystroke. The host re-checks this per event regardless (§2.3); this
//!   only keeps the local keyboard from being taken for no reason.
//! - **Has not released it**, because claiming `Alt+Tab` means the operator
//!   cannot `Alt+Tab` away from the window. `Ctrl+Alt+Shift+K` gives the
//!   chords back, and the toolbar's hotkey list says so — a grab nobody can
//!   see or reverse would be worse than the bug it fixes.
//!
//! The grab is on by default, because the reason it exists is that a remote
//! machine could not be given its own hotkeys.

use std::sync::Mutex;

use lumepeer_core::protocol::{InputDetail, InputEventPayload};
use lumepeer_guestkeys::{Grab, GrabbedKey};
use tauri::Manager as _;

/// Whether a freshly opened view window takes the system chords.
///
/// On: a remote machine that cannot be sent `Win+R` is the report this whole
/// path exists to answer. The operator releases it per session with
/// `Ctrl+Alt+Shift+K`, and that choice lasts as long as the process — it is a
/// preference about this operator's keyboard, not about any one host.
const GRABBED_BY_DEFAULT: bool = true;

/// Who wants the keyboard, and who currently has it.
#[derive(Debug, Default)]
struct State {
    /// Whether the operator has left the grab on.
    wanted: bool,
    /// Pseudonymized label of the focused view window that holds `input`, if
    /// one is focused at all.
    focused: Option<String>,
    /// The live grab, and the host it is sending to.
    live: Option<Live>,
}

/// A grab that is installed right now.
struct Live {
    /// Host the grabbed keystrokes are going to.
    peer: String,
    /// Dropping this unhooks and releases everything still held down.
    #[allow(
        dead_code,
        reason = "held for its Drop, which is the whole of releasing a grab"
    )]
    grab: Grab,
}

/// Written by hand rather than derived because a [`Grab`] is a live OS hook
/// with nothing worth printing: which host the keystrokes go to is the whole
/// of what this state is.
impl std::fmt::Debug for Live {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Live")
            .field("peer", &self.peer)
            .finish_non_exhaustive()
    }
}

/// Owner of the process's one keyboard grab.
#[derive(Debug)]
pub struct KeyboardGrab {
    /// Handle the drain task reaches the actor through.
    app: tauri::AppHandle,
    state: Mutex<State>,
}

impl KeyboardGrab {
    /// A grab nobody holds yet, wanted or not as [`GRABBED_BY_DEFAULT`] says.
    #[must_use]
    pub fn new(app: tauri::AppHandle) -> Self {
        Self {
            app,
            state: Mutex::new(State {
                wanted: GRABBED_BY_DEFAULT,
                focused: None,
                live: None,
            }),
        }
    }

    /// Whether the operator is letting the system chords through to the host.
    #[must_use]
    pub fn wanted(&self) -> bool {
        self.state
            .lock()
            .map_or(GRABBED_BY_DEFAULT, |state| state.wanted)
    }

    /// Turns the grab on or off, and answers with what it now is.
    ///
    /// Takes effect immediately in both directions: turning it off releases a
    /// live grab on the spot, which is what makes `Ctrl+Alt+Shift+K` a way out
    /// rather than a preference for next time.
    pub fn set_wanted(&self, wanted: bool) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        state.wanted = wanted;
        self.reconcile(&mut state);
        state.wanted
    }

    /// A view window gained or lost focus. `input` is whether that window's
    /// session may inject at all.
    pub fn focus_changed(&self, peer: &str, input: bool, focused: bool) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if focused && input {
            state.focused = Some(peer.to_owned());
        } else if state.focused.as_deref() == Some(peer) {
            state.focused = None;
        }
        self.reconcile(&mut state);
    }

    /// A view window is gone. Its grab goes with it even if no blur arrived
    /// first, which is the case when the session was revoked rather than the
    /// window closed.
    pub fn window_closed(&self, peer: &str) {
        self.focus_changed(peer, false, false);
    }

    /// Makes the live grab match what the state says it should be.
    ///
    /// Idempotent, and the only place a grab is installed or dropped, so
    /// "focused, permitted and wanted" is checked once rather than at each of
    /// the three callers.
    fn reconcile(&self, state: &mut State) {
        let should_hold = state.wanted.then(|| state.focused.clone()).flatten();
        match (&state.live, should_hold) {
            // Already grabbing for the right host, or already not grabbing.
            (Some(live), Some(peer)) if live.peer == peer => {}
            (None, None) => {}
            // Hand it back. Dropping the grab is what releases the keys it
            // was holding, so this happens before any new one is installed:
            // `guestkeys` refuses a second grab while one is live.
            (Some(_), None) => {
                state.live = None;
            }
            (live, Some(peer)) => {
                if live.is_some() {
                    state.live = None;
                }
                state.live = self.install(peer);
            }
        }
    }

    /// Installs a grab that sends to `peer`, or `None` if it cannot be.
    fn install(&self, peer: String) -> Option<Live> {
        let network = self.app.state::<crate::AppState>().network.clone();
        let label = peer.clone();
        // Spawned on the application's own runtime, by hand. Every caller of
        // this reaches it from a Tauri window event, which runs on the
        // platform's main thread — outside any runtime — so a bare
        // `tokio::spawn` here would panic rather than start a task.
        let runtime = self.app.state::<tokio::runtime::Runtime>().handle().clone();
        // The hook callback has a deadline measured in milliseconds and must
        // not block (see `lumepeer-guestkeys`), while `ActorHandle::input`
        // awaits the actor's reply. So the callback only queues, and a task
        // does the awaiting.
        let (keys, mut queued) = tokio::sync::mpsc::unbounded_channel::<GrabbedKey>();
        runtime.spawn(async move {
            while let Some(key) = queued.recv().await {
                let event = InputEventPayload {
                    // 0 is "this key has no character meaning", which is what
                    // makes the host press it by position — the only correct
                    // answer for a chord (ADR 0065, ADR 0090).
                    logical: 0,
                    scancode: key.scancode,
                    modifiers: key.modifiers,
                    detail: if key.pressed {
                        InputDetail::Press
                    } else {
                        InputDetail::Release
                    },
                };
                if let Err(error) = network.input(label.clone(), event).await {
                    // The ordinary case is a session that ended between the
                    // keystroke and this await; the host refusing is its own
                    // right (§2.3). Either way there is nothing to retry.
                    tracing::debug!(?error, "a grabbed keystroke went nowhere");
                    return;
                }
            }
        });
        let grab = lumepeer_guestkeys::grab(std::sync::Arc::new(move |key| {
            // A closed channel means the drain task is gone, which only
            // happens once the session has. Dropping the keystroke is right;
            // there is nothing left to send it to.
            let _ = keys.send(key);
        }))?;
        Some(Live { peer, grab })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default is stated in one place and read in two, so a change to it
    /// cannot leave the toolbar's label disagreeing with the behaviour.
    #[test]
    fn the_grab_is_on_by_default() {
        const {
            assert!(
                GRABBED_BY_DEFAULT,
                "a remote machine that cannot be sent Win+R is the report ADR 0090 answers"
            );
        }
    }

    /// A grab is only ever wanted for a focused window that may inject: the
    /// hook is global, so one held past a blur would send the operator's own
    /// chords to someone else's machine.
    #[test]
    fn nothing_is_grabbed_without_a_focused_window_that_holds_input() {
        let mut state = State {
            wanted: true,
            focused: None,
            live: None,
        };
        assert!(
            state
                .wanted
                .then(|| state.focused.clone())
                .flatten()
                .is_none()
        );

        state.focused = Some("abc".to_owned());
        assert_eq!(
            state.wanted.then(|| state.focused.clone()).flatten(),
            Some("abc".to_owned())
        );

        state.wanted = false;
        assert!(
            state
                .wanted
                .then(|| state.focused.clone())
                .flatten()
                .is_none()
        );
    }
}
