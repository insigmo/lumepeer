//! Lumepeer desktop application (design doc §4, §13).
//!
//! The Tauri layer owns the window and forwards typed IPC calls into the
//! network actor. It holds no authority of its own: the webview is an
//! untrusted presentation layer (§2.3).

#![forbid(unsafe_code)]
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![allow(
    unreachable_pub,
    reason = "binary crate: `pub` marks the IPC surface of §13, not a library API"
)]

mod address_book_store;
mod audit_store;
mod autostart;
mod clipboard_os;
mod commands;
mod config;
mod connection_history;
mod logging;
mod network;
mod recorder;
mod remembered_password;
mod service_control;
mod unattended_store;
mod view;

/// State shared by every IPC command: a handle into the network actor.
#[derive(Debug)]
pub struct AppState {
    /// Channel handle into the `NetworkActor` (§2.3): the only way commands
    /// reach `SessionManager` or the transport.
    pub network: network::ActorHandle,
    /// Update manifest this client checks, already resolved for the configured
    /// channel (§21; ADR 0042). `None` when updates are not configured.
    pub update_url: Option<String>,
    /// Whether this build starts with the user's session (ADR 0042), as the
    /// settings panel reads and writes it.
    pub autostart: autostart::Autostart,
}

/// Brings the main window back to the user: out of the tray, out of a
/// minimized state and in front of whatever they were doing.
///
/// All three calls are needed and none of them subsumes the others: `show`
/// undoes the hide the close handler in [`main`] does, `unminimize` undoes a
/// minimize, and `set_focus` is what actually raises the window. Every caller
/// wants all three, which is why they live here rather than being copied into
/// the tray handlers, the single-instance callback and the consent listener.
fn focus_main_window(app: &tauri::AppHandle) {
    use tauri::Manager as _;

    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let _ = window.unminimize();
    let _ = window.show();
    let _ = window.set_focus();
}

/// Installs the tray icon and its menu.
///
/// Closing the window must not stop remote sessions: the app keeps running in
/// the tray, and the close handler in [`main`] hides the window rather than
/// destroying it. That makes the tray the only way back to the UI, so a
/// missing bundled icon degrades to a blank tray entry and a warning rather
/// than taking the start down with it (§18).
///
/// This is the only tray icon the app has. `tauri.conf.json` deliberately
/// declares no `app.trayIcon`: Tauri would build a second entry from it, with
/// neither this menu nor this click handler, and an inert icon next to the
/// working one is what the user sees as "one of them does nothing".
fn install_tray(app: &tauri::App) -> tauri::Result<()> {
    use tauri::menu::{Menu, MenuItem};
    use tauri::tray::TrayIconBuilder;

    let show_item = MenuItem::with_id(app, "show", "Show Lumepeer", true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let tray_menu = Menu::with_items(app, &[&show_item, &quit_item])?;

    let mut tray = TrayIconBuilder::new();
    if let Some(icon) = app.default_window_icon().cloned() {
        tray = tray.icon(icon);
    } else {
        tracing::warn!("no bundled window icon: the tray entry will be blank");
    }

    tray.menu(&tray_menu)
        // Carried over from the removed `app.trayIcon` block: on macOS the
        // icon is a monochrome mask tinted by the menu bar, and elsewhere this
        // is a no-op.
        .icon_as_template(true)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "quit" => app.exit(0),
            "show" => focus_main_window(app),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let tauri::tray::TrayIconEvent::Click {
                button: tauri::tray::MouseButton::Left,
                button_state: tauri::tray::MouseButtonState::Up,
                ..
            } = event
            {
                focus_main_window(tray.app_handle());
            }
        })
        .build(app)?;

    Ok(())
}

/// Binds the endpoint, publishes the state every IPC command reads, and puts
/// the tray up.
///
/// Runs inside Tauri's `setup` hook because the actor now owns the remote-view
/// windows, and an `AppHandle` only exists once Tauri has set the application
/// up.
fn setup_app(
    app: &tauri::App,
    runtime: tokio::runtime::Runtime,
    settings: &config::Settings,
) -> Result<(), Box<dyn std::error::Error>> {
    use tauri::Manager as _;

    let network = runtime
        .block_on(network::spawn_actor(app.handle().clone(), settings))
        .unwrap_or_else(|error| {
            eprintln!("fatal: failed to bind the network endpoint: {error}");
            std::process::exit(1);
        });
    let update_url = settings.update_manifest_url();
    if let Some(url) = update_url.as_deref() {
        tracing::info!(channel = ?settings.update_channel(), url, "update channel");
    } else {
        tracing::warn!("no usable update manifest URL configured; updates are off");
    }
    let notifications = network.subscribe();
    let autostart = autostart::Autostart::for_this_app();
    // macOS only (docs/bugs/12-service-lifecycle.md task 4; D6): `.dmg` is a
    // drag-install with no post-install hook at all, unlike deb/rpm, so
    // there is nothing to call `--enable-autostart` at install time. Turning
    // it on happens here instead, once, the first time an installed copy
    // ever runs; the same call also removes a stale login item a previous,
    // since-deleted copy left behind, since a drag-install has no uninstall
    // hook to have done that either. A no-op on Windows and Linux, which get
    // autostart from a hook that runs exactly once already.
    autostart.reconcile_first_launch();
    app.manage(AppState {
        network,
        update_url,
        autostart,
    });
    runtime.spawn(watch_for_window_raising_notifications(
        app.handle().clone(),
        notifications,
    ));
    // The runtime owns every actor and connection task; dropping it here would
    // abort all of them.
    app.manage(runtime);

    install_tray(app)?;

    Ok(())
}

