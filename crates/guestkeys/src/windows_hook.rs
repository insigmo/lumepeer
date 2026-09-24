//! The low-level keyboard hook behind [`crate::grab`] (ADR 0090).
//!
//! `WH_KEYBOARD_LL` is the only way to see `Win+D` or `Alt+Tab` from a
//! process: the shell claims them before any window is told, so a webview
//! never gets a `keydown` to cancel. A hook sits above the shell, and above
//! every global hotkey another program registered, which is why a chord can
//! be taken at all — and each keystroke goes exactly where [`crate::route`]
//! says.
//!
//! Three facts about `WH_KEYBOARD_LL` shape everything here:
//!
//! 1. **The callback runs on the thread that installed the hook**, which must
//!    therefore be pumping messages. So this owns a thread of its own with a
//!    bare `GetMessageW` loop rather than borrowing the application's: the
//!    Tauri main thread also lays out and paints a webview, and a hook on it
//!    would be called between frames.
//! 2. **The callback has a deadline.** Windows silently removes a
//!    `WH_KEYBOARD_LL` hook whose callback overruns `LowLevelHooksTimeout`
//!    (300 ms by default) — the operator's keyboard would come back on its
//!    own, but so would the bug. Nothing in the callback may block, which is
//!    why the sink is a channel send and not the actor call itself.
//! 3. **The callback takes no context**, so the sink lives in a `static`. Only
//!    one grab exists at a time ([`install`] refuses a second), so this is one
//!    slot rather than a registry.

#![allow(
    unsafe_code,
    reason = "SetWindowsHookExW and the hook callback are raw FFI with no safe \
              binding; same justification standard as SendInput (ADR 0012) and \
              the rest of this workspace's Win32 surface"
)]

use std::collections::BTreeSet;
use std::sync::Mutex;
use std::sync::mpsc;

use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, HC_ACTION, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_INJECTED,
    LLKHF_UP, MSG, PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL,
    WM_QUIT,
};

use crate::{GrabbedKey, Held, Route, Sink, evdev_of_virtual_key, route};

/// What the hook callback needs, and the only mutable state this module has.
struct Grabbing {
    /// Where a claimed keystroke goes. Must not block; see the module header.
    sink: Sink,
    /// Positions this grab has sent a press for and not yet a release.
    ///
    /// A chord interrupted by the operator clicking away — or by the grab
    /// being released mid-`Win+D` — would otherwise leave `Win` held down on
    /// the host, and every later keystroke would silently be a chord (§11,
    /// the same failure `ViewInput::releaseHeld` exists for on the webview
    /// side). It is also where the grab reads which modifiers are held
    /// ([`Held::of_positions`]): every modifier pressed while it is live is
    /// sent, so this is exactly what the host has down.
    held: BTreeSet<u32>,
}

/// The one live grab, or `None`.
static GRABBING: Mutex<Option<Grabbing>> = Mutex::new(None);

/// A hook that is installed, with the thread that owns it.
#[derive(Debug)]
pub(crate) struct Installed {
    /// Thread running the hook's message loop, joined on drop.
    thread: Option<std::thread::JoinHandle<()>>,
    /// Its id, which is how `WM_QUIT` reaches that loop.
    thread_id: u32,
}

impl Drop for Installed {
    fn drop(&mut self) {
        // SAFETY: a thread id is a plain integer and the call posts a message
        // to whatever thread holds it. A thread that has already exited makes
        // this fail, which is the `let _` — there is then nothing to stop.
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(thread) = self.thread.take() {
            // The loop exits on `WM_QUIT`, unhooks, and lets go of whatever
            // it was holding down. Joining rather than detaching is what
            // makes "the grab is released" true by the time this returns,
            // which the caller relies on when it arms a grab for another
            // window.
            let _ = thread.join();
        }
        tracing::info!("the keyboard grab is released");
    }
}

/// Installs the hook on a thread of its own, or `None` if it cannot be.
///
/// Refuses a second grab while one is live: two hooks would both claim the
/// same chord and the host would receive it twice.
pub(crate) fn install(sink: Sink) -> Option<Installed> {
    {
        let mut grabbing = GRABBING.lock().ok()?;
        if grabbing.is_some() {
            tracing::warn!("a keyboard grab is already installed; not installing a second");
            return None;
        }
        *grabbing = Some(Grabbing {
            sink,
            held: BTreeSet::new(),
        });
    }

    let (ready_tx, ready_rx) = mpsc::channel::<Option<u32>>();
    let thread = std::thread::Builder::new()
        .name("lumepeer-keyboard-grab".to_owned())
        .spawn(move || run(&ready_tx))
        .inspect_err(|error| tracing::warn!(%error, "cannot start the keyboard-grab thread"))
        .ok()?;

    if let Ok(Some(thread_id)) = ready_rx.recv() {
        tracing::info!("the system chords now reach the remote machine");
        return Some(Installed {
            thread: Some(thread),
            thread_id,
        });
    }
    // The hook did not install, or the thread died before saying so. Either
    // way there is no grab, and the slot has to be given back or no later
    // attempt could ever take it.
    let _ = thread.join();
    release_slot();
    None
}

/// Empties the one-grab slot, letting go over there of anything still held.
fn release_slot() {
    let Ok(mut slot) = GRABBING.lock() else {
        // A poisoned lock means a panic inside the callback. The keys it was
        // holding cannot be released through it, and taking the slot back
        // would only invite a second grab into the same broken state.
        tracing::error!("the keyboard grab's state is poisoned; not reusing it");
        return;
    };
    if let Some(grabbing) = slot.take() {
        for scancode in grabbing.held {
            (grabbing.sink)(GrabbedKey {
                scancode,
                modifiers: 0,
                pressed: false,
            });
        }
    }
}

