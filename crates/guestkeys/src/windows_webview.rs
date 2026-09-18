//! Stopping `WebView2` acting on chords that belong to the remote machine
//! (ADR 0090).
//!
//! `WebView2` ships with `AreBrowserAcceleratorKeysEnabled` on, and Tauri
//! does not expose it, so every view window inherited a browser's idea of
//! what a keystroke means: `Ctrl+R` and `F5` reloaded the page the remote
//! picture is drawn on, `Ctrl+P` opened a print dialog for it, `Ctrl+F` a
//! find bar over it, `Ctrl+0`/`Ctrl+-`/`Ctrl++` zoomed it, `Ctrl+U` offered
//! its source, `Ctrl+S` offered to save it, `Alt+Left` navigated it, and
//! `F12`/`Ctrl+Shift+I` opened the developer tools. None of them reached the
//! host, which is exactly the "some combinations just do not work" report.
//!
//! Turning the flag off removes only the *browser-specific* accelerators. The
//! ones that are not a browser's — `Ctrl+C`, `Ctrl+V`, `Ctrl+X`, `Ctrl+A`,
//! `Ctrl+Z`, the arrows — keep working as keystrokes the page receives and
//! `ViewInput` forwards, which is what they were always meant to be.
//!
//! This is the *view* window only. The main window is an ordinary application
//! window whose keystrokes belong to it, and taking a browser's shortcuts away
//! from it would buy nothing.

#![allow(
    unsafe_code,
    reason = "every ICoreWebView2 method is an `unsafe fn` COM call; same \
              justification standard as the rest of this workspace's Win32 surface"
)]

use webview2_com::Microsoft::Web::WebView2::Win32::{
    ICoreWebView2Controller, ICoreWebView2Settings3,
};
use windows_core::Interface as _;

/// Takes `WebView2`'s own accelerator keys away from one webview, so they
/// reach the machine the window is showing instead.
///
/// Every failure is a warning and nothing more. A webview whose settings
/// cannot be reached — an older `WebView2` runtime with no
/// `ICoreWebView2Settings3`, a controller that has already gone — still shows
/// the remote screen and still forwards everything else; losing a handful of
/// chords is not worth losing the session over (§18).
pub fn keep_accelerators_for_the_remote_machine(controller: &ICoreWebView2Controller) {
    // SAFETY: `controller` is a live COM interface owned by the caller, and
    // each call below only reads a child interface off it or sets one
    // property. Nothing is retained past this function.
    let settings = unsafe { controller.CoreWebView2().and_then(|core| core.Settings()) };
    let settings = match settings {
        Ok(settings) => settings,
        Err(error) => {
            tracing::warn!(%error, "cannot reach this webview's settings: its browser chords stay local");
            return;
        }
    };
    let Ok(settings) = settings.cast::<ICoreWebView2Settings3>() else {
        tracing::warn!(
            "this WebView2 runtime has no ICoreWebView2Settings3: its browser chords stay local"
        );
        return;
    };
    // SAFETY: as above.
    match unsafe { settings.SetAreBrowserAcceleratorKeysEnabled(false) } {
        Ok(()) => tracing::info!("this view window's browser chords now reach the remote machine"),
        Err(error) => tracing::warn!(%error, "cannot turn off this webview's browser chords"),
    }
}