/// Puts this app's window in front of the user when something needs their
/// attention: a guest asking the host to decide (`ConsentRequested`), or a
/// host asking this node, as a guest, for device credentials
/// (`UnattendedChallenge`; docs/bugs/02-connect-form.md, task 5).
///
/// Without this the relevant dialog renders into a window that is hidden in
/// the tray or simply behind something else, and the far side waits until the
/// user happens to look. Nothing else is done with either notification:
/// showing the window is not deciding anything or answering anything, and
/// both stay with the person in the dialog (§8).
///
/// [`ActorNotification`](network::ActorNotification) deliberately carries no
/// peer identity (§15) and does not need to: both dialogs already poll for
/// what they need (`session_status`, `connect_status`).
async fn watch_for_window_raising_notifications(
    app: tauri::AppHandle,
    mut notifications: tokio::sync::broadcast::Receiver<network::ActorNotification>,
) {
    use tauri::Manager as _;
    use tokio::sync::broadcast::error::RecvError;

    loop {
        match notifications.recv().await {
            Ok(
                network::ActorNotification::ConsentRequested
                | network::ActorNotification::UnattendedChallenge,
            ) => {
                focus_main_window(&app);
                // Raising the window can lose to the foreground-lock rules of
                // the platform when the user is busy elsewhere. The taskbar
                // button flashing is the fallback that still gets noticed.
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.request_user_attention(Some(tauri::UserAttentionType::Critical));
                }
            }
            Ok(_) => {}
            // A burst of notifications outran this listener. The ones that
            // were dropped are gone, but the next one that matters must still
            // raise the window, so keep reading.
            Err(RecvError::Lagged(missed)) => {
                tracing::warn!(missed, "window-raising listener fell behind");
            }
            Err(RecvError::Closed) => return,
        }
    }
}

/// Every IPC command this app answers, as Tauri's dispatch table.
///
/// Kept as a function of its own rather than inline in `main`: the list only
/// grows, and it was already what was pushing `main` over clippy's
/// `too_many_lines` threshold — extracting it fixes that at the source
/// instead of suppressing the lint.
fn invoke_handler() -> impl Fn(tauri::ipc::Invoke<tauri::Wry>) -> bool {
    tauri::generate_handler![
        commands::session_grant,
        commands::session_revoke,
        commands::session_status,
        commands::session_set_grant,
        commands::address_book_list,
        commands::address_book_upsert,
        commands::address_book_remove,
        commands::address_book_set_trusted,
        commands::unattended_status,
        commands::unattended_set_password,
        commands::unattended_disable,
        commands::unattended_set_totp,
        commands::unattended_set_role,
        commands::unattended_submit,
        commands::connection_history,
        commands::history_connect,
        commands::history_remove,
        commands::connect_status,
        commands::connect_cancel,
        commands::network_status,
        commands::connection_stats,
        commands::license_status,
        commands::invite_create,
        commands::invite_connect,
        commands::view_next_frame,
        commands::view_cursor,
        commands::input_pointer_move,
        commands::input_press,
        commands::input_wheel,
        commands::chat_send,
        commands::chat_transcript,
        commands::clipboard_push,
        commands::clipboard_pull,
        commands::file_offer,
        commands::file_offer_clipboard,
        commands::file_accept,
        commands::file_abort,
        commands::file_transfers,
        commands::audio_toggle,
        commands::recording_toggle,
        commands::record_request,
        commands::mic_toggle,
        commands::sas_request,
        commands::sas_available,
        commands::monitor_select,
        commands::monitors_list,
        commands::view_set_scale,
        commands::host_display_modes,
        commands::host_display_set_mode,
        commands::recordings_list,
        commands::recording_export,
        commands::audit_list,
        commands::audit_kinds,
        commands::audit_status,
        commands::audit_export,
        commands::audit_clear,
        commands::update_check,
        commands::update_install,
        commands::autostart_status,
        commands::autostart_set,
        commands::service_status,
        commands::service_set,
        commands::host_bar_expand,
        commands::host_bar_focus_main,
    ]
}

/// Reads `--enable-autostart` / `--disable-autostart` off the command line.
///
/// The deb/rpm packaging scripts under `apps/desktop/src-tauri/packaging`
/// call these two flags from `postinst`/`postInstallScript`'s and
/// `prerm`/`preRemoveScript`'s hooks (docs/bugs/12-service-lifecycle.md task
/// 4; D6) — a real install/removal hook exists on Linux, unlike macOS's
/// `.dmg`, so the same per-user mechanism the settings panel's toggle uses
/// (`autostart.rs`, ADR 0042) can be invoked headlessly instead of teaching
/// the packaging scripts a second way to write the same file. A plain
/// function rather than inline matching in `main` so a typo in either flag
/// silently doing nothing is a test failure rather than a packaging script
/// that quietly never enables anything.
fn autostart_cli_flag(args: &[String]) -> Option<bool> {
    if args.iter().any(|arg| arg == "--enable-autostart") {
        Some(true)
    } else if args.iter().any(|arg| arg == "--disable-autostart") {
        Some(false)
    } else {
        None
    }
}