/// The grab thread: install, pump, unhook.
fn run(ready: &mpsc::Sender<Option<u32>>) {
    // SAFETY: `hook_proc` is a `'static` function pointer with the exact
    // signature `HOOKPROC` names. `WH_KEYBOARD_LL` ignores the module handle
    // and requires thread id 0 (a global hook), which is what is passed.
    let hook = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), None, 0) };
    let hook = match hook {
        Ok(hook) => hook,
        Err(error) => {
            tracing::warn!(%error, "cannot install the keyboard hook");
            let _ = ready.send(None);
            return;
        }
    };

    // SAFETY: reads the calling thread's own id and returns an integer.
    let thread_id = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
    if ready.send(Some(thread_id)).is_err() {
        // Nobody is waiting for this hook any more.
        // SAFETY: `hook` is live and owned by this thread.
        unsafe {
            let _ = UnhookWindowsHookEx(hook);
        }
        return;
    }

    pump();

    // SAFETY: `hook` is live and owned by this thread, and nothing below uses
    // it again. The callback cannot be running concurrently: it runs on this
    // very thread, and this thread is here.
    unsafe {
        let _ = UnhookWindowsHookEx(hook);
    }
    release_slot();
}

/// Waits for `WM_QUIT`, dispatching nothing: this thread owns no window, and
/// the only message it ever expects is the one that ends it. The loop exists
/// because a `WH_KEYBOARD_LL` callback is delivered *by* the message pump of
/// the installing thread, so a thread that does not pump gets no callbacks.
fn pump() {
    let mut message = MSG::default();
    loop {
        // SAFETY: `message` is a live, fully initialized `MSG` for the
        // duration of the call. `GetMessageW` returns 0 for `WM_QUIT` and -1
        // for an error, both of which end the loop.
        let result = unsafe { GetMessageW(&raw mut message, None, 0, 0) };
        if result.0 <= 0 {
            return;
        }
    }
}

/// Whether this keystroke is hidden from this machine, having already been
/// handed to the sink if it goes to the remote one.
///
/// Split out of [`hook_proc`] so the FFI boundary holds nothing but the
/// pointer read and the return value: everything that could fail — a poisoned
/// lock, a key with no known position, a sink that has gone away — is decided
/// here, in safe code, and answers "not hidden" so the keystroke keeps
/// travelling the ordinary way.
fn claim(event: &KBDLLHOOKSTRUCT) -> bool {
    if event.flags.contains(LLKHF_INJECTED) && !pressed_by_the_e2e_matrix(event) {
        // Something synthesized this — quite possibly this very application,
        // on a host that is also a guest. Claiming it would be a loop.
        return false;
    }
    let Ok(virtual_key) = u16::try_from(event.vkCode) else {
        return false;
    };
    // A key whose position this build cannot name is better left to the
    // local machine than forwarded as a key nobody pressed.
    let Some(scancode) = evdev_of_virtual_key(virtual_key, event.flags.contains(LLKHF_EXTENDED))
    else {
        return false;
    };
    let pressed = !event.flags.contains(LLKHF_UP);
    let Ok(mut slot) = GRABBING.lock() else {
        return false;
    };
    let Some(grabbing) = slot.as_mut() else {
        return false;
    };
    let sent = grabbing.held.contains(&scancode);
    let route = route(
        virtual_key,
        pressed,
        sent,
        Held::of_positions(&grabbing.held),
    );
    if route == Route::Here {
        return false;
    }
    // A key held down repeats, and every repeat is a press the host wants:
    // holding an arrow under `Win` has to keep moving the window over there.
    // The set is what has to be released later, so a repeat only ever adds.
    if pressed {
        grabbing.held.insert(scancode);
    } else {
        grabbing.held.remove(&scancode);
    }
    (grabbing.sink)(GrabbedKey {
        scancode,
        // After the update, so a modifier's own press carries its bit and its
        // release does not — what a browser reports, and what the host has
        // always been sent.
        modifiers: Held::of_positions(&grabbing.held).bits(),
        pressed,
    });
    route == Route::There
}

/// `dwExtraInfo` of the keystrokes the e2e matrix injects on a guest
/// (`E2E_MARK` in e2e/matrix/agent.py): "LUME".
const E2E_MARK: usize = 0x4C55_4D45;

/// Whether an injected keystroke is the e2e matrix pressing a key, which a
/// pilot debug build treats as a person's.
///
/// Every injected keystroke is otherwise left alone (see [`claim`]), so no
/// test could ever press a chord this hook sees, and the chords it sends went
/// untested while the ones a webview sends passed. The mark keeps this to the
/// harness: this application's own injection, as a host, carries none.
fn pressed_by_the_e2e_matrix(event: &KBDLLHOOKSTRUCT) -> bool {
    cfg!(all(feature = "e2e", debug_assertions)) && event.dwExtraInfo == E2E_MARK
}

/// The hook callback. Returns 1 for a keystroke this machine must not act on,
/// and defers to the rest of the hook chain for everything else.
unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION.cast_signed() {
        // For `HC_ACTION` on a `WH_KEYBOARD_LL` hook, Windows documents
        // `lparam` as a pointer to a `KBDLLHOOKSTRUCT` that is valid for the
        // duration of this call. Nothing is kept past it.
        let event = lparam.0 as *const KBDLLHOOKSTRUCT;
        if !event.is_null() {
            // SAFETY: the pointer is non-null, points at a `KBDLLHOOKSTRUCT`
            // Windows owns for the length of this callback, and nothing
            // derived from it outlives the borrow.
            let event = unsafe { &*event };
            if claim(event) {
                return LRESULT(1);
            }
        }
    }
    // SAFETY: passing the call on with the arguments as received is what the
    // documentation requires of every hook that does not consume an event.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}
