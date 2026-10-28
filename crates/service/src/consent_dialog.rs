//! The session host's consent dialog: one `TaskDialogIndirect`, drawn by the
//! `LocalSystem` process itself (ADR 0169 §5).
//!
//! This is the whole native surface, and nothing about what is asked: the
//! desktop crate (`session_host_consent.rs`) writes the words, chooses the
//! buttons and decides what an answer means. Here a question is put up, the
//! person's answer — or its absence — comes back, and that is all.
//!
//! Why a dialog drawn here can be trusted with a yes: the session host is a
//! System-integrity process, so its window is one UIPI stops every program the
//! signed-in person runs from clicking, typing into or reading — measured in
//! pack 01 (`docs/research/session-host-probe.md`, 2.5). That holds only while
//! nothing in it renders a peer's string as anything but text, so:
//!
//! - **No hyperlinks.** `TDF_ENABLE_HYPERLINKS` is never set; a peer's
//!   `<a href=…>` is shown as the characters it is.
//! - **Every string is plain**, and an interior NUL cannot cut one short: it
//!   is dropped on the way to UTF-16 ([`wide`]). Bounding and cleaning a
//!   peer's strings is the caller's, which knows which strings are a peer's.
//! - **No answer is a no.** The question ends by itself after
//!   [`Question::timeout`], and when the caller withdraws it (the guest left,
//!   or was let in by its device password); both come back as their own
//!   [`Outcome`], never as a button.
//!
//! `TaskDialogIndirect` lives only in Common-Controls v6, which a process gets
//! from its manifest. The desktop binary's carries it (`apps/desktop/src-tauri/
//! build.rs`); this crate's own binary and test binaries do not, and the import
//! would keep them from loading at all — so nothing here calls [`ask`] in a
//! test, and the service never calls it.

#![allow(
    unsafe_code,
    reason = "a TaskDialog, its callback and the machine's UI language have no \
              safe bindings; same justification standard as the rest of this \
              crate's Win32 surface (ADR 0043, ADR 0085)"
)]

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, S_OK, WPARAM};
use windows::Win32::Globalization::GetSystemDefaultUILanguage;
use windows::Win32::UI::Controls::{
    TASKDIALOG_BUTTON, TASKDIALOG_NOTIFICATIONS, TASKDIALOGCONFIG, TDF_ALLOW_DIALOG_CANCELLATION,
    TDF_CALLBACK_TIMER, TDM_CLICK_BUTTON, TDN_CREATED, TDN_TIMER, TaskDialogIndirect,
};
use windows::Win32::UI::WindowsAndMessaging::{
    HWND_TOPMOST, IDCANCEL, SWP_NOMOVE, SWP_NOSIZE, SendMessageW, SetForegroundWindow, SetWindowPos,
};
use windows::core::{BOOL, HRESULT, PCWSTR};

/// `LANG_RUSSIAN`, the primary language of a `LANGID`.
const LANG_RUSSIAN: u16 = 0x19;

/// The bits of a `LANGID` that name its primary language.
const PRIMARY_LANGUAGE_MASK: u16 = 0x3ff;

/// One question for the person at this machine.
#[derive(Debug, Clone, Copy)]
pub struct Question<'a> {
    /// The window's title.
    pub title: &'a str,
    /// The line in large type: what is being asked.
    pub instruction: &'a str,
    /// The text under it.
    pub content: &'a str,
    /// The buttons, as `(id, text)`, left to right. An id is the caller's to
    /// choose; [`IDCANCEL`]'s value is not one, because closing the window
    /// answers with it.
    pub buttons: &'a [(i32, &'a str)],
    /// The button Enter presses. The caller's "no".
    pub default_button: i32,
    /// The check box under the buttons, when there is one.
    pub check_box: Option<&'a str>,
    /// How long the question stays up unanswered.
    pub timeout: Duration,
}

/// How a question ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The person pressed `button`, with the check box ticked or not.
    Pressed {
        /// The id of the button pressed.
        button: i32,
        /// Whether the check box was ticked when it was.
        checked: bool,
    },
    /// The person closed the window, or pressed Esc.
    Dismissed,
    /// Nobody answered within [`Question::timeout`].
    Expired,
    /// The caller withdrew the question before anybody answered.
    Withdrawn,
    /// The dialog could not be shown at all.
    Failed,
}

/// Why the dialog closed itself, kept by its callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ending {
    Expired,
    Withdrawn,
}

/// What the callback reads and writes, on the dialog's own thread.
struct Watch<'a> {
    started: Instant,
    timeout: Duration,
    withdrawn: &'a AtomicBool,
    ending: Cell<Option<Ending>>,
}

