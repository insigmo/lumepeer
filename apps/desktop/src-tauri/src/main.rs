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
mod network;

/// State shared by every IPC command: a handle into the network actor.
#[derive(Debug)]
pub struct AppState {
    /// Channel handle into the `NetworkActor` (§2.3): the only way commands
    /// reach `SessionManager` or the transport.
    pub network: network::ActorHandle,
}

fn main() {
    init_tracing();

    tauri::Builder::default()
        .manage(AppState {
            network: network::spawn_actor(),
        })
        .invoke_handler(tauri::generate_handler![
            commands::session_grant,
            commands::session_revoke,
            commands::session_status,
            commands::license_status,
        ])
        .run(tauri::generate_context!())
        .unwrap_or_else(|error| {
            // Nothing sensitive here: this is a startup failure of the window
            // layer, before any peer or license data exists (§15).
            eprintln!("fatal: failed to start the application: {error}");
            std::process::exit(1);
        });
}

/// Human-readable logs in development, structured JSON in release (§16.1).
fn init_tracing() {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    if cfg!(debug_assertions) {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    } else {
        tracing_subscriber::fmt()
            .json()
            .with_env_filter(filter)
            .init();
    }
}
