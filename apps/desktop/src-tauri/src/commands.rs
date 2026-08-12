//! Tauri IPC surface (design doc §13).
//!
//! Exactly five commands are exposed, matching the allowlist in
//! `capabilities/main.json`. Every command takes a typed DTO, never
//! `serde_json::Value`, and every decision is taken by `lumepeer-core`: the
//! webview is an untrusted presentation layer (§2.3, §4).

#![allow(
    clippy::needless_pass_by_value,
    reason = "tauri command handlers take Window and State by value"
)]

use lumepeer_core::NodeId;
use lumepeer_core::license::Plan;
use serde::{Deserialize, Serialize};
use tauri::Window;

use crate::AppState;

/// Label of the only window allowed to call these commands.
const MAIN_WINDOW_LABEL: &str = "main";

/// Error returned to the webview. Carries a code and a short message, never
/// secrets, tickets, tokens or raw peer identities (§15).
#[derive(Debug, Clone, Serialize)]
pub struct IpcError {
    /// Stable machine-readable code.
    pub code: &'static str,
    /// Short human-readable message, safe to display.
    pub message: String,
}

impl IpcError {
    fn denied() -> Self {
        Self {
            code: "WINDOW_NOT_ALLOWED",
            message: "command is not available from this window".to_owned(),
        }
    }

    fn core(error: &lumepeer_core::CoreError) -> Self {
        Self {
            code: "CORE",
            message: error.to_string(),
        }
    }

    fn bad_peer() -> Self {
        Self {
            code: "BAD_PEER",
            message: "peer identity is not a valid endpoint id".to_owned(),
        }
    }

    fn poisoned() -> Self {
        Self {
            code: "STATE_POISONED",
            message: "session state is unavailable".to_owned(),
        }
    }
}

/// Parses the hex endpoint id the UI passes back to us.
///
/// The webview only ever echoes an identity the core handed it; an identity it
/// invented is refused here and authorizes nothing either way (§2.3).
fn parse_peer(peer: &str) -> Result<NodeId, IpcError> {
    peer.parse::<NodeId>().map_err(|_| IpcError::bad_peer())
}

