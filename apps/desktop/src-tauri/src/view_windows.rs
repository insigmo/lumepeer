//! The Tauri half of the view surface (ADR 0085 §1).
//!
//! `lumepeer_runtime::view` says *when* a window should exist and what is in
//! it; this says *how* one is made, because a window is the one thing the
//! runtime crate deliberately does not know about. Everything here is a call
//! into Tauri and nothing here decides anything: the actor asked, and this
//! carries it out on the platform's main thread.

use lumepeer_core::consent::HostAttendance;
use lumepeer_runtime::view::ViewWindows;

/// Default width of a freshly opened view window.
const VIEW_WINDOW_WIDTH: f64 = 1280.0;
/// Default height of a freshly opened view window.
const VIEW_WINDOW_HEIGHT: f64 = 720.0;

/// Label of the host's always-on-top session bar.
pub const HOST_BAR_LABEL: &str = "hostbar";

/// Logical width of the session bar while it is open.
pub const HOST_BAR_WIDTH: f64 = 262.0;
/// Logical height of the session bar while it is open.
pub const HOST_BAR_HEIGHT: f64 = 188.0;
/// Logical width of the collapsed edge tab — just the chevron that brings the
/// bar back.
pub const HOST_BAR_TAB_WIDTH: f64 = 20.0;
/// Logical height of the collapsed edge tab.
pub const HOST_BAR_TAB_HEIGHT: f64 = 58.0;

/// [`ViewWindows`] backed by the real Tauri application.
#[derive(Debug)]
pub struct TauriViewWindows {
    app: tauri::AppHandle,
}

impl TauriViewWindows {
    /// Wraps the handle `spawn_actor` was given.
    #[must_use]
    pub const fn new(app: tauri::AppHandle) -> Self {
        Self { app }
    }
}

impl ViewWindows for TauriViewWindows {
    fn open(&self, label: &str, peer_label: &str, input: bool) {
        // Only the pseudonymized label ever reaches a URL (§15), and it is hex
        // from `peer_tag`, so there is nothing to escape.
        let url = format!("view.html?peer={peer_label}&input={}", u8::from(input));
        let label = label.to_owned();
        let peer = peer_label.to_owned();
        let app = self.app.clone();
        // Window creation must happen on the platform's main thread; the actor
        // runs on a tokio worker.
        let queued = self.app.run_on_main_thread(move || {
            let built = tauri::WebviewWindowBuilder::new(
                &app,
                label.clone(),
                tauri::WebviewUrl::App(url.into()),
            )
            .title("Lumepeer — remote screen")
            .inner_size(VIEW_WINDOW_WIDTH, VIEW_WINDOW_HEIGHT)
            .resizable(true)
            .build();
            match built {
                Ok(window) => {
                    // A window Tauri built while the user was working in
                    // another application does not reliably come to the front
                    // on Windows: the session is live and the remote screen is
                    // drawing behind whatever they were looking at. Raise it
                    // the same way a consent request raises the main window.
                    crate::raise_window(&window);
                    // Before anything is typed into it: a webview that still
                    // holds its own accelerator keys answers Ctrl+R by
                    // reloading the page the remote picture is drawn on, and
                    // the host never sees the chord at all (ADR 0090).
                    give_the_chords_to_the_remote_machine(&window);
                    watch_focus_for_the_keyboard_grab(&window, &peer, input);
                    tracing::info!(window = %label, input, "view window opened");
                }
                Err(error) => {
                    tracing::warn!(window = %label, %error, "cannot open the view window");
                }
            }
        });
        if let Err(error) = queued {
            tracing::warn!(%error, "cannot reach the main thread to open a view window");
        }
    }

    fn close(&self, label: &str) {
        let label = label.to_owned();
        let app = self.app.clone();
        let queued = self.app.run_on_main_thread(move || {
            use tauri::Manager as _;
            if let Some(window) = app.get_webview_window(&label)
                && let Err(error) = window.close()
            {
                tracing::warn!(window = %label, %error, "cannot close the view window");
            }
        });
        if let Err(error) = queued {
            tracing::warn!(%error, "cannot reach the main thread to close a view window");
        }
    }

    fn set_host_bar(&self, visible: bool) {
        let app = self.app.clone();
        // Same reason the view windows queue: a window is created and
        // destroyed on the platform's main thread, and the actor is on a
        // tokio worker.
        let queued = self.app.run_on_main_thread(move || {
            use tauri::Manager as _;

            let existing = app.get_webview_window(HOST_BAR_LABEL);
            match (visible, existing) {
                (false, None) => {}
                // A bar that is already up stays as it is, except for one
                // case: anything that hid it rather than closing it leaves a
                // live window nobody can see, and the next session would find
                // it here and do nothing. `show` on a visible window is a
                // no-op, so this costs nothing in the ordinary case.
                (true, Some(bar)) => {
                    let _ = bar.show();
                }
                // `destroy`, not `close`: the application-wide close handler
                // in `main.rs` turns a close request into a hide, which is
                // right for the main window and would leave this one alive
                // and invisible.
                (false, Some(bar)) => {
                    if let Err(error) = bar.destroy() {
                        tracing::warn!(%error, "cannot take the host bar down");
                    }
                }
                (true, None) => open_host_bar(&app),
            }
        });
        if let Err(error) = queued {
            tracing::warn!(%error, "cannot reach the main thread to move the host bar");
        }
    }

