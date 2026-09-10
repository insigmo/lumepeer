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

use lumepeer_core::consent::IndependentGrant;
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

    /// The far side runs a protocol minor without the message this needs.
    ///
    /// A distinct code rather than a refusal: nothing is denied and nothing
    /// is broken — the other device is simply older, and the UI should say so
    /// rather than implying the host said no (§9.1, §18).
    fn unsupported() -> Self {
        Self {
            code: "PEER_TOO_OLD",
            message: "the other device runs a version without this feature".to_owned(),
        }
    }

    /// The volume the file was going to has no room for it (ADR 0077).
    ///
    /// Its own code because it is the one refusal here the person reading it
    /// can act on: free some space and try again. Answered before the first
    /// byte moves rather than after most of them have (§18).
    fn no_space() -> Self {
        Self {
            code: "NO_SPACE",
            message: "there is not enough free space where that file was going".to_owned(),
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

    /// An unattended-access setting the host tried to make and could not.
    ///
    /// Detailed on purpose, unlike the wire rejection a *guest* gets: this is
    /// the host's own settings screen being told why its own change was
    /// refused, and nothing here describes a login attempt (§18; ADR 0033).
    fn unattended(error: &lumepeer_core::unattended::UnattendedError) -> Self {
        Self {
            code: "UNATTENDED",
            message: error.to_string(),
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
            ActorError::Unattended(e) => Self::unattended(&e),
            ActorError::ChannelClosed => Self::poisoned(),
            ActorError::Unsupported => Self::unsupported(),
            ActorError::NoSpace => Self::no_space(),
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

/// Rejects calls that come from neither the main window nor the host's
/// always-on-top session bar.
///
/// The bar is the same person at the same machine as the main window — it
/// exists so the host keeps its own controls while that window is minimized
/// — so the two surfaces share exactly the commands the bar needs and no
/// others. Which ones those are is enumerated in `capabilities/hostbar.json`;
/// this is the Rust half of the same rule (§2.3, §13).
fn check_host_surface(window: &Window) -> Result<(), IpcError> {
    if matches!(
        window.label(),
        MAIN_WINDOW_LABEL | crate::view::HOST_BAR_LABEL
    ) {
        Ok(())
    } else {
        Err(IpcError::denied())
    }
}

/// Rejects calls that do not come from the host's session bar.
fn check_host_bar(window: &Window) -> Result<(), IpcError> {
    if window.label() == crate::view::HOST_BAR_LABEL {
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
    /// Retire every code handed out so far and issue a fresh one (ADR 0062).
    ///
    /// Absent or `false` asks for the code that is already live, which is what
    /// the sidebar wants: a code that changes every time the host looks at it
    /// is not a code anyone can save. The settings window's "reissue" control
    /// is the one that sends `true`.
    #[serde(default)]
    pub renew: bool,
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
#[allow(
    clippy::struct_excessive_bools,
    reason = "mirrors `Grants`: §2.2 requires these permissions to stay               independent flags, and folding them here would hide that"
)]
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
    /// Whether the guest may read this host's clipboard (§8.2).
    pub clipboard_read: bool,
    /// Whether the guest may write this host's clipboard (§8.2).
    pub clipboard_write: bool,
    /// Whether the guest may exchange files over `rd/file/1` (§8.2).
    pub file_transfer: bool,
    /// Whether this session may be recorded (§8.2).
    pub recording: bool,
    /// Whether the guest may switch this host's own physical display mode
    /// (§8.2; docs/bugs/16-host-display-mode.md; ADR 0048).
    pub display_mode: bool,
    /// Whether a recording of this session is being written right now (§17).
    ///
    /// Distinct from `recording` above, which is only permission: the
    /// indicator both sides must show while capture is happening (§2.2) hangs
    /// off this one.
    pub recording_active: bool,
    /// Whether this guest asked to be recorded and is still waiting for the
    /// host user's answer (§17).
    pub record_request: bool,
    /// Whether this guest may see the host's secure desktop (UAC prompt,
    /// lock screen, fast user switch) instead of the honest "can't see this"
    /// message (ADR 0049). Independent of every other grant, `input`
    /// included, and off by default.
    pub secure_desktop: bool,
    /// Whether this guest may inject input into the host's secure desktop —
    /// click the UAC prompt, type into the lock screen (ADR 0057). The most
    /// consequential grant: deny-by-default and derived from no role, not even
    /// full control, so it is always the host toggling this switch by hand.
    pub secure_desktop_input: bool,
    /// Whether this guest is, right now, actually seeing it. Distinct from
    /// `secure_desktop` above the same way `recording_active` is distinct
    /// from `recording`: the host's non-removable indicator hangs off this
    /// one (ADR 0049).
    pub secure_desktop_active: bool,
    /// Whether this guest may forward TCP connections into the host's own
    /// network (§8.2; ADR 0078).
    ///
    /// Permission only, and half a decision: the addresses it may reach are a
    /// separate list the host writes one entry at a time, and this flag
    /// without an entry on it reaches nothing.
    pub tunnel: bool,
    /// Whether this guest may start a shell on this host (§8.2; ADR 0079).
    ///
    /// Carried by full control alone and separately revocable, like every
    /// other flag here. `input` does not imply it: a host that handed over
    /// the keyboard can watch what happens on its own screen, and a shell is
    /// not on the screen.
    pub terminal: bool,
    /// Whether this guest has a shell running right now. Distinct from
    /// `terminal` above the same way `recording_active` is distinct from
    /// `recording`: the indicator the host cannot switch off hangs off this
    /// one (ADR 0079).
    pub terminal_active: bool,
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
    /// Unix seconds this row was last written — a connect or a disconnect,
    /// whichever happened most recently (docs/bugs/03-connection-list.md,
    /// task 4).
    pub last_seen_at: u64,
    /// Whether a device password is remembered for this host (§8; ADR 0033).
    ///
    /// A boolean, never the password: the webview may know that reconnecting
    /// will log itself in, so it can offer to stop that, and nothing more.
    pub has_password: bool,
}

/// Argument of [`history_connect`].
#[derive(Debug, Clone, Deserialize)]
pub struct HistoryConnectArgs {
    /// Label of the remembered host, as `connection_history` handed it out.
    pub peer: String,
}

/// Argument of [`history_remove`].
#[derive(Debug, Clone, Deserialize)]
pub struct HistoryRemoveArgs {
    /// Label of the remembered host, as `connection_history` handed it out.
    pub peer: String,
}

/// Argument of [`history_forget_password`].
#[derive(Debug, Clone, Deserialize)]
pub struct HistoryForgetPasswordArgs {
    /// Label of the remembered host, as `connection_history` handed it out.
    pub peer: String,
}

/// Guest-side state of this node's own outgoing connect attempt (§21
/// punch-list item 6).
#[derive(Debug, Clone, Serialize)]
pub struct ConnectStatusDto {
    /// One of `idle`, `dialing`, `awaiting_consent`, `awaiting_credentials`,
    /// `connected`, `denied`, `failed`.
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
    /// Whether the host's credential challenge asked for a one-time code, so
    /// the form knows whether to show the field (§8; ADR 0033).
    ///
    /// Only meaningful alongside `phase: "awaiting_credentials"`.
    pub code_required: bool,
    /// Seconds to wait before another attempt, after a lockout (§18).
    ///
    /// The host's own number, passed through unchanged: this side does not
    /// count it down and must not pretend to know better than the host that
    /// is enforcing it.
    pub retry_secs: Option<u64>,
    /// Whether the credential attempt in flight was started automatically
    /// from a remembered password (docs/bugs/02-connect-form.md, task 6). The
    /// form uses this to stay on its status line rather than popping the
    /// credentials modal open for a host it already knows the password to.
    pub credentials_auto: bool,
}

/// One saved device of the host's address book (§8; ADR 0034).
#[derive(Debug, Clone, Serialize)]
pub struct AddressBookEntryDto {
    /// Pseudonymized peer label; never a raw `NodeId` (§15). Also the handle
    /// every other address-book command names this device by.
    pub peer_label: String,
    /// Name the host user gave this device. Free text: rendered through
    /// `lit-html`'s escaping, never assembled into HTML by hand.
    pub name: String,
    /// Grouping tags the host user typed.
    pub tags: Vec<String>,
    /// Free-text note.
    pub notes: String,
    /// Whether this device may attempt an unattended login (§8).
    pub trusted: bool,
    /// Whether it is connected right now.
    pub connected: bool,
}

/// Argument of [`address_book_upsert`].
#[derive(Debug, Clone, Deserialize)]
pub struct AddressBookUpsertArgs {
    /// Pseudonymized label of the device, from `session_status` or
    /// `address_book_list`.
    pub peer: String,
    /// Name to show it under.
    pub name: String,
    /// Grouping tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Free-text note.
    #[serde(default)]
    pub notes: String,
}

/// Argument of [`address_book_remove`].
#[derive(Debug, Clone, Deserialize)]
pub struct AddressBookRemoveArgs {
    /// Pseudonymized label of the device to forget.
    pub peer: String,
}

/// Argument of [`address_book_set_trusted`].
#[derive(Debug, Clone, Deserialize)]
pub struct AddressBookSetTrustedArgs {
    /// Pseudonymized label of the device.
    pub peer: String,
    /// Whether it may attempt an unattended login from now on.
    pub trusted: bool,
}

/// What the host's settings screen may know about unattended access (§8).
///
/// There is no field here for the password, its hash or the TOTP secret, and
/// that is the point: the webview cannot be handed what the type cannot carry
/// (§2.3, §13; ADR 0033).
#[derive(Debug, Clone, Serialize)]
pub struct UnattendedStatusDto {
    /// Whether a device password is set.
    pub enabled: bool,
    /// Whether a second factor is required as well.
    pub totp_enabled: bool,
    /// Role a successful unattended login is granted.
    pub role: RoleDto,
}

/// Argument of [`unattended_set_password`].
#[derive(Debug, Clone, Deserialize)]
pub struct UnattendedSetPasswordArgs {
    /// The new device password, in the clear from the field the host typed it
    /// into. It is hashed in `lumepeer-core` and never stored, logged or
    /// returned in any form.
    pub password: String,
}

/// Argument of [`unattended_set_totp`].
#[derive(Debug, Clone, Deserialize)]
pub struct UnattendedSetTotpArgs {
    /// Whether the second factor should be on.
    pub enabled: bool,
}

/// Argument of [`unattended_set_role`].
#[derive(Debug, Clone, Deserialize)]
pub struct UnattendedSetRoleArgs {
    /// Role a successful unattended login should be granted.
    pub role: RoleDto,
}

/// The one-time provisioning payload for an authenticator app (§8).
#[derive(Debug, Clone, Serialize)]
pub struct TotpProvisioningDto {
    /// The shared secret in base32, for typing in by hand.
    pub secret_base32: String,
    /// The same secret as an `otpauth://` URI, for a QR code.
    pub uri: String,
}

/// Argument of [`unattended_submit`].
#[derive(Debug, Clone, Deserialize)]
pub struct UnattendedSubmitArgs {
    /// The device password for the host being connected to.
    pub password: String,
    /// The one-time code, when the host asked for one.
    #[serde(default)]
    pub code: Option<String>,
    /// Whether to remember this password for this host in the OS keystore
    /// (docs/bugs/02-connect-form.md, task 6; docs/bugs/DECISIONS.md D2).
    #[serde(default)]
    pub remember: bool,
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

/// What one live connection's link actually looks like (§18; ADR 0026,
/// ADR 0037).
///
/// The panel ADR 0026 was written about: the product could not say *why* a
/// session was bad, so a user could only guess. Every field here is measured
/// on this machine, and one that nothing has measured yet is `null` rather
/// than a zero pretending to be a reading.
#[derive(Debug, Clone, Serialize)]
pub struct ConnectionStatsDto {
    /// Pseudonymized peer label; never a raw `NodeId` (§15).
    pub peer_label: String,
    /// Smoothed control-channel round trip, in milliseconds.
    pub rtt_ms: Option<u32>,
    /// Share of frames the receiving side could not turn into a picture, in
    /// permille.
    pub loss_permille: Option<u16>,
    /// Media throughput the receiving side observed, in kilobits per second.
    pub goodput_kbps: Option<u32>,
    /// `direct`, `relay`, `mixed` or `unknown`, from iroh's own open paths —
    /// what is happening, not what the settings asked for.
    pub path: &'static str,
    /// Region of the relay in use, when one is.
    ///
    /// The leading label of its hostname and nothing more. A relay address is
    /// a fact about the *host's* network, and §15 keeps that class of detail
    /// off a screen the host does not control: what a person needs is "through
    /// a relay, roughly there", never an address.
    pub relay_region: Option<String>,
    /// Encoder bitrate this machine is sending at; `null` when it is the one
    /// watching rather than the one sending.
    pub bitrate_kbps: Option<u32>,
    /// Frame rate this machine is sending at; `null` on the watching side.
    pub fps: Option<u8>,
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
/// Callable from the host's own two surfaces — the main window and the
/// always-on-top session bar — for any peer, and from a `view-{peer}` window
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
    if check_host_surface(&window).is_err() {
        check_view_window(&window, &args.peer)?;
    }
    state.network.revoke(args.peer).await?;
    Ok(())
}

/// Lists pending and active sessions for the status/consent UI, and for the
/// session bar that shows the same list while the main window is away.
///
/// # Errors
/// Rejects calls from other windows; [`IpcError`] if the actor is gone.
#[tauri::command]
pub async fn session_status(
    window: Window,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<SessionStatusDto>, IpcError> {
    check_host_surface(&window)?;
    let snapshot = state.network.status().await?;
    Ok(snapshot
        .into_iter()
        .map(|s| SessionStatusDto {
            peer_label: s.label,
            state: s.state.into(),
            role: s.role.into(),
            input: s.input,
            clipboard_read: s.grants.clipboard_read,
            clipboard_write: s.grants.clipboard_write,
            file_transfer: s.grants.file_transfer,
            recording: s.grants.recording,
            display_mode: s.grants.display_mode,
            recording_active: s.recording_active,
            record_request: s.record_request,
            secure_desktop: s.grants.secure_desktop,
            secure_desktop_input: s.grants.secure_desktop_input,
            secure_desktop_active: s.secure_desktop_active,
            tunnel: s.grants.tunnel,
            terminal: s.grants.terminal,
            terminal_active: s.terminal_active,
        })
        .collect())
}

/// Argument of [`session_set_grant`].
#[derive(Debug, Clone, Deserialize)]
pub struct SessionSetGrantArgs {
    /// Pseudonymized label of the guest whose session is changing.
    pub peer: String,
    /// Which independent grant moves (§8.2).
    pub grant: IndependentGrant,
    /// Its new state.
    pub allowed: bool,
}

/// Host side: turns one independent grant of a running session on or off
/// (§8.2; ADR 0029).
///
/// Main window only, and deliberately not in `capabilities/view.json`: a view
/// window belongs to a guest's side of a session, and a guest granting itself
/// the clipboard would be the whole authorization model inverted (§2.3). The
/// core refuses anything but an active session and cannot reach `view` or
/// `input` through this path at all.
///
/// # Errors
/// Rejects calls from any other window; propagates [`ActorError`] as an
/// [`IpcError`].
#[tauri::command]
pub async fn session_set_grant(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: SessionSetGrantArgs,
) -> Result<(), IpcError> {
    check_window(&window)?;
    state
        .network
        .set_grant(args.peer, args.grant, args.allowed)
        .await?;
    Ok(())
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
            last_seen_at: e.last_seen_at,
            has_password: e.has_password,
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

/// Forgets a remembered host (docs/bugs/03-connection-list.md, task 5).
///
/// # Errors
/// Rejects calls from other windows; [`IpcError`] if the actor is gone.
/// Removing a label that was never remembered is not an error.
#[tauri::command]
pub async fn history_remove(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: HistoryRemoveArgs,
) -> Result<(), IpcError> {
    check_window(&window)?;
    state.network.history_remove(args.peer).await?;
    Ok(())
}

/// Forgets the device password remembered for one host, keeping its history
/// row (§8; ADR 0033).
///
/// Deleting the row forgets the password too, but a host worth keeping in the
/// list is not necessarily one worth logging into unasked, and until this
/// existed there was no way to say so short of deleting the row.
///
/// # Errors
/// Rejects calls from other windows; [`IpcError`] if the actor is gone. A
/// label that never had a password is not an error.
#[tauri::command]
pub async fn history_forget_password(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: HistoryForgetPasswordArgs,
) -> Result<(), IpcError> {
    check_window(&window)?;
    state.network.history_forget_password(args.peer).await?;
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
    let snapshot = state.network.connect_state().await?;
    Ok(ConnectStatusDto {
        phase: snapshot.phase.as_str(),
        pending: snapshot.phase.is_pending(),
        code: snapshot.code,
        code_required: snapshot.code_required,
        retry_secs: snapshot.retry_secs,
        credentials_auto: snapshot.credentials_auto,
    })
}

/// Abandons this node's own outgoing connect attempt, whatever stage it is
/// at (docs/bugs/02-connect-form.md, task 3). Always the one attempt this
/// node has in flight, so there is nothing to name.
///
/// # Errors
/// Rejects calls from other windows; [`IpcError`] if the actor is gone.
#[tauri::command]
pub async fn connect_cancel(
    window: Window,
    state: tauri::State<'_, AppState>,
) -> Result<(), IpcError> {
    check_window(&window)?;
    state.network.connect_cancel().await?;
    Ok(())
}

/// What one [`report_decoder_codecs`] call carries.
#[derive(Debug, Clone, Deserialize)]
pub struct DecoderCodecsArgs {
    /// `MediaCodec` wire bytes this window's `VideoDecoder` said it supports,
    /// as `view-decoder.ts`'s `supportedOptionalCodecs()` answered.
    pub codecs: Vec<u8>,
}

/// Records which optional video codecs this process's `WebView` can decode,
/// so this node's next `Hello` can advertise them (§11; ADR 0070).
///
/// Called once by the main window at startup. It carries no authorization:
/// the only thing a wrong answer changes is which codec this guest asks a
/// host for, and this guest is the side that would fail to decode it (§2.3).
/// A byte the core has no `MediaCodec` name for is dropped rather than
/// refused, so a webview bundle newer than this binary is not an error.
///
/// # Errors
/// Rejects calls from other windows; [`IpcError`] if the actor is gone.
#[tauri::command]
pub async fn report_decoder_codecs(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: DecoderCodecsArgs,
) -> Result<(), IpcError> {
    check_window(&window)?;
    state.network.report_decoder_codecs(args.codecs).await?;
    Ok(())
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
    let dto = state
        .network
        .invite_create(args.role.into(), args.renew)
        .await?;
    Ok(InviteDto {
        code: dto.code,
        expires_at: dto.expires_at,
    })
}

/// Hands back the invite this host is already living with, or `null`.
///
/// Never issues one: the sidebar calls this on every start so the code is on
/// screen without a click, and a host that has not been asked to invite
/// anybody must not end up handing out a code anyway (ADR 0062).
///
/// # Errors
/// Rejects calls from other windows; [`IpcError`] if the actor is gone.
#[tauri::command]
pub async fn invite_current(
    window: Window,
    state: tauri::State<'_, AppState>,
) -> Result<Option<InviteDto>, IpcError> {
    check_window(&window)?;
    Ok(state.network.invite_current().await?.map(|dto| InviteDto {
        code: dto.code,
        expires_at: dto.expires_at,
    }))
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

/// Reports what every live connection's link actually looks like (§18).
///
/// Read-only and measured: round trip from this session's own `Ping`/`Pong`,
/// path type from iroh's open paths, loss and goodput from whichever side is
/// receiving pictures, and the quality target from the encode loop. Nothing
/// here is a setting, which is the point — ADR 0026 is about a product that
/// could only tell a user what it had intended.
///
/// # Errors
/// Rejects calls from other windows; [`IpcError`] if the actor is gone.
#[tauri::command]
pub async fn connection_stats(
    window: Window,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<ConnectionStatsDto>, IpcError> {
    check_window(&window)?;
    let rows = state.network.connection_stats().await?;
    Ok(rows
        .into_iter()
        .map(|row| ConnectionStatsDto {
            peer_label: row.label,
            rtt_ms: row.rtt_ms,
            loss_permille: row.loss_permille,
            goodput_kbps: row.goodput_kbps,
            path: row.path.code(),
            relay_region: row.relay_region,
            bitrate_kbps: row.bitrate_kbps,
            fps: row.fps,
        })
        .collect())
}

/// Lists the host's saved devices (§8; ADR 0034).
///
/// # Errors
/// Rejects calls from any window but the main one; [`IpcError`] if the actor
/// is gone.
#[tauri::command]
pub async fn address_book_list(
    window: Window,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<AddressBookEntryDto>, IpcError> {
    check_window(&window)?;
    let rows = state.network.address_book_list().await?;
    Ok(rows
        .into_iter()
        .map(|row| AddressBookEntryDto {
            peer_label: row.peer_label,
            name: row.name,
            tags: row.tags,
            notes: row.notes,
            trusted: row.trusted,
            connected: row.connected,
        })
        .collect())
}

/// Saves or updates one saved device (§8; ADR 0034).
///
/// Never changes the trust flag — that is [`address_book_set_trusted`]'s job
/// alone, so editing a name can never widen what a device may do.
///
/// # Errors
/// Rejects calls from any window but the main one; `UNKNOWN_PEER` if the label
/// names nothing this run knows about.
#[tauri::command]
pub async fn address_book_upsert(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: AddressBookUpsertArgs,
) -> Result<(), IpcError> {
    check_window(&window)?;
    state
        .network
        .address_book_upsert(args.peer, args.name, args.tags, args.notes)
        .await?;
    Ok(())
}

/// Forgets one saved device, and any trust it held (§8; ADR 0034).
///
/// # Errors
/// As [`address_book_upsert`].
#[tauri::command]
pub async fn address_book_remove(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: AddressBookRemoveArgs,
) -> Result<(), IpcError> {
    check_window(&window)?;
    state.network.address_book_remove(args.peer).await?;
    Ok(())
}

/// Marks a device trusted, or withdraws that (§8; ADR 0034).
///
/// Trusting a device is what lets it try the unattended device password at
/// all, so this is a widening of the host's own exposure: it is reachable only
/// from the main window, it is never called automatically by a successful
/// connection, and the core logs it as an audit event.
///
/// # Errors
/// As [`address_book_upsert`].
#[tauri::command]
pub async fn address_book_set_trusted(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: AddressBookSetTrustedArgs,
) -> Result<(), IpcError> {
    check_window(&window)?;
    state
        .network
        .address_book_set_trusted(args.peer, args.trusted)
        .await?;
    Ok(())
}

/// Reports whether unattended access is on, and how (§8; ADR 0033).
///
/// # Errors
/// Rejects calls from any window but the main one.
#[tauri::command]
pub async fn unattended_status(
    window: Window,
    state: tauri::State<'_, AppState>,
) -> Result<UnattendedStatusDto, IpcError> {
    check_window(&window)?;
    let settings = state.network.unattended_status().await?;
    Ok(UnattendedStatusDto {
        enabled: settings.enabled,
        totp_enabled: settings.totp_enabled,
        role: settings.role.into(),
    })
}

/// Sets or replaces the device password, turning unattended access on (§8).
///
/// The password travels one way only. It is hashed with Argon2id inside
/// `lumepeer-core`, the hash goes to the OS keystore, and no command hands
/// either back — `unattended_status` has no field that could carry them.
///
/// # Errors
/// `UNATTENDED` if the password fails the policy of §8; `WINDOW_NOT_ALLOWED`
/// from any window but the main one.
#[tauri::command]
pub async fn unattended_set_password(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: UnattendedSetPasswordArgs,
) -> Result<(), IpcError> {
    check_window(&window)?;
    state.network.unattended_set_password(args.password).await?;
    Ok(())
}

/// Turns unattended access off and forgets both factors (§8).
///
/// # Errors
/// Rejects calls from any window but the main one; propagates a keystore
/// failure rather than reporting a success that did not persist.
#[tauri::command]
pub async fn unattended_disable(
    window: Window,
    state: tauri::State<'_, AppState>,
) -> Result<(), IpcError> {
    check_window(&window)?;
    state.network.unattended_disable().await?;
    Ok(())
}

/// Turns the second factor on or off (§8).
///
/// Turning it on returns the provisioning payload **once**: an authenticator
/// app cannot be set up without seeing the secret, and nothing keeps a copy
/// for a second look. Turning it off returns `null`.
///
/// # Errors
/// `UNATTENDED` if no device password is set — a second factor without a
/// first is not a gate; `WINDOW_NOT_ALLOWED` from any other window.
#[tauri::command]
pub async fn unattended_set_totp(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: UnattendedSetTotpArgs,
) -> Result<Option<TotpProvisioningDto>, IpcError> {
    check_window(&window)?;
    let provisioning = state.network.unattended_set_totp(args.enabled).await?;
    Ok(provisioning.map(|p| TotpProvisioningDto {
        secret_base32: p.secret_base32,
        uri: p.uri,
    }))
}

/// Chooses the role a successful unattended login is granted (§8.2).
///
/// Applies to the next admission; a session already running keeps the grants
/// it was given.
///
/// # Errors
/// Rejects calls from any window but the main one.
#[tauri::command]
pub async fn unattended_set_role(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: UnattendedSetRoleArgs,
) -> Result<(), IpcError> {
    check_window(&window)?;
    state.network.unattended_set_role(args.role.into()).await?;
    Ok(())
}

/// Guest side: answers a host's credential challenge (§8; ADR 0033).
///
/// `Ok(())` means the answer was sent, not that it was accepted. The verdict
/// arrives on the wire and shows up in the next `connect_status` poll, which
/// is also where a refusal's §18 code appears.
///
/// # Errors
/// `CORE` if nothing is waiting on a challenge; `WINDOW_NOT_ALLOWED` from any
/// window but the main one — a remote-view window has no business collecting
/// this user's password.
#[tauri::command]
pub async fn unattended_submit(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: UnattendedSubmitArgs,
) -> Result<(), IpcError> {
    check_window(&window)?;
    state
        .network
        .unattended_submit(args.password, args.code, args.remember)
        .await?;
    Ok(())
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
/// The fallback path since ADR 0058, for a `WebView` with no usable
/// `VideoDecoder`: calling this at all is what tells the Rust side to start
/// the sandboxed worker of §11.3 and decode to RGBA. A window that can decode
/// H.264 itself calls [`view_next_chunk`] instead and never pays for the
/// pixels below.
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

/// Argument of [`view_next_chunk`].
#[derive(Debug, Clone, Deserialize)]
pub struct ViewChunkArgs {
    /// Pseudonymized label of the host being watched.
    pub peer: String,
    /// Whether the caller's own decoder has nothing left to decode against
    /// and needs an intra frame — it has just been configured, or it failed.
    #[serde(default)]
    pub need_keyframe: bool,
}

/// Encoded video for a view window that decodes it itself (ADR 0058).
///
/// The counterpart of [`view_next_frame`], and the reason that one is now the
/// fallback rather than the path: this hands the window the H.264 bitstream —
/// tens of kilobytes — instead of a decoded RGBA picture, which at 1080p is
/// 8 MiB and cost over a hundred milliseconds per frame to push through
/// `WebView2`'s IPC before anything could be drawn. The window decodes it with
/// `VideoDecoder`, in the `WebView`'s own sandboxed renderer and on the same
/// hardware decoder the sandboxed worker process would have used.
///
/// The call is held open until a frame exists rather than answered
/// immediately, so a window is never polling and a frame is never waiting: it
/// returns with everything that arrived since the previous call, in order,
/// and with an empty body at most every
/// `BITSTREAM_POLL_TIMEOUT_MS` so `status` and the grant flags stay live on a
/// still screen.
///
/// Layout, little endian: `status:u8 | flags:u8 | count:u16 | reserved:u32`,
/// then `count` frames of `keyframe:u8 | timestamp_us:u64 | length:u32 |
/// bitstream`. `flags` bit 2 says the stream was broken since the previous
/// call and the window must reset its decoder.
///
/// # Errors
/// Rejects calls from anything but this peer's own view window; [`IpcError`]
/// if no such view exists or the actor is gone.
#[tauri::command]
pub async fn view_next_chunk(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: ViewChunkArgs,
) -> Result<tauri::ipc::Response, IpcError> {
    check_view_window(&window, &args.peer)?;
    let bytes = state
        .network
        .view_chunk(&args.peer, args.need_keyframe)
        .await?;
    Ok(tauri::ipc::Response::new(bytes))
}

/// Argument of [`view_cursor`].
#[derive(Debug, Clone, Deserialize)]
pub struct ViewCursorArgs {
    /// Pseudonymized label of the host being watched.
    pub peer: String,
    /// Sequence number of the shape this window already has, or 0 for none.
    pub since_seq: u32,
}

/// The host's cursor for a view window, as raw bytes (§11).
///
/// Binary for the same reason `view_next_frame` is, and polled rather than
/// pushed for a different one: a cursor changes when a pointer crosses a text
/// field, not thirty times a second, so the window asks with the sequence
/// number it already has and gets pixels back only when the host has since
/// announced a different shape. Layout, little endian:
/// `seq:u32 | width:u16 | height:u16 | hotspot_x:u16 | hotspot_y:u16 | BGRA8`.
///
/// A `seq` of 0 means this host has announced no cursor at all — it is still
/// drawing one into the picture — and the window must not draw a second.
///
/// # Errors
/// Rejects calls from anything but this peer's own view window; [`IpcError`]
/// if no such view exists.
#[tauri::command]
pub async fn view_cursor(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: ViewCursorArgs,
) -> Result<tauri::ipc::Response, IpcError> {
    check_view_window(&window, &args.peer)?;
    let bytes = state.network.view_cursor(&args.peer, args.since_seq)?;
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
pub struct FileAcceptArgs {
    /// Pseudonymized label of the session partner.
    pub peer: String,
    /// Whether to take the file.
    pub accept: bool,
    /// Whether the offer being answered came from the peer's clipboard
    /// (docs/bugs/14-clipboard-files.md #3), as the panel showed it.
    ///
    /// Only ever changes which picker this command runs, never what the
    /// actor authorizes: a clipboard-sourced offer always lands in this
    /// node's own clipboard-receive directory regardless of what this field
    /// says, and a lie here in either direction costs at most an
    /// unnecessary dialog or a clean refusal, never a widened grant.
    #[serde(default)]
    pub from_clipboard: bool,
}

/// Answers the oldest offer `peer` made (§9.2; docs/bugs/
/// 14-clipboard-files.md #3).
///
/// On an acceptance of an ordinary offer, the OS directory picker runs here,
/// for the same reason the file picker does: the *receiving* user chooses
/// where the file lands, and the sender only ever chose a name, normalized
/// to a basename before it is ever joined to the chosen directory. A
/// clipboard-sourced offer skips the picker entirely — it always lands in
/// this node's own clipboard-receive directory, so a paste has something to
/// point at.
///
/// # Errors
/// [`IpcError`] when the window is not allowed, there is no offer to answer,
/// or the grant is gone.
#[tauri::command]
pub async fn file_accept(
    app: tauri::AppHandle,
    window: Window,
    state: tauri::State<'_, AppState>,
    args: FileAcceptArgs,
) -> Result<(), IpcError> {
    check_view_window(&window, &args.peer).or_else(|_| check_window(&window))?;
    let directory = if args.accept && !args.from_clipboard {
        let Some(directory) = pick_directory(&app).await else {
            // The picker was dismissed: nothing has been answered yet, so the
            // offer is still there to accept or decline. Saying "declined"
            // here would answer for the user.
            return Ok(());
        };
        Some(directory)
    } else {
        None
    };
    state
        .network
        .file_accept(args.peer, args.accept, directory)
        .await?;
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct FileAbortArgs {
    /// Pseudonymized label of the session partner.
    pub peer: String,
    /// Transfer to stop, as `file_transfers` reported it.
    pub transfer_id: u64,
}

/// Stops one running transfer (§9.2).
///
/// # Errors
/// [`IpcError`] when the window is not allowed or no such transfer exists.
#[tauri::command]
pub async fn file_abort(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: FileAbortArgs,
) -> Result<(), IpcError> {
    check_view_window(&window, &args.peer).or_else(|_| check_window(&window))?;
    state
        .network
        .file_abort(args.peer, args.transfer_id)
        .await?;
    Ok(())
}

/// Every offer waiting for an answer and every transfer in flight.
///
/// # Errors
/// [`IpcError`] when the actor is gone.
#[tauri::command]
pub async fn file_transfers(
    window: Window,
    state: tauri::State<'_, AppState>,
) -> Result<crate::network::FileTransfersDto, IpcError> {
    // Readable from either window: a guest watching its own transfer list is
    // reading its own side of the session, and every row it can see is one it
    // is already a party to.
    if check_window(&window).is_err() && !window.label().starts_with(VIEW_WINDOW_PREFIX) {
        return Err(IpcError::denied());
    }
    Ok(state.network.file_transfers().await?)
}

// -------------------------------------------------------------------------
// File manager (§9.2; ADR 0075, ADR 0076)
// -------------------------------------------------------------------------

/// One entry of either pane of the file manager.
///
/// The same four fields on both sides on purpose: the local pane shows what
/// the remote pane would show of the same directory, so a person comparing
/// them is comparing like with like (ADR 0075 fixes the set — no attributes,
/// no owner, no permissions).
#[derive(Debug, Clone, Serialize)]
pub struct DirEntryDto {
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
    pub modified_unix: u64,
}

impl From<lumepeer_core::protocol::DirEntry> for DirEntryDto {
    fn from(entry: lumepeer_core::protocol::DirEntry) -> Self {
        Self {
            name: entry.name,
            size: entry.size,
            is_dir: entry.is_dir,
            modified_unix: entry.modified_unix,
        }
    }
}

/// Why a directory listing carries no entries (§18; ADR 0075).
///
/// A code and not a sentence: the window owns the wording, in the user's own
/// language, and the three cases need three different sentences — "ask the
/// host for permission", "that is not a path", "that directory would not
/// open" — rather than one "nothing found" for all of them.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DirRefusalDto {
    NotGranted,
    BadPath,
    Unreadable,
}

impl From<lumepeer_core::protocol::DirListRefusal> for DirRefusalDto {
    fn from(refusal: lumepeer_core::protocol::DirListRefusal) -> Self {
        use lumepeer_core::protocol::DirListRefusal;
        match refusal {
            DirListRefusal::NotGranted => Self::NotGranted,
            DirListRefusal::BadPath => Self::BadPath,
            DirListRefusal::Unreadable => Self::Unreadable,
        }
    }
}

/// Why the host will not send a file this window asked for (§18; ADR 0076).
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FetchRefusalDto {
    NotGranted,
    BadPath,
    Unreadable,
    TooMany,
}

impl From<lumepeer_core::protocol::FileFetchRefusal> for FetchRefusalDto {
    fn from(refusal: lumepeer_core::protocol::FileFetchRefusal) -> Self {
        use lumepeer_core::protocol::FileFetchRefusal;
        match refusal {
            FileFetchRefusal::NotGranted => Self::NotGranted,
            FileFetchRefusal::BadPath => Self::BadPath,
            FileFetchRefusal::Unreadable => Self::Unreadable,
            FileFetchRefusal::TooMany => Self::TooMany,
        }
    }
}

/// One directory of this machine, for the local pane.
#[derive(Debug, Clone, Serialize)]
pub struct LocalDirDto {
    /// The directory this listing is about, as it was resolved.
    pub path: String,
    /// The directory above it, or `None` at a root — what the "up" button
    /// uses, so no path arithmetic happens in the webview.
    pub parent: Option<String>,
    pub entries: Vec<DirEntryDto>,
    /// Whether the listing stopped at the same entry bound the wire uses.
    pub truncated: bool,
}

/// The remote pane's whole state in one poll (ADR 0075, ADR 0076).
#[derive(Debug, Clone, Serialize)]
pub struct RemoteDirDto {
    /// The directory the last answer was about, empty before the first one.
    pub path: String,
    /// The directory above it, or `None` at a root.
    pub parent: Option<String>,
    pub entries: Vec<DirEntryDto>,
    pub truncated: bool,
    /// Why `entries` is empty, when it is empty for a reason.
    pub refused: Option<DirRefusalDto>,
    /// Why the last download was refused, if one was.
    pub fetch_refused: Option<FetchRefusalDto>,
    /// Whether an answer has arrived at all yet.
    pub answered: bool,
}

/// The directory above `path`, or `None` when `path` is already a root.
///
/// Its own string arithmetic rather than `std::path::Path::parent`, for the
/// same reason `lumepeer_core::remote_path` parses paths itself: the remote
/// pane's paths belong to an operating system that need not be this one, so
/// `/` and `\` are both separators here whatever this build was compiled for.
fn parent_of(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches(['/', '\\']);
    // A Unix root, or a Windows drive root: nothing above either.
    if trimmed.is_empty() || (trimmed.len() == 2 && trimmed.ends_with(':')) {
        return None;
    }
    let cut = trimmed.rfind(['/', '\\'])?;
    let head = &trimmed[..cut];
    if head.is_empty() {
        // `/a` sits directly under the Unix root.
        Some("/".to_owned())
    } else if head.len() == 2 && head.ends_with(':') {
        // `C:\a` sits directly under the drive root, which keeps its
        // separator: `C:` on its own is drive-relative, not a directory.
        Some(format!("{head}\\"))
    } else {
        Some(head.to_owned())
    }
}

#[derive(Debug, Deserialize)]
pub struct LocalDirListArgs {
    /// Pseudonymized label of the host being watched, for the window check.
    pub peer: String,
    /// Directory to list; `None` starts at this user's home directory.
    pub path: Option<String>,
}

/// Guest side: one directory of *this* machine, for the local pane of the
/// file manager (ADR 0076).
///
/// The path goes through the same parser a path from the wire goes through.
/// Nothing here arrives from a peer, but the webview is not a source of
/// authority either (§2.3), and one parser for both panes is what keeps a
/// path that the remote side refuses from being browsable on the local side
/// of the same window.
///
/// # Errors
/// [`IpcError`] when the window is not this peer's, when the path is not one
/// this build will resolve, or when the directory will not open.
#[tauri::command]
pub async fn local_dir_list(
    window: Window,
    args: LocalDirListArgs,
) -> Result<LocalDirDto, IpcError> {
    check_view_window(&window, &args.peer)?;
    let path = match args.path {
        Some(path) => path,
        None => crate::config::home()
            .map(|home| home.to_string_lossy().into_owned())
            .ok_or(IpcError {
                code: "NO_HOME_DIR",
                message: "this machine has no home directory".to_owned(),
            })?,
    };
    let resolved = lumepeer_core::remote_path::safe_browse_path(&path).ok_or(IpcError {
        code: "BAD_PATH",
        message: "that is not a directory this build will open".to_owned(),
    })?;
    let (entries, truncated) = crate::network::read_directory(std::path::Path::new(resolved))
        .map_err(|_| IpcError {
            code: "UNREADABLE",
            message: "that directory could not be read".to_owned(),
        })?;
    Ok(LocalDirDto {
        parent: parent_of(resolved),
        path: resolved.to_owned(),
        entries: entries.into_iter().map(DirEntryDto::from).collect(),
        truncated,
    })
}

#[derive(Debug, Deserialize)]
pub struct RemoteDirListArgs {
    /// Pseudonymized label of the host being watched.
    pub peer: String,
    /// Absolute path on the host.
    pub path: String,
}

/// Guest side: asks the watched host to list one of its directories
/// (ADR 0075).
///
/// Nothing is authorized here: the host re-reads its own `file_browse` grant
/// when the request lands, and answers or refuses on its own. The answer is
/// read back with [`remote_dir_status`].
///
/// # Errors
/// [`IpcError`] when the window is not this peer's, the host is too old to
/// understand the message, or the path is not one this build will send.
#[tauri::command]
pub async fn remote_dir_list(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: RemoteDirListArgs,
) -> Result<(), IpcError> {
    check_view_window(&window, &args.peer)?;
    state.network.request_dir_list(args.peer, args.path).await?;
    Ok(())
}

/// Guest side: the remote pane's state — the last listing and the last
/// refused download (ADR 0075, ADR 0076).
///
/// # Errors
/// [`IpcError`] when the window is not this peer's or the session is gone.
#[tauri::command]
pub async fn remote_dir_status(
    window: Window,
    state: tauri::State<'_, AppState>,
    peer: String,
) -> Result<RemoteDirDto, IpcError> {
    check_view_window(&window, &peer)?;
    let status = state.network.dir_listing(peer).await?;
    let fetch_refused = status.fetch_refused.map(FetchRefusalDto::from);
    let Some(listing) = status.listing else {
        return Ok(RemoteDirDto {
            path: String::new(),
            parent: None,
            entries: Vec::new(),
            truncated: false,
            refused: None,
            fetch_refused,
            answered: false,
        });
    };
    Ok(RemoteDirDto {
        parent: parent_of(&listing.path),
        path: listing.path,
        entries: listing.entries.into_iter().map(DirEntryDto::from).collect(),
        truncated: listing.truncated,
        refused: listing.refused.map(DirRefusalDto::from),
        fetch_refused,
        answered: true,
    })
}

#[derive(Debug, Deserialize)]
pub struct RemoteDownloadArgs {
    /// Pseudonymized label of the host being watched.
    pub peer: String,
    /// Absolute path of the file on the host.
    pub path: String,
    /// Directory on this machine the file lands in — the one the local pane
    /// is showing, which is why no picker runs here.
    pub into: String,
}

/// Guest side: asks the watched host for one file (ADR 0076).
///
/// # Errors
/// [`IpcError`] when the window is not this peer's, the host is too old, the
/// path is not one this build will send, or `into` is not a directory on this
/// machine.
#[tauri::command]
pub async fn remote_download(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: RemoteDownloadArgs,
) -> Result<(), IpcError> {
    check_view_window(&window, &args.peer)?;
    state
        .network
        .remote_download(args.peer, args.path, args.into)
        .await?;
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct RemoteUploadArgs {
    /// Pseudonymized label of the host being watched.
    pub peer: String,
    /// Absolute path of the file on this machine, as the local pane named it.
    pub local_path: String,
    /// Absolute path of the directory on the host it is for.
    pub remote_dir: String,
}

/// Guest side: offers one local file to the watched host, for a directory on
/// the host's own disk (ADR 0076).
///
/// # Errors
/// [`IpcError`] when the window is not this peer's, the host is too old, or
/// the destination is not a path this build will send.
#[tauri::command]
pub async fn remote_upload(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: RemoteUploadArgs,
) -> Result<(), IpcError> {
    check_view_window(&window, &args.peer)?;
    state
        .network
        .remote_upload(args.peer, args.local_path, args.remote_dir)
        .await?;
    Ok(())
}

// -------------------------------------------------------------------------
// Tunnel (§4.1; ADR 0078)
// -------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct TunnelOpenArgs {
    /// Pseudonymized label of the host being watched.
    pub peer: String,
    /// Port on **this** machine to listen on. Bound to the loopback only.
    pub local_port: u16,
    /// Address on the host to forward to.
    pub host: String,
    /// TCP port on the host.
    pub port: u16,
}

/// Guest side: forwards a local port to an address on the watched host
/// (§4.1; ADR 0078).
///
/// Authorizes nothing. The host re-reads its own `tunnel` grant and its own
/// address list for every connection that goes through this, and refuses each
/// one on its own; what can fail here is local — a port already taken, an
/// address this build will not parse, a host too old to understand the
/// message.
///
/// # Errors
/// [`IpcError`] when the window is not this peer's, the host is too old, the
/// address is not one, or the local port cannot be bound.
#[tauri::command]
pub async fn tunnel_open(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: TunnelOpenArgs,
) -> Result<(), IpcError> {
    check_view_window(&window, &args.peer)?;
    state
        .network
        .tunnel_open(args.peer, args.local_port, args.host, args.port)
        .await?;
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct TunnelCloseArgs {
    /// Pseudonymized label of the session whose tunnel is to be closed.
    pub peer: String,
}

/// Closes every forwarded connection with `peer`, and the tunnel under them
/// (§4, §8.1; ADR 0078).
///
/// Reachable from the host's own window and from a guest's view window: on
/// the host it is the "close everything" the operator needs while a tunnel is
/// live, and on the guest it is giving back a port it asked for. Neither is a
/// permission — closing is always allowed.
///
/// # Errors
/// [`IpcError`] when the window is not allowed or no session matches.
#[tauri::command]
pub async fn tunnel_close_all(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: TunnelCloseArgs,
) -> Result<(), IpcError> {
    check_view_window(&window, &args.peer).or_else(|_| check_window(&window))?;
    state.network.tunnel_close_all(args.peer).await?;
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct TunnelTargetArgs {
    /// Pseudonymized label of the guest session.
    pub peer: String,
    /// Address the tunnel may reach.
    pub host: String,
    /// TCP port.
    pub port: u16,
    /// `true` adds it to this session's list, `false` takes it back — which
    /// also closes the connections that were using it.
    pub allowed: bool,
}

/// Host side: names one address this session's tunnel may reach, or takes it
/// back (§8.2; ADR 0078).
///
/// The second half of the decision the `tunnel` grant starts. Main window
/// only: this is the host deciding about its own network, and a guest's view
/// window has no business in it.
///
/// # Errors
/// [`IpcError`] when called from another window, no session matches, the
/// address is not one, or the list is full.
#[tauri::command]
pub async fn tunnel_set_target(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: TunnelTargetArgs,
) -> Result<(), IpcError> {
    check_window(&window)?;
    state
        .network
        .tunnel_set_target(args.peer, args.host, args.port, args.allowed)
        .await?;
    Ok(())
}

/// Host side: every tunnel this machine is carrying right now (§15;
/// ADR 0078).
///
/// Main window only, where the session list is: a tunnel shows up on the row
/// of the session carrying it, next to the switch that ends it.
///
/// # Errors
/// [`IpcError`] when the window is not allowed or the actor is gone.
#[tauri::command]
pub async fn tunnel_status(
    window: Window,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<crate::network::TunnelRow>, IpcError> {
    check_window(&window)?;
    Ok(state.network.tunnel_status().await?)
}

// -------------------------------------------------------------------------
// Terminal (§4.1; ADR 0079)
// -------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct TerminalOpenArgs {
    /// Pseudonymized label of the host being watched.
    pub peer: String,
    /// Width of the window asking, in character cells.
    pub cols: u16,
    /// Its height, in character cells.
    pub rows: u16,
}

/// Guest side: asks the watched host to start a shell (§4.1; ADR 0079).
///
/// Authorizes nothing, and names nothing. There is no program in these
/// arguments and no arguments for one: the shell is the host's own, and a
/// guest that could name the executable would hold arbitrary process
/// execution under another name. The host re-reads its `terminal` grant for
/// every one of these and answers with an id or a refusal, which arrives
/// through [`terminal_poll`].
///
/// # Errors
/// [`IpcError`] when the window is not this peer's, the host is too old for
/// the terminal messages, or the geometry is not a window.
#[tauri::command]
pub async fn terminal_open(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: TerminalOpenArgs,
) -> Result<(), IpcError> {
    check_view_window(&window, &args.peer)?;
    state
        .network
        .terminal_open(args.peer, args.cols, args.rows)
        .await?;
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct TerminalInputArgs {
    /// Pseudonymized label of the host being watched.
    pub peer: String,
    /// Which shell, as the host named it.
    pub shell: u32,
    /// What was typed, as bytes: a terminal's input is not text, and a key
    /// that produces an escape sequence produces bytes that are not one.
    pub data: Vec<u8>,
}

/// Guest side: what was typed, on its way to one shell (ADR 0079).
///
/// Never logged, never kept and never written anywhere on the way through —
/// not here, not in the actor, not on the host (§15; ADR 0041).
///
/// # Errors
/// [`IpcError`] when the window is not this peer's, the payload is over
/// `TERMINAL_OUTPUT_MAX_BYTES`, or the channel to that shell is full.
#[tauri::command]
pub async fn terminal_input(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: TerminalInputArgs,
) -> Result<(), IpcError> {
    check_view_window(&window, &args.peer)?;
    state
        .network
        .terminal_input(args.peer, args.shell, args.data)
        .await?;
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct TerminalResizeArgs {
    /// Pseudonymized label of the host being watched.
    pub peer: String,
    /// Which shell, as the host named it.
    pub shell: u32,
    /// New width in character cells.
    pub cols: u16,
    /// New height in character cells.
    pub rows: u16,
}

/// Guest side: tells the host one shell's window changed size (ADR 0079).
///
/// # Errors
/// [`IpcError`] when the window is not this peer's or the geometry is not a
/// window.
#[tauri::command]
pub async fn terminal_resize(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: TerminalResizeArgs,
) -> Result<(), IpcError> {
    check_view_window(&window, &args.peer)?;
    state
        .network
        .terminal_resize(args.peer, args.shell, args.cols, args.rows)
        .await?;
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct TerminalCloseArgs {
    /// Pseudonymized label of the host being watched.
    pub peer: String,
    /// Which shell, as the host named it.
    pub shell: u32,
}

/// Guest side: this window is finished with one shell (ADR 0079).
///
/// The host kills the process and gives the pseudo-terminal back; closing is
/// never a permission, so this only fails when the peer is not one.
///
/// # Errors
/// [`IpcError`] when the window is not this peer's.
#[tauri::command]
pub async fn terminal_close(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: TerminalCloseArgs,
) -> Result<(), IpcError> {
    check_view_window(&window, &args.peer)?;
    state.network.terminal_close(args.peer, args.shell).await?;
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct TerminalPollArgs {
    /// Pseudonymized label of the host being watched.
    pub peer: String,
}

/// Guest side: everything this window's shells have said since it last asked
/// (ADR 0079).
///
/// Binary for the same reason `view_cursor` is, and polled for the same one:
/// a terminal produces bytes rather than text, and a `Vec<u8>` through the
/// JSON side of the IPC boundary is one number per byte. Layout, little
/// endian: `count:u16`, then `count` records of `shell:u32 | event:u8 |
/// length:u32 | payload`. `event` is 0 for output, 1 for a shell that opened
/// under that id, 2 for one that closed, and 3 to 6 for the four refusals of
/// §18 — which name no shell.
///
/// The queue is emptied by the call: nothing arrives twice, and nothing is
/// held once it has been read (§15).
///
/// # Errors
/// [`IpcError`] when the window is not this peer's.
#[tauri::command]
pub async fn terminal_poll(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: TerminalPollArgs,
) -> Result<tauri::ipc::Response, IpcError> {
    check_view_window(&window, &args.peer)?;
    let bytes = state.network.terminal_poll(args.peer).await?;
    Ok(tauri::ipc::Response::new(bytes))
}

/// Runs the OS directory picker, for where a received file should land.
async fn pick_directory(app: &tauri::AppHandle) -> Option<String> {
    use tauri_plugin_dialog::DialogExt as _;
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog().file().pick_folder(move |chosen| {
        let _ = tx.send(chosen);
    });
    rx.await.ok().flatten().and_then(path_string)
}

/// Runs the OS save dialog, for a file this process is about to write.
async fn pick_save_path(app: &tauri::AppHandle, suggested: &str) -> Option<String> {
    use tauri_plugin_dialog::DialogExt as _;
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_file_name(suggested)
        .save_file(move |chosen| {
            let _ = tx.send(chosen);
        });
    rx.await.ok().flatten().and_then(path_string)
}

/// A picked path as a plain string, or `None` when it is not a local path.
///
/// The picker can hand back a content URI on mobile; this build is desktop
/// only, and a URI is not something the transfer engine can open.
fn path_string(path: tauri_plugin_dialog::FilePath) -> Option<String> {
    path.into_path()
        .ok()
        .map(|path| path.to_string_lossy().into_owned())
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
    /// `true` starts the recording, `false` stops it — and, when the guest
    /// had asked, `false` is also how the host declines.
    pub on: bool,
}

/// Host side: starts or stops the session recording of `peer` (§17).
///
/// The `recording` grant is checked inside the actor (§8.2); this command
/// carries the host user's choice and nothing else. Where the file lands is
/// decided in Rust and only reported back: an untrusted view layer does not
/// pick what path this process writes to (§2.3). Main-window only — the host
/// records, so a guest's view window has nothing to start.
///
/// # Errors
/// [`IpcError`] when unallowed or refused for lack of the grant.
#[tauri::command]
pub async fn recording_toggle(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: RecordToggleArgs,
) -> Result<Option<String>, IpcError> {
    check_window(&window)?;
    Ok(state.network.record_toggle(args.peer, args.on).await?)
}

/// Argument of [`record_request`].
#[derive(Debug, Deserialize)]
pub struct RecordRequestArgs {
    /// Pseudonymized label of the host being watched.
    pub peer: String,
}

/// Guest side: asks the host to record the session (§17).
///
/// View-window-only by construction — the toolbar lives there. `Ok` means the
/// request left this node, nothing more: the host user decides, the answer
/// arrives as `RecordAck`, and a refusal is an ordinary answer rather than an
/// error. Nothing on this side can start a recording on the host.
///
/// # Errors
/// [`IpcError`] when called from another window, or without a live view.
#[tauri::command]
pub async fn record_request(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: RecordRequestArgs,
) -> Result<(), IpcError> {
    check_view_window(&window, &args.peer)?;
    state.network.record_request(args.peer).await?;
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

#[derive(Debug, Deserialize)]
pub struct ViewSetScaleArgs {
    /// Pseudonymized label of the host being watched.
    pub peer: String,
    /// Requested ceiling, as a percentage of the host's own captured size
    /// (§11; D7, docs/bugs/13-stream-resolution.md).
    pub scale_percent: u32,
}

/// Guest side: caps the picture at `scale_percent` of the host's own
/// captured size (§11; D7, docs/bugs/13-stream-resolution.md task 3).
///
/// The host re-checks the `view` grant and the range before applying
/// anything; this call only says whether the request could be sent at all.
///
/// # Errors
/// [`IpcError`] when unallowed, the value is outside the guest-selectable
/// range, or the host never confirmed it understands the message.
#[tauri::command]
pub async fn view_set_scale(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: ViewSetScaleArgs,
) -> Result<(), IpcError> {
    check_view_window(&window, &args.peer)?;
    state
        .network
        .set_stream_scale(args.peer, args.scale_percent)
        .await?;
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct ViewSetSizeArgs {
    /// Pseudonymized label of the host being watched.
    pub peer: String,
    /// Width, in this window's own device pixels, the picture will be drawn
    /// at (§11; ADR 0060).
    pub width: u32,
    /// Height, in this window's own device pixels.
    pub height: u32,
}

/// Guest side: names the picture size this window will draw (§11; ADR 0060).
///
/// Distinct from [`view_set_scale`], which names a fraction of the *host's*
/// screen: this names device pixels on *this* screen, which is the only
/// number that can make one frame pixel land on one device pixel. The host
/// fits its capture inside the box and never enlarges it, so a request larger
/// than the host's own screen is how a window says "send me everything you
/// have".
///
/// The host re-checks the `view` grant and the range before applying
/// anything; this call only says whether the request could be sent at all.
///
/// # Errors
/// [`IpcError`] when unallowed, the size is outside the range §9.1 bounds, or
/// the host never confirmed it understands the message.
#[tauri::command]
pub async fn view_set_size(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: ViewSetSizeArgs,
) -> Result<(), IpcError> {
    check_view_window(&window, &args.peer)?;
    state
        .network
        .set_stream_size(args.peer, args.width, args.height)
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

/// DTO of one display mode the watched host's own physical monitor supports
/// (docs/bugs/16-host-display-mode.md #2; ADR 0048).
#[derive(Debug, Clone, Serialize)]
pub struct HostDisplayModeDto {
    /// Host-assigned id a `host_display_set_mode` call passes back.
    pub id: u32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Refresh rate in Hz.
    pub refresh_hz: u32,
}

/// Why the watched host announced no display modes (docs/bugs/
/// 16-host-display-mode.md #2; ADR 0048), for the toolbar to explain rather
/// than showing an empty select.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HostDisplayModeUnavailableReasonDto {
    /// This session does not hold the `display_mode` grant.
    NotGranted,
    /// The host's platform cannot enumerate or change display modes at all.
    PlatformUnsupported,
    /// The host's platform can enumerate but reported nothing for its
    /// currently targeted monitor.
    NoModesReported,
}

impl From<lumepeer_core::protocol::DisplayModeUnavailableReason>
    for HostDisplayModeUnavailableReasonDto
{
    fn from(reason: lumepeer_core::protocol::DisplayModeUnavailableReason) -> Self {
        match reason {
            lumepeer_core::protocol::DisplayModeUnavailableReason::NotGranted => Self::NotGranted,
            lumepeer_core::protocol::DisplayModeUnavailableReason::PlatformUnsupported => {
                Self::PlatformUnsupported
            }
            lumepeer_core::protocol::DisplayModeUnavailableReason::NoModesReported => {
                Self::NoModesReported
            }
        }
    }
}

/// What `host_display_modes` hands back: the list, empty exactly when
/// `reason` explains why (docs/bugs/16-host-display-mode.md #2; ADR 0048).
#[derive(Debug, Clone, Serialize)]
pub struct HostDisplayModesDto {
    pub modes: Vec<HostDisplayModeDto>,
    pub reason: Option<HostDisplayModeUnavailableReasonDto>,
}

/// Guest side: the watched host's own physical display modes, as it last
/// announced them (docs/bugs/16-host-display-mode.md #2; ADR 0048).
///
/// # Errors
/// [`IpcError`] when unallowed or the actor refuses.
#[tauri::command]
pub async fn host_display_modes(
    window: Window,
    state: tauri::State<'_, AppState>,
    peer: String,
) -> Result<HostDisplayModesDto, IpcError> {
    check_view_window(&window, &peer)?;
    let (modes, reason) = state.network.host_display_modes(peer).await?;
    Ok(HostDisplayModesDto {
        modes: modes
            .into_iter()
            .map(|mode| HostDisplayModeDto {
                id: mode.id,
                width: mode.width,
                height: mode.height,
                refresh_hz: mode.refresh_hz,
            })
            .collect(),
        reason: reason.map(HostDisplayModeUnavailableReasonDto::from),
    })
}

/// Argument of [`host_display_set_mode`].
#[derive(Debug, Deserialize)]
pub struct HostDisplaySetModeArgs {
    /// Pseudonymized label of the host being watched.
    pub peer: String,
    /// Mode id as announced by `host_display_modes`.
    pub mode_id: u32,
}

/// Guest side: asks the watched host to switch its own physical monitor to
/// `mode_id` (docs/bugs/16-host-display-mode.md #2; ADR 0048).
///
/// The host re-checks the independent `display_mode` grant and the id's
/// validity; this call only says whether the request could be sent at all.
///
/// # Errors
/// [`IpcError`] when unallowed, the host announced no such id, or the host
/// never confirmed it understands the message.
#[tauri::command]
pub async fn host_display_set_mode(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: HostDisplaySetModeArgs,
) -> Result<(), IpcError> {
    check_view_window(&window, &args.peer)?;
    state
        .network
        .host_display_set_mode(args.peer, args.mode_id)
        .await?;
    Ok(())
}

/// One recording this device has written, as the host's own screen sees it
/// (§9.2, §17; ADR 0031, ADR 0035).
#[derive(Debug, Clone, Serialize)]
pub struct RecordingDto {
    /// File name inside the app's recordings directory, never a path.
    ///
    /// A name is all the webview gets and all it may hand back: the directory
    /// is this process's own and is joined on the Rust side, so no string
    /// from the view layer can point the export at a file elsewhere (§2.3).
    pub name: String,
    /// Size on disk in bytes.
    pub bytes: u64,
    /// Unix seconds of the last write, for ordering and for showing an age.
    pub modified: u64,
    /// Whether an export of this recording already sits in `exports/`.
    pub exported: bool,
}

/// Directory the exports of §9.2 are written into, under the recordings
/// directory so one folder holds a session's recording and what came out of
/// it.
const EXPORTS_SUBDIR: &str = "exports";

/// Extension of a session recording (`lumepeer_media::record`).
const RECORDING_EXTENSION: &str = "lmrc";

/// The recordings directory, or the §18 error for a machine with no per-user
/// data directory at all.
fn recordings_dir() -> Result<std::path::PathBuf, IpcError> {
    crate::config::recordings_dir().ok_or(IpcError {
        code: "NO_DATA_DIR",
        message: "this machine has no per-user data directory".to_owned(),
    })
}

/// Rejects anything that is not a plain recording file name.
///
/// The check is deliberately whole-string rather than a scan for `..`: a name
/// passes only if it is exactly its own `file_name()` and carries the
/// recording extension, so a separator, a drive letter, a parent segment or
/// an absolute path all fail on the same rule. Where this process reads and
/// writes is not a decision the untrusted view layer takes (§2.3).
fn recording_file_name(name: &str) -> Result<&str, IpcError> {
    let path = std::path::Path::new(name);
    // `\` is only a separator to `Path` on Windows; checked here regardless of
    // target so a name a Windows peer would treat as a subdirectory is
    // refused the same way on every platform this runs on.
    let plain =
        !name.contains('\\') && path.file_name().and_then(std::ffi::OsStr::to_str) == Some(name);
    let is_recording = path
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|ext| ext.eq_ignore_ascii_case(RECORDING_EXTENSION));
    if plain && is_recording {
        Ok(name)
    } else {
        Err(IpcError {
            code: "BAD_RECORDING",
            message: "not the name of a recording of this device".to_owned(),
        })
    }
}

/// Host side: the recordings this device has written (§9.2, §17; ADR 0035).
///
/// Reads the app's own recordings directory and nothing else — the webview
/// names no path here either, it only receives names. A missing directory is
/// an empty list rather than an error: a host that has never recorded has
/// nothing to show, which is not a failure.
///
/// Main-window only: recordings belong to the machine that wrote them, and a
/// guest's view window has no business enumerating them.
///
/// # Errors
/// [`IpcError`] when called from another window, or when this machine has no
/// per-user data directory.
#[tauri::command]
pub async fn recordings_list(window: Window) -> Result<Vec<RecordingDto>, IpcError> {
    check_window(&window)?;
    let dir = recordings_dir()?;
    let exports = dir.join(EXPORTS_SUBDIR);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(Vec::new());
    };
    let mut recordings: Vec<RecordingDto> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = recording_file_name(path.file_name()?.to_str()?)
                .ok()?
                .to_owned();
            let meta = entry.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            let stem = path.file_stem()?.to_str()?;
            let exported = exports
                .join(format!(
                    "{stem}.{}",
                    lumepeer_media::export::VIDEO_EXTENSION
                ))
                .exists()
                || exports
                    .join(format!(
                        "{stem}.{}",
                        lumepeer_media::export::AUDIO_EXTENSION
                    ))
                    .exists();
            Some(RecordingDto {
                name,
                bytes: meta.len(),
                modified: modified_unix_secs(&meta),
                exported,
            })
        })
        .collect();
    // Newest first: the recording someone just stopped is the one they are
    // looking for.
    recordings.sort_unstable_by(|a, b| b.modified.cmp(&a.modified).then(a.name.cmp(&b.name)));
    Ok(recordings)
}

/// Last-write time of `meta` in unix seconds, `0` where the platform has none.
fn modified_unix_secs(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_secs())
}

/// Argument of [`recording_export`].
#[derive(Debug, Clone, Deserialize)]
pub struct RecordingExportArgs {
    /// Name of the recording, as a previous [`recordings_list`] reported it.
    pub name: String,
}

/// What one export produced, for the panel that asked for it.
#[derive(Debug, Clone, Serialize)]
pub struct ExportDto {
    /// Directory both files were written into, shown so the host can find
    /// them. Travels outwards only, exactly like a recording's own path.
    pub dir: String,
    /// File name of the H.264 elementary stream, when the recording had
    /// video.
    pub video: Option<String>,
    /// File name of the Ogg Opus stream, when the recording had audio.
    pub audio: Option<String>,
    /// Video records written.
    pub video_frames: u64,
    /// Opus packets written.
    pub audio_packets: u64,
    /// Event records skipped: the action log is not a media track (§15).
    pub events_skipped: u64,
}

/// Host side: exports one of this device's recordings into files a player can
/// open (§9.2; ADR 0031, ADR 0035).
///
/// Local from end to end: it reads a file this process wrote, writes two next
/// to it, and touches no session, no peer and no grant. The `recording` grant
/// governed *making* the recording; what the host does afterwards with its own
/// file is not a decision a guest ever had a say in.
///
/// Main-window only, and the name is validated before it is joined onto the
/// recordings directory ([`recording_file_name`]).
///
/// # Errors
/// [`IpcError`] when called from another window, when `name` is not a plain
/// recording name, or when the recording cannot be read or the export
/// written.
#[tauri::command]
pub async fn recording_export(
    window: Window,
    args: RecordingExportArgs,
) -> Result<ExportDto, IpcError> {
    check_window(&window)?;
    let dir = recordings_dir()?;
    let source = dir.join(recording_file_name(&args.name)?);
    let out_dir = dir.join(EXPORTS_SUBDIR);
    // Off the async runtime: an hour-long session is a streaming read of a
    // file that can run to gigabytes, and the IPC executor is shared with
    // every other command, including a revoke.
    let out_for_task = out_dir.clone();
    let output = tauri::async_runtime::spawn_blocking(move || {
        lumepeer_media::export::export_file(&source, &out_for_task)
    })
    .await
    .map_err(|_| IpcError {
        code: "EXPORT",
        message: "the export did not finish".to_owned(),
    })?
    .map_err(|error| IpcError {
        code: "EXPORT",
        // Safe to show: `RecordingError` carries a format complaint or an I/O
        // string about this machine's own file, and names no peer (§15).
        message: error.to_string(),
    })?;
    Ok(ExportDto {
        dir: out_dir.to_string_lossy().into_owned(),
        video: file_name_of(output.video.as_deref()),
        audio: file_name_of(output.audio.as_deref()),
        video_frames: output.summary.video_frames,
        audio_packets: output.summary.audio_packets,
        events_skipped: output.summary.events_skipped,
    })
}

/// File name of an exported track, dropping the directory the DTO already
/// carries once.
fn file_name_of(path: Option<&std::path::Path>) -> Option<String> {
    path.and_then(std::path::Path::file_name)
        .map(|name| name.to_string_lossy().into_owned())
}

/// What an update check found, when it found something.
#[derive(Debug, Clone, Serialize)]
pub struct UpdateDto {
    /// Version offered by the manifest.
    pub version: String,
    /// Version running right now.
    pub current: String,
    /// Release notes as the manifest carries them, or empty.
    pub notes: String,
}

/// Turns an updater failure into an IPC error without leaking a URL.
///
/// The manifest URL is configuration, not a secret, but an error string that
/// carries it ends up in the webview and in screenshots; the code is what the
/// UI branches on and the log already has the detail.
fn update_error(error: &tauri_plugin_updater::Error) -> IpcError {
    tracing::warn!(%error, "update check failed");
    IpcError {
        code: "UPDATE",
        message: "the update check failed".to_owned(),
    }
}

/// Builds the updater against the configured channel's manifest (ADR 0042).
fn updater(
    app: &tauri::AppHandle,
    state: &AppState,
) -> Result<tauri_plugin_updater::Updater, IpcError> {
    use tauri_plugin_updater::UpdaterExt as _;

    let url = state.update_url.as_deref().ok_or(IpcError {
        code: "UPDATE_OFF",
        message: "this build has no update endpoint configured".to_owned(),
    })?;
    let parsed = url.parse().map_err(|_| IpcError {
        code: "UPDATE_OFF",
        message: "the configured update endpoint is not a URL".to_owned(),
    })?;
    app.updater_builder()
        .endpoints(vec![parsed])
        .map_err(|error| update_error(&error))?
        .build()
        .map_err(|error| update_error(&error))
}

/// Asks the configured channel whether a newer release exists (§21; ADR 0042).
///
/// Only *asks*. Nothing is downloaded and nothing is installed: this app can be
/// in the middle of someone else's remote session, and an update that restarted
/// the process on its own would end that session without anyone deciding to.
///
/// # Errors
/// Rejects calls from any window but the main one; [`IpcError`] when updates
/// are not configured or the manifest cannot be read.
#[tauri::command]
pub async fn update_check(
    window: Window,
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<Option<UpdateDto>, IpcError> {
    check_window(&window)?;
    let updater = updater(&app, &state)?;
    let found = updater
        .check()
        .await
        .map_err(|error| update_error(&error))?;
    Ok(found.map(|update| UpdateDto {
        version: update.version.clone(),
        current: update.current_version.clone(),
        notes: update.body.clone().unwrap_or_default(),
    }))
}

/// Downloads and installs the update the channel offers (§21; ADR 0042).
///
/// The signature is checked by `tauri-plugin-updater` against the public key
/// baked into the bundle before a single byte is installed. There is no path
/// past that check in this command and there must never be one: an artifact
/// whose signature does not verify is not installed, whatever the manifest
/// said, and a network error is a failed update rather than a reason to take
/// anything unsigned.
///
/// # Errors
/// Rejects calls from any window but the main one; [`IpcError`] when there is
/// nothing to install, the download fails, or the signature does not verify.
#[tauri::command]
pub async fn update_install(
    window: Window,
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<(), IpcError> {
    check_window(&window)?;
    let updater = updater(&app, &state)?;
    let Some(update) = updater
        .check()
        .await
        .map_err(|error| update_error(&error))?
    else {
        return Err(IpcError {
            code: "UPDATE_NONE",
            message: "there is no newer release on this channel".to_owned(),
        });
    };
    tracing::info!(version = %update.version, "installing an update");
    update
        .download_and_install(|_, _| {}, || {})
        .await
        .map_err(|error| update_error(&error))?;
    tracing::info!("update installed; restart to run it");
    Ok(())
}

/// Whether this installation starts with the user's session (ADR 0042).
///
/// # Errors
/// Rejects calls from any window but the main one; [`IpcError`] when the
/// platform mechanism cannot be read.
#[tauri::command]
pub fn autostart_status(
    window: Window,
    state: tauri::State<'_, AppState>,
) -> Result<bool, IpcError> {
    check_window(&window)?;
    Ok(state.autostart.is_enabled())
}

/// Argument of [`autostart_set`].
#[derive(Debug, Clone, Deserialize)]
pub struct AutostartArgs {
    /// `true` adds the per-user startup entry, `false` removes it outright.
    pub enabled: bool,
}

/// Turns autostart on or off (ADR 0042).
///
/// Per-user only, and switching it off removes the entry rather than disabling
/// it. Starting with the session grants nothing: the app comes up and waits for
/// consent exactly as it does when a person launches it.
///
/// # Errors
/// Rejects calls from any window but the main one; [`IpcError`] when the
/// platform refuses the change.
#[tauri::command]
pub fn autostart_set(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: AutostartArgs,
) -> Result<(), IpcError> {
    check_window(&window)?;
    state
        .autostart
        .set(args.enabled)
        .map_err(|message| IpcError {
            code: "AUTOSTART",
            message,
        })?;
    tracing::info!(enabled = args.enabled, "autostart changed");
    Ok(())
}

/// Filter of one [`audit_list`] call.
#[derive(Debug, Clone, Deserialize)]
pub struct AuditListArgs {
    /// Oldest wall-clock second to include, or `None` for "from the start".
    pub since: Option<i64>,
    /// Newest wall-clock second to include, or `None` for "up to now".
    pub until: Option<i64>,
    /// One event kind, as [`audit_kinds`] lists them, or `None` for all.
    pub kind: Option<String>,
}

/// One audit record as the panel shows it (§15).
#[derive(Debug, Clone, Serialize)]
pub struct AuditRowDto {
    /// Wall-clock second the event was recorded at.
    pub at_unix_secs: i64,
    /// Pseudonymized peer label — a hash prefix, never a `NodeId`.
    pub peer: String,
    /// Event kind from the closed vocabulary of [`audit_kinds`].
    pub kind: String,
    /// Extra detail from the same closed vocabulary, or empty.
    pub detail: String,
}

/// Host side: the audit log, filtered by time window and event kind (§15).
///
/// A host running without a usable log answers with an empty list rather than
/// an error: "there is no log" is a true answer, and the panel says as much
/// through [`audit_status`].
///
/// # Errors
/// Rejects calls from any window but the main one; [`IpcError`] when the query
/// fails.
#[tauri::command]
pub async fn audit_list(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: AuditListArgs,
) -> Result<Vec<AuditRowDto>, IpcError> {
    check_window(&window)?;
    let Some(store) = state.network.audit() else {
        return Ok(Vec::new());
    };
    let rows = store
        .list(args.since, args.until, args.kind.as_deref())
        .await
        .map_err(|error| IpcError {
            code: "AUDIT",
            message: error.to_string(),
        })?;
    Ok(rows
        .into_iter()
        .map(|row| AuditRowDto {
            at_unix_secs: row.at_unix_secs,
            peer: row.peer,
            kind: row.kind,
            detail: row.detail,
        })
        .collect())
}

/// Every event kind the log can hold, for the panel's filter (§15).
///
/// Served from Rust rather than hardcoded in the webview so the filter cannot
/// drift away from what `audit_store::event_columns` actually writes.
#[tauri::command]
pub fn audit_kinds() -> Vec<&'static str> {
    crate::audit_store::EVENT_KINDS.to_vec()
}

/// Whether this host is keeping an audit log at all (§15).
///
/// The panel needs to tell "nothing happened yet" apart from "nothing is being
/// recorded", which are the same empty list otherwise (§18).
#[tauri::command]
pub fn audit_status(window: Window, state: tauri::State<'_, AppState>) -> Result<bool, IpcError> {
    check_window(&window)?;
    Ok(state.network.audit().is_some())
}

/// Host side: writes the whole log to a file the host user picks (§15).
///
/// The path comes from the OS save dialog driven here, in Rust: the webview
/// holds no `fs` permission and never names a path, exactly as with recordings
/// (§2.3). Returns the path written, or `None` when the dialog was dismissed.
///
/// CSV of the stored rows, not a copy of the database file — what leaves the
/// machine should be the pseudonymized records themselves.
///
/// # Errors
/// Rejects calls from any window but the main one; [`IpcError`] when the log
/// cannot be read or the file cannot be written.
#[tauri::command]
pub async fn audit_export(
    window: Window,
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<Option<String>, IpcError> {
    check_window(&window)?;
    let Some(store) = state.network.audit() else {
        return Ok(None);
    };
    let csv = store.export_csv().await.map_err(|error| IpcError {
        code: "AUDIT",
        message: error.to_string(),
    })?;
    let Some(path) = pick_save_path(&app, "lumepeer-audit.csv").await else {
        return Ok(None);
    };
    std::fs::write(&path, csv).map_err(|error| IpcError {
        code: "AUDIT",
        message: error.to_string(),
    })?;
    tracing::info!("audit log exported");
    Ok(Some(path))
}

/// Host side: deletes every audit record (§15).
///
/// §15 requires the host user be able to erase the log, not only read it. The
/// confirmation is the webview's; this end simply does it, and returns how
/// many records went.
///
/// # Errors
/// Rejects calls from any window but the main one; [`IpcError`] when the
/// delete fails.
#[tauri::command]
pub async fn audit_clear(
    window: Window,
    state: tauri::State<'_, AppState>,
) -> Result<u64, IpcError> {
    check_window(&window)?;
    let Some(store) = state.network.audit() else {
        return Ok(0);
    };
    store.purge().await.map_err(|error| IpcError {
        code: "AUDIT",
        message: error.to_string(),
    })
}

/// Argument of [`host_bar_expand`].
#[derive(Debug, Clone, Deserialize)]
pub struct HostBarExpandArgs {
    /// Whether the bar shows its full card, or collapses to the edge tab.
    pub expanded: bool,
}

/// Opens or collapses the host's session bar (the two states of ADR 0055).
///
/// Geometry only. The bar carries no authority of its own — everything it can
/// actually do goes through the same commands the main window uses — and this
/// exists because the window is undecorated: there is no chrome for the user
/// to resize it by, so the page asks for the size its own state needs.
///
/// The right edge stays where it is across the change, so a bar the host
/// dragged somewhere stays there and one docked to the screen edge does not
/// walk inwards every time it is opened. The result is clamped to the monitor
/// the bar is on, so a collapse near the left edge cannot push the open card
/// off the screen.
///
/// # Errors
/// Rejects calls from any window but the bar itself. A bar that is not up is
/// not an error: it means the last session ended between the click and this
/// call, and there is nothing left to resize.
#[tauri::command]
pub async fn host_bar_expand(
    window: Window,
    app: tauri::AppHandle,
    args: HostBarExpandArgs,
) -> Result<(), IpcError> {
    use tauri::{LogicalSize, Manager as _, PhysicalPosition};

    check_host_bar(&window)?;
    let Some(bar) = app.get_webview_window(crate::view::HOST_BAR_LABEL) else {
        return Ok(());
    };
    let (width, height) = if args.expanded {
        (crate::view::HOST_BAR_WIDTH, crate::view::HOST_BAR_HEIGHT)
    } else {
        (
            crate::view::HOST_BAR_TAB_WIDTH,
            crate::view::HOST_BAR_TAB_HEIGHT,
        )
    };

    // Read the old geometry before resizing: the anchors are the right edge
    // and the vertical middle the bar has right now, and after `set_size`
    // both have already moved.
    let anchor = bar.outer_position().ok().and_then(|position| {
        let size = bar.outer_size().ok()?;
        let scale = bar.scale_factor().ok()?;
        #[allow(
            clippy::cast_possible_truncation,
            reason = "a window's size in physical pixels is far inside i32"
        )]
        let (new_width, new_height) = (
            (width * scale).round() as i32,
            (height * scale).round() as i32,
        );
        let old_width = i32::try_from(size.width).unwrap_or(i32::MAX);
        let old_height = i32::try_from(size.height).unwrap_or(i32::MAX);
        Some((
            position.x + old_width - new_width,
            position.y + (old_height - new_height) / 2,
        ))
    });

    bar.set_size(LogicalSize::new(width, height))
        .map_err(|_ignored| IpcError {
            code: "HOST_BAR",
            message: "the session bar could not be resized".to_owned(),
        })?;
    if let Some((x, y)) = anchor {
        let (x, y) = clamp_to_monitor(&bar, (x, y), (width, height));
        let _ = bar.set_position(PhysicalPosition::new(x, y));
    }
    Ok(())
}

/// Keeps the whole bar on the monitor it is already on.
///
/// Without this, opening the card while the bar sits near an edge would place
/// it partly off-screen — the anchors are the right edge and the vertical
/// middle, so the extra width and height have to come from somewhere. A
/// monitor Tauri cannot name leaves the position alone rather than guessing
/// at one.
fn clamp_to_monitor(
    bar: &tauri::WebviewWindow,
    (x, y): (i32, i32),
    (logical_width, logical_height): (f64, f64),
) -> (i32, i32) {
    let Ok(Some(monitor)) = bar.current_monitor() else {
        return (x, y);
    };
    let scale = monitor.scale_factor();
    #[allow(
        clippy::cast_possible_truncation,
        reason = "a window's size in physical pixels is far inside i32"
    )]
    let (width, height) = (
        (logical_width * scale).round() as i32,
        (logical_height * scale).round() as i32,
    );
    let size = monitor.size();
    let (left, top) = (monitor.position().x, monitor.position().y);
    let right = left + i32::try_from(size.width).unwrap_or(i32::MAX);
    let bottom = top + i32::try_from(size.height).unwrap_or(i32::MAX);
    (
        x.clamp(left, (right - width).max(left)),
        y.clamp(top, (bottom - height).max(top)),
    )
}

/// Brings the main window back from the tray or the taskbar (§18).
///
/// The bar's way out to the full UI: it deliberately carries only the two
/// things a host needs mid-session, and everything else — settings, the audit
/// log, files, chat — lives in the window this raises.
///
/// # Errors
/// Rejects calls from any window but the bar itself.
#[tauri::command]
pub async fn host_bar_focus_main(window: Window, app: tauri::AppHandle) -> Result<(), IpcError> {
    check_host_bar(&window)?;
    crate::focus_main_window(&app);
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "a failed assumption must fail the test"
    )]

    use super::*;

    #[test]
    fn a_plain_recording_name_passes() {
        assert_eq!(
            recording_file_name("session-1700000000-ab12cd.lmrc").unwrap(),
            "session-1700000000-ab12cd.lmrc"
        );
    }

    #[test]
    fn a_name_that_is_a_path_is_refused() {
        for name in [
            "../secrets.lmrc",
            "sub/dir.lmrc",
            "sub\\dir.lmrc",
            "/etc/passwd.lmrc",
            "..",
            "",
        ] {
            assert_eq!(
                recording_file_name(name).unwrap_err().code,
                "BAD_RECORDING",
                "{name} was not refused"
            );
        }
    }

    #[test]
    fn a_file_that_is_not_a_recording_is_refused() {
        for name in ["notes.txt", "session.lmrc.txt", "session", "session."] {
            assert_eq!(
                recording_file_name(name).unwrap_err().code,
                "BAD_RECORDING",
                "{name} was not refused"
            );
        }
    }

    /// Every command Tauri is told to handle must also be declared to
    /// `tauri-build`, or no `allow-<name>` permission is generated for it and
    /// the capability file naming it fails the build — but only once someone
    /// deletes the stale autogenerated `.toml` a previous build left behind.
    /// That drift is invisible until then, which is exactly why it is a test.
    #[test]
    fn every_handled_command_is_declared_to_tauri_build() {
        let declared = command_list(
            include_str!("../build.rs"),
            "const COMMANDS: &[&str] = &[",
            "];",
        );
        let handled: Vec<String> =
            command_list(include_str!("main.rs"), "tauri::generate_handler![", "]")
                .into_iter()
                .map(|line| line.trim_start_matches("commands::").to_owned())
                .collect();
        assert!(!handled.is_empty(), "no handled commands were parsed");
        for command in &handled {
            assert!(
                declared.contains(command),
                "{command} is in generate_handler! but not in build.rs COMMANDS"
            );
        }
        for command in &declared {
            assert!(
                handled.contains(command),
                "{command} is declared in build.rs COMMANDS but is never handled"
            );
        }
    }

    /// The entries of a bracketed list in a source file, unquoted and without
    /// their trailing commas.
    fn command_list(source: &str, open: &str, close: &str) -> Vec<String> {
        source
            .split_once(open)
            .expect("list opener")
            .1
            .split_once(close)
            .expect("list terminator")
            .0
            .lines()
            .map(|line| line.trim().trim_end_matches(',').trim_matches('"'))
            .filter(|line| !line.is_empty() && !line.starts_with("//"))
            .map(str::to_owned)
            .collect()
    }
}