/// Puts `question` up and blocks until it is answered, expires, or
/// `withdrawn` is set; how it ended.
///
/// Run it on a thread of its own: a task dialog runs its own message loop on
/// the calling thread for as long as it is up.
#[must_use]
pub fn ask(question: &Question<'_>, withdrawn: &AtomicBool) -> Outcome {
    let title = wide(question.title);
    let instruction = wide(question.instruction);
    let content = wide(question.content);
    let check_box = question.check_box.map(wide);
    let texts: Vec<Vec<u16>> = question
        .buttons
        .iter()
        .map(|(_, text)| wide(text))
        .collect();
    let buttons: Vec<TASKDIALOG_BUTTON> = question
        .buttons
        .iter()
        .zip(&texts)
        .map(|((id, _), text)| TASKDIALOG_BUTTON {
            nButtonID: *id,
            pszButtonText: PCWSTR(text.as_ptr()),
        })
        .collect();
    let watch = Watch {
        started: Instant::now(),
        timeout: question.timeout,
        withdrawn,
        ending: Cell::new(None),
    };
    let (Ok(size), Ok(count)) = (
        u32::try_from(size_of::<TASKDIALOGCONFIG>()),
        u32::try_from(buttons.len()),
    ) else {
        return Outcome::Failed;
    };
    let config = TASKDIALOGCONFIG {
        cbSize: size,
        // Esc and the close box answer, as a no; the timer is how the dialog
        // notices it expired or was withdrawn. `TDF_ENABLE_HYPERLINKS` is
        // deliberately absent (module doc).
        dwFlags: TDF_ALLOW_DIALOG_CANCELLATION | TDF_CALLBACK_TIMER,
        pszWindowTitle: PCWSTR(title.as_ptr()),
        pszMainInstruction: PCWSTR(instruction.as_ptr()),
        pszContent: PCWSTR(content.as_ptr()),
        cButtons: count,
        pButtons: buttons.as_ptr(),
        nDefaultButton: question.default_button,
        pszVerificationText: check_box
            .as_ref()
            .map_or(PCWSTR::null(), |text| PCWSTR(text.as_ptr())),
        pfCallback: Some(callback),
        lpCallbackData: (&raw const watch) as isize,
        ..Default::default()
    };
    let mut pressed = 0i32;
    let mut checked = BOOL(0);
    // SAFETY: `config`, every wide string and button it points at, and
    // `watch` (read by `callback` through `lpCallbackData`) live on this
    // stack frame until the call returns; the call blocks until the dialog is
    // gone, and the callback only runs within it, on this thread.
    let shown = unsafe {
        TaskDialogIndirect(
            &raw const config,
            Some(&raw mut pressed),
            None,
            Some(&raw mut checked),
        )
    };
    if shown.is_err() {
        return Outcome::Failed;
    }
    match watch.ending.get() {
        Some(Ending::Expired) => Outcome::Expired,
        Some(Ending::Withdrawn) => Outcome::Withdrawn,
        None if pressed == IDCANCEL.0 => Outcome::Dismissed,
        None => Outcome::Pressed {
            button: pressed,
            checked: checked.as_bool(),
        },
    }
}

/// The dialog's notifications: raise it when it appears, close it when it has
/// expired or been withdrawn.
unsafe extern "system" fn callback(
    hwnd: HWND,
    msg: TASKDIALOG_NOTIFICATIONS,
    _wparam: WPARAM,
    _lparam: LPARAM,
    data: isize,
) -> HRESULT {
    // SAFETY: `data` is the `Watch` `ask` put in `lpCallbackData`, alive on
    // `ask`'s frame for as long as the dialog is up, and this callback runs on
    // that same thread.
    let watch = unsafe { &*(data as *const Watch<'_>) };
    if msg == TDN_CREATED {
        // On top of everything: a question nobody sees is answered by its
        // timeout, which is a no the guest did not earn. Foreground is a
        // request the shell may refuse; topmost is not.
        // SAFETY: `hwnd` is the dialog, alive during its own notification.
        unsafe {
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE,
            );
            let _ = SetForegroundWindow(hwnd);
        }
    } else if msg == TDN_TIMER && watch.ending.get().is_none() {
        let ending = if watch.withdrawn.load(Ordering::SeqCst) {
            Some(Ending::Withdrawn)
        } else if watch.started.elapsed() >= watch.timeout {
            Some(Ending::Expired)
        } else {
            None
        };
        if let Some(ending) = ending {
            watch.ending.set(Some(ending));
            // Closes it as Esc would; `ask` reads why from `ending`.
            // SAFETY: `hwnd` is the dialog, alive during its own
            // notification; the message carries two integers.
            unsafe {
                let _ = SendMessageW(
                    hwnd,
                    u32::try_from(TDM_CLICK_BUTTON.0).unwrap_or_default(),
                    Some(WPARAM(usize::try_from(IDCANCEL.0).unwrap_or_default())),
                    Some(LPARAM(0)),
                );
            }
        }
    }
    S_OK
}

/// Whether this machine's own UI language is Russian (ADR 0169 §5: the
/// dialog speaks the system's language, Russian or English).
///
/// The system's, not the signed-in person's: this process is `LocalSystem`,
/// whose user language is the machine default anyway.
#[must_use]
pub fn system_ui_is_russian() -> bool {
    // SAFETY: no arguments, no pointers; returns a `LANGID`.
    let language = unsafe { GetSystemDefaultUILanguage() };
    is_russian(language)
}

/// Whether `language`, a `LANGID`, is Russian in any of its regions.
fn is_russian(language: u16) -> bool {
    language & PRIMARY_LANGUAGE_MASK == LANG_RUSSIAN
}

/// `text` as a NUL-terminated UTF-16 string with no NUL inside it: an interior
/// NUL would end the string the dialog shows wherever a peer put one.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16()
        .filter(|&unit| unit != 0)
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A peer's NUL does not cut the string short; the terminator is the only
    /// NUL left.
    #[test]
    fn a_wide_string_has_exactly_one_nul_at_its_end() {
        let text = wide("guest\0 — admin");
        assert_eq!(text.iter().filter(|&&unit| unit == 0).count(), 1);
        assert_eq!(text.last(), Some(&0));
        assert_eq!(
            String::from_utf16_lossy(&text[..text.len() - 1]),
            "guest — admin"
        );
    }

    /// Russian in every region reads as Russian; everything else as not.
    #[test]
    fn the_ui_language_is_read_from_the_primary_language() {
        assert!(is_russian(0x0419), "ru-RU");
        assert!(is_russian(0x0819), "ru-MD");
        assert!(!is_russian(0x0409), "en-US");
        assert!(!is_russian(0x0422), "uk-UA");
    }
}
