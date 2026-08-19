//! Tauri IPC surface (design doc §13).
//!
//! Every command takes a typed DTO, never `serde_json::Value`, and every
//! decision is taken by the network actor: the webview is an untrusted
//! presentation layer (§2.3, §4). The only peer-identifying string that
//! ever crosses this boundary is the pseudonymized label the actor handed
//! out on a previous `session_status` poll — never a raw `NodeId`.

#![allow(
    clippy::needless_pass_by_value,
    reason = "tauri command handlers take Window and State by value"
)]

use serde::{Deserialize, Serialize};
use tauri::Window;

use crate::AppState;
use crate::network::{ActorError, SessionStateDto};

/// Label of the window allowed to call the session/invite/license commands.
const MAIN_WINDOW_LABEL: &str = "main";

/// Prefix of a remote-view window's label (`view-{peer label}`), the only other
/// window this build ever creates.
const VIEW_WINDOW_PREFIX: &str = "view-";

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

    fn poisoned() -> Self {
        Self {
            code: "STATE_POISONED",
            message: "session state is unavailable".to_owned(),
        }
    }

    fn unknown_peer() -> Self {
        Self {
            code: "UNKNOWN_PEER",
            message: "no session matches that peer".to_owned(),
        }
    }

    fn core(error: &lumepeer_core::CoreError) -> Self {
        Self {
            code: "CORE",
            message: error.to_string(),
        }
    }

    /// Maps a transport failure onto the error matrix of §18.
    ///
    /// The message is a fixed string chosen per code, never the `Display` of
    /// the underlying error: an iroh dial failure spells out the address and
    /// `NodeId` it was trying to reach, and that must not reach the webview
    /// through the error channel any more than through the happy path (§15).
    ///
    /// Everything that is not a bad ticket, an unreachable host or a version
    /// mismatch collapses into `REJECTED` on purpose, so a stranger probing a
    /// ticket learns nothing about *why* it failed.
    fn net(error: &lumepeer_net::NetError) -> Self {
        use lumepeer_core::CoreError;
        use lumepeer_net::NetError;

        let (code, message) = match *error {
            NetError::MalformedTicket | NetError::InvalidTicket => {
                ("BAD_TICKET", "the invite is not valid or has expired")
            }
            NetError::Dial(_) | NetError::Endpoint(_) => {
                ("DIAL_FAILED", "the host could not be reached")
            }
            NetError::Framing(CoreError::IncompatibleVersion { .. }) => (
                "INCOMPATIBLE_VERSION",
                "the host speaks an incompatible protocol version",
            ),
            _ => ("REJECTED", "the host refused the connection"),
        };
        Self {
            code,
            message: message.to_owned(),
        }
    }
}

impl From<ActorError> for IpcError {
    fn from(error: ActorError) -> Self {
        match error {
            ActorError::UnknownPeer => Self::unknown_peer(),
            ActorError::Core(e) => Self::core(&e),
            ActorError::Net(e) => Self::net(&e),
            ActorError::ChannelClosed => Self::poisoned(),
        }
    }
}

/// Rejects calls coming from any window other than the main one (§13).
fn check_window(window: &Window) -> Result<(), IpcError> {
    if window.label() == MAIN_WINDOW_LABEL {
        Ok(())
    } else {
        Err(IpcError::denied())
    }
}

