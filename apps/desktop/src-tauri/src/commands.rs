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
        let (code, message) = classify_net(error);
        Self {
            code,
            message: message.to_owned(),
        }
    }
}

/// The §18 code of a transport failure, without its message.
///
/// The dial now runs off the actor loop, so a failure can no longer be the
/// IPC call's own `Err`: the actor keeps this code instead and `connect_status`
/// hands it to the webview, which owns the wording in the user's language
/// (ADR 0027). Same classification as [`IpcError::net`], so nothing is
/// disclosed here that the error channel would not have disclosed anyway.
pub fn net_error_code(error: &lumepeer_net::NetError) -> &'static str {
    classify_net(error).0
}

/// Maps a transport failure onto the (code, message) pair of §18.
fn classify_net(error: &lumepeer_net::NetError) -> (&'static str, &'static str) {
    use lumepeer_core::CoreError;
    use lumepeer_net::NetError;

    match *error {
        NetError::MalformedTicket | NetError::InvalidTicket => {
            ("BAD_TICKET", "the invite is not valid or has expired")
        }
        NetError::AlreadyConnected => (
            "ALREADY_CONNECTED",
            "you are already connected to this device",
        ),
        NetError::Dial(_) | NetError::Endpoint(_) => {
            ("DIAL_FAILED", "the host could not be reached")
        }
        // This device, not the peer: nothing is wrong with the invite or
        // the far side, so it must not read like a rejection.
        NetError::Offline => (
            "OFFLINE",
            "this device is not reachable from the internet yet — wait for the status to turn ready, then try again",
        ),
        NetError::Framing(CoreError::IncompatibleVersion { .. }) => (
            "INCOMPATIBLE_VERSION",
            "the host speaks an incompatible protocol version",
        ),
        // Transport, not verdict. This is what *this* side observed — a
        // stream that stopped — so saying so leaks nothing about why the
        // far end did anything, and it keeps a flapping link from being
        // reported as a rejection, which sends the user hunting on the
        // wrong machine (ADR 0026).
        NetError::Io(_) => (
            "TRANSPORT_LOST",
            "the connection dropped before the session was set up — check the network and try again",
        ),
        _ => ("REJECTED", "the host refused the connection"),
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

/// What [`invite_create`] hands back to the UI to show as the invite code.
#[derive(Debug, Clone, Serialize)]
pub struct InviteDto {
    /// The invite code itself: plain text the host shows and a guest pastes.
    pub code: String,
    /// Unix seconds after which the invite is dead.
    pub expires_at: u64,
}

/// Argument of [`invite_connect`].
#[derive(Debug, Clone, Deserialize)]
pub struct InviteConnectArgs {
    /// The pasted invite code.
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

/// One remembered host this node has connected to (§21 punch-list item 5).
///
/// The invite code the row carries stays in Rust: the webview reconnects by
/// label through [`history_connect`], never by handing a code back (§13).
#[derive(Debug, Clone, Serialize)]
pub struct HistoryEntryDto {
    /// Pseudonymized host label, stable across restarts; never a raw `NodeId`
    /// (§15).
    pub peer_label: String,
    /// Role the host last granted.
    pub role: RoleDto,
    /// Unix seconds the last session with this host ended.
    pub ended_at: u64,
}

/// Argument of [`history_connect`].
#[derive(Debug, Clone, Deserialize)]
pub struct HistoryConnectArgs {
    /// Label of the remembered host, as `connection_history` handed it out.
    pub peer: String,
}

/// Guest-side state of this node's own outgoing connect attempt (§21
/// punch-list item 6).
#[derive(Debug, Clone, Serialize)]
pub struct ConnectStatusDto {
    /// One of `idle`, `dialing`, `awaiting_consent`, `connected`, `denied`,
    /// `failed`.
    pub phase: &'static str,
    /// Whether an attempt is still in flight — dialing, or waiting on the far
    /// side's decision — which is what keeps the Connect button disabled.
    pub pending: bool,
    /// §18 code of the last failure, set only alongside `phase: "failed"`.
    ///
    /// Present because the dial runs off the actor loop and can no longer
    /// fail the IPC call it started from: without it every transport problem
    /// would reach the user as one undifferentiated "could not connect", which
    /// is the report ADR 0026 was written about (ADR 0027).
    pub code: Option<&'static str>,
}

/// Whether this host is ready to accept incoming connections.
#[derive(Debug, Clone, Serialize)]
pub struct NetworkStatusDto {
    /// True once the local endpoint has reached a relay and is dialable
    /// from outside the LAN.
    pub ready: bool,
    /// False when this machine has no screen-capture backend at all: it can
    /// still accept a session, grant consent and take input, but it can never
    /// send a picture (§18, docs/adr/0024).
    pub can_capture: bool,
    /// False once a session has found this machine has no video encoder.
    ///
    /// True until then rather than "checked and fine": an encoder is only
    /// ever built inside a session, so nothing has been asked yet on a host
    /// nobody has connected to.
    pub can_encode: bool,
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

/// Lists hosts this node has connected to before, most recent first (§21
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

/// Dials a remembered host again, using the invite code kept alongside its
/// history row.
///
/// The code itself never crosses this boundary in either direction: the
/// webview names a remembered host by label, and the actor looks the code up
/// (§13, ADR 0016). The host still decides — reconnecting asks for consent
/// exactly like a first connection does (§2.3).
///
/// # Errors
/// Rejects calls from other windows; propagates [`ActorError`].
#[tauri::command]
pub async fn history_connect(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: HistoryConnectArgs,
) -> Result<(), IpcError> {
    check_window(&window)?;
    state.network.history_connect(args.peer).await?;
    Ok(())
}

/// Reports how this node's own outgoing connect attempt is going, so the
/// connect form can stay disabled while the far side is deciding.
///
/// # Errors
/// Rejects calls from other windows; [`IpcError`] if the actor is gone.
#[tauri::command]
pub async fn connect_status(
    window: Window,
    state: tauri::State<'_, AppState>,
) -> Result<ConnectStatusDto, IpcError> {
    check_window(&window)?;
    let (phase, code) = state.network.connect_state().await?;
    Ok(ConnectStatusDto {
        phase: phase.as_str(),
        pending: phase.is_pending(),
        code,
    })
}

/// Issues an invite for `args.role` and returns its code.
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
        code: dto.code,
        expires_at: dto.expires_at,
    })
}

/// Starts connecting to the host named by `args.ticket`.
///
/// Returns as soon as the attempt is under way, not when it lands: the dial
/// itself runs off the actor loop so that it cannot freeze the app that
/// started it (ADR 0027). An invite that is malformed, expired or already
/// connected still fails here, synchronously — everything after that is
/// reported through `connect_status`.
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

/// Reports whether this host is ready to accept incoming connections, and
/// whether it can actually produce a picture if one does (§18).
///
/// The second part is why this command exists at all beyond the relay pill:
/// an operator sharing their screen from a machine with no capture backend
/// used to learn nothing, while the guest waited out a reconnect window and
/// was then told the connection had failed. Both sides now say what is really
/// wrong (docs/adr/0024).
///
/// # Errors
/// Rejects calls from other windows.
#[tauri::command]
pub fn network_status(
    window: Window,
    state: tauri::State<'_, AppState>,
) -> Result<NetworkStatusDto, IpcError> {
    check_window(&window)?;
    let health = state.network.media_health();
    Ok(NetworkStatusDto {
        ready: state.network.online(),
        can_capture: health.can_capture(),
        can_encode: health.can_encode(),
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
    let bytes = state.network.view_frame(&args.peer, args.since_us)?;
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

/// DTO of one chat transcript row.
#[derive(Debug, Clone, Serialize)]
pub struct ChatEntryDto {
    /// `true` when sent by the local user, `false` when received.
    pub outgoing: bool,
    /// Message text (already validated by the core).
    pub text: String,
    /// Local wall-clock time in Unix seconds; display-only (§15).
    pub at_unix: u64,
}

#[derive(Debug, Deserialize)]
pub struct ChatSendArgs {
    /// Pseudonymized label of the session partner.
    pub peer: String,
    /// Text to send, validated again inside the core (§9.2).
    pub text: String,
}

/// Sends one chat message to `peer` and returns the stored entry (§9.2).
///
/// # Errors
/// [`IpcError`] when the window is not allowed or the actor refuses.
#[tauri::command]
pub async fn chat_send(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: ChatSendArgs,
) -> Result<ChatEntryDto, IpcError> {
    check_view_window(&window, &args.peer).or_else(|_| check_window(&window))?;
    let stored = state.network.chat_send(args.peer, args.text).await?;
    Ok(ChatEntryDto {
        outgoing: stored.outgoing,
        text: stored.text,
        at_unix: stored.at_unix,
    })
}

/// Returns the chat transcript with `peer`, oldest first (§9.2).
///
/// # Errors
/// [`IpcError`] when the window is not allowed.
#[tauri::command]
pub async fn chat_transcript(
    window: Window,
    state: tauri::State<'_, AppState>,
    peer: String,
) -> Result<Vec<ChatEntryDto>, IpcError> {
    check_view_window(&window, &peer).or_else(|_| check_window(&window))?;
    let rows = state.network.chat_transcript(peer).await?;
    Ok(rows
        .into_iter()
        .map(|e| ChatEntryDto {
            outgoing: e.outgoing,
            text: e.text,
            at_unix: e.at_unix,
        })
        .collect())
}

#[derive(Debug, Deserialize)]
pub struct ClipboardPushArgs {
    /// Pseudonymized label of the session partner.
    pub peer: String,
    /// UTF-8 text to sync, bounded by §9.2 inside the actor/core.
    pub text: String,
}

/// Pushes the local clipboard text to `peer` (grant-gated, §8.2).
///
/// # Errors
/// [`IpcError`] when unallowed or refused for lack of a grant.
#[tauri::command]
pub async fn clipboard_push(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: ClipboardPushArgs,
) -> Result<(), IpcError> {
    check_view_window(&window, &args.peer).or_else(|_| check_window(&window))?;
    state.network.clipboard_push(args.peer, args.text).await?;
    Ok(())
}

/// Pulls the newest inbound clipboard payload from `peer`, if any (§9.2).
///
/// Pull semantics keep payloads off any broadcast surface (§15).
///
/// # Errors
/// [`IpcError`] when the window is not allowed.
#[tauri::command]
pub async fn clipboard_pull(
    window: Window,
    state: tauri::State<'_, AppState>,
    peer: String,
) -> Result<Option<String>, IpcError> {
    check_view_window(&window, &peer).or_else(|_| check_window(&window))?;
    Ok(state.network.clipboard_pull(peer).await?)
}

#[derive(Debug, Deserialize)]
pub struct AudioToggleArgs {
    /// Pseudonymized label of the guest session to stream audio to.
    pub peer: String,
    /// `true` starts the stream (§11 `AudioStart`), `false` stops it.
    pub on: bool,
}

/// Host side: turns the desktop-audio stream to `peer` on or off (§11).
///
/// The decision lives in the actor: a live granted session is required there,
/// exactly like every other media surface. Host-side only, so only the main
/// window may call it — a guest's view window has nothing to toggle.
///
/// # Errors
/// [`IpcError`] when unallowed or refused for lack of a grant.
#[tauri::command]
pub async fn audio_toggle(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: AudioToggleArgs,
) -> Result<(), IpcError> {
    check_window(&window)?;
    state.network.audio_toggle(args.peer, args.on).await?;
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct RecordToggleArgs {
    /// Pseudonymized label of the session to record.
    pub peer: String,
    /// Destination `.lmrc` path chosen by the host user; `None` stops.
    #[serde(default)]
    pub path: Option<String>,
}

/// Host side: starts or stops the session recording of `peer` (§17).
///
/// The `recording` grant is checked inside the actor (§8.2); this command
/// only carries the host user's choice and the destination path.
///
/// # Errors
/// [`IpcError`] when unallowed or refused for lack of the grant.
#[tauri::command]
pub async fn recording_toggle(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: RecordToggleArgs,
) -> Result<(), IpcError> {
    check_window(&window)?;
    state.network.record_toggle(args.peer, args.path).await?;
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct MicToggleArgs {
    /// Pseudonymized label of the host being watched.
    pub peer: String,
    /// `true` starts the guest's microphone stream (§11; ADR 0028), `false`
    /// stops it.
    pub on: bool,
}

/// Guest side: turns the view window's own microphone towards the watched
/// host on or off (§11; ADR 0028).
///
/// View-window-only by construction — the toolbar lives there — and gated
/// inside the actor on a live session with a live `input` grant: a guest
/// whose role was lowered mid-flight cannot open a new mic stream, and an
/// already-open one is stopped by the actor on the same check.
///
/// # Errors
/// [`IpcError`] when unallowed or refused for lack of the grant.
#[tauri::command]
pub async fn mic_toggle(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: MicToggleArgs,
) -> Result<(), IpcError> {
    check_view_window(&window, &args.peer)?;
    state.network.mic_toggle(args.peer, args.on).await?;
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct SasRequestArgs {
    /// Pseudonymized label of the host being watched.
    pub peer: String,
}

/// Guest side: asks the watched host to deliver Ctrl+Alt+Del to its user
/// (§11; ADR 0028).
///
/// The `input` grant is re-checked on both sides — here to refuse early, and
/// on the host authoritatively, per request. The answer arrives on the wire
/// as `SasAck` and is surfaced in the toolbar; this command's `Ok` only says
/// the request went out.
///
/// # Errors
/// [`IpcError`] when unallowed or refused for lack of the grant.
#[tauri::command]
pub async fn sas_request(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: SasRequestArgs,
) -> Result<(), IpcError> {
    check_view_window(&window, &args.peer)?;
    state.network.sas_request(args.peer).await?;
    Ok(())
}

/// Whether this platform can attempt to deliver the Secure Attention
/// Sequence at all (§11; ADR 0028). The toolbar grays the button out on a
/// `false` instead of letting someone press it into a dead end.
#[tauri::command]
pub fn sas_available() -> bool {
    lumepeer_media::sas::sas_available()
}

#[derive(Debug, Deserialize)]
pub struct MonitorSelectArgs {
    /// Pseudonymized label of the host being watched.
    pub peer: String,
    /// Monitor id as announced in `MonitorsList` (§11).
    pub monitor_id: u32,
}

/// Guest side: picks which of the host's monitors to watch (§11; ADR 0028).
///
/// The host re-checks the `view` grant and the id's range; the next picture
/// this window receives simply shows the new monitor.
///
/// # Errors
/// [`IpcError`] when unallowed or the id names no announced monitor.
#[tauri::command]
pub async fn monitor_select(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: MonitorSelectArgs,
) -> Result<(), IpcError> {
    check_view_window(&window, &args.peer)?;
    state
        .network
        .monitor_select(args.peer, args.monitor_id)
        .await?;
    Ok(())
}

/// DTO of one monitor of the watched host (§11 `MonitorsList`).
#[derive(Debug, Clone, Serialize)]
pub struct MonitorDto {
    /// Host-assigned stable id a `monitor_select` call passes back.
    pub id: u32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Whether this is the host's primary display.
    pub primary: bool,
}

/// Guest side: asks the watched host to announce its monitors and returns
/// the list (§11; ADR 0028).
///
/// # Errors
/// [`IpcError`] when unallowed or the actor refuses.
#[tauri::command]
pub async fn monitors_list(
    window: Window,
    state: tauri::State<'_, AppState>,
    peer: String,
) -> Result<Vec<MonitorDto>, IpcError> {
    check_view_window(&window, &peer)?;
    let monitors = state.network.monitors_list(peer).await?;
    Ok(monitors
        .into_iter()
        .map(|monitor| MonitorDto {
            id: monitor.id,
            width: monitor.width,
            height: monitor.height,
            primary: monitor.primary,
        })
        .collect())
}