/// Pseudonymized label of a peer: the first 8 bytes of the salted BLAKE3 of its
/// identity, never the identity itself (§15).
fn peer_label(install_salt: &[u8; 32], peer: &NodeId) -> String {
    let hash = lumepeer_core::audit::peer_hash(install_salt, peer);
    hash[..8].iter().fold(String::new(), |mut out, byte| {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// Rejects calls coming from any window other than the main one (§13).
fn check_window(window: &Window) -> Result<(), IpcError> {
    if window.label() == MAIN_WINDOW_LABEL {
        Ok(())
    } else {
        Err(IpcError::denied())
    }
}

/// Role as seen by the webview.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleDto {
    /// Only `view`.
    ViewOnly,
    /// `view` plus allowlisted actions.
    ControlLimited,
    /// `view` plus keyboard and mouse.
    FullControl,
}

impl From<RoleDto> for lumepeer_core::consent::Role {
    fn from(value: RoleDto) -> Self {
        match value {
            RoleDto::ViewOnly => Self::ViewOnly,
            RoleDto::ControlLimited => Self::ControlLimited,
            RoleDto::FullControl => Self::FullControl,
        }
    }
}

impl From<lumepeer_core::consent::Role> for RoleDto {
    fn from(value: lumepeer_core::consent::Role) -> Self {
        match value {
            lumepeer_core::consent::Role::ViewOnly => Self::ViewOnly,
            lumepeer_core::consent::Role::ControlLimited => Self::ControlLimited,
            lumepeer_core::consent::Role::FullControl => Self::FullControl,
        }
    }
}

/// Argument of [`session_request`].
#[derive(Debug, Clone, Deserialize)]
pub struct SessionRequestArgs {
    /// Peer that asked for consent, as the hex identity shown in the UI.
    pub peer: String,
    /// Role the peer asked for.
    pub role: RoleDto,
}

/// Argument of [`session_grant`].
#[derive(Debug, Clone, Deserialize)]
pub struct SessionGrantArgs {
    /// Peer being granted.
    pub peer: String,
    /// Role the host chose, which may be lower than the requested one.
    pub role: RoleDto,
}

/// Argument of [`session_revoke`].
#[derive(Debug, Clone, Deserialize)]
pub struct SessionRevokeArgs {
    /// Peer being revoked.
    pub peer: String,
}

/// Snapshot of one session for the status UI.
#[derive(Debug, Clone, Serialize)]
pub struct SessionStatusDto {
    /// Pseudonymized peer label; never a raw `NodeId` (§15).
    pub peer_label: String,
    /// Role currently held.
    pub role: RoleDto,
    /// Whether input injection is currently permitted.
    pub input: bool,
}

/// License state for the UI.
#[derive(Debug, Clone, Serialize)]
pub struct LicenseStatusDto {
    /// Plan name.
    pub plan: String,
    /// Seconds left in the current license window, if bounded.
    pub seconds_left: Option<u64>,
    /// Whether the app is currently running on cached license data (§12.4).
    pub offline: bool,
}

/// Queues a consent request raised by the transport layer.
///
/// # Errors
/// Rejects calls from other windows, and propagates the rate limit of §9.2 and
/// the queue limit of §8.1.
#[tauri::command]
pub fn session_request(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: SessionRequestArgs,
) -> Result<(), IpcError> {
    check_window(&window)?;
    let peer = parse_peer(&args.peer)?;
    let mut sessions = state.sessions.lock().map_err(|_| IpcError::poisoned())?;
    sessions
        .request_consent_as(peer, args.role.into())
        .map(|_ticket| ())
        .map_err(|e| IpcError::core(&e))
}

/// Grants a role. The decision is taken here on the Rust side, never in the
/// webview (§2.3).
///
/// # Errors
/// Propagates [`lumepeer_core::CoreError`] as an [`IpcError`]: the plan ceiling
/// of §8.2 and the single-controller rule reject the call here, not in the UI.
#[tauri::command]
pub fn session_grant(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: SessionGrantArgs,
) -> Result<(), IpcError> {
    check_window(&window)?;
    let peer = parse_peer(&args.peer)?;
    let mut sessions = state.sessions.lock().map_err(|_| IpcError::poisoned())?;
    sessions
        .grant(peer, args.role.into())
        .map_err(|e| IpcError::core(&e))
}

/// Revokes every grant of a peer immediately (§8.1).
///
/// # Errors
/// Propagates [`lumepeer_core::CoreError`] as an [`IpcError`].
#[tauri::command]
pub fn session_revoke(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: SessionRevokeArgs,
) -> Result<(), IpcError> {
    check_window(&window)?;
    let peer = parse_peer(&args.peer)?;
    let mut sessions = state.sessions.lock().map_err(|_| IpcError::poisoned())?;
    sessions.revoke(peer).map_err(|e| IpcError::core(&e))
}

/// Lists active sessions for the status indicator.
///
/// # Errors
/// Rejects calls from other windows.
#[tauri::command]
pub fn session_status(
    window: Window,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<SessionStatusDto>, IpcError> {
    check_window(&window)?;
    let sessions = state.sessions.lock().map_err(|_| IpcError::poisoned())?;
    Ok(sessions
        .active()
        .into_iter()
        .map(|(peer, role, grants)| SessionStatusDto {
            peer_label: peer_label(&state.install_salt, &peer),
            role: role.into(),
            input: grants.input,
        })
        .collect())
}

/// Reports the license state.
///
/// # Errors
/// Rejects calls from other windows.
#[tauri::command]
pub fn license_status(
    window: Window,
    state: tauri::State<'_, AppState>,
) -> Result<LicenseStatusDto, IpcError> {
    check_window(&window)?;
    let sessions = state.sessions.lock().map_err(|_| IpcError::poisoned())?;
    let plan = match sessions.plan() {
        Plan::Trial => "trial",
        Plan::Pro => "pro",
        Plan::Team => "team",
    };
    Ok(LicenseStatusDto {
        plan: plan.to_owned(),
        // Phase 3 fills these from a verified license token (§12.1, §12.4).
        seconds_left: None,
        offline: true,
    })
}