/// Rejects calls that do not come from the view window of `peer`.
///
/// The Tauri capability in `capabilities/view.json` already limits these four
/// commands to `view-*` windows; this narrows it one step further, to *that*
/// peer's own window, so one open view cannot poll frames from or drive input
/// into another session. Cheap, and it keeps the rule in the Rust layer where
/// the rest of the authorization lives rather than only in a JSON file (§2.3,
/// §13).
fn check_view_window(window: &Window, peer: &str) -> Result<(), IpcError> {
    let label = window.label();
    if label.strip_prefix(VIEW_WINDOW_PREFIX) == Some(peer) {
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

/// Session state as seen by the webview.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStateDtoWire {
    /// Queued, waiting for the host's decision.
    Pending,
    /// Consent granted, grants are live.
    Active,
}

impl From<SessionStateDto> for SessionStateDtoWire {
    fn from(value: SessionStateDto) -> Self {
        match value {
            SessionStateDto::Pending => Self::Pending,
            SessionStateDto::Active => Self::Active,
        }
    }
}

/// Argument of [`session_grant`].
#[derive(Debug, Clone, Deserialize)]
pub struct SessionGrantArgs {
    /// Pseudonymized label of the peer, as handed out by `session_status`.
    pub peer: String,
    /// Role the host chose, which may be lower than the requested one.
    pub role: RoleDto,
}

/// Argument of [`session_revoke`].
#[derive(Debug, Clone, Deserialize)]
pub struct SessionRevokeArgs {
    /// Pseudonymized label of the peer being revoked.
    pub peer: String,
}

/// Argument of [`invite_create`].
#[derive(Debug, Clone, Deserialize)]
pub struct InviteCreateArgs {
    /// Role the invite allows the guest to request.
    pub role: RoleDto,
}

/// What [`invite_create`] hands back to the UI to render as a QR code.
#[derive(Debug, Clone, Serialize)]
pub struct InviteDto {
    /// String to encode as a QR code (also usable as plain text).
    pub qr_string: String,
    /// Unix seconds after which the invite is dead.
    pub expires_at: u64,
}

/// Argument of [`invite_connect`].
#[derive(Debug, Clone, Deserialize)]
pub struct InviteConnectArgs {
    /// The scanned/pasted QR string.
    pub ticket: String,
}

/// Snapshot of one session for the status UI.
#[derive(Debug, Clone, Serialize)]
pub struct SessionStatusDto {
    /// Pseudonymized peer label; never a raw `NodeId` (§15).
    pub peer_label: String,
    /// Pending or active.
    pub state: SessionStateDtoWire,
    /// Role requested (pending) or granted (active).
    pub role: RoleDto,
    /// Whether input injection is currently permitted.
    pub input: bool,
}

/// One row of the past-connections list (§21 punch-list item 5).
#[derive(Debug, Clone, Serialize)]
pub struct HistoryEntryDto {
    /// Pseudonymized peer label, as `session_status` uses (§15).
    pub peer_label: String,
    /// Role the peer held before the session ended.
    pub role: RoleDto,
    /// Unix seconds the session ended.
    pub ended_at: u64,
}

/// Whether this host is ready to accept incoming connections.
#[derive(Debug, Clone, Serialize)]
pub struct NetworkStatusDto {
    /// True once the local endpoint has reached a relay and is dialable
    /// from outside the LAN.
    pub ready: bool,
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

/// Grants a role. The decision is taken by the actor, never in the webview
/// (§2.3).
///
/// # Errors
/// Rejects calls from other windows; propagates [`ActorError`] as an
/// [`IpcError`] (plan ceiling of §8.2, single-controller rule, unknown
/// label).
#[tauri::command]
pub async fn session_grant(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: SessionGrantArgs,
) -> Result<(), IpcError> {
    check_window(&window)?;
    state.network.grant(args.peer, args.role.into()).await?;
    Ok(())
}

/// Revokes every grant of a peer immediately (§8.1).
///
/// Callable from the main window for any peer, and from a `view-{peer}` window
/// for its own peer only: closing a view window ends that session, which is the
/// same one on/off switch as the status list's revoke button rather than a
/// second state to keep in sync.
///
/// # Errors
/// Rejects calls from any other window; propagates [`ActorError`] as an
/// [`IpcError`].
#[tauri::command]
pub async fn session_revoke(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: SessionRevokeArgs,
) -> Result<(), IpcError> {
    if check_window(&window).is_err() {
        check_view_window(&window, &args.peer)?;
    }
    state.network.revoke(args.peer).await?;
    Ok(())
}

/// Lists pending and active sessions for the status/consent UI.
///
/// # Errors
/// Rejects calls from other windows; [`IpcError`] if the actor is gone.
#[tauri::command]
pub async fn session_status(
    window: Window,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<SessionStatusDto>, IpcError> {
    check_window(&window)?;
    let snapshot = state.network.status().await?;
    Ok(snapshot
        .into_iter()
        .map(|s| SessionStatusDto {
            peer_label: s.label,
            state: s.state.into(),
            role: s.role.into(),
            input: s.input,
        })
        .collect())
}

/// Lists past connections that have already ended, newest first (§21
/// punch-list item 5).
///
/// # Errors
/// Rejects calls from other windows; [`IpcError`] if the actor is gone.
#[tauri::command]
pub async fn connection_history(
    window: Window,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<HistoryEntryDto>, IpcError> {
    check_window(&window)?;
    let entries = state.network.history().await?;
    Ok(entries
        .into_iter()
        .map(|e| HistoryEntryDto {
            peer_label: e.peer_label,
            role: e.role.into(),
            ended_at: e.ended_at,
        })
        .collect())
}

/// Issues an invite for `args.role` and returns the QR payload.
///
/// # Errors
/// Rejects calls from other windows; propagates [`ActorError`].
#[tauri::command]
pub async fn invite_create(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: InviteCreateArgs,
) -> Result<InviteDto, IpcError> {
    check_window(&window)?;
    let dto = state.network.invite_create(args.role.into()).await?;
    Ok(InviteDto {
        qr_string: dto.qr_string,
        expires_at: dto.expires_at,
    })
}

/// Connects to the host named by `args.ticket`.
///
/// # Errors
/// Rejects calls from other windows; propagates [`ActorError`].
#[tauri::command]
pub async fn invite_connect(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: InviteConnectArgs,
) -> Result<(), IpcError> {
    check_window(&window)?;
    state.network.invite_connect(args.ticket).await?;
    Ok(())
}

/// Reports whether this host is ready to accept incoming connections.
///
/// # Errors
/// Rejects calls from other windows.
#[tauri::command]
pub fn network_status(
    window: Window,
    state: tauri::State<'_, AppState>,
) -> Result<NetworkStatusDto, IpcError> {
    check_window(&window)?;
    Ok(NetworkStatusDto {
        ready: state.network.online(),
    })
}

/// Reports the license state.
///
/// # Errors
/// Rejects calls from other windows.
#[tauri::command]
pub fn license_status(window: Window) -> Result<LicenseStatusDto, IpcError> {
    check_window(&window)?;
    // Phase 3 fills these from a verified license token (§12.1, §12.4); the
    // plan is Trial until then, same default `SessionManager::new()` used.
    Ok(LicenseStatusDto {
        plan: "trial".to_owned(),
        seconds_left: None,
        offline: true,
    })
}

/// Argument of [`view_next_frame`].
#[derive(Debug, Clone, Deserialize)]
pub struct ViewArgs {
    /// Pseudonymized label of the host being watched.
    pub peer: String,
    /// Timestamp of the picture the caller already has, or 0 if it has none
    /// yet. Lets the actor skip re-serializing the pixel buffer when nothing
    /// new has arrived since the caller's last poll.
    #[serde(default)]
    pub since_us: u64,
}

/// Argument of [`input_pointer_move`].
#[derive(Debug, Clone, Deserialize)]
pub struct PointerMoveArgs {
    /// Pseudonymized label of the host being watched.
    pub peer: String,
    /// Horizontal position, normalized to `0..=65535` of the captured surface.
    pub x: u16,
    /// Vertical position, normalized to `0..=65535` of the captured surface.
    pub y: u16,
    /// Modifier bitmask.
    pub modifiers: u32,
}

/// Argument of [`input_press`].
#[derive(Debug, Clone, Deserialize)]
pub struct PressArgs {
    /// Pseudonymized label of the host being watched.
    pub peer: String,
    /// Platform-independent logical key or pointer-button identifier.
    pub logical: u32,
    /// Physical scancode as reported by this machine.
    pub scancode: u32,
    /// Modifier bitmask.
    pub modifiers: u32,
    /// `true` for a press, `false` for a release.
    pub pressed: bool,
}

/// Argument of [`input_wheel`].
#[derive(Debug, Clone, Deserialize)]
pub struct WheelArgs {
    /// Pseudonymized label of the host being watched.
    pub peer: String,
    /// Horizontal delta.
    pub dx: i16,
    /// Vertical delta.
    pub dy: i16,
    /// Modifier bitmask.
    pub modifiers: u32,
}

/// Newest decoded picture for a view window, as raw bytes.
///
/// Binary rather than JSON on purpose: a 1080p RGBA picture is ~8 MB, and
/// base64-ing that per frame would spend the whole latency budget of §15 on
/// encoding. Layout, little endian:
/// `status:u8 | input:u8 | width:u32 | height:u32 | timestamp_us:u64 | RGBA8`.
/// `status` is 0 waiting, 1 live, 2 reconnecting, 3 failed; before the first
/// picture only the 18-byte header comes back. The pixel payload is also
/// omitted whenever `args.since_us` already names the current picture — the
/// caller is polling faster than the video updates, and the header alone
/// tells it that.
///
/// # Errors
/// Rejects calls from anything but this peer's own view window; [`IpcError`] if
/// no such view exists or the actor is gone.
#[tauri::command]
pub async fn view_next_frame(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: ViewArgs,
) -> Result<tauri::ipc::Response, IpcError> {
    check_view_window(&window, &args.peer)?;
    let bytes = state.network.view_frame(args.peer, args.since_us).await?;
    Ok(tauri::ipc::Response::new(bytes))
}

/// Forwards absolute pointer motion to the host being watched (§11).
///
/// # Errors
/// Rejects calls from anything but this peer's own view window; [`IpcError`]
/// with `CORE` if the session no longer holds an `input` grant. The host checks
/// again, per event, and is the authority (§2.3).
#[tauri::command]
pub async fn input_pointer_move(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: PointerMoveArgs,
) -> Result<(), IpcError> {
    use lumepeer_core::protocol::{InputDetail, InputEventPayload};

    check_view_window(&window, &args.peer)?;
    state
        .network
        .input(
            args.peer,
            InputEventPayload {
                logical: 0,
                scancode: 0,
                modifiers: args.modifiers,
                detail: InputDetail::PointerMove {
                    x: args.x,
                    y: args.y,
                },
            },
        )
        .await?;
    Ok(())
}

/// Forwards a key or pointer-button press/release to the host (§11).
///
/// # Errors
/// As [`input_pointer_move`].
#[tauri::command]
pub async fn input_press(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: PressArgs,
) -> Result<(), IpcError> {
    use lumepeer_core::protocol::{InputDetail, InputEventPayload};

    check_view_window(&window, &args.peer)?;
    state
        .network
        .input(
            args.peer,
            InputEventPayload {
                logical: args.logical,
                scancode: args.scancode,
                modifiers: args.modifiers,
                detail: if args.pressed {
                    InputDetail::Press
                } else {
                    InputDetail::Release
                },
            },
        )
        .await?;
    Ok(())
}

/// Forwards a scroll wheel movement to the host (§11).
///
/// # Errors
/// As [`input_pointer_move`].
#[tauri::command]
pub async fn input_wheel(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: WheelArgs,
) -> Result<(), IpcError> {
    use lumepeer_core::protocol::{InputDetail, InputEventPayload};

    check_view_window(&window, &args.peer)?;
    state
        .network
        .input(
            args.peer,
            InputEventPayload {
                logical: 0,
                scancode: 0,
                modifiers: args.modifiers,
                detail: InputDetail::Wheel {
                    dx: args.dx,
                    dy: args.dy,
                },
            },
        )
        .await?;
    Ok(())
}