fn main() {
    // `--enable-autostart` / `--disable-autostart`: a headless call into the
    // same autostart mechanism the settings panel's toggle uses, for
    // packaging scripts that have an install/remove hook but no window to
    // click through (see `autostart_cli_flag`). Checked before anything else
    // starts — no config loaded, no log file opened, no webview built —
    // mirroring the equivalent `--install`/`--uninstall` short-circuit at the
    // top of `crates/service`'s own `main.rs`.
    let args: Vec<String> = std::env::args().collect();
    if let Some(enabled) = autostart_cli_flag(&args) {
        match autostart::Autostart::for_this_app().set(enabled) {
            Ok(()) => return,
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
    }

    // Configuration first: it decides where the log file goes, and tracing has
    // to be installed before anything worth logging happens (§5.1, §16.1).
    let (settings, notes) = config::Settings::load();
    let log_file = logging::init(&settings);
    for note in notes {
        tracing::info!("{note}");
    }
    if let Some(path) = log_file {
        tracing::info!(path = %path.display(), "logging to file");
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|error| {
            eprintln!("fatal: failed to start the async runtime: {error}");
            std::process::exit(1);
        });

    #[allow(
        unused_mut,
        reason = "only reassigned when built with --features pilot (debug-only tauri-pilot wiring below)"
    )]
    // The dialog plugin is registered so the *Rust* side can open the OS
    // file and directory pickers for §9.2. No `dialog:` permission appears in
    // any capability file, so the webview cannot invoke it: registering a
    // plugin makes it available to this process, not to the untrusted
    // presentation layer (§2.3; ADR 0032).
    let mut builder = tauri::Builder::default()
        // First in the chain on purpose: a second launch has to be turned away
        // before any other plugin or the setup hook gets to claim a resource
        // the running process already owns — above all the iroh endpoint,
        // whose NodeId is what the invite code already in someone's hands
        // points at (docs/bugs/01). The callback hands the first process's
        // window back instead, which is what the second launch was for.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            focus_main_window(app);
        }))
        // The plugin carries the public key from `tauri.conf.json`; the
        // endpoint is chosen per check instead, because it depends on the
        // configured channel (§21; ADR 0042) and a channel that could only be
        // changed by rebuilding would not be a channel.
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_dialog::init());

    // Local cross-platform debugging aid, never in a release binary: gated
    // on both the non-default `pilot` Cargo feature and debug_assertions.
    // Its capability grant lives in capabilities-pilot/, which build.rs only
    // reads when this feature is enabled (see build.rs) — the default
    // capabilities/ directory never mentions the `pilot:default` permission,
    // so a plain build never has to know the plugin exists.
    #[cfg(all(debug_assertions, feature = "pilot"))]
    {
        builder = builder.plugin(tauri_plugin_pilot::init());
    }

    builder
        // The actor is built here rather than before the builder because it now
        // owns the remote-view windows, and an `AppHandle` only exists once
        // Tauri has set the application up.
        .setup(move |app| setup_app(app, runtime, &settings))
        .on_window_event(|window, event| {
            // Closing the window hides it instead of quitting: lumepeer keeps
            // serving remote sessions from the tray until "Quit" is chosen.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                // The session bar is the exception: it is the host's "somebody
                // is connected, and here is the stop button" surface, and it
                // goes away when the last session ends and not before
                // (ADR 0055). Hiding it on an Alt+F4 would leave a live but
                // invisible window and no indicator at all, which is the gap
                // it exists to close.
                if window.label() != crate::view::HOST_BAR_LABEL {
                    let _ = window.hide();
                }
            }
        })
        .invoke_handler(invoke_handler())
        .run(tauri::generate_context!())
        .unwrap_or_else(|error| {
            eprintln!("fatal: failed to start the application: {error}");
            std::process::exit(1);
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(rest: &[&str]) -> Vec<String> {
        std::iter::once("lumepeer-desktop".to_owned())
            .chain(rest.iter().map(|arg| (*arg).to_owned()))
            .collect()
    }

    #[test]
    fn recognizes_enable_and_disable() {
        assert_eq!(
            autostart_cli_flag(&args(&["--enable-autostart"])),
            Some(true)
        );
        assert_eq!(
            autostart_cli_flag(&args(&["--disable-autostart"])),
            Some(false)
        );
    }

    #[test]
    fn ignores_everything_else() {
        assert_eq!(autostart_cli_flag(&args(&[])), None);
        assert_eq!(autostart_cli_flag(&args(&["--enable-autostar"])), None);
        assert_eq!(autostart_cli_flag(&args(&["enable-autostart"])), None);
    }
}
