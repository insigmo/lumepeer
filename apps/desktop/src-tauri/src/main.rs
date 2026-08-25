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

mod commands;
mod config;
mod connection_history;
mod logging;
mod network;
mod recorder;
mod view;

/// State shared by every IPC command: a handle into the network actor.
#[derive(Debug)]
pub struct AppState {
    /// Channel handle into the `NetworkActor` (§2.3): the only way commands
    /// reach `SessionManager` or the transport.
    pub network: network::ActorHandle,
}

fn main() {
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
    let mut builder =
        tauri::Builder::default().plugin(tauri_plugin_updater::Builder::new().build());

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
        .setup(move |app| {
            use tauri::Manager as _;

            let network = runtime
                .block_on(network::spawn_actor(app.handle().clone(), &settings))
                .unwrap_or_else(|error| {
                    eprintln!("fatal: failed to bind the network endpoint: {error}");
                    std::process::exit(1);
                });
            app.manage(AppState { network });
            // The runtime owns every actor and connection task; dropping it
            // here would abort all of them.
            app.manage(runtime);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::session_grant,
            commands::session_revoke,
            commands::session_status,
            commands::connection_history,
            commands::history_connect,
            commands::connect_status,
            commands::network_status,
            commands::license_status,
            commands::invite_create,
            commands::invite_connect,
            commands::view_next_frame,
            commands::input_pointer_move,
            commands::input_press,
            commands::input_wheel,
            commands::chat_send,
            commands::chat_transcript,
            commands::clipboard_push,
            commands::clipboard_pull,
            commands::audio_toggle,
            commands::recording_toggle,
            commands::mic_toggle,
            commands::sas_request,
            commands::sas_available,
            commands::monitor_select,
            commands::monitors_list,
        ])
        .run(tauri::generate_context!())
        .unwrap_or_else(|error| {
            eprintln!("fatal: failed to start the application: {error}");
            std::process::exit(1);
        });
}