    /// Always attended: this implementation exists only inside somebody's own
    /// desktop session, which is what an `AppHandle` is. Whether they are
    /// looking at the screen is not something any process can know, and
    /// [`HostAttendance`] does not claim to — it answers whether a dialog
    /// this host renders could be seen and answered at all (ADR 0085 §2).
    fn attendance(&self) -> HostAttendance {
        HostAttendance::Attended
    }

    /// Never: a desktop client notices the secure desktop per media session,
    /// from its own capture, and routes input by that (ADR 0057). This seam is
    /// for a host that has no capture of its own (ADR 0088 §1).
    fn on_secure_desktop(&self) -> bool {
        false
    }

    /// Never: this process runs inside somebody's signed-in session, so there
    /// is always somebody signed in (ADR 0088 §3).
    fn nobody_signed_in(&self) -> bool {
        false
    }
}

/// Arms the keyboard grab while this window is focused, and hands the chords
/// back when it is not (ADR 0090).
///
/// The grab is global — `WH_KEYBOARD_LL` sees every keystroke on the desktop —
/// so focus is what bounds it to the window that is actually showing a remote
/// machine. Tracked here, from Tauri's own window events, rather than from
/// `blur`/`focus` inside the webview: a webview that has not finished loading
/// reports neither, and the grab must never be left armed over a window the
/// operator has walked away from.
fn watch_focus_for_the_keyboard_grab(window: &tauri::WebviewWindow, peer: &str, input: bool) {
    use tauri::Manager as _;

    let app = window.app_handle().clone();
    let peer = peer.to_owned();
    window.on_window_event(move |event| {
        let grab = app.state::<crate::keyboard_grab::KeyboardGrab>();
        match event {
            tauri::WindowEvent::Focused(focused) => grab.focus_changed(&peer, input, *focused),
            // A window that is going away takes its grab with it, whether or
            // not a blur arrived first — a revoked session closes the window
            // without one.
            tauri::WindowEvent::Destroyed | tauri::WindowEvent::CloseRequested { .. } => {
                grab.window_closed(&peer);
            }
            _ => {}
        }
    });
}

/// Stops one view window's webview acting on the chords that belong to the
/// machine it is showing (ADR 0090).
///
/// `WebView2` ships with its browser accelerator keys on and Tauri exposes no
/// way to turn them off, so `Ctrl+R`, `F5`, `Ctrl+P`, `Ctrl+F`, `Ctrl+U`,
/// `Ctrl+S`, `Ctrl+0`, `Ctrl+±`, `Alt+Left` and `F12` were all answered
/// locally — by the window showing the remote screen — and never reached the
/// host. The COM call that fixes it lives in `lumepeer-guestkeys` because
/// this crate forbids `unsafe`.
///
/// Every failure is a warning inside that crate and nothing here: a window
/// that keeps a handful of chords still shows the remote screen.
fn give_the_chords_to_the_remote_machine(window: &tauri::WebviewWindow) {
    #[cfg(target_os = "windows")]
    {
        let queued = window.with_webview(|webview| {
            lumepeer_guestkeys::keep_accelerators_for_the_remote_machine(&webview.controller());
        });
        if let Err(error) = queued {
            tracing::warn!(%error, "cannot reach this view window's webview");
        }
    }
    // Neither `WebKitGTK` nor `WKWebView` claims the set `WebView2` does, and
    // what each of them does claim is its own task (ADR 0090).
    #[cfg(not(target_os = "windows"))]
    {
        let _ = window;
    }
}

/// Builds the session bar, docked to the right edge of the primary screen.
///
/// Undecorated, out of the taskbar and above everything else, because the
/// whole point is to survive the main window being minimized. It deliberately
/// does not take focus: it appears while the host is working in another
/// application, and stealing the keyboard at that moment would be worse than
/// the problem it solves.
fn open_host_bar(app: &tauri::AppHandle) {
    // Docked to the right edge of the primary screen, halfway down, which is
    // where its collapsed tab lives. A monitor that cannot be read is not a
    // reason to skip the bar: Tauri centres what it cannot place, and a bar
    // in the middle of the screen is still a bar the host can drag.
    let placement = app.primary_monitor().ok().flatten().map(|monitor| {
        let scale = monitor.scale_factor();
        let size = monitor.size().to_logical::<f64>(scale);
        let origin = monitor.position().to_logical::<f64>(scale);
        (
            origin.x + size.width - HOST_BAR_WIDTH,
            origin.y + (size.height - HOST_BAR_HEIGHT) / 2.0,
        )
    });

    let mut builder = tauri::WebviewWindowBuilder::new(
        app,
        HOST_BAR_LABEL,
        tauri::WebviewUrl::App("hostbar.html".into()),
    )
    .title("Lumepeer")
    .inner_size(HOST_BAR_WIDTH, HOST_BAR_HEIGHT)
    .decorations(false)
    .resizable(false)
    .always_on_top(true)
    .skip_taskbar(true)
    .focused(false);
    if let Some((x, y)) = placement {
        builder = builder.position(x, y);
    }
    match builder.build() {
        Ok(_) => tracing::info!("host session bar opened"),
        Err(error) => tracing::warn!(%error, "cannot open the host session bar"),
    }
}
