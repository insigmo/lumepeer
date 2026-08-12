//! Lumepeer desktop application (design doc §4, §13).
//!
//! The Tauri layer owns the window and forwards typed IPC calls into
//! `lumepeer-core`. It holds no authority of its own: the webview is an
//! untrusted presentation layer (§2.3).

#![forbid(unsafe_code)]
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![allow(
    unreachable_pub,
    reason = "binary crate: `pub` marks the IPC surface of §13, not a library API"
)]

mod commands;

use std::sync::Mutex;

use lumepeer_core::session::SessionManager;
use rand::Rng as _;

/// State shared by every IPC command.
///
/// The `SessionManager` is the single authorization decision point, so it is
/// behind one lock rather than cloned per command.
#[derive(Debug)]
pub struct AppState {
    /// Session state machine, consent queue and grants (§8).
    pub sessions: Mutex<SessionManager>,
    /// Per-install salt for the pseudonymized peer labels of §15. Regenerated
    /// on every start: labels are stable within a run and meaningless across
    /// runs, so a log cannot be correlated back to an identity.
    pub install_salt: [u8; 32],
}

impl Default for AppState {
    fn default() -> Self {
        let mut install_salt = [0u8; 32];
        rand::rng().fill_bytes(&mut install_salt);
        Self {
            sessions: Mutex::new(SessionManager::new()),
            install_salt,
        }
    }
}

fn main() {
    init_tracing();

    tauri::Builder::default()
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            commands::session_request,
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
