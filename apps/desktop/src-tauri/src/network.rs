//! Owner of the network/session runtime (design doc §2.3, §4, §13).
//!
//! `NetworkActor` is the single owner of `SessionManager`, `PeerEndpoint` and
//! `TicketRegistry`. Tauri commands never lock anything directly: they send an
//! `ActorCommand` and await the reply, so there is exactly one place that
//! decides authorization (§2.3).
//!
//! Every live control connection is split in two: a reader task owns the
//! inbound half and reports what it sees back to the actor, while the actor
//! keeps only an outbound channel. That is what makes `ConsentGrant` delivery
//! and disconnect detection work symmetrically on the host and the guest side,
//! and it keeps a partially read frame from ever being cancelled by an
//! outbound write.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ed25519_dalek::SigningKey;
use lumepeer_core::address_book::AddressEntry;
use lumepeer_core::audit::AuditEvent;
use lumepeer_core::chat::{ChatEntry, ChatLog};
use lumepeer_core::clipboard::{self as clip, ClipboardFlow, ClipboardSync};
use lumepeer_core::consent::{ConsentRateLimiter, Grants, IndependentGrant, Role};
use lumepeer_core::constants::{
    ABR_MIN_SCALE_PERCENT, CONNECT_ATTEMPT_TIMEOUT_SECS, CONTROL_HANDSHAKE_TIMEOUT_SECS,
    DIAL_ATTEMPTS, DIAL_RETRY_BACKOFF_JITTER_MS, DIAL_RETRY_BACKOFF_MS,
    DISPLAY_MODE_CONFIRM_TIMEOUT_SECS, FILE_OFFER_MAX_BYTES, FILE_TRANSFER_START_TIMEOUT_SECS,
    INCOMING_ACCEPT_TIMEOUT_SECS, KEYFRAME_MIN_INTERVAL_MS, MAX_INFLIGHT_HANDSHAKES,
    MAX_PENDING_FILE_OFFERS, PING_INTERVAL_SECS, RTT_EWMA_ALPHA, RTT_MAX_PLAUSIBLE_MS,
    STREAM_SCALE_MAX_PERCENT,
};
use lumepeer_core::protocol::{
    ClipboardFileEntry, CursorShapeData, DisplayModeInfo, DisplayModeUnavailableReason,
    FEATURE_CLIPBOARD_FILES, FEATURE_CURSOR_SHAPE, FEATURE_DISPLAY_MODE, FEATURE_FILE_TRANSFER,
    FEATURE_MEDIA_UNAVAILABLE, FEATURE_RECEIVER_REPORT, FEATURE_STREAM_SCALE, FEATURE_UNATTENDED,
    InputEventPayload, MediaUnavailableReason, MessageKind, MonitorInfo, UnattendedRejection,
};
use lumepeer_core::session::{SessionManager, SessionState};
use lumepeer_core::unattended::{UnattendedAccess, UnattendedError};
use lumepeer_core::{CoreError, NodeId};
use lumepeer_media::capture::{
    CaptureController, CaptureTarget, InputInjector, StubCapturer, platform_backend,
    platform_injector,
};
use lumepeer_net::file_transfer::{
    ReceiveTracker, StagedReceive, TransferId, hash_file, read_chunk, safe_file_name, send_file,
};
use lumepeer_net::keystore::{Keystore, load_or_create};
use lumepeer_net::ticket::TicketRegistry;
use lumepeer_net::{Channel, ControlConnection, InviteTicket, NetError, PeerEndpoint};
use rand::Rng as _;
use rand::RngExt as _;
use tokio::sync::{Semaphore, broadcast, mpsc, oneshot, watch};

use crate::address_book_store::AddressBookStore;
use crate::connection_history::{ConnectionHistory, HistoryEntry};
use crate::remembered_password::RememberedPasswordStore;
use crate::unattended_store::UnattendedStore;
use crate::view::{
    CursorFeed, EncodeControl, HostMedia, MediaFault, MediaHealth, MediaReport, MediaTarget,
    SharedCapture, ViewSlot, ViewStatus, ViewWindows, encode_cursor_response, encode_view_response,
    lock_capture, slot_for_poll, spawn_encode_loop, spawn_media_receiver, window_label,
};

/// First `PROTOCOL_MINOR` that carries `MessageKind::FileTransferStart`, and
/// therefore the floor for offering a file to a host (§9.1; ADR 0032).
///
/// Guest side only. A host reads the guest's `FEATURE_FILE_TRANSFER` string
/// instead, which is the more precise signal; the guest has no feature list
/// to read, because `HelloAck` does not carry one.
const FILE_TRANSFER_MINOR: u16 = 5;

/// First `PROTOCOL_MINOR` that carries `MessageKind::ReceiverReport`, and
/// therefore the floor for reporting reception to a host (§9.1; ADR 0037).
///
/// Guest side only, exactly like [`FILE_TRANSFER_MINOR`]: a host reads the
/// guest's `FEATURE_RECEIVER_REPORT` string instead, which is the more precise
/// signal, and `HelloAck` carries no feature list for the guest to read.
const RECEIVER_REPORT_MINOR: u16 = 6;

/// First `PROTOCOL_MINOR` that carries `MessageKind::StreamScaleRequest`, and
/// therefore the floor for asking a host to cap the picture (D7,
/// docs/bugs/13-stream-resolution.md).
///
/// Guest side only, exactly like [`RECEIVER_REPORT_MINOR`]: a host reads the
/// guest's `FEATURE_STREAM_SCALE` string instead, which is the more precise
/// signal, and `HelloAck` carries no feature list for the guest to read.
const STREAM_SCALE_MINOR: u16 = 7;

/// First `PROTOCOL_MINOR` that carries `MessageKind::ClipboardFileOffer` and
/// `MessageKind::ClipboardFileAccept`, and therefore the floor for offering
/// clipboard files to a host (docs/bugs/14-clipboard-files.md #2; ADR 0047).
///
/// Guest side only, exactly like [`FILE_TRANSFER_MINOR`] — the same
/// asymmetry, for the same reason: either side may have files on its own
/// clipboard, so a host reads the guest's `FEATURE_CLIPBOARD_FILES` string
/// instead of a minor, but the guest has no feature list of the host's to
/// read.
const CLIPBOARD_FILES_MINOR: u16 = 8;

/// First `PROTOCOL_MINOR` that carries `MessageKind::DisplayModesList` and
/// `MessageKind::DisplaySetMode` (docs/bugs/16-host-display-mode.md #2;
/// ADR 0048).
///
/// Guest side only, exactly like [`STREAM_SCALE_MINOR`]: a host reads the
/// guest's `FEATURE_DISPLAY_MODE` string instead, and `HelloAck` carries no
/// feature list for the guest to read the other way.
const DISPLAY_MODE_MINOR: u16 = 9;

/// Capacity of the notification broadcast. Listeners that fall behind lag;
/// nothing in the actor's own progress depends on them.
const NOTIFY_CAPACITY: usize = 32;

/// Denominator of the permille a `ReceiverReport` carries its loss in.
const PERMILLE: u16 = 1_000;

/// Capacity of the guest-side media report channel.
///
/// Small on purpose: reports supersede one another, so a backlog is worthless
/// — the newest one always says more than the three behind it. The media loop
/// drops rather than waits when this is full (§11).
const REPORT_CAPACITY: usize = 16;

/// Whether a keyframe request may be honoured now, given when this host last
/// honoured one for the same peer (§11).
///
/// Pure, and separate from the actor so the budget can be checked without a
/// session: this is the whole of what protects the host's uplink from a guest
/// that asks on every frame.
fn keyframe_budget_allows(last_honoured: Option<std::time::Instant>) -> bool {
    let budget = Duration::from_millis(KEYFRAME_MIN_INTERVAL_MS);
    last_honoured.is_none_or(|at| at.elapsed() >= budget)
}

/// Whether a display-mode auto-revert timeout should actually revert
/// (docs/bugs/16-host-display-mode.md #3; ADR 0048).
///
/// Pure, and separate from the actor so the generation-staleness and
/// confirmation rules can be checked without a runtime, a capture backend or
/// a real wait: `current_generation` is `None` once the state has already
/// been resolved (restored, or the peer that owned it left) by the time the
/// timeout fires, and any generation other than the one this timeout was
/// armed for means a later switch has already superseded it. Only a timeout
/// that still names the live generation, with capture never having
/// confirmed health since it was armed, reverts.
fn should_auto_revert_display_mode(
    current_generation: Option<u64>,
    timed_out_generation: u64,
    healthy_since_armed: bool,
) -> bool {
    current_generation == Some(timed_out_generation) && !healthy_since_armed
}

/// Blends one round-trip sample into the running average of [`RTT_EWMA_ALPHA`].
///
/// Pure, and separate from [`RttTracker`] so the smoothing can be checked
/// without a clock: this is the whole of what the constant means.
fn ewma_rtt(previous: Option<f32>, sample_ms: f32) -> f32 {
    previous.map_or(sample_ms, |previous| {
        previous + RTT_EWMA_ALPHA * (sample_ms - previous)
    })
}

/// One peer's control-channel round trip, measured by `Ping`/`Pong` (§9.1).
///
/// Not a liveness watchdog. QUIC and `RECONNECT_WINDOW_SECS` already decide
/// when a session is gone; a missing `Pong` here costs a measurement and
/// nothing else, which is why an unanswered ping is simply overwritten by the
/// next one.
#[derive(Debug, Default)]
struct RttTracker {
    /// The nonce this side is waiting on, and when it went out.
    outstanding: Option<(u64, std::time::Instant)>,
    /// Smoothed round trip in milliseconds; `None` until the first sample.
    smoothed_ms: Option<f32>,
}

impl RttTracker {
    /// Records that `nonce` has just gone out.
    fn sent(&mut self, nonce: u64) {
        self.outstanding = Some((nonce, std::time::Instant::now()));
    }

    /// Folds a returned nonce into the average, and returns the new value.
    ///
    /// `None` — and no change at all — for a nonce this side is not waiting
    /// on, and for a sample beyond [`RTT_MAX_PLAUSIBLE_MS`]. Both are silent:
    /// a peer echoing something else is not an error worth a log line, and a
    /// round trip that spans a suspended machine is not a measurement.
    fn pong(&mut self, nonce: u64) -> Option<u32> {
        let (expected, sent_at) = self.outstanding?;
        if nonce != expected {
            return None;
        }
        self.outstanding = None;
        let sample = u32::try_from(sent_at.elapsed().as_millis()).unwrap_or(u32::MAX);
        if sample > RTT_MAX_PLAUSIBLE_MS {
            return None;
        }
        #[expect(
            clippy::cast_precision_loss,
            reason = "a millisecond count under RTT_MAX_PLAUSIBLE_MS is exact in f32"
        )]
        let smoothed = ewma_rtt(self.smoothed_ms, sample as f32);
        self.smoothed_ms = Some(smoothed);
        self.smoothed()
    }

    /// The smoothed round trip, rounded to whole milliseconds.
    fn smoothed(&self) -> Option<u32> {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "the average of non-negative samples under RTT_MAX_PLAUSIBLE_MS \
                      is non-negative and far inside u32"
        )]
        self.smoothed_ms.map(|ms| ms.round() as u32)
    }
}

/// How a peer is actually being reached, as iroh reports it rather than as the
/// settings intended (§18; ADR 0026).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    /// Every open path is a direct IP path.
    Direct,
    /// Every open path goes through a relay.
    Relay,
    /// Both kinds are open; which one carries the next packet is iroh's.
    Mixed,
    /// The connection has no open path to report — it is coming up, or going
    /// away.
    Unknown,
}

impl PathKind {
    /// Stable identifier for the webview, which turns it into localized text.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Relay => "relay",
            Self::Mixed => "mixed",
            Self::Unknown => "unknown",
        }
    }
}

/// Classifies a live connection's open paths, and names the relay's region
/// when one is in use.
///
/// The region, never the address. A relay URL is a hostname the *host's*
/// network chose to reach, and §15 keeps that class of detail off a screen the
/// host does not control: what a person needs in order to know where they are
/// is "through a relay, roughly there", not an address they could look up. An
/// IP-literal relay has no region at all and gets `None` rather than an
/// invented one.
fn path_of(connection: &iroh::endpoint::Connection) -> (PathKind, Option<String>) {
    let mut direct = false;
    let mut relay = false;
    let mut region = None;
    for path in &connection.paths() {
        if path.is_ip() {
            direct = true;
        }
        if path.is_relay() {
            relay = true;
            if region.is_none()
                && let iroh::TransportAddr::Relay(url) = path.remote_addr()
            {
                region = relay_region(url);
            }
        }
    }
    let kind = match (direct, relay) {
        (true, true) => PathKind::Mixed,
        (true, false) => PathKind::Direct,
        (false, true) => PathKind::Relay,
        (false, false) => PathKind::Unknown,
    };
    (kind, region)
}

/// The leading DNS label of a relay URL, which is the region in every relay
/// naming scheme this ships against, or `None` when the URL names a bare
/// address.
fn relay_region(url: &iroh::RelayUrl) -> Option<String> {
    let host = url.host_str()?;
    if host.parse::<std::net::IpAddr>().is_ok() {
        return None;
    }
    let (label, rest) = host.split_once('.')?;
    if label.is_empty() || rest.is_empty() {
        return None;
    }
    Some(label.to_owned())
}

/// What a view window needs to paint one frame, readable without the actor.
///
/// The picture already lives in a `watch` channel the media task writes and
/// nothing else mutates, so serving a frame poll never needed the actor's own
/// thread — it only ever went through the mailbox because that was where the
/// grant lived. Routing it there put every frame of every view window behind
/// whatever else the actor was doing, which at 30 fps is the difference
/// between a remote desktop and a slideshow (ADR 0027).
///
/// This widens nothing (§2.3). `input` is a copy the *actor* writes, on grant
/// and on revoke, of a decision `lumepeer-core` already made; a reader here
/// can only observe it. The entry is removed before the window is told to
/// close, so a poll racing a revoke reads either the live grant or nothing.
#[derive(Debug, Clone)]
struct ViewFeed {
    slot: watch::Receiver<ViewSlot>,
    input: Arc<AtomicBool>,
    /// Whether the host says it is recording this session right now (§17).
    ///
    /// Announced by the host as `RecordAck`, never inferred here: the guest
    /// cannot know what the far side writes to disk, so the indicator it shows
    /// is the host's own statement and nothing else.
    recording: Arc<AtomicBool>,
    /// The host's cursor, when it announced one (§11's `CursorShape`).
    ///
    /// `None` means this host is still drawing the cursor into the picture,
    /// which is exactly what tells the window not to draw a second one.
    cursor: Arc<std::sync::RwLock<Option<CursorFeed>>>,
}

/// Live view feeds by window label, shared between the actor and the IPC layer.
type ViewFeeds = Arc<std::sync::RwLock<HashMap<String, ViewFeed>>>;

/// State of one session as the webview needs to know it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStateDto {
    /// Queued, waiting for the host's decision.
    Pending,
    /// Consent granted, grants are live.
    Active,
}

/// What one live connection's link looks like right now (§18; ADR 0026,
/// ADR 0037).
///
/// Everything in here is measured, not configured: the round trip comes from
/// this session's own `Ping`/`Pong`, the path from iroh's open paths, and loss
/// and goodput from whichever side is receiving pictures. A field is `None`
/// when nothing has measured it yet — never a zero standing in for "unknown",
/// which is the one thing a diagnostics panel must not say.
#[derive(Debug, Clone)]
pub struct ConnectionStats {
    /// Pseudonymized peer label (§15).
    pub label: String,
    /// Smoothed control-channel round trip, in milliseconds.
    pub rtt_ms: Option<u32>,
    /// Share of frames the receiver could not turn into a picture, permille.
    pub loss_permille: Option<u16>,
    /// Media throughput the receiver observed, in kilobits per second.
    pub goodput_kbps: Option<u32>,
    /// Whether this peer is reached directly, through a relay, or both.
    pub path: PathKind,
    /// Region of the relay in use, when one is; never its address (§15).
    pub relay_region: Option<String>,
    /// Encoder bitrate this host is sending at; `None` on the guest side,
    /// which is not the one encoding.
    pub bitrate_kbps: Option<u32>,
    /// Frame rate this host is sending at; `None` on the guest side.
    pub fps: Option<u8>,
}

/// One row of the status list the webview polls.
#[allow(
    clippy::struct_excessive_bools,
    reason = "mirrors Grants plus two independent activity flags (recording_active, secure_desktop_active); §2.2 requires the grants to stay independent fields rather than folded together"
)]
#[derive(Debug, Clone)]
pub struct SessionSnapshot {
    /// Pseudonymized label; the only peer-identifying string that ever
    /// crosses the IPC boundary in either direction.
    pub label: String,
    /// Pending or active.
    pub state: SessionStateDto,
    /// Requested role if pending, granted role if active.
    pub role: Role,
    /// Whether input injection is currently permitted (always `false` for
    /// a pending entry).
    pub input: bool,
    /// The four independent grants of §8.2, all `false` for a pending entry.
    ///
    /// Carried as the whole [`Grants`] value the core holds rather than four
    /// booleans copied out here: this row shows what the core decided, and a
    /// second place that assembles the same set is a second place to get it
    /// wrong.
    pub grants: Grants,
    /// Whether a recording of this session is being written right now (§17).
    ///
    /// Separate from `grants.recording`: the grant says the host *may* record,
    /// this says it *is*. The indicator both sides must show hangs off this
    /// one, so it is read from the live recorder rather than from the grant.
    pub recording_active: bool,
    /// Whether this guest has asked to be recorded and is still waiting for an
    /// answer (§17). Never auto-answered: a person at the host decides.
    pub record_request: bool,
    /// Whether this guest is, right now, actually seeing the secure desktop
    /// (ADR 0049).
    ///
    /// Separate from `grants.secure_desktop` the same way `recording_active`
    /// is separate from `recording`: the grant says the host *may* show it,
    /// this says it *is* happening. The host's non-removable indicator hangs
    /// off this one.
    pub secure_desktop_active: bool,
}

/// What `invite_create` hands back to the UI.
#[derive(Debug, Clone)]
pub struct InviteDto {
    /// The invite code to show in the sidebar (§7).
    pub code: String,
    /// Unix seconds after which the invite is dead.
    pub expires_at: u64,
}

/// Everything the connect form needs to know about this node's own outgoing
/// attempt, in one reply.
///
/// A struct rather than a tuple because the credential path of §8 added two
/// fields that only make sense together with the phase, and a four-tuple at
/// six call sites is how the wrong element gets read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectSnapshot {
    /// How far the attempt has got.
    pub phase: ConnectPhase,
    /// §18 code of the last failure, or of a refused credential.
    pub code: Option<&'static str>,
    /// Whether the host's credential challenge asked for a one-time code.
    pub code_required: bool,
    /// Seconds the host said to wait before trying again, after a lockout.
    pub retry_secs: Option<u64>,
    /// Whether the credential attempt in flight was started automatically
    /// from a remembered password, rather than by the user submitting the
    /// form (docs/bugs/02-connect-form.md, task 6). The connect form uses
    /// this to keep the credentials modal from flashing open for a host it
    /// already knows the password to.
    pub credentials_auto: bool,
}

/// One address-book device as the UI sees it (§8; ADR 0034).
///
/// `peer_label` is the pseudonymized per-run tag `label_of` hands out, the
/// same one every other panel names a peer by; the raw `NodeId` the book is
/// keyed on never crosses into the webview (§15). `name`, `tags` and `notes`
/// are what the host user typed.
#[derive(Debug, Clone)]
pub struct AddressBookRow {
    /// Pseudonymized peer label, never a raw `NodeId`.
    pub peer_label: String,
    /// Human name the host gave this device.
    pub name: String,
    /// Free-form grouping tags.
    pub tags: Vec<String>,
    /// Free-text note.
    pub notes: String,
    /// Whether this device may attempt an unattended login at all.
    pub trusted: bool,
    /// Whether this device is connected right now, so the UI can say so
    /// without a second lookup.
    pub connected: bool,
}

/// What the host's own settings screen may know about unattended access
/// (§8; ADR 0033).
///
/// Three booleans and a role. The password, its hash and the TOTP secret are
/// absent by construction: there is no field here that could carry them, which
/// is a stronger guarantee than remembering not to fill one in (§2.3, §13).
#[derive(Debug, Clone, Copy)]
pub struct UnattendedSettings {
    /// Whether a device password is set and unattended logins may be offered.
    pub enabled: bool,
    /// Whether a second factor is part of the gate.
    pub totp_enabled: bool,
    /// Role a successful admission is granted.
    pub role: Role,
}

/// The one-time provisioning payload for an authenticator app (§8).
///
/// Handed to the UI exactly once, at the moment the second factor is turned
/// on: an app cannot be provisioned without seeing the secret. It is never
/// re-readable afterwards — `unattended_status` has no field for it — so a
/// host that loses the app turns the factor off and on again.
#[derive(Debug, Clone)]
pub struct TotpProvisioning {
    /// The shared secret in RFC 4648 base32, for typing in by hand.
    pub secret_base32: String,
    /// The same secret as an `otpauth://` URI, for a QR code.
    pub uri: String,
}

/// Guest side: how far this node's own outgoing connect attempt has got (§21
/// punch-list item 6).
///
/// A dial is not the end of the story — the host user still has to decide, and
/// that decision can take as long as it takes. Without this the connect form
/// has nothing to wait on: it would go back to idle the moment the handshake
/// returned, invite a second attempt against a host that is already deciding
/// on the first, and show no sign of the wait at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectPhase {
    /// Nothing outgoing in flight.
    Idle,
    /// The dial is in flight: the ticket parsed, and this node is trying to
    /// reach the host it names (ADR 0027).
    ///
    /// A phase of its own because the dial no longer happens inside the IPC
    /// call. `invite_connect` returns as soon as the attempt has *started*, so
    /// without this the form would have nothing to stay disabled on between
    /// the click and the host's `Hello` landing.
    Dialing,
    /// Connected to the host; its user has not decided yet.
    AwaitingConsent,
    /// The host is configured for unattended access and asked for device
    /// credentials instead of waking a human (§8; ADR 0033).
    ///
    /// A phase rather than a flag on `AwaitingConsent`, because the two wait
    /// on opposite things: `AwaitingConsent` waits on the far side, this one
    /// waits on *this* user to type a password, and the connect form has to
    /// show a field rather than a spinner.
    AwaitingCredentials,
    /// The host granted and the view window is open.
    Connected,
    /// The host refused, or ended the request without granting.
    Denied,
    /// The dial or the handshake failed, or the host dropped mid-request.
    Failed,
}

impl ConnectPhase {
    /// Whether the connect form should stay disabled: something is in flight
    /// and a second attempt would only race it.
    #[must_use]
    pub const fn is_pending(self) -> bool {
        matches!(
            self,
            Self::Dialing | Self::AwaitingConsent | Self::AwaitingCredentials
        )
    }

    /// Stable wire string for the webview.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Dialing => "dialing",
            Self::AwaitingConsent => "awaiting_consent",
            Self::AwaitingCredentials => "awaiting_credentials",
            Self::Connected => "connected",
            Self::Denied => "denied",
            Self::Failed => "failed",
        }
    }
}

/// Something the actor observed that a listener may want to react to.
///
/// Deliberately carries no peer identity: it crosses no trust boundary today
/// and must not become a channel for one (§15).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActorNotification {
    /// A remote host granted `role` to this node (guest side).
    ConsentGranted {
        /// Role the host decided on, which may be lower than the one asked for.
        role: Role,
    },
    /// A remote host withdrew its grant (guest side).
    ConsentRevoked,
    /// A consent request was queued for the host user to decide (host side).
    ConsentRequested,
    /// This node was just asked for device credentials (guest side; §8; ADR
    /// 0033). Deliberately carries no peer identity, like every other variant
    /// here — the credentials modal already has what it needs from
    /// `connect_status` (docs/bugs/02-connect-form.md, task 5).
    UnattendedChallenge,
    /// A control connection closed, in either direction.
    Disconnected,
    /// A chat message arrived from a peer. The label is pseudonymized (§15);
    /// the transcript itself stays inside the actor and is polled by the UI.
    ChatFromPeer {
        /// Pseudonymized peer label the message came from.
        label: String,
    },
    /// The peer's clipboard changed and passed §9.2 validation; the payload
    /// is delivered through `clipboard_inbound`, never on the notification
    /// bus (§15: notifications are broadcast to every listener).
    ClipboardFromPeer,
    /// Something about this node's file transfers moved: an offer arrived, one
    /// was answered, a transfer progressed or ended.
    ///
    /// Carries nothing, for the same reason [`Self::ClipboardFromPeer`] does.
    /// A file name is not as sensitive as a clipboard, but it is still §15
    /// material and this bus reaches every listener; the UI polls
    /// `file_transfers` for the detail.
    FileTransferChanged,
}

/// Failure returned by an actor call.
#[derive(Debug)]
pub enum ActorError {
    /// The label the caller sent does not resolve to a known peer. Not a
    /// leak: an unknown label is exactly as safe to report as a known one,
    /// since the label never carried identity to begin with.
    UnknownPeer,
    /// A `SessionManager` decision was refused.
    Core(CoreError),
    /// A network operation failed.
    Net(NetError),
    /// An unattended-access operation was refused (§8; ADR 0033).
    ///
    /// Only ever produced for the *host's own* settings screen — setting a
    /// password that fails the policy, for instance. A guest's refused login
    /// never travels this way: it goes back on the wire as the deliberately
    /// coarse `MessageKind::UnattendedReject`.
    Unattended(UnattendedError),
    /// The actor task is gone; the caller's channel op failed.
    ChannelClosed,
    /// The peer speaks a protocol minor that does not have this message.
    ///
    /// Not a refusal and not a fault: §9.1 makes optional messages the
    /// sender's responsibility to withhold, so the honest answer to "offer
    /// this file" towards a peer that could not decode `FileTransferStart` is
    /// that this session cannot do it — never a transfer that starts and then
    /// cannot be acked, aborted or resumed.
    Unsupported,
}

/// Reply type of [`ActorCommand::DisplayModesList`] (docs/bugs/
/// 16-host-display-mode.md #2; ADR 0048): the modes, empty exactly when the
/// reason is `Some`, mirroring the wire message's own shape.
type DisplayModesReply =
    Result<(Vec<DisplayModeInfo>, Option<DisplayModeUnavailableReason>), ActorError>;

/// One request the actor understands.
enum ActorCommand {
    Status {
        reply: oneshot::Sender<Vec<SessionSnapshot>>,
    },
    /// Hosts this node has connected to before (§21 punch-list item 5).
    History {
        reply: oneshot::Sender<Vec<HistoryEntry>>,
    },
    /// Guest side: dial a remembered host again, using the invite code the
    /// history row kept. The code never leaves the Rust side (§13).
    HistoryConnect {
        label: String,
        reply: oneshot::Sender<Result<(), ActorError>>,
    },
    /// Guest side: forget a remembered host (docs/bugs/03-connection-list.md,
    /// task 5).
    HistoryRemove {
        label: String,
        reply: oneshot::Sender<bool>,
    },
    /// Guest side: how this node's own outgoing connect attempt is going, and
    /// the §18 code of the last failure if it ended in one.
    ConnectState {
        reply: oneshot::Sender<ConnectSnapshot>,
    },
    /// Guest side: abandon this node's own outgoing connect attempt
    /// (docs/bugs/02-connect-form.md, task 3). Always the one attempt this
    /// node has in flight, so no argument names it.
    ConnectCancel { reply: oneshot::Sender<()> },
    /// What every live connection's link actually looks like (§18).
    ConnectionStats {
        reply: oneshot::Sender<Vec<ConnectionStats>>,
    },
    Grant {
        label: String,
        role: Role,
        reply: oneshot::Sender<Result<(), ActorError>>,
    },
    Revoke {
        label: String,
        reply: oneshot::Sender<Result<(), ActorError>>,
    },
    InviteCreate {
        role: Role,
        reply: oneshot::Sender<Result<InviteDto, ActorError>>,
    },
    InviteConnect {
        ticket: String,
        reply: oneshot::Sender<Result<(), ActorError>>,
    },
    /// Guest side: one input event to forward to the host being viewed.
    Input {
        label: String,
        event: Box<InputEventPayload>,
        reply: oneshot::Sender<Result<(), ActorError>>,
    },
    /// Either side: send one chat message to `label`.
    ChatSend {
        label: String,
        text: String,
        reply: oneshot::Sender<Result<ChatEntry, ActorError>>,
    },
    /// Either side: fetch the chat transcript of `label`.
    ChatTranscript {
        label: String,
        reply: oneshot::Sender<Vec<ChatEntry>>,
    },
    /// Either side: push the local clipboard text to the peer, gated on the
    /// session's clipboard grants (§8.2).
    ClipboardPush {
        label: String,
        text: String,
        reply: oneshot::Sender<Result<(), ActorError>>,
    },
    /// Either side: answer the oldest offer this peer made. `directory` is
    /// where the receiving user chose to put it, and is ignored on a refusal.
    FileAccept {
        label: String,
        accept: bool,
        directory: Option<String>,
        reply: oneshot::Sender<Result<(), ActorError>>,
    },
    /// Either side: stop one transfer that is already running (§9.2).
    FileAbort {
        label: String,
        transfer_id: u64,
        reply: oneshot::Sender<Result<(), ActorError>>,
    },
    /// Either side: every offer and transfer this node knows about, for the
    /// UI to draw.
    FileTransfers {
        reply: oneshot::Sender<FileTransfersDto>,
    },
    /// Either side: fetch the newest inbound clipboard payload, if any.
    ClipboardPull {
        label: String,
        reply: oneshot::Sender<Option<String>>,
    },
    /// Host side: start streaming desktop audio to `label` (§11 `AudioStart`).
    /// Requires a live `view` grant and an accepted media connection.
    AudioOn {
        label: String,
        reply: oneshot::Sender<Result<(), ActorError>>,
    },
    /// Host side: stop the audio stream started by [`ActorCommand::AudioOn`].
    AudioOff {
        label: String,
        reply: oneshot::Sender<Result<(), ActorError>>,
    },
    /// Host side: start or stop recording the session with `label` (§9.2,
    /// §17). Requires the independent `recording` grant (§8.2).
    ///
    /// Starting answers with the path the actor chose; the webview never
    /// supplies one (§2.3).
    RecordToggle {
        label: String,
        on: bool,
        reply: oneshot::Sender<Result<Option<String>, ActorError>>,
    },
    /// Guest side: ask the host to record the session (§17). The host user
    /// answers; the guest learns the answer as `RecordAck`.
    RecordRequest {
        label: String,
        reply: oneshot::Sender<Result<(), ActorError>>,
    },
    /// Guest side: ask the host to deliver the Secure Attention Sequence
    /// (Ctrl+Alt+Del) to its user (§11; ADR 0028). The host answers on the
    /// wire with `SasAck`, and the reply here only says the request went out
    /// from a session shape that could carry it.
    SasRequest {
        label: String,
        reply: oneshot::Sender<Result<(), ActorError>>,
    },
    /// Guest side: turn the view window's own microphone towards `label`'s
    /// host on or off (§11; ADR 0028). Requires a live session with a live
    /// `input` grant.
    MicToggle {
        label: String,
        on: bool,
        reply: oneshot::Sender<Result<(), ActorError>>,
    },
    /// Guest side: ask the watched host to show the monitor the operator
    /// picked (§11 `MonitorSelect`; ADR 0028).
    MonitorSelect {
        label: String,
        monitor_id: u32,
        reply: oneshot::Sender<Result<(), ActorError>>,
    },
    /// Guest side: the monitors the watched host announced
    /// (§11 `MonitorsList`; ADR 0028).
    MonitorsList {
        label: String,
        reply: oneshot::Sender<Result<Vec<MonitorInfo>, ActorError>>,
    },
    /// Guest side: ask the watched host to cap the picture at a percentage of
    /// its own captured size (§11; D7, docs/bugs/13-stream-resolution.md).
    StreamScaleRequest {
        label: String,
        scale_percent: u32,
        reply: oneshot::Sender<Result<(), ActorError>>,
    },
    /// Guest side: the modes the watched host announced for its own physical
    /// monitor (docs/bugs/16-host-display-mode.md #2; ADR 0048).
    DisplayModesList {
        label: String,
        reply: oneshot::Sender<DisplayModesReply>,
    },
    /// Guest side: ask the watched host to switch its own physical monitor
    /// to `mode_id` (docs/bugs/16-host-display-mode.md #2; ADR 0048).
    DisplaySetMode {
        label: String,
        mode_id: u32,
        reply: oneshot::Sender<Result<(), ActorError>>,
    },
    /// Host side: turn one independent grant of `label`'s session on or off
    /// (§8.2; ADR 0029). Only the host's own main window reaches this.
    SetGrant {
        label: String,
        grant: IndependentGrant,
        allowed: bool,
        reply: oneshot::Sender<Result<(), ActorError>>,
    },
    /// Host side: every saved device (§8; ADR 0034).
    AddressBookList {
        reply: oneshot::Sender<Vec<AddressBookRow>>,
    },
    /// Host side: save or update one device. The peer is named by a label
    /// that already resolves — a connected session or an existing entry — so
    /// no `NodeId` ever has to come back from the webview (§13).
    AddressBookUpsert {
        label: String,
        name: String,
        tags: Vec<String>,
        notes: String,
        reply: oneshot::Sender<Result<(), ActorError>>,
    },
    /// Host side: forget one device.
    AddressBookRemove {
        label: String,
        reply: oneshot::Sender<Result<(), ActorError>>,
    },
    /// Host side: mark a device trusted, or withdraw that (§8; ADR 0034).
    /// Never called by anything but the host's own main window, and never
    /// automatically.
    AddressBookSetTrusted {
        label: String,
        trusted: bool,
        reply: oneshot::Sender<Result<(), ActorError>>,
    },
    /// Host side: what the settings screen may know about unattended access.
    UnattendedStatus {
        reply: oneshot::Sender<UnattendedSettings>,
    },
    /// Host side: set or replace the device password (§8; ADR 0033).
    UnattendedSetPassword {
        password: String,
        reply: oneshot::Sender<Result<(), ActorError>>,
    },
    /// Host side: turn unattended access off and forget the credentials.
    UnattendedDisable {
        reply: oneshot::Sender<Result<(), ActorError>>,
    },
    /// Host side: turn the second factor on (returning the one-time
    /// provisioning payload) or off (returning `None`).
    UnattendedSetTotp {
        enabled: bool,
        reply: oneshot::Sender<Result<Option<TotpProvisioning>, ActorError>>,
    },
    /// Host side: choose the role a successful admission is granted (§8.2).
    UnattendedSetRole {
        role: Role,
        reply: oneshot::Sender<Result<(), ActorError>>,
    },
    /// Guest side: answer the host's `UnattendedChallenge` (§8; ADR 0033).
    ///
    /// The password crosses this boundary once, from the field the user typed
    /// it into to the wire; the reply says only that it was sent. A copy does
    /// briefly live in `Actor::pending_remember` when `remember` is set — held
    /// only until the grant or refusal lands, then either written to the
    /// keystore or dropped (docs/bugs/02-connect-form.md, task 6).
    UnattendedSubmit {
        password: String,
        code: Option<String>,
        remember: bool,
        reply: oneshot::Sender<Result<(), ActorError>>,
    },
}

/// Thin handle IPC commands hold. Cloneable: every command gets its own
/// clone of the sender.
#[derive(Debug, Clone)]
pub struct ActorHandle {
    tx: mpsc::Sender<ActorCommand>,
    notify: broadcast::Sender<ActorNotification>,
    /// Whether the local endpoint has reached a relay and is dialable from
    /// outside the LAN. False from process start; flips true at most once,
    /// when `PeerEndpoint::online()` first resolves (see `spawn_actor`) —
    /// this crate never observes a relay going back offline.
    online: Arc<AtomicBool>,
    /// What this host knows about its own ability to produce a picture.
    /// Shared with the actor, which is what learns of an encoder fault.
    health: Arc<MediaHealth>,
    /// Guest side: one entry per open view window, so `view_frame` can serve a
    /// picture without queueing behind the actor's mailbox (ADR 0027).
    /// Written only by the actor.
    views: ViewFeeds,
    /// Host side: the audit log, for the read-only IPC commands (§15;
    /// ADR 0041).
    ///
    /// Shared with the actor for the same reason `views` is: listing, exporting
    /// and clearing a local table decides nothing, and routing those through
    /// the actor's mailbox would put disk latency in front of consent.
    audit: Option<crate::audit_store::AuditStore>,
}

impl ActorHandle {
    /// Stream of [`ActorNotification`]s from this point on.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<ActorNotification> {
        self.notify.subscribe()
    }

    /// Whether this host is currently ready to accept incoming connections
    /// from outside the LAN. Purely a status report for the UI — carries no
    /// authorization of its own (§2.3).
    /// The audit log, when one was opened (§15; ADR 0041).
    ///
    /// `None` means this host is running without an audit trail and has
    /// already said so in its own log; the commands turn it into an empty
    /// list rather than an error, because "no log" is a true answer.
    #[must_use]
    pub const fn audit(&self) -> Option<&crate::audit_store::AuditStore> {
        self.audit.as_ref()
    }

    #[must_use]
    pub fn online(&self) -> bool {
        self.online.load(Ordering::Relaxed)
    }

    /// What this host can do about producing a picture, for the status the UI
    /// shows its own operator (§18). Carries no authorization of its own.
    #[must_use]
    pub fn media_health(&self) -> &MediaHealth {
        &self.health
    }

    /// Snapshot of every pending and active session.
    ///
    /// # Errors
    /// [`ActorError::ChannelClosed`] if the actor task is gone.
    pub async fn status(&self) -> Result<Vec<SessionSnapshot>, ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::Status { reply })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)
    }

    /// Hosts this node has connected to before, most recent first (§21
    /// punch-list item 5).
    ///
    /// # Errors
    /// [`ActorError::ChannelClosed`] if the actor task is gone.
    pub async fn history(&self) -> Result<Vec<HistoryEntry>, ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::History { reply })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)
    }

    /// Dials a remembered host again by its history label.
    ///
    /// # Errors
    /// [`ActorError::UnknownPeer`] if no history row carries that label;
    /// [`ActorError::Net`] if the remembered invite no longer works or the
    /// dial fails; [`ActorError::ChannelClosed`] if the actor is gone.
    pub async fn history_connect(&self, label: String) -> Result<(), ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::HistoryConnect { label, reply })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)?
    }

    /// Forgets a remembered host (docs/bugs/03-connection-list.md, task 5).
    ///
    /// # Errors
    /// [`ActorError::ChannelClosed`] if the actor task is gone. Removing a
    /// label that was never remembered is not an error — the post-condition
    /// (nothing listed under that label) already holds.
    pub async fn history_remove(&self, label: String) -> Result<bool, ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::HistoryRemove { label, reply })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)
    }

    /// What every live connection's link actually looks like: round trip,
    /// path type, loss, goodput and the quality target being sent (§18).
    ///
    /// # Errors
    /// [`ActorError::ChannelClosed`] if the actor task is gone.
    pub async fn connection_stats(&self) -> Result<Vec<ConnectionStats>, ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::ConnectionStats { reply })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)
    }

    /// How this node's own outgoing connect attempt is going.
    ///
    /// # Errors
    /// [`ActorError::ChannelClosed`] if the actor task is gone.
    pub async fn connect_state(&self) -> Result<ConnectSnapshot, ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::ConnectState { reply })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)
    }

    /// Abandons this node's own outgoing connect attempt, whatever stage it
    /// is at (docs/bugs/02-connect-form.md, task 3).
    ///
    /// # Errors
    /// [`ActorError::ChannelClosed`] if the actor task is gone.
    pub async fn connect_cancel(&self) -> Result<(), ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::ConnectCancel { reply })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)
    }

    /// Grants `role` to the peer behind `label`.
    ///
    /// # Errors
    /// [`ActorError::UnknownPeer`] if `label` resolves to nothing;
    /// [`ActorError::Core`] if [`SessionManager::grant`] refuses;
    /// [`ActorError::ChannelClosed`] if the actor task is gone.
    pub async fn grant(&self, label: String, role: Role) -> Result<(), ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::Grant { label, role, reply })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)?
    }

    /// Revokes every grant of the peer behind `label`.
    ///
    /// # Errors
    /// Same as [`Self::grant`].
    pub async fn revoke(&self, label: String) -> Result<(), ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::Revoke { label, reply })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)?
    }

    /// Issues and registers an invite for `role`.
    ///
    /// # Errors
    /// [`ActorError::Net`] if the ticket cannot be signed/encoded;
    /// [`ActorError::ChannelClosed`] if the actor is gone.
    pub async fn invite_create(&self, role: Role) -> Result<InviteDto, ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::InviteCreate { role, reply })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)?
    }

    /// Parses `ticket` and dials the host it names.
    ///
    /// # Errors
    /// [`ActorError::Net`] if the ticket is malformed or the dial/handshake
    /// fails; [`ActorError::ChannelClosed`] if the actor is gone.
    pub async fn invite_connect(&self, ticket: String) -> Result<(), ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::InviteConnect { ticket, reply })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)?
    }

    /// Newest picture of the view onto `label`, as the raw bytes
    /// `view_next_frame` hands to the webview.
    ///
    /// `since_us` is the timestamp of the picture the caller already has, or
    /// 0 for none; the pixel payload is omitted when it already matches the
    /// current frame (§15: a caller polling faster than the video updates
    /// should not pay for re-serializing an unchanged picture).
    ///
    /// Read straight from the shared feed rather than through the actor's
    /// mailbox: a frame poll is a read of state the actor does not have to be
    /// consulted about, and making it wait for the actor is what made a busy
    /// app drop the picture (ADR 0027).
    ///
    /// # Errors
    /// [`ActorError::UnknownPeer`] if no view window belongs to `label`.
    pub fn view_frame(&self, label: &str, since_us: u64) -> Result<Vec<u8>, ActorError> {
        let feed = self
            .views
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(label)
            .cloned()
            .ok_or(ActorError::UnknownPeer)?;
        let response = slot_for_poll(&feed.slot.borrow(), since_us);
        Ok(encode_view_response(
            &response,
            feed.input.load(Ordering::Relaxed),
            feed.recording.load(Ordering::Relaxed),
        ))
    }

    /// The host's cursor for a view window, as raw bytes (§11).
    ///
    /// Answered off the actor loop for the same reason `view_frame` is: it is
    /// a read of a cell the control task writes and nothing else mutates, and
    /// routing it through the mailbox would put it behind whatever the actor
    /// is doing. Nothing is widened — the entry disappears with the view.
    ///
    /// # Errors
    /// [`ActorError::UnknownPeer`] if no view window belongs to `label`.
    pub fn view_cursor(&self, label: &str, since_seq: u32) -> Result<Vec<u8>, ActorError> {
        let feed = self
            .views
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(label)
            .cloned()
            .ok_or(ActorError::UnknownPeer)?;
        let cursor = feed
            .cursor
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(encode_cursor_response(cursor.as_ref(), since_seq))
    }

    /// Forwards one input event to the host behind `label`.
    ///
    /// The guest drops it if its own copy of the grant no longer carries
    /// `input`; the host checks again, authoritatively, per event (§2.3, §8.1).
    ///
    /// # Errors
    /// [`ActorError::UnknownPeer`] if no view window belongs to `label`;
    /// [`ActorError::Core`] with [`CoreError::NotPermitted`] if the session
    /// holds no `input` grant; [`ActorError::ChannelClosed`] if the actor is
    /// gone.
    /// Sends one chat message to the session with `label` and returns the
    /// stored transcript entry.
    ///
    /// # Errors
    /// [`ActorError::UnknownPeer`] / [`ActorError::Core`] as refused;
    /// [`ActorError::ChannelClosed`] if the actor task is gone.
    pub async fn chat_send(&self, label: String, text: String) -> Result<ChatEntry, ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::ChatSend { label, text, reply })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)?
    }

    /// The chat transcript of `label`, oldest first.
    ///
    /// # Errors
    /// [`ActorError::ChannelClosed`] if the actor task is gone.
    pub async fn chat_transcript(&self, label: String) -> Result<Vec<ChatEntry>, ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::ChatTranscript { label, reply })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)
    }

    /// Pushes the local clipboard to `label` (grant-gated, §8.2).
    ///
    /// # Errors
    /// [`ActorError::Core::NotPermitted`] without the clipboard grant;
    /// [`ActorError::ChannelClosed`] if the actor task is gone.
    pub async fn clipboard_push(&self, label: String, text: String) -> Result<(), ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::ClipboardPush { label, text, reply })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)?
    }

    /// Answers the oldest offer `label` made.
    ///
    /// # Errors
    /// [`ActorError::UnknownPeer`] for an unknown label or with no offer
    /// outstanding; [`ActorError::Core`] as `NotPermitted` without the grant.
    pub async fn file_accept(
        &self,
        label: String,
        accept: bool,
        directory: Option<String>,
    ) -> Result<(), ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::FileAccept {
                label,
                accept,
                directory,
                reply,
            })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)?
    }

    /// Stops one running transfer with `label`.
    ///
    /// # Errors
    /// [`ActorError::UnknownPeer`] for an unknown label or transfer.
    pub async fn file_abort(&self, label: String, transfer_id: u64) -> Result<(), ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::FileAbort {
                label,
                transfer_id,
                reply,
            })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)?
    }

    /// Every pending offer and running transfer, for the UI to draw.
    ///
    /// # Errors
    /// [`ActorError::ChannelClosed`] if the actor is gone.
    pub async fn file_transfers(&self) -> Result<FileTransfersDto, ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::FileTransfers { reply })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)
    }

    /// Takes the newest inbound clipboard payload from `label`, if any.
    /// Pull semantics keep payloads off the broadcast bus (§15).
    ///
    /// # Errors
    /// [`ActorError::ChannelClosed`] if the actor task is gone.
    pub async fn clipboard_pull(&self, label: String) -> Result<Option<String>, ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::ClipboardPull { label, reply })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)
    }

    /// Host side: turns the desktop-audio stream to `label` on or off (§11).
    ///
    /// Refused without a live granted session; audio rides the same media
    /// connection the picture uses, so it also needs an accepted media dial.
    ///
    /// # Errors
    /// [`ActorError::Core::NotPermitted`] without a granted view session;
    /// [`ActorError::UnknownPeer`] when no media connection exists yet;
    /// [`ActorError::ChannelClosed`] if the actor task is gone.
    pub async fn audio_toggle(&self, label: String, on: bool) -> Result<(), ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(if on {
                ActorCommand::AudioOn { label, reply }
            } else {
                ActorCommand::AudioOff { label, reply }
            })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)?
    }

    /// Host side: starts or stops the session recording of `label` (§17).
    /// Gated on the independent `recording` grant inside the actor (§8.2).
    ///
    /// Answers with the file the actor chose when a recording started, so the
    /// UI can tell the operator where it landed. The destination is decided in
    /// Rust and only reported outwards: an untrusted view layer does not pick
    /// where this machine writes files (§2.3).
    ///
    /// # Errors
    /// [`ActorError::Core`] with [`CoreError::NotPermitted`] without the
    /// recording grant; [`ActorError::Net`] when the file cannot be created;
    /// [`ActorError::ChannelClosed`] if the actor task is gone.
    pub async fn record_toggle(
        &self,
        label: String,
        on: bool,
    ) -> Result<Option<String>, ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::RecordToggle { label, on, reply })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)?
    }

    /// Guest side: asks the host behind `label` to record the session (§17).
    ///
    /// `Ok(())` only means the request left this node. Whether the host user
    /// agrees comes back as `RecordAck`, and a refusal is an ordinary answer,
    /// not an error.
    ///
    /// # Errors
    /// [`ActorError::UnknownPeer`] without a live view onto that host;
    /// [`ActorError::ChannelClosed`] if the actor task is gone.
    pub async fn record_request(&self, label: String) -> Result<(), ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::RecordRequest { label, reply })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)?
    }

    pub async fn input(&self, label: String, event: InputEventPayload) -> Result<(), ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::Input {
                label,
                event: Box::new(event),
                reply,
            })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)?
    }

    /// Guest side: asks the host to deliver the Secure Attention Sequence
    /// (§11; ADR 0028). The `Ok(())` here only means the request was sent
    /// from a session that could carry it; whether the host actually
    /// synthesized the sequence arrives on the wire as `SasAck` and is
    /// surfaced in the view window's own UI.
    ///
    /// # Errors
    /// [`ActorError::Core::NotPermitted`] without a granted session with a
    /// live `input` grant; [`ActorError::ChannelClosed`] if the actor is gone.
    pub async fn sas_request(&self, label: String) -> Result<(), ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::SasRequest { label, reply })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)?
    }

    /// Guest side: asks the watched host to show `monitor_id` instead
    /// (§11 `MonitorSelect`; ADR 0028). Nothing comes back on this call — the
    /// next picture simply shows the other screen.
    ///
    /// # Errors
    /// [`ActorError::UnknownPeer`] when this node is not watching `label`;
    /// [`ActorError::Core::Malformed`] when the host announced no such id;
    /// [`ActorError::ChannelClosed`] if the actor is gone.
    pub async fn monitor_select(&self, label: String, monitor_id: u32) -> Result<(), ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::MonitorSelect {
                label,
                monitor_id,
                reply,
            })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)?
    }

    /// Guest side: asks the watched host to cap the picture at
    /// `scale_percent` of its own captured size (§11; D7,
    /// docs/bugs/13-stream-resolution.md). Nothing comes back on this call
    /// other than whether the request could be sent — the picture itself
    /// simply gets smaller once the host applies it.
    ///
    /// # Errors
    /// [`ActorError::UnknownPeer`] when this node is not watching `label`;
    /// [`ActorError::Core::Malformed`] for a value outside the
    /// guest-selectable range; [`ActorError::Unsupported`] towards a host
    /// that never confirmed it understands the message;
    /// [`ActorError::ChannelClosed`] if the actor is gone.
    pub async fn set_stream_scale(
        &self,
        label: String,
        scale_percent: u32,
    ) -> Result<(), ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::StreamScaleRequest {
                label,
                scale_percent,
                reply,
            })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)?
    }

    /// Guest side: the watched host's monitors, as it announced them when it
    /// granted this session (§11 `MonitorsList`; ADR 0028).
    ///
    /// Empty when the host announced none — a host that cannot produce a
    /// picture at all does not announce, and the picker says so rather than
    /// offering screens nothing will ever be shown on.
    ///
    /// # Errors
    /// [`ActorError::UnknownPeer`] when this node is not watching `label`;
    /// [`ActorError::ChannelClosed`] if the actor is gone.
    pub async fn monitors_list(&self, label: String) -> Result<Vec<MonitorInfo>, ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::MonitorsList { label, reply })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)?
    }

    /// Guest side: the watched host's own physical display modes, as it last
    /// announced them, plus an honest reason when there are none
    /// (docs/bugs/16-host-display-mode.md #2; ADR 0048).
    ///
    /// # Errors
    /// [`ActorError::UnknownPeer`] when this node is not watching `label`;
    /// [`ActorError::ChannelClosed`] if the actor is gone.
    pub async fn host_display_modes(&self, label: String) -> DisplayModesReply {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::DisplayModesList { label, reply })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)?
    }

    /// Guest side: asks the watched host to switch its own physical monitor
    /// to `mode_id` (docs/bugs/16-host-display-mode.md #2; ADR 0048).
    ///
    /// # Errors
    /// [`ActorError::UnknownPeer`] when this node is not watching `label`;
    /// [`ActorError::Core::Malformed`] when the host announced no such id;
    /// [`ActorError::Unsupported`] towards a host that never confirmed it
    /// understands the message; [`ActorError::ChannelClosed`] if the actor is
    /// gone.
    pub async fn host_display_set_mode(
        &self,
        label: String,
        mode_id: u32,
    ) -> Result<(), ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::DisplaySetMode {
                label,
                mode_id,
                reply,
            })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)?
    }
    /// Host side: every saved device of the address book (§8; ADR 0034).
    ///
    /// # Errors
    /// [`ActorError::ChannelClosed`] if the actor is gone.
    pub async fn address_book_list(&self) -> Result<Vec<AddressBookRow>, ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::AddressBookList { reply })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)
    }

    /// Host side: saves or updates one device (§8; ADR 0034).
    ///
    /// # Errors
    /// [`ActorError::UnknownPeer`] if the label names nothing this run knows
    /// about; [`ActorError::ChannelClosed`] if the actor is gone.
    pub async fn address_book_upsert(
        &self,
        label: String,
        name: String,
        tags: Vec<String>,
        notes: String,
    ) -> Result<(), ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::AddressBookUpsert {
                label,
                name,
                tags,
                notes,
                reply,
            })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)?
    }

    /// Host side: forgets one device.
    ///
    /// # Errors
    /// As [`Self::address_book_upsert`].
    pub async fn address_book_remove(&self, label: String) -> Result<(), ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::AddressBookRemove { label, reply })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)?
    }

    /// Host side: marks a device trusted, or withdraws that (§8; ADR 0034).
    ///
    /// # Errors
    /// As [`Self::address_book_upsert`].
    pub async fn address_book_set_trusted(
        &self,
        label: String,
        trusted: bool,
    ) -> Result<(), ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::AddressBookSetTrusted {
                label,
                trusted,
                reply,
            })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)?
    }

    /// Host side: what the settings screen may know about unattended access.
    ///
    /// # Errors
    /// [`ActorError::ChannelClosed`] if the actor is gone.
    pub async fn unattended_status(&self) -> Result<UnattendedSettings, ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::UnattendedStatus { reply })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)
    }

    /// Host side: sets or replaces the device password (§8; ADR 0033).
    ///
    /// # Errors
    /// [`ActorError::Unattended`] if the password fails the policy of §8 or
    /// the keystore refuses to keep it; [`ActorError::Net`] for a keystore
    /// that is unavailable.
    pub async fn unattended_set_password(&self, password: String) -> Result<(), ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::UnattendedSetPassword { password, reply })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)?
    }

    /// Host side: turns unattended access off and forgets the credentials.
    ///
    /// # Errors
    /// [`ActorError::Net`] if the keystore refuses to drop them.
    pub async fn unattended_disable(&self) -> Result<(), ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::UnattendedDisable { reply })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)?
    }

    /// Host side: turns the second factor on or off (§8).
    ///
    /// Returns the one-time provisioning payload when turning it on.
    ///
    /// # Errors
    /// [`ActorError::Unattended`] when no password is set — a second factor
    /// without a first is not a gate; [`ActorError::Net`] if the keystore
    /// refuses the write.
    pub async fn unattended_set_totp(
        &self,
        enabled: bool,
    ) -> Result<Option<TotpProvisioning>, ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::UnattendedSetTotp { enabled, reply })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)?
    }

    /// Host side: chooses the role a successful admission is granted (§8.2).
    ///
    /// # Errors
    /// [`ActorError::Net`] if the keystore refuses the write.
    pub async fn unattended_set_role(&self, role: Role) -> Result<(), ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::UnattendedSetRole { role, reply })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)?
    }

    /// Guest side: answers the host's credential challenge (§8; ADR 0033).
    ///
    /// # Errors
    /// [`ActorError::Core::NotPermitted`] if nothing is waiting on a
    /// challenge right now; [`ActorError::ChannelClosed`] if the actor is
    /// gone. Whether the credentials were *right* is not this call's answer:
    /// it arrives on the wire, as a grant or a rejection, and shows up in
    /// `connect_status`.
    pub async fn unattended_submit(
        &self,
        password: String,
        code: Option<String>,
        remember: bool,
    ) -> Result<(), ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::UnattendedSubmit {
                password,
                code,
                remember,
                reply,
            })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)?
    }

    /// Host side: turns one independent grant of `label`'s session on or off
    /// (§8.2; ADR 0029).
    ///
    /// The decision is the core's: this only carries the host user's answer to
    /// [`SessionManager::set_grant`], which refuses anything but an active
    /// session and never touches `view` or `input`.
    ///
    /// # Errors
    /// [`ActorError::UnknownPeer`] for a label with no session;
    /// [`ActorError::Core`] as [`SessionManager::set_grant`] returns it;
    /// [`ActorError::ChannelClosed`] if the actor is gone.
    pub async fn set_grant(
        &self,
        label: String,
        grant: IndependentGrant,
        allowed: bool,
    ) -> Result<(), ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::SetGrant {
                label,
                grant,
                allowed,
                reply,
            })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)?
    }

    /// Guest side: turns the view window's own microphone towards `label`'s
    /// host on or off (§11; ADR 0028). Gated inside the actor on a live
    /// session with a live `input` grant.
    ///
    /// # Errors
    /// [`ActorError::Core::NotPermitted`] without a granted session with a
    /// live `input` grant; [`ActorError::ChannelClosed`] if the actor is gone.
    pub async fn mic_toggle(&self, label: String, on: bool) -> Result<(), ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::MicToggle { label, on, reply })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)?
    }
}

/// Current Unix time in whole seconds; 0 if the clock is before the epoch.
fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Pseudonymized peer identifier, safe for logs and for the webview (§15).
///
/// The same value is used as the IPC label and as the `tracing` field, so an
/// operator reading the log can line a warning up with a row in the UI without
/// either side ever seeing a raw `NodeId`.
fn peer_tag(install_salt: &[u8; 32], peer: &NodeId) -> String {
    let hash = lumepeer_core::audit::peer_hash(install_salt, peer);
    hex_prefix(&hash)
}

/// File a recording of `label` is written into (§17, §2.3).
///
/// The whole path is decided here: the directory from the app's own data
/// directory, the name from the clock and the peer's pseudonymized label. The
/// webview never supplies any of it — a view layer that could choose the path
/// could make this process write a file anywhere it can reach, which is not a
/// decision an untrusted layer gets to make (§2.3), and the label keeps the
/// name free of anything that identifies the peer (§15).
fn recording_path(label: &str) -> Result<std::path::PathBuf, ActorError> {
    let dir = crate::config::recordings_dir()
        .ok_or_else(|| ActorError::Net(NetError::Io("no data directory".to_owned())))?;
    std::fs::create_dir_all(&dir).map_err(|e| ActorError::Net(NetError::Io(e.to_string())))?;
    // The label is a hex tag already, but it lands in a file name: anything
    // outside the safe set is dropped rather than trusted to be harmless.
    let safe: String = label
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(32)
        .collect();
    Ok(dir.join(format!("session-{}-{safe}.lmrc", unix_now_secs())))
}

/// Fixed domain-separation salt for [`host_tag`]. Not a secret and not an
/// install salt: its only job is to keep this hash from colliding with the
/// per-run `peer_tag` namespace.
const HOST_LABEL_SALT: [u8; 32] = *b"lumepeer/connection-history/v1\0\0";

/// Pseudonymized label of a *host this node connected to*, stable across
/// restarts (§15, ADR 0016).
///
/// [`peer_tag`]'s install salt is deliberately not used here. That salt is
/// regenerated on every start precisely so a guest's label cannot be
/// correlated across runs — right for someone else appearing in this host's
/// UI, and exactly wrong for the list of hosts this user chose to connect to
/// and expects to recognize tomorrow. It is still a one-way hash: no raw
/// `NodeId` reaches the webview or the history file.
fn host_tag(peer: &NodeId) -> String {
    let hash = lumepeer_core::audit::peer_hash(&HOST_LABEL_SALT, peer);
    hex_prefix(&hash)
}

fn hex_prefix(hash: &[u8; 32]) -> String {
    hash[..8].iter().fold(String::new(), |mut out, byte| {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// One offer this node made, waiting for the peer to answer it.
///
/// The path is kept because `FileOffer` deliberately does not carry it: what
/// crosses the wire is a basename, and where the file actually lives on this
/// machine is nobody else's business (§15).
struct OutgoingOffer {
    path: std::path::PathBuf,
    name: String,
    size: u64,
    hash: [u8; 32],
}

/// One offer this node accepted, waiting for the sender's
/// `FileTransferStart` to name it (§9.2; ADR 0032).
struct AcceptedOffer {
    name: String,
    size: u64,
    /// `None` for an offer accepted through `ClipboardFileAccept`: the
    /// sender had not hashed the file yet at accept time (docs/bugs/
    /// 14-clipboard-files.md #2), so there is nothing here to check the
    /// start against beyond name and size. `FileTransferStart`'s own hash is
    /// still what `ReceiveTracker::finish` verifies the bytes against either
    /// way — this only gates the *bait-and-switch* check a hash-carrying
    /// offer gets.
    hash: Option<[u8; 32]>,
    /// Where the verified file goes. Chosen by the *receiving* user for an
    /// ordinary offer; fixed to this node's own clipboard-receive directory
    /// for one accepted through `ClipboardFileAccept` (docs/bugs/
    /// 14-clipboard-files.md #3) — the sender picks a name, never a
    /// location, either way.
    destination: std::path::PathBuf,
    /// Whether this offer came from the peer's clipboard rather than its
    /// file picker (docs/bugs/14-clipboard-files.md #3), so the transfer it
    /// starts can be tagged for the UI and for the clipboard write-back on
    /// completion.
    from_clipboard: bool,
}

/// One offer that arrived and is waiting for this user to answer.
#[derive(Clone)]
enum IncomingOffer {
    /// Offered through the sender's file picker, with a hash already known
    /// (§9.2).
    Direct(PendingOffer),
    /// Offered through the sender's clipboard: name and size only, no hash
    /// yet (docs/bugs/14-clipboard-files.md #2).
    Clipboard(ClipboardFileEntry),
}

impl IncomingOffer {
    fn name(&self) -> &str {
        match self {
            Self::Direct(offer) => &offer.name,
            Self::Clipboard(entry) => &entry.name,
        }
    }

    fn size(&self) -> u64 {
        match self {
            Self::Direct(offer) => offer.size,
            Self::Clipboard(entry) => entry.size,
        }
    }
}

/// One offer that arrived through the sender's file picker and is waiting
/// for this user to answer (§9.2).
#[derive(Clone)]
struct PendingOffer {
    /// Basename, already through `safe_file_name`.
    name: String,
    size: u64,
    hash: [u8; 32],
}

/// Receiver-side state for one peer, shared with the tasks reading its chunk
/// streams.
///
/// Shared rather than owned by the actor because a 256 KiB chunk must not
/// travel through the actor's mailbox to be written: at that rate the loop
/// would spend a transfer doing nothing else, which is the shape of failure
/// ADR 0027 is about. The lock is held for one `apply_chunk` plus one append.
#[derive(Default)]
struct FileInbox {
    tracker: ReceiveTracker,
    staged: std::collections::HashMap<TransferId, StagedReceive>,
    /// Hash to verify, and where the file goes once it does.
    expected: std::collections::HashMap<TransferId, ([u8; 32], std::path::PathBuf)>,
}

/// A peer's inbox plus the signal that a transfer has been prepared.
///
/// `starts` exists because the control channel and `rd/file/1` are separate
/// QUIC connections (§4), so nothing orders `FileTransferStart` against the
/// first chunk. A stream reader that finds an unknown id waits on this rather
/// than refusing a transfer that is about to be announced. A `watch` and not
/// a `Notify`: `changed()` is edge-triggered from what this reader last saw,
/// so a start landing between the check and the wait cannot be missed.
#[derive(Clone)]
struct FileChannel {
    inbox: Arc<tokio::sync::Mutex<FileInbox>>,
    starts: watch::Sender<u64>,
}

impl FileChannel {
    fn new() -> Self {
        Self {
            inbox: Arc::new(tokio::sync::Mutex::new(FileInbox::default())),
            starts: watch::channel(0).0,
        }
    }

    /// Announces that one more transfer is now known to the inbox.
    fn announce_start(&self) {
        self.starts
            .send_modify(|count| *count = count.wrapping_add(1));
    }
}

/// One file this node is sending, queued until `rd/file/1` is up.
struct SendJob {
    id: TransferId,
    path: std::path::PathBuf,
    /// Resume point: the last offset the receiver acked (§10).
    from: u64,
}

/// How a transfer ended, as the UI shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferState {
    /// Bytes are still moving.
    Running,
    /// Every byte arrived and the BLAKE3 of the offer matched (§9.2).
    Completed,
    /// Either side stopped it. Nothing was exported from staging.
    Cancelled,
    /// It ended without being cancelled and without verifying — a hash
    /// mismatch, a disk that refused, a stream that died.
    Failed,
}

/// One transfer as the UI lists it.
#[derive(Clone, Debug, serde::Serialize)]
pub struct TransferRow {
    /// Pseudonymized peer label (§15).
    pub peer_label: String,
    /// Identifier both sides agreed on in `FileTransferStart`.
    pub transfer_id: u64,
    /// Basename, after normalization on the receiving side.
    pub name: String,
    /// Total bytes the offer announced.
    pub size: u64,
    /// Bytes moved so far.
    pub moved: u64,
    /// Whether this node is the one receiving.
    pub incoming: bool,
    /// Where it is up to.
    pub state: TransferState,
    /// Whether this transfer started from the peer's clipboard rather than
    /// its file picker (docs/bugs/14-clipboard-files.md #3).
    pub from_clipboard: bool,
}

/// One offer waiting for this user's answer, as the UI lists it.
#[derive(Clone, Debug, serde::Serialize)]
pub struct OfferRow {
    /// Pseudonymized peer label (§15).
    pub peer_label: String,
    /// Basename, after normalization.
    pub name: String,
    /// Size the offer announced.
    pub size: u64,
    /// Whether this offer came from the peer's clipboard rather than its
    /// file picker (docs/bugs/14-clipboard-files.md #1, #3).
    pub from_clipboard: bool,
}

/// Everything the transfer panel draws in one poll.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct FileTransfersDto {
    /// Offers waiting for this user to accept or decline.
    pub offers: Vec<OfferRow>,
    /// Transfers running or recently ended.
    pub transfers: Vec<TransferRow>,
}

/// One thing that happened to a transfer, off the actor loop.
enum FileEvent {
    /// `rd/file/1` towards this peer is up — either this node dialed it, or
    /// the peer's dial was accepted and authorized.
    Connected {
        peer: NodeId,
        connection: Box<iroh::endpoint::Connection>,
    },
    /// The dial failed; anything queued for this peer has nowhere to go.
    ConnectFailed { peer: NodeId },
    /// One transfer moved. `moved` is the contiguous byte count, which is
    /// also the resume point (§10).
    Progress {
        peer: NodeId,
        id: TransferId,
        moved: u64,
    },
    /// One transfer reached an end this side can see.
    Finished {
        peer: NodeId,
        id: TransferId,
        state: TransferState,
    },
    /// A file accepted through `ClipboardFileAccept` finished measuring and
    /// hashing and can now start (docs/bugs/14-clipboard-files.md #2): the
    /// human decision already happened when the peer answered the clipboard
    /// offer, so this goes straight to `FileTransferStart` rather than
    /// through another `FileOffer`/`FileAccept` round trip.
    ClipboardTransferReady {
        peer: NodeId,
        path: std::path::PathBuf,
        name: String,
        size: u64,
        hash: [u8; 32],
    },
    /// The file behind an accepted clipboard entry could not be measured,
    /// hashed or named after all — moved or deleted since it was announced,
    /// say. Nothing starts.
    ClipboardPrepareFailed { peer: NodeId },
}

/// The actor's end of one live control connection.
struct ConnectionHandle {
    /// Distinguishes generations of connection to the same peer, so a stale
    /// `Closed` event cannot tear down a freshly established replacement.
    id: u64,
    outbound: mpsc::Sender<MessageKind>,
    /// Kept so this side can close the QUIC connection outright. Dropping the
    /// outbound sender alone only ends the writer task; the reader would sit
    /// in `recv` until the far end noticed.
    connection: iroh::endpoint::Connection,
    /// Whether this peer's `Hello` advertised `FEATURE_MEDIA_UNAVAILABLE`.
    ///
    /// A peer that did not must never be sent `MessageKind::MediaUnavailable`:
    /// it speaks an older minor, where that discriminant does not exist, and
    /// an undecodable frame closes the connection (§9.1). Always false on the
    /// guest side, which never sends the message at all.
    announces_media_faults: bool,
    /// Whether this peer's `Hello` advertised `FEATURE_REMOTE_SAS`, the same
    /// gate for `MessageKind::SasAck` (§9.1; ADR 0028). Always false on the
    /// host side, which never sends the request.
    speaks_remote_sas: bool,
    /// Whether this peer's `Hello` advertised `FEATURE_UNATTENDED`, the same
    /// gate for `MessageKind::UnattendedChallenge` and `UnattendedReject`
    /// (§9.1; ADR 0033). Always false on the guest side, which never sends
    /// either.
    speaks_unattended: bool,
}

/// Whether this build may send `ReceiverReport` towards a peer, from the only
/// two signals each side actually has (§9.1; ADR 0037).
///
/// A host learns it from the guest's `Hello` feature string; a guest has no
/// feature list to read, because `HelloAck` carries none, so it goes by the
/// minor version instead — the same asymmetry `FILE_TRANSFER_MINOR` documents.
#[derive(Debug, Clone, Copy, Default)]
struct ReceiverReports {
    /// Host side: the guest advertised [`FEATURE_RECEIVER_REPORT`].
    from_peer: bool,
    /// Guest side: the host answered with a minor of at least
    /// [`RECEIVER_REPORT_MINOR`].
    to_peer: bool,
}

/// Whether this build may send `StreamScaleRequest` towards a peer, and
/// whether it may act on one received from it — the same shape
/// [`ReceiverReports`] uses, for the same reason (D7,
/// docs/bugs/13-stream-resolution.md).
#[derive(Debug, Clone, Copy, Default)]
struct StreamScaleFeature {
    /// Host side: the guest advertised [`FEATURE_STREAM_SCALE`].
    from_peer: bool,
    /// Guest side: the host answered with a minor of at least
    /// [`STREAM_SCALE_MINOR`].
    to_peer: bool,
}

/// Whether this build may announce `DisplayModesList` towards a peer and act
/// on a `DisplaySetMode` from it, and whether it may send `DisplaySetMode`
/// towards it and trust an incoming `DisplayModesList` — the same shape
/// [`StreamScaleFeature`] uses, for the same reason (docs/bugs/
/// 16-host-display-mode.md; ADR 0048). Both messages are gated by the one
/// feature string: which one this node actually sends towards a given peer
/// depends only on whether it is that peer's host or its guest.
#[derive(Debug, Clone, Copy, Default)]
struct DisplayModeFeature {
    /// Host side: the guest advertised [`FEATURE_DISPLAY_MODE`].
    from_peer: bool,
    /// Guest side: the host answered with a minor of at least
    /// [`DISPLAY_MODE_MINOR`].
    to_peer: bool,
}

/// Host side: this host's own physical monitor has been switched at least
/// once this "reversibility window", and has not been restored yet
/// (docs/bugs/16-host-display-mode.md #3; ADR 0048).
///
/// Exactly one of these exists at a time, host-wide — the monitor is a
/// single physical resource, so there is one original to remember and one
/// current owner, never one per guest.
#[derive(Debug, Clone, Copy)]
struct DisplayModeState {
    /// The mode this monitor was set up with before the *first* switch of
    /// this window. Every later switch inside the same window is still
    /// undone back to this one, never to whatever the immediately preceding
    /// switch was.
    original: lumepeer_media::capture::DisplayMode,
    /// The peer whose request most recently moved the mode — restoring on
    /// disconnect or on that peer losing the `display_mode` grant only makes
    /// sense against whoever is actually responsible right now.
    owner: NodeId,
    /// When the most recent switch was applied, for the auto-revert
    /// timeout's health window.
    armed_at: std::time::Instant,
    /// Bumped on every switch; a confirm-timeout event carries the
    /// generation it was armed for, so a timer from an already-superseded or
    /// already-restored switch is a no-op instead of reverting a mode
    /// nothing here still owns.
    generation: u64,
}

/// Host side: one running capture/encode loop, plus the connection it writes
/// on, so a revoke can stop both without waiting for the loop to notice.
/// `recorder` is the §17 slot the actor swaps a session recorder into; it
/// lives as long as the media session itself.
struct MediaSession {
    task: tokio::task::JoinHandle<()>,
    connection: iroh::endpoint::Connection,
    #[allow(
        dead_code,
        reason = "kept for the actor to swap recorders into mid-session"
    )]
    recorder: crate::view::SharedRecorder,
    /// The keyframe requests and receiver reports this peer sends, and the
    /// quality target the loop settled on, in the one place both sides can
    /// reach (§11; ADR 0037).
    control: EncodeControl,
}

impl MediaSession {
    /// Stops the loop and closes the media connection.
    fn stop(self) {
        self.task.abort();
        self.connection.close(
            lumepeer_net::connection::CLOSE_MALFORMED.into(),
            lumepeer_net::error::close_code::MALFORMED.as_bytes(),
        );
    }
}

/// Host side: one running audio loop for a peer, with its own stop flag.
///
/// Audio is opt-in per session: the host user turns it on (`AudioStart` of
/// §11) and off again, and every teardown path — revoke, disconnect, media
/// session replacement — flips the flag and aborts the task.
struct AudioSession {
    stop: Arc<AtomicBool>,
    /// The §17 recorder slot, swapped by the actor mid-session; the loop picks
    /// up whatever is here when it encodes its next packet.
    recorder: crate::view::SharedRecorder,
    task: tokio::task::JoinHandle<()>,
}

impl AudioSession {
    /// Stops capture and ends the loop.
    fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        self.task.abort();
    }

    /// Swaps the session recorder in or out (§17).
    fn set_recorder(&mut self, recorder: Option<Arc<crate::recorder::SessionRecorder>>) {
        *self
            .recorder
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = recorder;
    }
}

/// Host side: one guest-microphone playout loop, with its own stop flag
/// (§11; ADR 0028).
///
/// The guest opens the tagged `M` media stream when its user turns the mic
/// on; this side accepts it, decodes Opus and pushes PCM into the speakers.
/// Opt-out is the stream ending (the guest turned the mic off), which the
/// accept loop notices by itself; the entry here only bounds the task.
struct MicSession {
    task: tokio::task::JoinHandle<()>,
}

impl MicSession {
    /// Ends the playout loop.
    fn stop(self) {
        self.task.abort();
    }
}

/// Guest side: one open remote-view window and the pipeline feeding it.
struct ViewState {
    /// Tauri window label, `view-{peer label}`.
    label: String,
    /// Role the host announced, kept so the history row written when this view
    /// closes says what was actually granted.
    role: Role,
    /// Grants as the host last announced them. Advisory on this side: the host
    /// re-checks every event (§2.3).
    grants: Grants,
    /// `grants.input` as the frame poll reads it, without the actor. Written
    /// here, on grant and on role change; the entry is removed on revoke.
    input: Arc<AtomicBool>,
    /// Whether the host announced it is recording, as the frame poll reads it
    /// (§17). Written when a `RecordAck` arrives.
    recording: Arc<AtomicBool>,
    /// The host's cursor, as the cursor poll reads it (§11). Written when a
    /// `CursorShape` arrives, and never inferred: a host that composites its
    /// cursor into the picture sends none, and this stays `None`.
    cursor: Arc<std::sync::RwLock<Option<CursorFeed>>>,
    /// The host's monitors, as it announced them when it granted the session
    /// (§11 `MonitorsList`; ADR 0028).
    ///
    /// Only ever what arrived on the wire. This node's own displays are not a
    /// fallback and never stand in for the host's: an empty list means the
    /// host announced nothing, and the picker says so rather than offering
    /// screens that belong to the machine the operator is already sitting at.
    monitors: Vec<MonitorInfo>,
    /// The host's own display modes, as it last announced them (docs/bugs/
    /// 16-host-display-mode.md #2; ADR 0048). Empty exactly when
    /// `display_modes_reason` is `Some`, mirroring the wire message's own
    /// invariant.
    display_modes: Vec<DisplayModeInfo>,
    /// Why `display_modes` is empty, when it is.
    display_modes_reason: Option<DisplayModeUnavailableReason>,
    /// Single-slot newest picture plus pipeline health. Dropping this receiver
    /// is also how the media task learns the view is gone.
    slot: watch::Receiver<ViewSlot>,
    /// The other end of `slot`, shared with the media task.
    ///
    /// Held here too because one thing the window has to show does not come
    /// from the media pipeline at all: a host announcing it cannot produce a
    /// picture says so on the *control* stream, which only the actor reads
    /// (docs/adr/0024).
    slot_tx: Arc<watch::Sender<ViewSlot>>,
    task: tokio::task::JoinHandle<()>,
    /// The media connection the picture rides. Written by the media task
    /// once dialed, read by the mic toggle; `None` until the first dial
    /// lands and after the media task ends.
    media_connection: Arc<std::sync::Mutex<Option<iroh::endpoint::Connection>>>,
}

impl ViewState {
    /// The media connection the picture rides, for a second tagged stream
    /// (the guest's own microphone; §11; ADR 0028).
    ///
    /// The mic stream must ride the *same* `rd/media/1` connection as the
    /// picture: a second connection is indistinguishable on the host from a
    /// redial, and the host's accept path would replace the encode loop
    /// (§4.1). `None` means the media task has not landed a dial yet — the
    /// toolbar's mic press is refused and can be pressed again once a
    /// picture is showing.
    fn media_connection(&self) -> Option<iroh::endpoint::Connection> {
        self.media_connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// What a spawned per-connection task reports back to the main loop.
enum ActorEvent {
    /// A guest's `Hello` passed the handshake and the ed25519 signature
    /// check; the ticket still needs `TicketRegistry::claim` before any
    /// consent is queued, which only the actor's own thread can do.
    Handshaked {
        connection: Box<ControlConnection>,
        peer: NodeId,
        ticket: InviteTicket,
        /// Whether the guest's `Hello` advertised `FEATURE_MEDIA_UNAVAILABLE`.
        announces_media_faults: bool,
        /// Whether the guest's `Hello` advertised `FEATURE_REMOTE_SAS`
        /// (§9.1; ADR 0028).
        speaks_remote_sas: bool,
        /// Whether the guest's `Hello` advertised `FEATURE_FILE_TRANSFER`
        /// (§9.1; ADR 0032).
        speaks_file_transfer: bool,
        /// Whether the guest's `Hello` advertised `FEATURE_UNATTENDED`
        /// (§9.1; ADR 0033).
        speaks_unattended: bool,
        /// Whether the guest's `Hello` advertised `FEATURE_RECEIVER_REPORT`
        /// (§9.1; ADR 0037).
        speaks_receiver_report: bool,
        /// Whether the guest's `Hello` advertised `FEATURE_CURSOR_SHAPE`
        /// (§11), so this host may stop compositing the cursor for it.
        speaks_cursor_shape: bool,
        /// Whether the guest's `Hello` advertised `FEATURE_STREAM_SCALE` (D7,
        /// docs/bugs/13-stream-resolution.md).
        speaks_stream_scale: bool,
        /// Whether the guest's `Hello` advertised `FEATURE_CLIPBOARD_FILES`
        /// (docs/bugs/14-clipboard-files.md #2; ADR 0047).
        speaks_clipboard_files: bool,
        /// Whether the guest's `Hello` advertised `FEATURE_DISPLAY_MODE`
        /// (docs/bugs/16-host-display-mode.md; ADR 0048).
        speaks_display_mode: bool,
    },
    /// A live connection delivered a control message.
    Inbound {
        peer: NodeId,
        id: u64,
        kind: MessageKind,
    },
    /// A live connection's stream closed or errored.
    Closed { peer: NodeId, id: u64 },
    /// A control stream ended on a §9.1 framing violation, with the close code
    /// §18 names it by. Separate from [`ActorEvent::Closed`], which every
    /// ordinary hang-up also produces: only a violation belongs in the audit
    /// log (§15).
    Violation { peer: NodeId, code: &'static str },
    /// An incoming `rd/media/1` connection finished its QUIC handshake. It has
    /// proven nothing beyond its `NodeId`; whether it may exist at all is a
    /// question only the actor can answer, since only the actor knows which
    /// peers hold a live, granted control session (§4.1).
    MediaAccepted {
        connection: Box<iroh::endpoint::Connection>,
        peer: NodeId,
    },
    /// Guest side: an outgoing dial started by [`Actor::spawn_dial`] finished,
    /// one way or the other (ADR 0027).
    ///
    /// The dial runs off the actor loop, so this is how its result gets back
    /// to the one thread allowed to store a connection or move the connect
    /// phase. `peer` is what the ticket claimed, which is also what the
    /// handshake proved when `result` is `Ok`.
    Dialed {
        peer: NodeId,
        /// The invite code the dial used, kept so a successful connection can
        /// still be recorded in the remembered-hosts list (ADR 0016).
        code: String,
        addr: Box<iroh::EndpointAddr>,
        result: Result<Box<ControlConnection>, NetError>,
    },
    /// Something happened to a file transfer, on one of its own tasks.
    File(FileEvent),
    /// This node's own clipboard file list was read and measured, off the
    /// actor loop (docs/bugs/14-clipboard-files.md #2; ADR 0027).
    ClipboardFilesRead {
        peer: NodeId,
        files: Vec<ClipboardFileEntry>,
        /// The real local paths behind `files`, in the same order — never
        /// sent on the wire (§15), kept so an accept can be traced back to
        /// the file it named.
        paths: Vec<std::path::PathBuf>,
    },
    /// A display-mode switch's confirmation window elapsed off the actor
    /// loop (docs/bugs/16-host-display-mode.md #3; ADR 0048). `generation`
    /// ties this to the switch that armed it; the actor checks both that its
    /// own `display_mode_state` still matches this generation and that
    /// capture has answered successfully since the switch, before deciding
    /// whether there is anything left to revert.
    DisplayModeConfirmTimeout { generation: u64 },
}

/// Outcome of one accepted incoming connection, before the actor sees it.
enum Accepted {
    /// Control ALPN: handshake ran and the invite ticket verified.
    Control {
        connection: Box<ControlConnection>,
        peer: NodeId,
        ticket: Box<InviteTicket>,
        /// Whether the guest's `Hello` advertised `FEATURE_MEDIA_UNAVAILABLE`.
        announces_media_faults: bool,
        /// Whether the guest's `Hello` advertised `FEATURE_REMOTE_SAS`
        /// (§9.1; ADR 0028).
        speaks_remote_sas: bool,
        /// Whether the guest's `Hello` advertised `FEATURE_FILE_TRANSFER`
        /// (§9.1; ADR 0032).
        speaks_file_transfer: bool,
        /// Whether the guest's `Hello` advertised `FEATURE_UNATTENDED`
        /// (§9.1; ADR 0033).
        speaks_unattended: bool,
        /// Whether the guest's `Hello` advertised `FEATURE_RECEIVER_REPORT`
        /// (§9.1; ADR 0037).
        speaks_receiver_report: bool,
        /// Whether the guest's `Hello` advertised `FEATURE_CURSOR_SHAPE`
        /// (§11).
        speaks_cursor_shape: bool,
        /// Whether the guest's `Hello` advertised `FEATURE_STREAM_SCALE` (D7,
        /// docs/bugs/13-stream-resolution.md).
        speaks_stream_scale: bool,
        /// Whether the guest's `Hello` advertised `FEATURE_CLIPBOARD_FILES`
        /// (docs/bugs/14-clipboard-files.md #2; ADR 0047).
        speaks_clipboard_files: bool,
        /// Whether the guest's `Hello` advertised `FEATURE_DISPLAY_MODE`
        /// (docs/bugs/16-host-display-mode.md; ADR 0048).
        speaks_display_mode: bool,
    },
    /// Media ALPN: authenticated only, nothing decided.
    Media {
        connection: Box<iroh::endpoint::Connection>,
        peer: NodeId,
    },
    /// File ALPN: authenticated only, nothing decided — exactly like media,
    /// and for the same reason (§4.1, §2.3).
    File {
        connection: Box<iroh::endpoint::Connection>,
        peer: NodeId,
    },
}

/// The audit log as the actor holds it: a sink plus the persistent salt every
/// record's peer hash is mixed with (§15; ADR 0041).
///
/// The salt is *not* [`Actor::install_salt`] and must not be made to be. That
/// one is regenerated on every start precisely so a displayed label cannot be
/// correlated across runs; an audit log needs the opposite, or two visits by
/// the same device read as two devices. It lives in the keystore and is minted
/// once.
struct AuditContext {
    sink: Box<dyn lumepeer_core::audit::AuditSink>,
    salt: [u8; 32],
}

impl std::fmt::Debug for AuditContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuditContext").finish_non_exhaustive()
    }
}

/// Runtime state the actor owns and loops over.
struct Actor {
    rx: mpsc::Receiver<ActorCommand>,
    sessions: SessionManager,
    /// Per-process salt for pseudonymized labels (§15): regenerated on every
    /// start, so a label is stable within a run and meaningless across runs.
    install_salt: [u8; 32],
    /// Where audit records go, when a log could be opened at all (§15).
    ///
    /// `None` is a working host with no audit trail, not a broken one: §18
    /// says a storage failure degrades the feature, never the session.
    audit: Option<AuditContext>,
    /// label -> `NodeId`, rebuilt on every command that changes session state.
    labels: std::collections::HashMap<String, NodeId>,
    endpoint: PeerEndpoint,
    identity: SigningKey,
    tickets: TicketRegistry,
    /// Live control connections, keyed by peer, so `Grant`/`Revoke` can
    /// write `ConsentGrant`/`ConsentRevoke` back to the right stream.
    connections: std::collections::HashMap<NodeId, ConnectionHandle>,
    next_connection_id: u64,
    /// Caps concurrent handshakes so a flood of half-open connections cannot
    /// spawn unbounded tasks (§3.2).
    handshake_slots: Arc<Semaphore>,
    /// Handshake results, inbound messages and stream-closed notifications
    /// from the per-connection tasks.
    events_tx: mpsc::Sender<ActorEvent>,
    events_rx: mpsc::Receiver<ActorEvent>,
    /// Encode loops report here when they cannot produce a picture at all.
    /// Separate from `events_tx` so `crate::view` stays free of the actor's
    /// own event type (§18, docs/adr/0024).
    faults_tx: mpsc::Sender<MediaFault>,
    faults_rx: mpsc::Receiver<MediaFault>,
    /// Guest side: media receivers report what they got here, for the same
    /// reason faults have their own channel — a decode loop has no control
    /// connection of its own, and only the actor may write on one (§2.3).
    reports_tx: mpsc::Sender<(NodeId, MediaReport)>,
    reports_rx: mpsc::Receiver<(NodeId, MediaReport)>,
    /// Per-peer control-channel round trip, from this session's own
    /// `Ping`/`Pong` (§9.1).
    rtt: std::collections::HashMap<NodeId, RttTracker>,
    /// Newest `(loss_permille, goodput_kbps)` known for a peer's link.
    ///
    /// Written on whichever side is receiving pictures: the host stores what
    /// the guest reported, the guest stores what it measured itself. Neither
    /// invents the other's half.
    reception: std::collections::HashMap<NodeId, (u16, u32)>,
    /// Host side: when this host last honoured a `KeyframeRequest` from a
    /// peer, so a guest cannot decide what the uplink is spent on (§11).
    last_keyframe: std::collections::HashMap<NodeId, std::time::Instant>,
    /// Which side of the `ReceiverReport` exchange each peer can speak
    /// (§9.1; ADR 0037).
    receiver_reports: std::collections::HashMap<NodeId, ReceiverReports>,
    /// Which side of the `StreamScaleRequest` exchange each peer can speak
    /// (§9.1; D7, docs/bugs/13-stream-resolution.md).
    stream_scale: std::collections::HashMap<NodeId, StreamScaleFeature>,
    /// Which side of the `DisplayModesList`/`DisplaySetMode` exchange each
    /// peer can speak (§9.1; docs/bugs/16-host-display-mode.md; ADR 0048).
    display_mode: std::collections::HashMap<NodeId, DisplayModeFeature>,
    /// Host side: the host's own monitor's original mode, and who is
    /// responsible for restoring it, while a switch this session made has
    /// not been undone yet (docs/bugs/16-host-display-mode.md #3; ADR 0048).
    /// `None` when the monitor is in whatever mode it started this process
    /// in.
    display_mode_state: Option<DisplayModeState>,
    /// Monotonic counter identifying the most recent display-mode switch,
    /// so a late confirm-timeout event can tell whether it is still about
    /// the switch that armed it (docs/bugs/16-host-display-mode.md #3).
    display_mode_generation: u64,
    /// Host side: peers whose `Hello` advertised `FEATURE_CURSOR_SHAPE`, and
    /// which may therefore be sent one (§11).
    speaks_cursor_shape: std::collections::HashSet<NodeId>,
    /// Host side: cursor shapes the encode loops picked up, on their way to
    /// the control channel only this thread may write (§2.3).
    cursors_tx: mpsc::Sender<(NodeId, CursorShapeData)>,
    cursors_rx: mpsc::Receiver<(NodeId, CursorShapeData)>,
    /// What this host knows about its own ability to produce a picture.
    health: Arc<MediaHealth>,
    notify: broadcast::Sender<ActorNotification>,
    /// Host side: the single "capture only with a viewer" gate of §8.1/§11,
    /// shared with every encode loop.
    capture: SharedCapture,
    /// Host side: one encode loop per peer currently receiving video.
    media: std::collections::HashMap<NodeId, MediaSession>,
    /// Host side: one audio loop per peer the host user enabled sound for.
    /// Opt-in per §11's `AudioStart`; dies with the session like everything
    /// else per-peer.
    audio: std::collections::HashMap<NodeId, AudioSession>,
    /// Host side: one guest-mic playout loop per peer the guest enabled their
    /// microphone for (§11; ADR 0028). The guest opens the tagged `M` media
    /// stream; this side accepts it and plays it on the speakers.
    guest_mic: std::collections::HashMap<NodeId, MicSession>,
    /// Whether this peer's `Hello` advertised `FEATURE_REMOTE_SAS`, so a
    /// `SasAck` may be sent back to it (§9.1: never send what an older minor
    /// would decode as malformed).
    speaks_remote_sas: std::collections::HashSet<NodeId>,
    /// Host side: live session recordings keyed by peer (§17). Gated on the
    /// independent `recording` grant; flushed and dropped when the session
    /// ends so no recorder can outlive what it was allowed to record.
    recorders: HashMap<NodeId, Arc<crate::recorder::SessionRecorder>>,
    /// Host side: guests whose `RecordRequest` is still waiting for the host
    /// user's answer (§17).
    ///
    /// A set, not a queue: a guest that asks twice does not get two dialogs,
    /// and there is nothing to answer out of order. Cleared when the host
    /// answers either way, and when the session ends.
    record_requests: std::collections::HashSet<NodeId>,
    /// Host side: per-peer budget for `RecordRequest` (§9.2).
    ///
    /// The same limiter the consent path uses, on purpose: asking to be
    /// recorded puts a dialog in front of the host user exactly like asking
    /// for consent does, so it gets the same per-peer budget rather than a
    /// second counter with its own rules.
    record_request_rate: ConsentRateLimiter,
    /// Host side: platform input adapter, opened on the first authorized event
    /// so a host that never grants `input` never touches it.
    injector: Option<Box<dyn InputInjector>>,
    /// Guest side: one open view window per host being watched.
    views: std::collections::HashMap<NodeId, ViewState>,
    /// The same windows as `views`, in the form the IPC layer reads directly.
    /// Kept in step by `start_view` and `stop_view`, which are the only two
    /// places a view begins or ends.
    view_feeds: ViewFeeds,
    /// Guest side: dialable address per host, remembered from its invite so the
    /// media dial does not have to wait for discovery.
    host_addrs: std::collections::HashMap<NodeId, iroh::EndpointAddr>,
    /// Guest side: the invite code used to reach each host, kept so the history
    /// row written when the session ends can dial it again (ADR 0016).
    host_invites: std::collections::HashMap<NodeId, String>,
    /// Per-peer chat transcripts (§9.2), dropped when the session ends.
    chat: ChatLog,
    /// Per-peer clipboard sync state (§9.2): echo suppression both ways.
    clipboard: std::collections::HashMap<NodeId, ClipboardSync>,
    /// Newest inbound clipboard payload per peer, for the UI to pull.
    clipboard_inbound: std::collections::HashMap<NodeId, String>,
    /// Whether this peer's `Hello` (host side) or `HelloAck` minor (guest
    /// side) says it understands `FileTransferStart` (§9.1; ADR 0032).
    speaks_file_transfer: std::collections::HashSet<NodeId>,
    /// Whether this peer's `Hello` (host side) or `HelloAck` minor (guest
    /// side) says it understands `ClipboardFileOffer`/`ClipboardFileAccept`
    /// (docs/bugs/14-clipboard-files.md #2; ADR 0047).
    speaks_clipboard_files: std::collections::HashSet<NodeId>,
    /// Local paths behind the entries of this node's last `ClipboardFileOffer`
    /// to a peer, popped one at a time as each `ClipboardFileAccept` answers
    /// (docs/bugs/14-clipboard-files.md #2). Never sent on the wire — only
    /// the name and size in the entry are (§15).
    clipboard_offers_out:
        std::collections::HashMap<NodeId, std::collections::VecDeque<std::path::PathBuf>>,
    /// The `rd/file/1` connection per peer, opened lazily and only after an
    /// accepted offer (§4).
    file_conns: std::collections::HashMap<NodeId, iroh::endpoint::Connection>,
    /// Peers whose file connection is being dialed right now, so a second
    /// accepted offer does not start a second dial.
    file_dialing: std::collections::HashSet<NodeId>,
    /// Offers this node has made and not yet heard back on.
    file_offers_out: std::collections::HashMap<NodeId, std::collections::VecDeque<OutgoingOffer>>,
    /// Offers that arrived and are waiting for this user to answer, either
    /// through the sender's file picker or its clipboard (docs/bugs/
    /// 14-clipboard-files.md #2).
    file_offers_in: std::collections::HashMap<NodeId, std::collections::VecDeque<IncomingOffer>>,
    /// Offers this user accepted, waiting for the sender to name them.
    file_accepted: std::collections::HashMap<NodeId, std::collections::VecDeque<AcceptedOffer>>,
    /// Receiver-side state per peer, shared with its stream readers.
    file_channels: std::collections::HashMap<NodeId, FileChannel>,
    /// Sends queued until `rd/file/1` is up.
    file_pending_sends: std::collections::HashMap<NodeId, Vec<SendJob>>,
    /// Every transfer this node knows about, keyed by peer and id.
    file_transfers: std::collections::HashMap<(NodeId, TransferId), TransferRow>,
    /// Where an *incoming* transfer's bytes actually land, and whether it
    /// came from a clipboard offer — kept out of `TransferRow` deliberately,
    /// since that DTO reaches the webview and a destination is a path on
    /// this machine (§15). Consulted once, when the transfer finishes, to
    /// decide whether to put the path on this machine's own clipboard
    /// (docs/bugs/14-clipboard-files.md #3).
    file_receive_destinations:
        std::collections::HashMap<(NodeId, TransferId), (std::path::PathBuf, bool)>,
    /// Tasks pushing bytes, so an abort can stop one without waiting for the
    /// stream to notice.
    file_send_tasks: std::collections::HashMap<(NodeId, TransferId), tokio::task::JoinHandle<()>>,
    /// Next transfer id this node hands out. The *sender* names a transfer
    /// (ADR 0032), so this counter is only ever read on the sending side.
    next_transfer_id: TransferId,
    /// The machine's own clipboard, on its own thread (§9.2; ADR 0030).
    /// Reads happen only while a session holds `clipboard_read`; writes are
    /// how a peer's authorized payload actually lands on this desktop.
    clipboard_worker: crate::clipboard_os::ClipboardWorker,
    /// Local clipboard changes the watcher saw, delivered here rather than
    /// read on the actor loop: an X11 clipboard read is a round trip to
    /// another application, and one wedged application must not be able to
    /// delay a revoke (ADR 0027).
    clipboard_changes: mpsc::Receiver<crate::clipboard_os::ClipboardChange>,
    /// How view windows are created and closed, and how the host's
    /// always-on-top session bar goes up and down.
    windows: Arc<dyn ViewWindows>,
    /// Whether the session bar is up right now, so `reconcile_host_bar` can
    /// tell a change from the steady state and stay silent on every turn that
    /// did not move a session.
    host_bar_up: bool,
    /// Guest side: hosts this node has connected to before (§21 punch-list
    /// item 5). Nothing is recorded on the host side — see
    /// `connection_history`'s module docs.
    history: ConnectionHistory,
    /// Host side: the unattended credentials of §8, and the only thing that
    /// decides an unattended admission (ADR 0033).
    ///
    /// Owned by the actor rather than shared, for the same reason
    /// `SessionManager` is: the lockout counter inside it must see every
    /// attempt in one order, and a second holder could not be given a copy
    /// without giving away a second lockout budget with it.
    unattended: UnattendedAccess,
    /// Where those credentials are kept between runs (the OS keystore).
    unattended_store: UnattendedStore,
    /// Host side: peers that were offered a credential challenge and have not
    /// been admitted yet. A peer absent from here has its `UnattendedAuth`
    /// ignored, which is what stops credentials being replayed at a session
    /// that already exists or at one that was never offered the path.
    unattended_pending: std::collections::HashSet<NodeId>,
    /// Whether this peer's `Hello` advertised `FEATURE_UNATTENDED` (§9.1;
    /// ADR 0033), recorded before `on_handshaked` runs.
    speaks_unattended: std::collections::HashSet<NodeId>,
    /// Host side: saved devices and which of them are trusted (§8; ADR 0034).
    address_book: AddressBookStore,
    /// Guest side: whether the challenge this node is answering asked for a
    /// one-time code, so the connect form knows to show the field.
    connect_code_required: bool,
    /// Guest side: how long the host said to wait before trying again, from a
    /// `LockedOut` rejection. Seconds, and only ever the host's own number.
    connect_retry_secs: Option<u64>,
    /// Guest side: phase of this node's own outgoing connect attempt.
    connect_phase: ConnectPhase,
    /// Host the phase above is about, once the dial has resolved one.
    connect_peer: Option<NodeId>,
    /// Why the last attempt failed, as the §18 code the UI shows. Set only
    /// alongside `ConnectPhase::Failed`; cleared when a new attempt starts.
    ///
    /// The dial no longer runs inside the IPC call, so its error cannot come
    /// back as the call's own `Err` any more — without this the user would be
    /// told "could not connect" and nothing else, which is the report ADR 0026
    /// was written about (ADR 0027).
    connect_failure: Option<&'static str>,
    /// Guest side: remembered device passwords, one per host (§8; ADR 0033;
    /// docs/bugs/02-connect-form.md, task 6; docs/bugs/DECISIONS.md D2).
    remembered_passwords: RememberedPasswordStore,
    /// Guest side: the password of an outstanding `unattended_submit`, held
    /// only until the host answers — saved to the keystore on a grant,
    /// dropped on a refusal. Never read back out of this field; the one copy
    /// that matters afterwards lives in the keystore, not here.
    pending_remember: Option<String>,
    /// Guest side: whether the credential attempt in flight was started
    /// automatically from a remembered password, rather than by the user
    /// submitting the form. The connect form uses this to keep the modal from
    /// flashing open for a host it already knows the password to.
    connect_credentials_auto: bool,
}

impl Actor {
    fn label_of(&self, peer: &NodeId) -> String {
        peer_tag(&self.install_salt, peer)
    }

    /// Records one audit event against `peer` (§15; ADR 0041).
    ///
    /// Never blocks and never fails: the sink queues, and a host with no
    /// usable log simply has none. Nothing on the consent path may wait on
    /// disk, which is what `AuditSink`'s contract says in words.
    ///
    /// Wall-clock time on purpose — an audit record is evidence, and §12.3's
    /// rollback defence belongs to licensing, not to this.
    fn audit(&mut self, peer: &NodeId, event: lumepeer_core::audit::AuditEvent) {
        let Some(context) = self.audit.as_mut() else {
            return;
        };
        let record = lumepeer_core::audit::AuditRecord {
            peer_hash: lumepeer_core::audit::peer_hash(&context.salt, peer),
            at_unix_secs: unix_now_secs(),
            event,
        };
        context.sink.append(record);
    }

    fn resolve(&self, label: &str) -> Result<NodeId, ActorError> {
        self.labels
            .get(label)
            .copied()
            .ok_or(ActorError::UnknownPeer)
    }

    /// Rebuilds the label table from current pending + active peers, and
    /// returns the snapshot list in the same pass.
    ///
    /// A session in `Reconnecting` is deliberately omitted: `SessionManager::
    /// active()` still returns it (it holds its slot against the plan ceiling
    /// for the reconnect window of §10), but its transport is gone, so showing
    /// it as `Active` would tell the host user that a guest is watching when
    /// nobody is.
    fn rebuild_labels_and_snapshot(&mut self) -> Vec<SessionSnapshot> {
        self.labels.clear();
        let mut out = Vec::new();
        for ticket in self.sessions.pending() {
            let label = peer_tag(&self.install_salt, &ticket.peer);
            self.labels.insert(label.clone(), ticket.peer);
            out.push(SessionSnapshot {
                label,
                state: SessionStateDto::Pending,
                role: ticket.requested_role,
                input: false,
                grants: Grants::default(),
                recording_active: false,
                record_request: false,
                secure_desktop_active: false,
            });
        }
        for (peer, role, grants) in self.sessions.active() {
            if self.sessions.state(&peer) != SessionState::Active {
                continue;
            }
            let label = peer_tag(&self.install_salt, &peer);
            self.labels.insert(label.clone(), peer);
            out.push(SessionSnapshot {
                label,
                state: SessionStateDto::Active,
                role,
                input: grants.input,
                grants,
                recording_active: self.recorders.contains_key(&peer),
                record_request: self.record_requests.contains(&peer),
                secure_desktop_active: self
                    .media
                    .get(&peer)
                    .is_some_and(|s| s.control.secure_desktop_active()),
            });
        }
        // Host side: every saved device gets a label too, whether or not it is
        // connected. Without this the address book panel could list a device
        // and then fail to act on it: every command names a peer by label, and
        // a label nothing registered resolves to nothing (§13).
        let saved: Vec<NodeId> = self.address_book.book().peers().map(|(p, _)| p).collect();
        for peer in saved {
            let label = peer_tag(&self.install_salt, &peer);
            self.labels.insert(label, peer);
        }
        // Guest side: a host being watched has no entry in this node's own
        // `SessionManager` (it is not *our* guest), but its view window still
        // has to be able to name it over IPC — `view_next_frame`, the input
        // commands and the window's own close/revoke all go through a label.
        for peer in self.views.keys().copied().collect::<Vec<_>>() {
            let label = peer_tag(&self.install_salt, &peer);
            self.labels.insert(label, peer);
        }
        out
    }

    async fn run(mut self) {
        // The keepalive of §9.1, and the only source of the round trip the
        // diagnostics panel shows. It fires immediately on the first tick,
        // which is what gets a number on screen without a 20-second wait.
        let mut ping = tokio::time::interval(Duration::from_secs(PING_INTERVAL_SECS));
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                command = self.rx.recv() => {
                    let Some(command) = command else { break };
                    self.handle_command(command);
                }
                // Single await, so losing this race cannot drop a connection
                // that is already half accepted.
                incoming = self.endpoint.accept_incoming() => {
                    let Some(incoming) = incoming else { break };
                    self.spawn_handshake(incoming);
                }
                event = self.events_rx.recv() => {
                    if let Some(event) = event {
                        self.handle_event(event);
                    }
                }
                fault = self.faults_rx.recv() => {
                    if let Some((peer, reason)) = fault {
                        self.on_media_fault(peer, reason);
                    }
                }
                change = self.clipboard_changes.recv() => {
                    match change {
                        Some(crate::clipboard_os::ClipboardChange::Text(text)) => {
                            self.on_local_clipboard(&text);
                        }
                        Some(crate::clipboard_os::ClipboardChange::Files(paths)) => {
                            self.on_local_clipboard_files(&paths);
                        }
                        None => {}
                    }
                }
                report = self.reports_rx.recv() => {
                    if let Some((peer, report)) = report {
                        self.on_media_report(peer, report);
                    }
                }
                cursor = self.cursors_rx.recv() => {
                    if let Some((peer, shape)) = cursor {
                        self.on_cursor_shape(peer, shape);
                    }
                }
                _ = ping.tick() => self.send_pings(),
            }
            // One place, after every turn, rather than at each of the half
            // dozen paths that start or end a session: a consent, a revoke, a
            // disconnect and a session that simply timed out all have to move
            // the bar, and a path that forgot to would leave the host looking
            // at a bar for a guest who left.
            self.reconcile_host_bar();
        }
        // Nothing is being served any more, so nothing is left to show.
        self.windows.set_host_bar(false);
    }

    /// Puts the host's session bar up while at least one guest is connected,
    /// and takes it down when the last one leaves.
    ///
    /// Counted the same way `session_status` counts, so the bar is up exactly
    /// when the connections list would have a row in it. A session inside its
    /// reconnect window is deliberately not `Active` and does not hold the
    /// bar up: a bar that stayed after the guest's link dropped would be
    /// claiming someone is watching when nobody is.
    fn reconcile_host_bar(&mut self) {
        let connected = self
            .sessions
            .active()
            .into_iter()
            .any(|(peer, _, _)| self.sessions.state(&peer) == SessionState::Active);
        if connected != self.host_bar_up {
            self.host_bar_up = connected;
            self.windows.set_host_bar(connected);
        }
    }

    /// Sends one `Ping` to every live connection and remembers its nonce.
    ///
    /// A ping that was never answered is simply replaced: this is a
    /// measurement, not a second liveness watchdog on top of QUIC's own and
    /// `RECONNECT_WINDOW_SECS` (§9.1, §10).
    fn send_pings(&mut self) {
        let peers: Vec<NodeId> = self.connections.keys().copied().collect();
        for peer in peers {
            let nonce = rand::rng().next_u64();
            self.rtt.entry(peer).or_default().sent(nonce);
            self.send_to(&peer, MessageKind::Ping(nonce));
        }
    }

    /// Guest side: turns what the media receiver measured into the control
    /// messages only this thread may write (§2.3).
    ///
    /// A report reaches the host only when the host can decode it: an older
    /// peer would read the unknown discriminant as a malformed frame and close
    /// the connection (§9.1), so a link to one keeps the host-local estimate
    /// of ADR 0015 and nothing is lost but precision.
    fn on_media_report(&mut self, peer: NodeId, report: MediaReport) {
        match report {
            MediaReport::Feedback {
                loss_permille,
                goodput_kbps,
            } => {
                // Kept whatever the far side speaks: this is also what this
                // node's own diagnostics panel shows about the link it is
                // watching.
                self.reception.insert(peer, (loss_permille, goodput_kbps));
                if !self.may_report_to(&peer) {
                    return;
                }
                let rtt_ms = self.rtt.get(&peer).and_then(RttTracker::smoothed);
                self.send_to(
                    &peer,
                    MessageKind::ReceiverReport {
                        loss_permille,
                        rtt_ms: rtt_ms.unwrap_or(0),
                        goodput_kbps,
                    },
                );
            }
            MediaReport::KeyframeNeeded => {
                self.send_to(&peer, MessageKind::KeyframeRequest);
            }
        }
    }

    /// Host side: one changed cursor shape, on its way to the guest drawing
    /// it (§11).
    ///
    /// Only ever sent to a peer that advertised `FEATURE_CURSOR_SHAPE`: a
    /// guest that cannot draw a cursor and is no longer sent one in the
    /// picture would have no cursor at all.
    fn on_cursor_shape(&mut self, peer: NodeId, shape: CursorShapeData) {
        if !self.speaks_cursor_shape.contains(&peer) {
            return;
        }
        self.send_to(&peer, MessageKind::CursorShape { shape });
    }

    /// Host side: whether the cursor may travel on its own channel right now.
    ///
    /// One capture backend feeds every viewer, so this is an all-or-nothing
    /// decision, and it takes two things of *every* peer currently receiving a
    /// picture:
    ///
    /// - it advertised `FEATURE_CURSOR_SHAPE`, so it can draw the cursor. One
    ///   older guest among them and the cursor goes back into the frame for
    ///   all of them: a guest that cannot draw it must never be left with a
    ///   screen that has no pointer on it.
    /// - it holds the `input` grant, so the pointer it would draw is the one
    ///   it is moving. A view-only guest is watching someone else work, and
    ///   drawing at its own pointer would show a cursor that has nothing to do
    ///   with the one on the host's screen — worse than the latency the
    ///   channel exists to remove (ADR 0038).
    fn refresh_cursor_embedding(&mut self) {
        let separate = !self.media.is_empty()
            && self.media.keys().all(|peer| {
                self.speaks_cursor_shape.contains(peer)
                    && self.sessions.grants(peer).is_some_and(|g| g.input)
            });
        lock_capture(&self.capture).set_cursor_embedded(!separate);
    }

    /// Guest side: whether `peer` speaks minor 6 and can decode a
    /// `ReceiverReport` at all (§9.1; ADR 0037).
    fn may_report_to(&self, peer: &NodeId) -> bool {
        self.receiver_reports
            .get(peer)
            .is_some_and(|speaks| speaks.to_peer)
    }

    /// Guest side: whether `peer` speaks minor 7 and can decode a
    /// `StreamScaleRequest` at all (§9.1; D7, docs/bugs/13-stream-resolution.md).
    fn may_request_scale_to(&self, peer: &NodeId) -> bool {
        self.stream_scale
            .get(peer)
            .is_some_and(|speaks| speaks.to_peer)
    }

    /// Guest side: whether `peer` speaks minor 9 and can decode a
    /// `DisplaySetMode` at all (§9.1; docs/bugs/16-host-display-mode.md;
    /// ADR 0048).
    fn may_set_display_mode_to(&self, peer: &NodeId) -> bool {
        self.display_mode
            .get(peer)
            .is_some_and(|speaks| speaks.to_peer)
    }

    /// Host side: honours a guest's `KeyframeRequest`, at most once per
    /// [`KEYFRAME_MIN_INTERVAL_MS`].
    ///
    /// The budget lives here rather than in the encode loop because this is
    /// the only place that knows *which* peer asked. Without it a guest could
    /// hold the host at one keyframe per frame, which is a guest deciding what
    /// the host's uplink is spent on (§11).
    fn on_keyframe_request(&mut self, peer: NodeId) {
        let Some(session) = self.media.get(&peer) else {
            tracing::debug!(
                peer = %self.label_of(&peer),
                "keyframe request without a media session; ignored"
            );
            return;
        };
        if !keyframe_budget_allows(self.last_keyframe.get(&peer).copied()) {
            tracing::debug!(peer = %self.label_of(&peer), "keyframe request rate limited");
            return;
        }
        session.control.request_keyframe();
        self.last_keyframe.insert(peer, std::time::Instant::now());
    }

    /// Host side: hands a guest's report to the encode loop feeding it (§11).
    ///
    /// Range-checked here and nowhere else, because here is where it stops
    /// being a claim by an untrusted peer and starts being an input to the
    /// controller: a loss outside `0.0..=1.0` or an implausible round trip is
    /// a frame of feedback to drop, not a reason to disbelieve the session
    /// (§9.1).
    fn on_receiver_report(&mut self, peer: NodeId, loss_permille: u16, rtt_ms: u32, goodput: u32) {
        let tag = self.label_of(&peer);
        if !self
            .receiver_reports
            .get(&peer)
            .is_some_and(|speaks| speaks.from_peer)
        {
            tracing::debug!(peer = %tag, "receiver report from a peer that never advertised one");
            return;
        }
        if loss_permille > PERMILLE || rtt_ms > RTT_MAX_PLAUSIBLE_MS {
            tracing::warn!(peer = %tag, loss_permille, rtt_ms, "dropping an implausible receiver report");
            return;
        }
        self.reception.insert(peer, (loss_permille, goodput));
        let Some(session) = self.media.get(&peer) else {
            tracing::debug!(peer = %tag, "receiver report without a media session; recorded only");
            return;
        };
        session
            .control
            .report(lumepeer_media::abr::ReceiverFeedback {
                loss: f32::from(loss_permille) / f32::from(PERMILLE),
                rtt_ms,
                goodput_kbps: goodput,
                // Left for the encode loop to fill: it is the only place that
                // knows how much this host actually wrote over the window the
                // guest was measuring.
                sent_kbps: 0,
            });
    }

    /// Host side: applies a guest's manual scale ceiling to the picture this
    /// session's encode loop produces (§11; D7,
    /// docs/bugs/13-stream-resolution.md task 2).
    ///
    /// The range was already checked while decoding (§9.1: a static bound is
    /// exactly what that check exists for). What is left here is
    /// authorization, and it is re-checked from scratch rather than trusted
    /// from the fact that the message arrived at all: a `view` grant revoked
    /// a moment ago must not be reopened by a message already in flight
    /// (§2.3), the same rule [`Self::on_monitor_select`] enforces for the
    /// same reason.
    fn on_stream_scale_request(&mut self, peer: NodeId, scale_percent: u32) {
        let tag = self.label_of(&peer);
        if !self
            .stream_scale
            .get(&peer)
            .is_some_and(|speaks| speaks.from_peer)
        {
            tracing::debug!(peer = %tag, "stream scale request from a peer that never advertised one");
            return;
        }
        let granted = self.connections.contains_key(&peer)
            && self.sessions.state(&peer) == SessionState::Active
            && self.sessions.grants(&peer).is_some_and(|g| g.view);
        if !granted {
            tracing::warn!(peer = %tag, "stream scale request without a live view grant; ignored");
            return;
        }
        let Some(session) = self.media.get(&peer) else {
            tracing::debug!(peer = %tag, "stream scale request without a media session; ignored");
            return;
        };
        // A repeated value has nothing new to draw, so it does not spend a
        // keyframe: task 2.4 asks for one on a *change*, not on every message
        // a guest happens to send (§11).
        if session.control.set_manual_cap(Some(scale_percent)) {
            session.control.request_keyframe();
        }
    }

    /// Host side: switches this host's own physical monitor to `mode_id`
    /// (docs/bugs/16-host-display-mode.md #2, #3; ADR 0048).
    ///
    /// `view` is not the grant this checks. Changing the operator's own
    /// screen is materially riskier than anything a `view` grant covers —
    /// it moves every window on the host's desktop, not only the picture a
    /// guest receives — so it needs its own independent `display_mode`
    /// grant, re-checked here from scratch exactly as
    /// [`Self::on_stream_scale_request`] re-checks `view` for the same
    /// reason (§2.3): a grant revoked a moment ago must not be honored by a
    /// message already in flight.
    ///
    /// Reversibility, task 3's whole point: the mode this monitor was in
    /// before the *first* switch of the current window is read and
    /// remembered before anything is applied, and refused outright if it
    /// cannot be read — a switch with no way back must not happen at all.
    /// A successful switch arms the auto-revert timeout of
    /// [`Self::arm_display_mode_confirm_timeout`].
    fn on_display_set_mode(&mut self, peer: NodeId, mode_id: u32) {
        let tag = self.label_of(&peer);
        if !self
            .display_mode
            .get(&peer)
            .is_some_and(|feature| feature.from_peer)
        {
            tracing::debug!(
                peer = %tag,
                "display set-mode request from a peer that never advertised the feature"
            );
            return;
        }
        let granted = self.connections.contains_key(&peer)
            && self.sessions.state(&peer) == SessionState::Active
            && self.sessions.grants(&peer).is_some_and(|g| g.display_mode);
        if !granted {
            tracing::warn!(
                peer = %tag,
                "display set-mode request without a live display_mode grant; ignored"
            );
            return;
        }
        // Recomputed fresh rather than trusted from whatever list was last
        // announced: a monitor can change between the announcement and this
        // request, the same reasoning `on_monitor_select` applies to its own
        // id.
        let modes = lock_capture(&self.capture).display_modes();
        let Some(mode) = usize::try_from(mode_id)
            .ok()
            .and_then(|index| modes.get(index).copied())
        else {
            tracing::warn!(peer = %tag, mode_id, "no such display mode was announced");
            return;
        };

        let original = if let Some(state) = self.display_mode_state {
            state.original
        } else {
            let Some(current) = lock_capture(&self.capture).current_display_mode() else {
                tracing::warn!(
                    peer = %tag,
                    "refusing a display mode switch: the current mode could not be read back, so there would be no way to undo it"
                );
                return;
            };
            current
        };

        match lock_capture(&self.capture).set_display_mode(mode) {
            Ok(()) => {
                tracing::info!(peer = %tag, ?mode, "display mode switched by the guest");
                self.display_mode_generation += 1;
                let generation = self.display_mode_generation;
                self.display_mode_state = Some(DisplayModeState {
                    original,
                    owner: peer,
                    armed_at: std::time::Instant::now(),
                    generation,
                });
                self.arm_display_mode_confirm_timeout(generation);
            }
            Err(error) => {
                tracing::warn!(peer = %tag, ?mode, %error, "the platform refused the display mode");
            }
        }
    }

    /// Spawns the auto-revert timeout for the switch identified by
    /// `generation` (docs/bugs/16-host-display-mode.md #3).
    ///
    /// Only the waiting happens off the actor loop, per ADR 0027: the
    /// spawned task reports back through `events_tx` once the deadline
    /// passes, and [`Self::on_display_mode_confirm_timeout`] — back on the
    /// actor's own thread — is what actually decides whether to revert,
    /// against whatever state is authoritative by then.
    fn arm_display_mode_confirm_timeout(&self, generation: u64) {
        let events = self.events_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(DISPLAY_MODE_CONFIRM_TIMEOUT_SECS)).await;
            let _ = events
                .send(ActorEvent::DisplayModeConfirmTimeout { generation })
                .await;
        });
    }

    /// Host side: a display-mode confirmation window elapsed (docs/bugs/
    /// 16-host-display-mode.md #3; ADR 0048).
    ///
    /// A no-op unless `generation` still names the switch currently armed —
    /// a later switch, an explicit restore, or the owning peer leaving may
    /// already have resolved it by the time this fires. Otherwise, capture
    /// having answered successfully at all since the switch is what counts
    /// as confirmed (§18: nothing here waits on a human, attended or not);
    /// anything else reverts.
    fn on_display_mode_confirm_timeout(&mut self, generation: u64) {
        let current_generation = self.display_mode_state.map(|state| state.generation);
        let healthy = self
            .display_mode_state
            .is_some_and(|state| lock_capture(&self.capture).healthy_since(state.armed_at));
        if !should_auto_revert_display_mode(current_generation, generation, healthy) {
            if current_generation == Some(generation) {
                tracing::debug!(
                    generation,
                    "display mode confirmed: capture answered within the timeout"
                );
            }
            return;
        }
        tracing::warn!(
            generation,
            "display mode auto-reverted: capture never confirmed it within the timeout"
        );
        self.restore_display_mode();
    }

    /// Restores this host's monitor to the mode it was in before the first
    /// switch of the current window, if any switch is still outstanding
    /// (docs/bugs/16-host-display-mode.md #3; ADR 0048).
    ///
    /// Always clears `display_mode_state` even if the restore itself fails:
    /// a failed restore is not a switch to keep retrying blindly, and the
    /// error is loud in the log either way.
    fn restore_display_mode(&mut self) {
        let Some(state) = self.display_mode_state.take() else {
            return;
        };
        match lock_capture(&self.capture).set_display_mode(state.original) {
            Ok(()) => {
                tracing::info!(original = ?state.original, "display mode restored");
            }
            Err(error) => {
                tracing::error!(
                    original = ?state.original,
                    %error,
                    "failed to restore the original display mode"
                );
            }
        }
    }

    /// What every live connection's link actually looks like (§18).
    fn connection_stats(&self) -> Vec<ConnectionStats> {
        self.connections
            .iter()
            .map(|(peer, handle)| {
                let (path, relay_region) = path_of(&handle.connection);
                let reception = self.reception.get(peer).copied();
                let target = self.media.get(peer).map(|session| session.control.target());
                ConnectionStats {
                    label: peer_tag(&self.install_salt, peer),
                    rtt_ms: self.rtt.get(peer).and_then(RttTracker::smoothed),
                    loss_permille: reception.map(|(loss, _)| loss),
                    goodput_kbps: reception.map(|(_, goodput)| goodput),
                    path,
                    relay_region,
                    bitrate_kbps: target.map(|t| t.bitrate_kbps),
                    fps: target.map(|t| t.fps),
                }
            })
            .collect()
    }

    /// Finishes the QUIC handshake, checks the ALPN and runs the control
    /// handshake, all on its own task and under one deadline (§9.1, §18).
    fn spawn_handshake(&self, incoming: iroh::endpoint::Incoming) {
        let Ok(permit) = Arc::clone(&self.handshake_slots).try_acquire_owned() else {
            tracing::warn!(
                limit = MAX_INFLIGHT_HANDSHAKES,
                "refusing an incoming connection: handshake slots exhausted"
            );
            drop(incoming);
            return;
        };
        let tx = self.events_tx.clone();
        let verifying_key = self.identity.verifying_key();
        let salt = self.install_salt;
        tokio::spawn(async move {
            let _permit = permit;
            // Two deadlines, not one. Finishing the QUIC handshake is a
            // network wait whose length is the far side's hole punching, and
            // it used to share the ten seconds meant for a single control
            // round trip — so a guest that was still working got dropped by
            // the host just before it would have arrived, and both sides then
            // reported a failure neither had caused (ADR 0027).
            let accept_deadline = std::time::Duration::from_secs(INCOMING_ACCEPT_TIMEOUT_SECS);
            let handshake_deadline = std::time::Duration::from_secs(CONTROL_HANDSHAKE_TIMEOUT_SECS);
            let Ok(connection) = tokio::time::timeout(accept_deadline, async move {
                PeerEndpoint::finish_accept(incoming).await.ok()
            })
            .await
            else {
                tracing::warn!(
                    timeout_secs = INCOMING_ACCEPT_TIMEOUT_SECS,
                    "dropping an incoming connection that did not finish its QUIC handshake in time"
                );
                return;
            };
            let Ok(outcome) = tokio::time::timeout(
                handshake_deadline,
                classify_incoming(connection, &verifying_key, &salt),
            )
            .await
            else {
                tracing::warn!(
                    timeout_secs = CONTROL_HANDSHAKE_TIMEOUT_SECS,
                    "dropping an incoming connection that did not finish its control handshake in time"
                );
                return;
            };
            let event = match outcome {
                Some(Accepted::Control {
                    connection,
                    peer,
                    ticket,
                    announces_media_faults,
                    speaks_remote_sas,
                    speaks_file_transfer,
                    speaks_unattended,
                    speaks_receiver_report,
                    speaks_cursor_shape,
                    speaks_stream_scale,
                    speaks_clipboard_files,
                    speaks_display_mode,
                }) => ActorEvent::Handshaked {
                    connection,
                    peer,
                    announces_media_faults,
                    speaks_remote_sas,
                    speaks_file_transfer,
                    speaks_unattended,
                    speaks_receiver_report,
                    speaks_cursor_shape,
                    speaks_stream_scale,
                    speaks_clipboard_files,
                    speaks_display_mode,
                    ticket: *ticket,
                },
                Some(Accepted::Media { connection, peer }) => {
                    ActorEvent::MediaAccepted { connection, peer }
                }
                Some(Accepted::File { connection, peer }) => {
                    ActorEvent::File(FileEvent::Connected { connection, peer })
                }
                None => return,
            };
            let _ = tx.send(event).await;
        });
    }

    /// Takes ownership of an authenticated connection: the reader half runs as
    /// its own task, the writer half is driven from the actor's outbound
    /// channel. Neither can cancel the other mid-frame.
    ///
    /// This is what makes a stored connection *live* rather than merely
    /// retained: without the reader, nothing would ever observe a
    /// `ConsentGrant` on the guest side or a closed stream on either side.
    fn adopt(
        &mut self,
        connection: ControlConnection,
        peer: NodeId,
        announces_media_faults: bool,
        speaks_remote_sas: bool,
        speaks_unattended: bool,
    ) {
        self.next_connection_id = self.next_connection_id.wrapping_add(1);
        let id = self.next_connection_id;
        let (outbound_tx, mut outbound_rx) = mpsc::channel::<MessageKind>(8);
        let quic = connection.connection().clone();
        let (mut reader, mut writer) = connection.split();
        let tag = self.label_of(&peer);

        let events = self.events_tx.clone();
        let read_tag = tag.clone();
        tokio::spawn(async move {
            loop {
                match reader.recv().await {
                    Ok(envelope) => {
                        if events
                            .send(ActorEvent::Inbound {
                                peer,
                                id,
                                kind: envelope.kind,
                            })
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(error) => {
                        tracing::debug!(peer = %read_tag, %error, "control stream ended");
                        // A framing error is the peer breaking §9.1, not a peer
                        // hanging up: only that earns a §15 record. Every other
                        // read error — an ordinary close, a lost link — is the
                        // end of a session, which `Closed` already covers.
                        if matches!(error, NetError::Framing(_)) {
                            let (_, code) = lumepeer_net::connection::close_for(&error);
                            let _ = events.send(ActorEvent::Violation { peer, code }).await;
                        }
                        let _ = events.send(ActorEvent::Closed { peer, id }).await;
                        return;
                    }
                }
            }
        });

        tokio::spawn(async move {
            while let Some(kind) = outbound_rx.recv().await {
                if let Err(error) = writer.send(kind).await {
                    tracing::warn!(peer = %tag, %error, "control send failed");
                    return;
                }
            }
        });

        // Replacing an entry drops the old outbound sender, which ends that
        // generation's writer task; its reader reports `Closed` with the older
        // id, which `handle_event` ignores.
        self.connections.insert(
            peer,
            ConnectionHandle {
                id,
                outbound: outbound_tx,
                connection: quic,
                announces_media_faults,
                speaks_remote_sas,
                speaks_unattended,
            },
        );
    }

    /// Queues one message on a peer's connection without ever blocking the
    /// actor loop on a slow or stalled writer.
    fn send_to(&mut self, peer: &NodeId, kind: MessageKind) {
        let Some(handle) = self.connections.get(peer) else {
            return;
        };
        if handle.outbound.try_send(kind).is_err() {
            tracing::warn!(
                peer = %peer_tag(&self.install_salt, peer),
                "dropping a control message: the connection is gone or backed up"
            );
        }
    }

    fn handle_event(&mut self, event: ActorEvent) {
        match event {
            ActorEvent::Handshaked {
                connection,
                peer,
                ticket,
                announces_media_faults,
                speaks_remote_sas,
                speaks_file_transfer,
                speaks_unattended,
                speaks_receiver_report,
                speaks_cursor_shape,
                speaks_stream_scale,
                speaks_clipboard_files,
                speaks_display_mode,
            } => {
                // Host side of the exchange: whether this guest's own reports
                // may be read at all (ADR 0037). The guest side is recorded in
                // `on_dialed`, from the `HelloAck` minor.
                self.receiver_reports.entry(peer).or_default().from_peer = speaks_receiver_report;
                // Same shape, for the manual scale ceiling (D7,
                // docs/bugs/13-stream-resolution.md).
                self.stream_scale.entry(peer).or_default().from_peer = speaks_stream_scale;
                // Same shape again, for the host's own display modes
                // (docs/bugs/16-host-display-mode.md; ADR 0048).
                self.display_mode.entry(peer).or_default().from_peer = speaks_display_mode;
                if speaks_cursor_shape {
                    self.speaks_cursor_shape.insert(peer);
                } else {
                    self.speaks_cursor_shape.remove(&peer);
                }
                if speaks_unattended {
                    self.speaks_unattended.insert(peer);
                } else {
                    self.speaks_unattended.remove(&peer);
                }
                if speaks_remote_sas {
                    self.speaks_remote_sas.insert(peer);
                } else {
                    self.speaks_remote_sas.remove(&peer);
                }
                if speaks_file_transfer {
                    self.speaks_file_transfer.insert(peer);
                } else {
                    self.speaks_file_transfer.remove(&peer);
                }
                if speaks_clipboard_files {
                    self.speaks_clipboard_files.insert(peer);
                } else {
                    self.speaks_clipboard_files.remove(&peer);
                }
                self.on_handshaked(*connection, peer, &ticket, announces_media_faults);
            }
            ActorEvent::Inbound { peer, id, kind } => self.on_inbound(peer, id, &kind),
            ActorEvent::Closed { peer, id } => self.on_closed(peer, id),
            ActorEvent::Violation { peer, code } => {
                tracing::warn!(peer = %self.label_of(&peer), code, "protocol violation");
                self.audit(
                    &peer,
                    lumepeer_core::audit::AuditEvent::ProtocolViolation { code },
                );
            }
            ActorEvent::MediaAccepted { connection, peer } => {
                self.on_media_accepted(*connection, peer);
            }
            ActorEvent::Dialed {
                peer,
                code,
                addr,
                result,
            } => self.on_dialed(peer, code, *addr, result),
            ActorEvent::File(event) => self.on_file_event(event),
            ActorEvent::ClipboardFilesRead { peer, files, paths } => {
                self.on_clipboard_files_read(peer, files, paths);
            }
            ActorEvent::DisplayModeConfirmTimeout { generation } => {
                self.on_display_mode_confirm_timeout(generation);
            }
        }
    }

    /// Host side: a media connection may exist only for a peer that already
    /// holds a live, granted control session with a `view` grant (§4.1, §8.1).
    ///
    /// Deny-by-default: everything else is closed, including a peer whose
    /// control session is merely pending or already revoked. The check is made
    /// here, on the actor's own thread, because this is the only place that can
    /// read `SessionManager` — a media connection must never be able to
    /// authorize itself.
    fn on_media_accepted(&mut self, connection: iroh::endpoint::Connection, peer: NodeId) {
        let tag = self.label_of(&peer);
        let granted = self.connections.contains_key(&peer)
            && self.sessions.state(&peer) == SessionState::Active
            && self.sessions.grants(&peer).is_some_and(|g| g.view);
        if !granted {
            tracing::warn!(peer = %tag, "refusing a media connection without a granted view session");
            connection.close(
                lumepeer_net::connection::CLOSE_MALFORMED.into(),
                lumepeer_net::error::close_code::MALFORMED.as_bytes(),
            );
            return;
        }
        // A redial replaces the previous stream rather than adding a second
        // encode loop against the same capture.
        if let Some(previous) = self.media.remove(&peer) {
            previous.stop();
        }
        // §18: a host that cannot produce a picture says so, instead of
        // accepting a media connection it will never write a frame on and
        // leaving the guest to time the session out and blame the network
        // (docs/adr/0024). Dropping `connection` here closes it.
        if let Some(reason) = self.health.fault() {
            tracing::warn!(
                peer = %tag,
                ?reason,
                "refusing a media connection this host cannot feed"
            );
            self.announce_media_fault(peer, reason);
            return;
        }
        tracing::info!(peer = %tag, "media connection accepted; starting the encode loop");
        let recorder: crate::view::SharedRecorder = Arc::new(std::sync::Mutex::new(None));
        // The cursor channel exists only for a guest that said it will draw
        // the cursor itself; without it the loop never even reads the shape.
        let cursors = self
            .speaks_cursor_shape
            .contains(&peer)
            .then(|| self.cursors_tx.clone());
        let control = EncodeControl::new(peer, cursors);
        // The loop's own copy of the `secure_desktop` grant, seeded from what
        // the session actually holds rather than assumed off: a full-control
        // guest carries the grant from the moment consent is given
        // (`Grants::from_role`), and the loop checks this flag — not the
        // core — before every secure-desktop frame (ADR 0049).
        control.set_secure_desktop_allowed(
            self.sessions
                .grants(&peer)
                .is_some_and(|grants| grants.secure_desktop),
        );
        let task = spawn_encode_loop(
            connection.clone(),
            Arc::clone(&self.capture),
            Arc::clone(&recorder),
            tag.clone(),
            peer,
            self.faults_tx.clone(),
            control.clone(),
        );
        self.media.insert(
            peer,
            MediaSession {
                task,
                connection: connection.clone(),
                recorder,
                control,
            },
        );
        // One capture backend feeds every viewer, so whether the cursor is
        // drawn into the frame is decided across all of them, not per session.
        self.refresh_cursor_embedding();
        // The guest-mic pass rides the same connection and parks until the
        // guest actually opens its tagged `M` stream (§11; ADR 0028); it is
        // bounded by the media session's own lifetime.
        crate::view::spawn_guest_mic_pass(connection, tag);
    }

    /// Host side: an encode loop found it cannot produce a picture at all —
    /// or, for [`MediaUnavailableReason::SecureDesktopActive`], that it is
    /// stuck behind one for now (`docs/bugs/11-uac-degradation.md`).
    ///
    /// Recorded on this host first. A missing encoder is a property of the
    /// machine, not of whichever peer happened to ask for it first, so the
    /// operator's own status must keep saying so after this session ends.
    ///
    /// The secure-desktop case is deliberately not treated like the other
    /// two: the encode loop that sent it is still running — it is retrying
    /// its own reopen in `WindowsCapturer` — so there is no session to tear
    /// down and nothing durable about this host to record. Removing the
    /// media session here would force a full reconnect for a condition that
    /// clears on its own.
    fn on_media_fault(&mut self, peer: NodeId, reason: MediaUnavailableReason) {
        if matches!(reason, MediaUnavailableReason::SecureDesktopActive) {
            self.announce_media_fault(peer, reason);
            return;
        }
        self.health.record(reason);
        // The loop has already returned; its media connection has nothing
        // left to carry, and dropping the entry closes it.
        self.media.remove(&peer);
        self.announce_media_fault(peer, reason);
    }

    /// Host side: tells `peer` that this session will carry no picture, and
    /// why — but only if its `Hello` said it understands the message (§9.1).
    ///
    /// A guest that did not is left exactly where it was before this existed:
    /// a window that waits, and the reason in this host's log. Sending it
    /// anyway would put a discriminant its minor does not know on the wire,
    /// and an undecodable control frame closes the connection (§9.1) — a
    /// strictly worse outcome than the missing picture.
    fn announce_media_fault(&mut self, peer: NodeId, reason: MediaUnavailableReason) {
        let announces = self
            .connections
            .get(&peer)
            .is_some_and(|c| c.announces_media_faults);
        if !announces {
            tracing::debug!(
                peer = %self.label_of(&peer),
                ?reason,
                "not announcing: this guest does not speak MediaUnavailable"
            );
            return;
        }
        tracing::info!(
            peer = %self.label_of(&peer),
            ?reason,
            "telling the guest this host cannot produce a picture"
        );
        self.send_to(&peer, MessageKind::MediaUnavailable(reason));
    }

    /// Guest side: the host says this session will never carry a picture —
    /// or, for [`MediaUnavailableReason::SecureDesktopActive`], that it
    /// currently cannot but expects to again (`docs/bugs/11-uac-degradation.md`).
    ///
    /// Not a revoke and not a failure of this connection: the control session
    /// and every grant on it stay as they are, and the window stays open with
    /// the real reason on it. What ends is the waiting — the receiver's
    /// recovery pass has nothing to reconnect to, so it is stopped rather
    /// than left to time out and report a connection that was never lost.
    ///
    /// The secure-desktop case does not stop anything: the host's media
    /// stream is still open and will start carrying frames again on its own,
    /// so aborting the receive task here would tear down a connection that
    /// is about to recover and force a needless reconnect. Only the status
    /// shown changes, and only while nothing more urgent already has it —
    /// a status this message reports as recovered underneath must never
    /// clobber a status that turned terminal in the meantime.
    fn on_media_unavailable(&mut self, peer: NodeId, reason: MediaUnavailableReason) {
        let tag = self.label_of(&peer);
        let Some(state) = self.views.get(&peer) else {
            return;
        };
        if matches!(reason, MediaUnavailableReason::SecureDesktopActive) {
            tracing::info!(peer = %tag, "the host's capture is behind a secure desktop; waiting");
            state.slot_tx.send_modify(|slot| {
                if !slot.status.is_terminal() {
                    slot.status = ViewStatus::SecureDesktop;
                }
            });
            return;
        }
        tracing::warn!(peer = %tag, ?reason, "the host cannot produce a picture for this session");
        state.task.abort();
        let status = ViewStatus::from(reason);
        state.slot_tx.send_modify(|slot| slot.status = status);
    }

    /// Host side: stops sending video to `peer` and drops it as a viewer, which
    /// stops capture altogether if it was the last one (§8.1, §11).
    /// The audio stream, if the host user had enabled it, dies with the media
    /// session — there is no audio without a picture's session. A live
    /// recording is flushed and closed too: a recording may only cover the
    /// session it was granted for (§8.2, §17).
    fn stop_media(&mut self, peer: NodeId) {
        if let Some(session) = self.media.remove(&peer) {
            session.stop();
        }
        if let Some(session) = self.audio.remove(&peer) {
            session.stop();
        }
        if let Some(session) = self.guest_mic.remove(&peer) {
            session.stop();
        }
        if let Some(recorder) = self.recorders.remove(&peer) {
            recorder.write_event(0, r#"{"event":"record-stop","reason":"session-end"}"#);
            let clean = recorder.stop();
            let dropped = recorder.dropped();
            tracing::info!(
                peer = %self.label_of(&peer),
                clean,
                dropped,
                "recording flushed at session end"
            );
            self.audit(
                &peer,
                lumepeer_core::audit::AuditEvent::RecordingToggled { enabled: false },
            );
        }
        // A request nobody answered dies with the session it was about, and
        // the guest's budget goes with it: a new session starts over.
        self.record_requests.remove(&peer);
        self.record_request_rate.forget(&peer);
        lock_capture(&self.capture).remove_viewer(&peer);
        // With one viewer fewer, whether the cursor may stay out of the frame
        // is a different question than it was a moment ago.
        self.refresh_cursor_embedding();
    }

    /// Guest side: opens the view window for a host that just granted, or
    /// refreshes the grants of one already open.
    fn start_view(&mut self, peer: NodeId, role: Role) {
        let grants = Grants::from_role(role);
        let tag = self.label_of(&peer);
        if !grants.view {
            self.stop_view(peer);
            return;
        }
        // A second `ConsentGrant` on a live view is a role change, not a new
        // window: keep the pipeline, update what the window may do.
        if let Some(state) = self.views.get_mut(&peer) {
            state.grants = grants;
            state.role = role;
            // The feed carries the live grant, so a role change has to reach it
            // before the next frame is served — the window stops accepting
            // input on the very next poll (§8.1).
            state.input.store(grants.input, Ordering::Relaxed);
            tracing::info!(peer = %tag, input = grants.input, "view grants updated");
            return;
        }
        let Some(addr) = self.host_addrs.get(&peer).cloned() else {
            tracing::warn!(peer = %tag, "no remembered address for this host: cannot open media");
            return;
        };

        let label = window_label(&tag);
        let (slot_tx, slot_rx) = watch::channel(ViewSlot::waiting());
        // Shared rather than handed over: the media task writes pictures into
        // it, and the actor writes the one status that does not come from the
        // media pipeline at all (`MediaUnavailable`, docs/adr/0024).
        let slot_tx = Arc::new(slot_tx);
        // The media connection lands here once the media task dials, so the
        // mic toggle can open its tagged stream on the *same* `rd/media/1`
        // the picture uses (§4.1; ADR 0028).
        let media_connection: Arc<std::sync::Mutex<Option<iroh::endpoint::Connection>>> =
            Arc::new(std::sync::Mutex::new(None));
        let task = spawn_media_receiver(
            MediaTarget {
                endpoint: self.endpoint.clone(),
                addr,
                peer,
                reports: self.reports_tx.clone(),
                tag: tag.clone(),
                worker: None,
                connection_cell: Arc::clone(&media_connection),
            },
            Arc::clone(&slot_tx),
        );
        let input = Arc::new(AtomicBool::new(grants.input));
        // Starts false and only the host can raise it: a view window claims a
        // recording is running because the host said so, never because this
        // side guessed (§17).
        let recording = Arc::new(AtomicBool::new(false));
        // Starts empty for the same reason: a host that composites its cursor
        // into the picture never announces one, and an overlay drawn on a
        // guess would be a second cursor next to the real one (§11).
        let cursor: Arc<std::sync::RwLock<Option<CursorFeed>>> =
            Arc::new(std::sync::RwLock::new(None));
        // Keyed by the peer tag: that is the only name the view window knows
        // itself by across IPC — `view_next_frame` takes `peer`, and the
        // window label is derived from it, not the other way round.
        self.view_feeds
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                tag.clone(),
                ViewFeed {
                    slot: slot_rx.clone(),
                    input: Arc::clone(&input),
                    recording: Arc::clone(&recording),
                    cursor: Arc::clone(&cursor),
                },
            );
        self.views.insert(
            peer,
            ViewState {
                label: label.clone(),
                role,
                grants,
                input,
                recording,
                cursor,
                monitors: Vec::new(),
                display_modes: Vec::new(),
                display_modes_reason: None,
                slot: slot_rx,
                slot_tx,
                task,
                media_connection,
            },
        );
        self.windows.open(&label, &tag, grants.input);
        self.rebuild_labels_and_snapshot();
        // A fresh entry in `self.views` is one of the two reasons the
        // watcher can be on (docs/bugs/10-clipboard-auto.md #1): this node
        // now has something worth offering the host it just started
        // watching.
        self.refresh_clipboard_watch();
    }

    /// Guest side: closes the view window, tears the pipeline down, and
    /// remembers the host (§21 punch-list item 5, ADR 0016).
    ///
    /// Every way a session this node started can end runs through here — the
    /// host revoking, the transport dropping, the operator closing the window
    /// — so this is the one place the remembered-hosts list has to be written,
    /// and it is written on the side that dialed rather than the side that was
    /// dialed.
    fn stop_view(&mut self, peer: NodeId) {
        let Some(state) = self.views.remove(&peer) else {
            return;
        };
        // Taken out before the window is told to close, so a frame poll racing
        // a revoke can only read the live grant or nothing at all — never a
        // grant that has just been withdrawn (§8.1).
        self.view_feeds
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.label_of(&peer));
        // Dropping the receiver already tells the media task to stop; aborting
        // makes sure a decoder does not outlive the session it belonged to
        // (§8.1).
        state.task.abort();
        drop(state.slot);
        self.windows.close(&state.label);
        if let Some(code) = self.host_invites.get(&peer).cloned() {
            self.history.record(host_tag(&peer), state.role, code);
        }
        tracing::info!(peer = %self.label_of(&peer), "view window closed");
        // The last view closing may be the only reason the watcher was on
        // (docs/bugs/10-clipboard-auto.md #1): with `self.views` empty and no
        // host-side `clipboard_read` live, nothing is left to justify it.
        self.refresh_clipboard_watch();
    }

    /// Drops this node's control connection to `peer` outright, citing the
    /// malformed close code — the far end sees this as a protocol fault.
    ///
    /// Dropping the outbound sender alone only ends the writer task; the far
    /// end would sit in `recv` until it noticed by itself.
    fn close_connection(&mut self, peer: NodeId) {
        self.close_connection_with(
            peer,
            lumepeer_net::connection::CLOSE_MALFORMED,
            lumepeer_net::error::close_code::MALFORMED,
        );
    }

    /// Drops this node's control connection to `peer` outright, citing the
    /// normal close code: this is the user leaving on purpose — cancelling a
    /// connect attempt or closing the view window — not a protocol fault
    /// (docs/bugs/02-connect-form.md task 3, docs/bugs/03-connection-list.md
    /// task 3).
    fn close_connection_normal(&mut self, peer: NodeId) {
        self.close_connection_with(
            peer,
            lumepeer_net::connection::CLOSE_NORMAL,
            lumepeer_net::error::close_code::NORMAL,
        );
    }

    fn close_connection_with(&mut self, peer: NodeId, code: u32, reason: &str) {
        if let Some(handle) = self.connections.remove(&peer) {
            handle.connection.close(code.into(), reason.as_bytes());
        }
    }

    /// Guest side: moves the connect form out of its pending state when the
    /// host it was waiting on resolves, one way or the other.
    ///
    /// `outcome` is what to report if the host never granted. Once it has,
    /// the form has nothing left to wait for, so a session ending after that
    /// is simply idle rather than a failure worth putting on screen.
    fn settle_connect(&mut self, peer: NodeId, outcome: ConnectPhase) {
        if self.connect_peer != Some(peer) {
            return;
        }
        // A dial is still in flight towards this very peer: whatever just
        // ended belongs to an older connection, and settling now would report
        // an outcome for an attempt that has not produced one yet.
        if self.connect_phase == ConnectPhase::Dialing && outcome != ConnectPhase::Connected {
            return;
        }
        if outcome == ConnectPhase::Connected {
            self.connect_phase = ConnectPhase::Connected;
            return;
        }
        self.connect_phase = if self.connect_phase.is_pending() {
            outcome
        } else {
            ConnectPhase::Idle
        };
        self.connect_peer = None;
    }

    /// Host side: injects one authorized input event (§11).
    ///
    /// The authorization is re-taken per event rather than once at grant time,
    /// so a `session_grant` that lowered the role in between takes effect on
    /// the very next event.
    fn inject(&mut self, peer: NodeId, event: &InputEventPayload) {
        let tag = self.label_of(&peer);
        if let Err(error) = self.sessions.authorize_input(&peer, event) {
            tracing::warn!(peer = %tag, %error, "dropping an unauthorized input event");
            return;
        }
        if self.injector.is_none() {
            match platform_injector() {
                Ok(injector) => self.injector = Some(injector),
                Err(error) => {
                    // §18: the session degrades to view-only and says so in the
                    // log rather than failing the session.
                    tracing::warn!(peer = %tag, %error, "no input adapter: staying view-only");
                    return;
                }
            }
        }
        if let Some(injector) = self.injector.as_mut()
            && let Err(error) = injector.inject(event)
        {
            tracing::warn!(peer = %tag, %error, "input injection failed");
        }
    }

    fn on_handshaked(
        &mut self,
        connection: ControlConnection,
        peer: NodeId,
        ticket: &InviteTicket,
        announces_media_faults: bool,
    ) {
        // `speaks_remote_sas` is already recorded by `handle_event` before
        // this runs; the parameter list stays untouched here.
        let tag = self.label_of(&peer);
        // Single-use enforcement runs here, on the actor's own thread, so two
        // connections racing the same ticket cannot both win it.
        if let Err(error) = self.tickets.claim(ticket, unix_now()) {
            tracing::warn!(peer = %tag, %error, "invite claim refused");
            connection.close_with(&NetError::InvalidTicket);
            return;
        }
        // A trusted device reaching a host with nobody at it answers to
        // credentials instead of to a dialog nobody would see (§8; ADR 0033).
        // The invite still had to verify and still had to be claimed above:
        // this replaces the human's decision, not the ticket.
        if self.may_try_unattended(&peer) {
            let code_required = self.unattended.code_required();
            self.adopt(
                connection,
                peer,
                announces_media_faults,
                self.speaks_remote_sas.contains(&peer),
                true,
            );
            self.unattended_pending.insert(peer);
            self.send_to(&peer, MessageKind::UnattendedChallenge { code_required });
            tracing::info!(
                peer = %tag,
                code_required,
                "unattended challenge offered to a trusted device"
            );
            self.rebuild_labels_and_snapshot();
            return;
        }
        // Every connection, first time or reconnect, gets a fresh decision.
        if let Err(error) = self
            .sessions
            .request_consent_as(peer, ticket.allowed_request)
        {
            tracing::warn!(peer = %tag, %error, "cannot queue a consent request");
            // Which refusal it was is worth a record: §15 separates "the host
            // was too busy to ask" from "the plan does not allow another
            // guest", and the two lead to different answers for the operator.
            match error {
                CoreError::PendingConsentQueueFull => self.audit(
                    &peer,
                    lumepeer_core::audit::AuditEvent::ConsentRejectedQueueFull,
                ),
                CoreError::ConcurrentGuestLimit { limit } => self.audit(
                    &peer,
                    lumepeer_core::audit::AuditEvent::ConsentRejectedGuestLimit { limit },
                ),
                // Anything else is not one of the two §15 names a rejection;
                // the warning above is the whole record it gets.
                _ => {}
            }
            // The ticket is already burned and nobody will ever decide on this
            // peer, so the connection must not linger: close it here, before
            // it is ever stored.
            connection.close_with(&NetError::ConsentUnavailable);
            return;
        }
        tracing::info!(peer = %tag, "consent request queued");
        self.audit(
            &peer,
            lumepeer_core::audit::AuditEvent::ConsentRequested {
                role: ticket.allowed_request,
            },
        );
        self.adopt(
            connection,
            peer,
            announces_media_faults,
            self.speaks_remote_sas.contains(&peer),
            self.speaks_unattended.contains(&peer),
        );
        let _ = self.notify.send(ActorNotification::ConsentRequested);
        self.rebuild_labels_and_snapshot();
    }

    /// Sends `UnattendedReject` to `peer`, but only if its `Hello` advertised
    /// [`FEATURE_UNATTENDED`] — an older guest would decode the unknown
    /// discriminant as malformed and drop the connection (§9.1).
    ///
    /// Nothing that reaches here should ever fail that check, since the
    /// challenge is offered under the same condition; it is asked again
    /// because "should never happen" is not how a send gate earns its keep.
    fn send_unattended_reject(&mut self, peer: NodeId, reason: UnattendedRejection) {
        if self
            .connections
            .get(&peer)
            .is_some_and(|c| c.speaks_unattended)
        {
            self.send_to(&peer, MessageKind::UnattendedReject(reason));
        } else {
            tracing::debug!(
                peer = %self.label_of(&peer),
                "not refusing on the wire: this guest does not speak unattended"
            );
        }
    }

    /// Whether this connection should be offered the unattended credential
    /// path of §8 instead of a consent dialog (ADR 0033, ADR 0034).
    ///
    /// Three conditions, all of them necessary, none of them sufficient:
    ///
    /// - the host user configured a device password at all — without one
    ///   there is nothing to verify and the gate is off (§8);
    /// - the host user marked *this device* trusted in the address book.
    ///   Trust is not a way past the password; it is a way of narrowing who is
    ///   even allowed to spend attempts against it, and the lockout is a
    ///   shared budget, so letting anyone with an invite spend it would let a
    ///   stranger lock the owner out (ADR 0034);
    /// - the guest can answer a challenge. A peer that never advertised
    ///   `FEATURE_UNATTENDED` would decode the message as malformed and drop
    ///   the connection (§9.1), so it takes the ordinary path — which asks a
    ///   human, the safe direction to fall back in.
    fn may_try_unattended(&self, peer: &NodeId) -> bool {
        self.unattended.enabled()
            && self.address_book.book().is_trusted(peer)
            && self.speaks_unattended.contains(peer)
    }

    /// Host side: one guest's answer to the credential challenge (§8).
    ///
    /// Everything decided here is decided by `lumepeer-core`: `admit` verifies
    /// both factors, counts the failure and hands back the role the host
    /// configured. This function chooses nothing — it routes an answer in and
    /// a verdict out (§2.1, §2.3).
    fn on_unattended_auth(&mut self, peer: NodeId, password: &str, code: Option<&str>) {
        let tag = self.label_of(&peer);
        if !self.unattended_pending.remove(&peer) {
            // No challenge was offered on this connection, so there is nothing
            // to answer. Silence rather than a rejection: an unsolicited
            // credential message tells this host nothing it should reply to.
            tracing::warn!(peer = %tag, "unattended credentials without a challenge; ignored");
            return;
        }
        // Trust is re-read here rather than trusted from challenge time: the
        // host user may have withdrawn it while the guest was typing, and a
        // decision in flight must not outlive the permission it was taken
        // under — the same per-event re-check every injected key gets (§8.1).
        if !self.may_try_unattended(&peer) {
            tracing::warn!(peer = %tag, "unattended access withdrawn mid-login; refusing");
            self.send_unattended_reject(peer, UnattendedRejection::Unavailable);
            return;
        }

        match self.unattended.admit(Some(password), code) {
            Ok(role) => {
                // The audit line records the verdict and nothing else: which
                // factor was presented, and how nearly it matched, would make
                // the log the oracle the error type refuses to be (§15).
                tracing::info!(peer = %tag, ?role, "unattended login accepted");
                self.audit(&peer, AuditEvent::UnattendedLogin { accepted: true });
                if let Err(error) = self.grant_role(peer, role) {
                    tracing::warn!(peer = %tag, ?error, "cannot start the admitted session");
                    self.send_unattended_reject(peer, UnattendedRejection::Unavailable);
                    return;
                }
                self.rebuild_labels_and_snapshot();
            }
            Err(error) => {
                // Back on the pending list: the lockout inside `admit` is what
                // bounds retries, and a guest that mistyped a code should not
                // have to redial to try again.
                self.unattended_pending.insert(peer);
                tracing::warn!(peer = %tag, "unattended login refused");
                self.audit(&peer, AuditEvent::UnattendedLogin { accepted: false });
                self.send_unattended_reject(peer, rejection_of(&error));
            }
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one arm per message kind reads best; splitting arms into \
                  helper fns would scatter the protocol across the file"
    )]
    fn on_inbound(&mut self, peer: NodeId, id: u64, kind: &MessageKind) {
        if self.connections.get(&peer).is_none_or(|c| c.id != id) {
            return;
        }
        let tag = self.label_of(&peer);
        #[allow(
            clippy::too_many_lines,
            reason = "one arm per message kind reads best; splitting arms into \
                      helper fns would scatter the protocol across the file"
        )]
        match *kind {
            // Guest side: the host decided.
            MessageKind::ConsentGrant(role) => {
                tracing::info!(peer = %tag, ?role, "remote host granted consent");
                let _ = self.notify.send(ActorNotification::ConsentGranted { role });
                // Reacted to here rather than through the notification stream:
                // `ActorNotification` deliberately carries no peer identity
                // (§15), and opening a media connection needs to know exactly
                // which host granted.
                self.settle_connect(peer, ConnectPhase::Connected);
                // Guest side: remembered the moment the session actually
                // starts, not only when it ends (docs/bugs/03-connection-
                // list.md, task 4) — a session that never reaches an explicit
                // end (a crash, a lost machine) must not lose the invite it
                // took to get here. `ConnectionHistory::record` is a
                // retain-and-reinsert keyed on the label, so the write
                // `stop_view` still makes when this session ends only
                // refreshes this same row rather than duplicating it.
                if let Some(code) = self.host_invites.get(&peer).cloned() {
                    self.history.record(host_tag(&peer), role, code);
                }
                // Only set when this grant followed a credential submission
                // with "remember" checked (§8; ADR 0033; docs/bugs/02-connect-
                // form.md, task 6) — a grant reached through the ordinary
                // consent dialog leaves this `None` and nothing happens here.
                if let Some(password) = self.pending_remember.take()
                    && let Err(error) = self.remembered_passwords.save(&host_tag(&peer), &password)
                {
                    tracing::warn!(%error, "could not remember this device's password");
                }
                self.connect_credentials_auto = false;
                self.start_view(peer, role);
            }
            MessageKind::ConsentRevoke => {
                tracing::info!(peer = %tag, "remote host revoked consent");
                let _ = self.notify.send(ActorNotification::ConsentRevoked);
                // A revoke while the request is still pending is the host
                // pressing Deny; after a grant it is an ordinary end of
                // session, and the connect form should just go quiet.
                let dialed = self.host_addrs.contains_key(&peer);
                self.settle_connect(peer, ConnectPhase::Denied);
                self.stop_view(peer);
                if dialed {
                    // Guest side: with the grant withdrawn there is nothing
                    // left to say on this connection. Leaving it open would
                    // hold a stream on the host for a session that no longer
                    // exists, and would make the next Connect to the same host
                    // look like the duplicate `connect_with_ticket` refuses.
                    self.close_connection(peer);
                }
            }
            // Host side: a guest asks for input. Authorization is `lumepeer-
            // core`'s, per event.
            MessageKind::InputEvent(ref event) => self.inject(peer, event),
            // Chat: validate, store, tell the UI there is something to pull.
            // A refused message is dropped with a log line — it never closes
            // the session (chat is content, not control).
            MessageKind::Chat { ref text } => {
                let at = unix_now_secs();
                match self.chat.record(peer, false, text, at) {
                    Ok(_) => {
                        let _ = self
                            .notify
                            .send(ActorNotification::ChatFromPeer { label: tag.clone() });
                    }
                    Err(error) => {
                        tracing::warn!(peer = %tag, %error, "dropping an invalid chat message");
                    }
                }
            }
            // Clipboard from the peer: §9.2 validation, then the grant
            // check that belongs to *this* side, then the payload onto this
            // machine's clipboard.
            //
            // The two sides do not ask the same question, and that asymmetry
            // is the model rather than an oversight (§2.3; ADR 0029, ADR
            // 0030). A host receiving a guest's clipboard is being written
            // to, which is `clipboard_write` — and the host holds the only
            // copy of that grant that decides anything. A guest receiving the
            // host's clipboard holds no grants at all: the host already
            // decided, under `clipboard_read`, that this guest may see it.
            // All the guest can check is that the payload came from a host it
            // has an open session with, and it checks exactly that.
            MessageKind::ClipboardSync { ref data } => {
                let permitted = if self.views.contains_key(&peer) {
                    true
                } else {
                    self.sessions
                        .grants(&peer)
                        .is_some_and(|g| clip::permits(g, ClipboardFlow::GuestToHost))
                };
                if !permitted {
                    tracing::warn!(peer = %tag, "clipboard update without a grant; ignored");
                    return;
                }
                let sync = self.clipboard.entry(peer).or_default();
                match sync.remote_received(data) {
                    Ok(bytes) => {
                        let text = String::from_utf8_lossy(bytes).into_owned();
                        // Onto the real clipboard, from Rust: this is what the
                        // grant was for, and the webview never gets a handle
                        // on it (§2.3).
                        self.clipboard_worker.write(text.clone());
                        // Staged as well, so the UI can say "a clipboard
                        // arrived" without the content ever crossing the
                        // broadcast bus (§15).
                        self.clipboard_inbound.insert(peer, text);
                        let _ = self.notify.send(ActorNotification::ClipboardFromPeer);
                    }
                    Err(error) => {
                        tracing::warn!(peer = %tag, %error, "dropping an invalid clipboard payload");
                    }
                }
            }
            // File transfer (§9.2; ADR 0032). Every arm re-checks the grant
            // rather than trusting the last one: a revoke can land between an
            // offer and its answer, and between an answer and the first byte.
            MessageKind::FileOffer {
                ref name,
                size,
                hash,
            } => self.on_file_offer_inbound(peer, name, size, hash),
            MessageKind::FileAccept(accepted) => self.on_file_accept_inbound(peer, accepted),
            MessageKind::FileTransferStart {
                transfer_id,
                ref name,
                size,
                hash,
            } => self.on_file_transfer_start(peer, transfer_id, name, size, hash),
            MessageKind::FileChunkAck {
                transfer_id,
                offset,
            } => self.on_file_chunk_ack(peer, transfer_id, offset),
            MessageKind::FileAbort { transfer_id } => {
                tracing::info!(peer = %tag, "the peer aborted a transfer");
                self.cancel_transfer(peer, transfer_id);
            }
            // "These files are on my clipboard" (docs/bugs/
            // 14-clipboard-files.md #2; ADR 0047). Names and sizes only; the
            // grant re-check and the queue capacity are identical to an
            // ordinary `FileOffer`, because this is one.
            MessageKind::ClipboardFileOffer { ref files } => {
                self.on_clipboard_file_offer_inbound(peer, files);
            }
            MessageKind::ClipboardFileAccept(accepted) => {
                self.on_clipboard_file_accept_inbound(peer, accepted);
            }
            // Keepalive of §9.1, both directions. Answered immediately and
            // with the same value: the sender is the only side that can turn
            // it into a round trip, because it is the only side that knows
            // when the nonce went out.
            MessageKind::Ping(nonce) => self.send_to(&peer, MessageKind::Pong(nonce)),
            // The other half. A nonce this side is not waiting on — a stale
            // one from before a reconnect, or a value the peer made up — is
            // ignored without a log line: it is not an error, it is simply not
            // a measurement.
            MessageKind::Pong(nonce) => {
                if let Some(rtt_ms) = self.rtt.entry(peer).or_default().pong(nonce) {
                    tracing::debug!(peer = %tag, rtt_ms, "round trip measured");
                }
            }
            // Host side: the guest's decoder has nothing to decode against
            // (§11). Rate-limited here, on the side whose uplink pays for it.
            MessageKind::KeyframeRequest => self.on_keyframe_request(peer),
            // Guest side: the host announced its cursor, which also says the
            // picture no longer contains one and drawing it is this side's
            // job now (§11). Geometry was already checked while decoding, so
            // what is left is only that a view exists to draw it.
            MessageKind::CursorShape { ref shape } => {
                let Some(state) = self.views.get(&peer) else {
                    tracing::debug!(peer = %tag, "cursor shape without a view; ignored");
                    return;
                };
                let mut cursor = state
                    .cursor
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let seq = cursor.as_ref().map_or(0, |current| current.seq);
                *cursor = Some(CursorFeed {
                    // Wrapping rather than saturating: the window compares for
                    // inequality, never for order, so a wrap is one extra
                    // repaint and never a shape that stops updating.
                    seq: seq.wrapping_add(1).max(1),
                    shape: shape.clone(),
                });
            }
            // Host side: what the guest actually received, which is what §11's
            // adaptation was always specified to run on (ADR 0037).
            MessageKind::ReceiverReport {
                loss_permille,
                rtt_ms,
                goodput_kbps,
            } => self.on_receiver_report(peer, loss_permille, rtt_ms, goodput_kbps),
            // Host side: the guest asked to cap the picture at a percentage
            // of this host's own captured size (§11; D7,
            // docs/bugs/13-stream-resolution.md task 2).
            MessageKind::StreamScaleRequest { scale_percent } => {
                self.on_stream_scale_request(peer, scale_percent);
            }
            // Host side: the guest asked to switch this host's own physical
            // monitor (docs/bugs/16-host-display-mode.md #2; ADR 0048).
            MessageKind::DisplaySetMode { mode_id } => {
                self.on_display_set_mode(peer, mode_id);
            }
            // Guest side: the watched host announced its own physical
            // display modes, or an honest reason there are none
            // (docs/bugs/16-host-display-mode.md #2; ADR 0048).
            MessageKind::DisplayModesList {
                ref modes,
                ref reason,
            } => {
                if let Some(view) = self.views.get_mut(&peer) {
                    view.display_modes.clone_from(modes);
                    view.display_modes_reason = *reason;
                }
            }
            // Guest side: the host announced it has no picture to send.
            MessageKind::MediaUnavailable(reason) => self.on_media_unavailable(peer, reason),
            // Guest side: this host wants credentials rather than a dialog
            // (§8; ADR 0033). Only ever acted on for the host this node is
            // actually dialing: an inbound peer has no business telling this
            // side to collect a password from its user.
            MessageKind::UnattendedChallenge { code_required } => {
                if self.connect_peer != Some(peer) {
                    tracing::warn!(peer = %tag, "unsolicited credential challenge; ignored");
                    return;
                }
                tracing::info!(peer = %tag, code_required, "the host asked for device credentials");
                self.connect_phase = ConnectPhase::AwaitingCredentials;
                self.connect_code_required = code_required;
                self.connect_failure = None;
                self.connect_retry_secs = None;
                self.connect_credentials_auto = false;
                let _ = self.notify.send(ActorNotification::UnattendedChallenge);
                // A remembered password is tried once, automatically, without
                // ever showing the modal — but never when a second factor is
                // also required: that one is never saved, so there would be
                // nothing to answer with anyway (docs/bugs/02-connect-form.md,
                // task 6; docs/bugs/DECISIONS.md D2).
                if !code_required
                    && let Ok(Some(password)) = self.remembered_passwords.load(&host_tag(&peer))
                {
                    self.connect_credentials_auto = true;
                    let _ = self.on_unattended_submit(&password, None, false);
                }
            }
            // Host side: a guest answered the challenge.
            MessageKind::UnattendedAuth {
                ref password,
                ref code,
            } => self.on_unattended_auth(peer, password, code.as_deref()),
            // Guest side: the credentials were refused (§8, §18). The phase
            // stays on the credential form so the user can try again — except
            // when the host says it cannot decide at all, which no retry
            // fixes.
            MessageKind::UnattendedReject(reason) => {
                if self.connect_peer != Some(peer) {
                    return;
                }
                // A submitted password that turned out wrong is not worth
                // remembering, and a remembered one that was just refused is
                // worth forgetting — silently retrying it would only burn the
                // consent-rate budget on a password already known to be wrong
                // (docs/bugs/02-connect-form.md, task 6).
                self.pending_remember = None;
                if self.connect_credentials_auto {
                    self.connect_credentials_auto = false;
                    if let Err(error) = self.remembered_passwords.forget(&host_tag(&peer)) {
                        tracing::warn!(%error, "could not forget a refused remembered password");
                    }
                }
                let (code, retry_secs) = match reason {
                    UnattendedRejection::BadPassword => ("UNATTENDED_BAD_PASSWORD", None),
                    UnattendedRejection::BadCode => ("UNATTENDED_BAD_CODE", None),
                    UnattendedRejection::LockedOut { remaining_secs } => {
                        ("UNATTENDED_LOCKED_OUT", Some(remaining_secs))
                    }
                    UnattendedRejection::Unavailable => ("UNATTENDED_UNAVAILABLE", None),
                };
                tracing::info!(peer = %tag, code, "the host refused the device credentials");
                self.connect_failure = Some(code);
                self.connect_retry_secs = retry_secs;
                if matches!(reason, UnattendedRejection::Unavailable) {
                    self.connect_phase = ConnectPhase::Failed;
                    self.connect_peer = None;
                } else {
                    self.connect_phase = ConnectPhase::AwaitingCredentials;
                }
            }
            // Host side: the guest asks to be recorded (§17). Nothing here
            // answers by itself — an automatic yes would make `recording` a
            // grant a guest could hand itself, which is exactly what §8.2
            // splits it out to prevent. The request is parked for the host
            // user, who answers it by starting or refusing the recording.
            MessageKind::RecordRequest => {
                if self.sessions.state(&peer) != SessionState::Active {
                    tracing::warn!(peer = %tag, "record request without an active session; ignored");
                    return;
                }
                // Same budget as a consent request, for the same reason: this
                // puts a dialog in front of a person (§9.2).
                if self.record_request_rate.check(peer).is_err() {
                    tracing::warn!(peer = %tag, "record request rate limited; refused");
                    self.send_to(&peer, MessageKind::RecordAck(false));
                    return;
                }
                if self.recorders.contains_key(&peer) {
                    // Already recording: the answer is a fact, not a decision.
                    self.send_to(&peer, MessageKind::RecordAck(true));
                    return;
                }
                if self.record_requests.insert(peer) {
                    tracing::info!(peer = %tag, "the guest asked to be recorded");
                }
            }
            // Guest side: the host announced whether it is recording (§17).
            // This is the only source the view window's indicator has — the
            // host's own statement, never something this side inferred.
            MessageKind::RecordAck(recording) => {
                if let Some(state) = self.views.get(&peer) {
                    state.recording.store(recording, Ordering::Relaxed);
                    tracing::info!(peer = %tag, recording, "the host announced its recording state");
                } else {
                    tracing::debug!(peer = %tag, recording, "record ack without a view; ignored");
                }
            }
            // Host side: the guest asks for the Secure Attention Sequence
            // (§11; ADR 0028). Same per-event re-check as every injected key:
            // only a live `input` grant acts, everything else is refused.
            MessageKind::SasRequest => {
                let permitted = self.sessions.authorize_input(
                    &peer,
                    &InputEventPayload {
                        logical: 0,
                        scancode: 0,
                        modifiers: 0,
                        detail: lumepeer_core::protocol::InputDetail::Press,
                    },
                );
                if permitted.is_err() {
                    tracing::warn!(peer = %tag, "SAS request without a live input grant; refused");
                    self.send_sas_ack(peer, false);
                    return;
                }
                // The helper service first, this process second (ADR 0043).
                // Both paths end in the same `SendSAS`; the difference is
                // whose rights it runs with, and only the service's are the
                // ones the OS honours without the user having launched this
                // app elevated. A machine with no service falls back to the
                // in-process call, which is what happened before there was
                // one — never to a silent success.
                let delivered = if lumepeer_service::client::deliver_sas() {
                    tracing::info!(peer = %tag, "SAS delivered by the helper service");
                    true
                } else {
                    match lumepeer_media::sas::send_sas() {
                        Ok(()) => {
                            tracing::info!(peer = %tag, "SAS delivered in-process");
                            true
                        }
                        Err(reason) => {
                            tracing::warn!(peer = %tag, %reason, "SAS refused by the host OS");
                            false
                        }
                    }
                };
                self.send_sas_ack(peer, delivered);
            }
            // Guest side: the host announced its screens when it granted the
            // session (§11; ADR 0028). Kept until the view closes, because
            // this is the only place the guest ever learns them — the picker
            // reads this list rather than asking, so opening it costs no round
            // trip and cannot show a stale answer from a previous session.
            MessageKind::MonitorsList { ref monitors } => {
                if let Some(view) = self.views.get_mut(&peer) {
                    view.monitors.clone_from(monitors);
                    tracing::debug!(peer = %tag, count = monitors.len(), "host announced its screens");
                } else {
                    tracing::debug!(peer = %tag, "screens announced without a view to show them in");
                }
            }
            // Host side: the guest picked a monitor to watch (§11; ADR 0028).
            // The id must name a monitor this host announced; anything else is
            // a malformed request and is dropped with a log line.
            MessageKind::MonitorSelect { monitor_id } => {
                // Not gated on a live media connection. The `view` grant is
                // what authorizes this and `on_monitor_select` checks it;
                // capture is already running because the grant added the
                // viewer, and a pick made while the guest's media dial is
                // between attempts has to be honoured, or the picture that
                // comes back is the screen the operator just moved away from
                // — a control that looks live, does nothing, and says nothing
                // (§18).
                match self.on_monitor_select(&self.label_of(&peer), monitor_id) {
                    Ok(()) => tracing::info!(peer = %tag, monitor_id, "capture retargeted"),
                    Err(error) => tracing::warn!(
                        peer = %tag,
                        monitor_id,
                        ?error,
                        "monitor select refused"
                    ),
                }
            }
            // Everything else belongs to a phase this build does not run yet.
            // Nothing a peer sends may ever grant itself consent (§2.3).
            ref other => tracing::debug!(peer = %tag, ?other, "ignoring a control message"),
        }
    }

    /// Sends `SasAck` to `peer`, but only if its `Hello` advertised
    /// [`lumepeer_core::protocol::FEATURE_REMOTE_SAS`] — an older guest would
    /// decode the unknown discriminant as malformed and close the connection
    /// (§9.1).
    fn send_sas_ack(&mut self, peer: NodeId, delivered: bool) {
        if self
            .connections
            .get(&peer)
            .is_some_and(|c| c.speaks_remote_sas)
        {
            self.send_to(&peer, MessageKind::SasAck { delivered });
        } else {
            tracing::debug!(
                peer = %self.label_of(&peer),
                delivered,
                "not acking: this guest does not speak remote-sas"
            );
        }
    }

    fn on_closed(&mut self, peer: NodeId, id: u64) {
        // Only the current generation may tear the peer's state down.
        if self.connections.get(&peer).is_some_and(|c| c.id != id) {
            return;
        }
        self.connections.remove(&peer);
        // An unanswered credential challenge dies with the connection that
        // carried it: a later connection from the same device gets a fresh
        // challenge, and its `UnattendedAuth` is never accepted against a
        // pending flag left over from an older one (§8; ADR 0033).
        self.unattended_pending.remove(&peer);
        self.speaks_unattended.remove(&peer);
        self.speaks_cursor_shape.remove(&peer);
        // Link measurements belong to the connection that produced them: a
        // later connection to the same device measures its own path, and must
        // not inherit a round trip taken over one that no longer exists.
        self.rtt.remove(&peer);
        self.reception.remove(&peer);
        self.last_keyframe.remove(&peer);
        self.receiver_reports.remove(&peer);
        self.stream_scale.remove(&peer);
        self.display_mode.remove(&peer);
        // Reversibility, always, including an ungraceful disconnect: this is
        // the same per-peer teardown funnel every other kind of session
        // state already goes through here, and a display-mode switch this
        // peer owns must not outlive the connection that asked for it
        // (docs/bugs/16-host-display-mode.md #3; ADR 0048).
        if self
            .display_mode_state
            .is_some_and(|state| state.owner == peer)
        {
            self.restore_display_mode();
        }
        // Chat and clipboard state are per-session by design (§15): nothing
        // about a past peer survives its connection here.
        self.chat.drop_transcript(&peer);
        self.clipboard.remove(&peer);
        self.clipboard_inbound.remove(&peer);
        // Staging goes with the session that was allowed to fill it, and
        // nothing in it is exported on the way out (§8.1, §9.2).
        self.drop_file_state(peer);
        // Both sides of the media pipeline end with the control connection: the
        // host stops capturing for this viewer, the guest closes its window
        // (and, on the guest, records the host it was watching).
        self.stop_media(peer);
        self.settle_connect(peer, ConnectPhase::Failed);
        self.stop_view(peer);
        let label = peer_tag(&self.install_salt, &peer);
        if self.sessions.on_disconnect(peer).is_err() {
            // No active session to move into the reconnect window, so this was
            // a guest that dropped before the host decided: drop its queued
            // request instead of leaving it pending forever.
            let _ = self.sessions.revoke(peer);
        } else {
            // `on_disconnect` only succeeds for a peer that had reached an
            // active, granted session — never for one still only queued —
            // so forgetting the consent-rate counter here cannot be used to
            // flood past it: a peer that was never granted anything gets no
            // forget, no matter how many times it disconnects
            // (docs/bugs/03-connection-list.md, task 2).
            self.sessions.forget_consent_rate(&peer);
        }
        tracing::info!(peer = %label, "peer disconnected");
        self.refresh_clipboard_watch();
        let _ = self.notify.send(ActorNotification::Disconnected);
        self.rebuild_labels_and_snapshot();
    }

    /// Not `async` any more, and that is the property worth keeping: the loop
    /// awaits this, so anything that blocks here blocks the whole actor. The
    /// dial was the last thing in it that talked to the network (ADR 0027).
    #[allow(
        clippy::too_many_lines,
        reason = "one arm per command reads best; splitting the dispatch would                   hide which commands the actor answers"
    )]
    fn handle_command(&mut self, command: ActorCommand) {
        match command {
            ActorCommand::Status { reply } => {
                let snapshot = self.rebuild_labels_and_snapshot();
                let _ = reply.send(snapshot);
            }
            ActorCommand::History { reply } => {
                let _ = reply.send(self.history.entries().to_vec());
            }
            ActorCommand::HistoryConnect { label, reply } => {
                let result = match self.history.code_of(&label).map(ToOwned::to_owned) {
                    Some(code) => self.spawn_dial(&code),
                    None => Err(ActorError::UnknownPeer),
                };
                if let Err(ActorError::Net(ref error)) = result {
                    tracing::warn!(%error, "reconnecting to a remembered host failed");
                }
                let _ = reply.send(result);
            }
            ActorCommand::HistoryRemove { label, reply } => {
                let _ = reply.send(self.history.remove(&label));
            }
            ActorCommand::ConnectionStats { reply } => {
                let _ = reply.send(self.connection_stats());
            }
            ActorCommand::ConnectState { reply } => {
                let _ = reply.send(ConnectSnapshot {
                    phase: self.connect_phase,
                    code: self.connect_failure,
                    code_required: self.connect_code_required,
                    retry_secs: self.connect_retry_secs,
                    credentials_auto: self.connect_credentials_auto,
                });
            }
            ActorCommand::ConnectCancel { reply } => {
                self.on_connect_cancel();
                let _ = reply.send(());
            }
            ActorCommand::Grant { label, role, reply } => {
                let result = self.on_grant(&label, role);
                self.rebuild_labels_and_snapshot();
                let _ = reply.send(result);
            }
            ActorCommand::Revoke { label, reply } => {
                let result = self.on_revoke(&label);
                self.rebuild_labels_and_snapshot();
                let _ = reply.send(result);
            }
            ActorCommand::InviteCreate { role, reply } => {
                let result = self.on_invite_create(role);
                if let Err(ActorError::Net(ref error)) = result {
                    tracing::warn!(%error, "could not issue an invite");
                }
                let _ = reply.send(result);
            }
            ActorCommand::InviteConnect { ticket, reply } => {
                let result = self.spawn_dial(&ticket);
                if let Err(ActorError::Net(ref error)) = result {
                    tracing::warn!(%error, "invite connect refused before dialing");
                }
                let _ = reply.send(result);
            }
            ActorCommand::Input {
                label,
                event,
                reply,
            } => {
                let result = self.on_input(&label, *event);
                let _ = reply.send(result);
            }
            ActorCommand::ChatSend { label, text, reply } => {
                let result = self.on_chat_send(&label, &text);
                let _ = reply.send(result);
            }
            ActorCommand::ChatTranscript { label, reply } => {
                let _ = reply.send(self.on_chat_transcript(&label));
            }
            ActorCommand::ClipboardPush { label, text, reply } => {
                let result = self.on_clipboard_push(&label, &text);
                let _ = reply.send(result);
            }
            ActorCommand::ClipboardPull { label, reply } => {
                let _ = reply.send(self.on_clipboard_pull(&label));
            }
            ActorCommand::FileAccept {
                label,
                accept,
                directory,
                reply,
            } => {
                let result = self.on_file_accept(&label, accept, directory);
                let _ = reply.send(result);
            }
            ActorCommand::FileAbort {
                label,
                transfer_id,
                reply,
            } => {
                let result = self.on_file_abort(&label, transfer_id);
                let _ = reply.send(result);
            }
            ActorCommand::FileTransfers { reply } => {
                let _ = reply.send(self.file_transfers_dto());
            }
            ActorCommand::AudioOn { label, reply } => {
                let _ = reply.send(self.on_audio_toggle(&label, true));
            }
            ActorCommand::AudioOff { label, reply } => {
                let _ = reply.send(self.on_audio_toggle(&label, false));
            }
            ActorCommand::RecordToggle { label, on, reply } => {
                let _ = reply.send(self.on_record_toggle(&label, on));
            }
            ActorCommand::RecordRequest { label, reply } => {
                let _ = reply.send(self.on_record_request(&label));
            }
            ActorCommand::SasRequest { label, reply } => {
                let _ = reply.send(self.on_sas_request(&label));
            }
            ActorCommand::MicToggle { label, on, reply } => {
                let _ = reply.send(self.on_mic_toggle(&label, on));
            }
            ActorCommand::MonitorSelect {
                label,
                monitor_id,
                reply,
            } => {
                let _ = reply.send(self.on_pick_monitor(&label, monitor_id));
            }
            ActorCommand::MonitorsList { label, reply } => {
                let _ = reply.send(self.on_announced_monitors(&label));
            }
            ActorCommand::StreamScaleRequest {
                label,
                scale_percent,
                reply,
            } => {
                let _ = reply.send(self.on_request_stream_scale(&label, scale_percent));
            }
            ActorCommand::DisplayModesList { label, reply } => {
                let _ = reply.send(self.on_announced_display_modes(&label));
            }
            ActorCommand::DisplaySetMode {
                label,
                mode_id,
                reply,
            } => {
                let _ = reply.send(self.on_request_display_set_mode(&label, mode_id));
            }
            ActorCommand::SetGrant {
                label,
                grant,
                allowed,
                reply,
            } => {
                let _ = reply.send(self.on_set_grant(&label, grant, allowed));
            }
            ActorCommand::AddressBookList { reply } => {
                let _ = reply.send(self.on_address_book_list());
            }
            ActorCommand::AddressBookUpsert {
                label,
                name,
                tags,
                notes,
                reply,
            } => {
                let result = self.on_address_book_upsert(&label, name, tags, notes);
                self.rebuild_labels_and_snapshot();
                let _ = reply.send(result);
            }
            ActorCommand::AddressBookRemove { label, reply } => {
                let result = self.on_address_book_remove(&label);
                self.rebuild_labels_and_snapshot();
                let _ = reply.send(result);
            }
            ActorCommand::AddressBookSetTrusted {
                label,
                trusted,
                reply,
            } => {
                let _ = reply.send(self.on_address_book_set_trusted(&label, trusted));
            }
            ActorCommand::UnattendedStatus { reply } => {
                let _ = reply.send(UnattendedSettings {
                    enabled: self.unattended.enabled(),
                    totp_enabled: self.unattended.code_required(),
                    role: self.unattended.role(),
                });
            }
            ActorCommand::UnattendedSetPassword { password, reply } => {
                let _ = reply.send(self.on_unattended_set_password(&password));
            }
            ActorCommand::UnattendedDisable { reply } => {
                let _ = reply.send(self.on_unattended_disable());
            }
            ActorCommand::UnattendedSetTotp { enabled, reply } => {
                let _ = reply.send(self.on_unattended_set_totp(enabled));
            }
            ActorCommand::UnattendedSetRole { role, reply } => {
                let _ = reply.send(self.on_unattended_set_role(role));
            }
            ActorCommand::UnattendedSubmit {
                password,
                code,
                remember,
                reply,
            } => {
                let _ = reply.send(self.on_unattended_submit(&password, code, remember));
            }
        }
    }

    /// Host side: starts or stops the session recording of `label` (§17).
    ///
    /// Gated on the independent `recording` grant (§8.2): a session that was
    /// granted view/input/clipboard but not `recording` cannot be recorded,
    /// no matter what the UI asks.
    ///
    /// The destination is decided here and only reported back (§2.3): the
    /// webview is the untrusted view layer, so it says *whether* to record and
    /// never *where* the file lands. Starting answers with the path so the
    /// operator can find it; stopping answers with `None`.
    ///
    /// Both sides are told. The host sees its own indicator through
    /// `session_status`; the guest is sent `RecordAck`, which is also the
    /// answer to a pending `RecordRequest` when there is one. There is no
    /// recording anybody is not told about (§2.2: no hidden capture).
    fn on_record_toggle(&mut self, label: &str, on: bool) -> Result<Option<String>, ActorError> {
        let peer = self.resolve(label)?;
        if on {
            let permitted = self.connections.contains_key(&peer)
                && self.sessions.state(&peer) == SessionState::Active
                && self.sessions.grants(&peer).is_some_and(|g| g.recording);
            if !permitted {
                // A refusal answers a waiting guest too: leaving the request
                // pending would hide the decision the host just made.
                if self.record_requests.remove(&peer) {
                    self.send_to(&peer, MessageKind::RecordAck(false));
                }
                return Err(ActorError::Core(CoreError::NotPermitted));
            }
            if let Some(running) = self.recorders.get(&peer) {
                // Already recording this session: idempotent, and the path is
                // still the answer so a second press cannot look like failure.
                return Ok(Some(running.path().to_string_lossy().into_owned()));
            }
            let path = recording_path(label)?;
            let recorder = Arc::new(
                crate::recorder::SessionRecorder::start(path.clone()).map_err(|error| {
                    tracing::warn!(peer = %label, %error, "cannot open the recording file");
                    ActorError::Net(NetError::Io(error.to_string()))
                })?,
            );
            recorder.write_event(
                0,
                &format!(r#"{{"event":"record-start","session":"{label}"}}"#),
            );
            // Hand the recorder to whichever loops are running for this
            // session: the video slot is picked up on the next frame; the
            // audio loop reads its own copy the same way.
            if let Some(session) = self.media.get_mut(&peer) {
                *session
                    .recorder
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    Some(Arc::clone(&recorder));
            }
            if let Some(audio) = self.audio.get_mut(&peer) {
                audio.set_recorder(Some(Arc::clone(&recorder)));
            }
            self.recorders.insert(peer, recorder);
            self.record_requests.remove(&peer);
            // The guest is told either way, whether or not it asked: the
            // indicator on its screen is this message.
            self.send_to(&peer, MessageKind::RecordAck(true));
            // §15 keeps paths out of the audit log; the event says that
            // recording started, not where it is being written.
            tracing::info!(peer = %label, "recording started");
            self.audit(
                &peer,
                lumepeer_core::audit::AuditEvent::RecordingToggled { enabled: true },
            );
            Ok(Some(path.to_string_lossy().into_owned()))
        } else {
            if let Some(recorder) = self.recorders.remove(&peer) {
                // Take it out of the live loops first so no new record lands
                // after the stop event.
                if let Some(session) = self.media.get_mut(&peer) {
                    *session
                        .recorder
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
                }
                if let Some(audio) = self.audio.get_mut(&peer) {
                    audio.set_recorder(None);
                }
                recorder.write_event(0, r#"{"event":"record-stop"}"#);
                let clean = recorder.stop();
                let dropped = recorder.dropped();
                tracing::info!(peer = %label, clean, dropped, "recording stopped");
                self.audit(
                    &peer,
                    lumepeer_core::audit::AuditEvent::RecordingToggled { enabled: false },
                );
            }
            // Off is also how the host declines a pending request: either way
            // the guest ends up being told nothing is being recorded.
            let asked = self.record_requests.remove(&peer);
            if asked || self.connections.contains_key(&peer) {
                self.send_to(&peer, MessageKind::RecordAck(false));
            }
            Ok(None)
        }
    }

    /// Guest side: asks the host behind `label` to record the session (§17).
    ///
    /// Nothing is decided here and nothing is started: this only puts the
    /// question in front of the person at the host, who answers with
    /// `RecordAck`. A guest cannot record a host's screen by asking twice.
    fn on_record_request(&mut self, label: &str) -> Result<(), ActorError> {
        let peer = self.resolve(label)?;
        // Only from a session this node is actually watching: a guest with no
        // view has nothing to ask about, and a host must not be able to send
        // this to its own guest.
        if !self.views.contains_key(&peer) {
            return Err(ActorError::UnknownPeer);
        }
        self.send_to(&peer, MessageKind::RecordRequest);
        Ok(())
    }

    /// Host side: turns the desktop-audio stream to `label` on or off (§11).
    ///
    /// Authorization mirrors `on_media_accepted`: a live control connection,
    /// an Active session and a `view` grant — audio is part of what `view`
    /// may carry (§8.1 "receive video **and audio**"), but it is opt-in per
    /// session because it is the host user's microphone-adjacent surface.
    fn on_audio_toggle(&mut self, label: &str, on: bool) -> Result<(), ActorError> {
        let peer = self.resolve(label)?;
        let granted = self.connections.contains_key(&peer)
            && self.sessions.state(&peer) == SessionState::Active
            && self.sessions.grants(&peer).is_some_and(|g| g.view);
        if !granted {
            return Err(ActorError::Core(CoreError::NotPermitted));
        }
        if on {
            if self.audio.contains_key(&peer) {
                return Ok(()); // already streaming; idempotent
            }
            let Some(session) = self.media.get(&peer) else {
                // No media connection to ride on yet.
                return Err(ActorError::UnknownPeer);
            };
            let stop = Arc::new(AtomicBool::new(false));
            // The §17 slot starts empty; the actor fills it when a recording
            // is turned on, and the loop picks it up on its next packet.
            let recorder: crate::view::SharedRecorder = Arc::new(std::sync::Mutex::new(
                self.recorders.get(&peer).map(Arc::clone),
            ));
            let task = crate::view::spawn_audio_loop(
                session.connection.clone(),
                Arc::clone(&stop),
                Arc::clone(&recorder),
                self.label_of(&peer),
            );
            self.audio.insert(
                peer,
                AudioSession {
                    stop,
                    recorder,
                    task,
                },
            );
            tracing::info!(peer = %label, "audio streaming enabled");
        } else if let Some(session) = self.audio.remove(&peer) {
            session.stop();
            tracing::info!(peer = %label, "audio streaming disabled");
        }
        Ok(())
    }

    /// Guest side: turns the view window's own microphone towards `label`'s
    /// host on or off (§11; ADR 0028).
    ///
    /// The mic is an *input* surface — it carries the guest user's voice, not
    /// the host's screen — so it is gated on the same live `input` grant the
    /// keyboard and pointer use, re-checked here at toggle time and by the
    /// host per request. The stream rides the media connection the picture
    /// already dialed; without one there is nothing to ride, which is the
    /// same refusal `audio_toggle` gives.
    fn on_mic_toggle(&mut self, label: &str, on: bool) -> Result<(), ActorError> {
        let peer = self.resolve(label)?;
        // The guest's own microphone is a surface that feeds the host, so it
        // is gated exactly like [`Self::on_input`] (§8.1): a view-only grant
        // may watch but not speak.
        let permitted = self
            .views
            .get(&peer)
            .ok_or(ActorError::UnknownPeer)?
            .grants
            .input;
        if !permitted {
            return Err(ActorError::Core(CoreError::NotPermitted));
        }
        if on {
            if self.guest_mic.contains_key(&peer) {
                return Ok(()); // already streaming; idempotent
            }
            let view = self.views.get(&peer).ok_or(ActorError::UnknownPeer)?;
            // The mic stream rides the media connection the picture already
            // dialed (§4.1); without one there is nothing to ride yet, and
            // the toolbar's press is refused — a picture showing means the
            // cell is populated.
            let Some(connection) = view.media_connection() else {
                tracing::debug!(peer = %label, "no media connection yet: mic press refused");
                return Err(ActorError::UnknownPeer);
            };
            let task = crate::view::spawn_mic_loop(connection, self.label_of(&peer));
            self.guest_mic.insert(peer, MicSession { task });
            tracing::info!(peer = %label, "guest microphone streaming enabled");
        } else if let Some(session) = self.guest_mic.remove(&peer) {
            session.stop();
            tracing::info!(peer = %label, "guest microphone streaming disabled");
        }
        Ok(())
    }

    /// Guest side: forwards the toolbar's Ctrl+Alt+Del request to the host
    /// being watched (§11; ADR 0028).
    ///
    /// Gated exactly like [`Self::on_input`], and for the same reason: this
    /// is one more injected key, so it needs the `input` grant the host
    /// announced for the view — which is what `views` holds on this side.
    ///
    /// It used to ask `self.sessions` instead, and that map is the *host*
    /// role's register of sessions this node has granted to *its* guests. A
    /// guest has no entry in it for the host it is watching, so the lookup
    /// missed every time and the request was refused before it could be sent:
    /// the button and the Ctrl+Alt+Shift+D chord both did nothing at all, on
    /// a perfectly good session with a live `input` grant.
    ///
    /// Whether the host actually synthesized the sequence arrives as `SasAck`
    /// on the wire; this reply only covers the send itself.
    fn on_sas_request(&mut self, label: &str) -> Result<(), ActorError> {
        let peer = self.resolve(label)?;
        let permitted = self
            .views
            .get(&peer)
            .ok_or(ActorError::UnknownPeer)?
            .grants
            .input;
        if !permitted {
            return Err(ActorError::Core(CoreError::NotPermitted));
        }
        self.send_to(&peer, MessageKind::SasRequest);
        Ok(())
    }

    /// Host side: retargets capture at `monitor_id` for `label`'s guest
    /// (§11 `MonitorSelect`; ADR 0028).
    ///
    /// The id must be one of this host's own monitors, counted in the same
    /// DXGI order the announcement uses. Retargeting restarts the capturer on
    /// the new display; the running encode loop simply picks the new geometry
    /// up on its next frame, exactly as it would if the user had changed the
    /// display resolution.
    fn on_monitor_select(&mut self, label: &str, monitor_id: u32) -> Result<(), ActorError> {
        let peer = self.resolve(label)?;
        let granted = self.connections.contains_key(&peer)
            && self.sessions.state(&peer) == SessionState::Active
            && self.sessions.grants(&peer).is_some_and(|g| g.view);
        if !granted {
            return Err(ActorError::Core(CoreError::NotPermitted));
        }
        let count = crate::view::host_display_count().map_err(|error| {
            tracing::warn!(peer = %label, %error, "cannot enumerate this host's monitors");
            ActorError::Core(CoreError::Malformed)
        })?;
        if usize::try_from(monitor_id).is_ok_and(|index| index >= count) {
            tracing::warn!(peer = %label, monitor_id, count, "monitor id out of range");
            return Err(ActorError::Core(CoreError::Malformed));
        }
        {
            let shared = Arc::clone(&self.capture);
            crate::view::lock_capture(&shared)
                .set_target(CaptureTarget::Display(monitor_id))
                .map_err(|error| {
                    tracing::warn!(
                        peer = %label,
                        monitor_id,
                        %error,
                        "the capturer refused the new monitor"
                    );
                    ActorError::Core(CoreError::NotPermitted)
                })?;
        }
        tracing::info!(peer = %label, monitor_id, "capture retargeted by the guest");
        Ok(())
    }

    /// Guest side: which of the host's screens the operator may pick from
    /// (§11 `MonitorsList`; ADR 0028).
    ///
    /// Reads what the host announced and nothing else. There is no request
    /// message in the protocol, so this cannot ask, and it must not fall back
    /// to enumerating *this* machine's displays: that is what the code here
    /// used to do, and it offered the operator their own screens as if they
    /// were the host's.
    ///
    /// # Errors
    /// [`ActorError::UnknownPeer`] when this node is not watching `label`.
    fn on_announced_monitors(&self, label: &str) -> Result<Vec<MonitorInfo>, ActorError> {
        let peer = self.resolve(label)?;
        let view = self.views.get(&peer).ok_or(ActorError::UnknownPeer)?;
        Ok(view.monitors.clone())
    }

    /// Guest side: asks the host to show `monitor_id` instead (§11
    /// `MonitorSelect`; ADR 0028).
    ///
    /// The host re-checks the `view` grant and the id's range and is the only
    /// side that decides; the check here only keeps an id the host never
    /// announced off the wire. Nothing comes back — the next picture simply
    /// shows the other screen, and `view.ts` already handles a frame that
    /// changed size mid-session.
    ///
    /// # Errors
    /// [`ActorError::UnknownPeer`] when this node is not watching `label`;
    /// [`ActorError::Core::Malformed`] when the host announced no such id.
    fn on_pick_monitor(&mut self, label: &str, monitor_id: u32) -> Result<(), ActorError> {
        let peer = self.resolve(label)?;
        let view = self.views.get(&peer).ok_or(ActorError::UnknownPeer)?;
        if !view.monitors.iter().any(|monitor| monitor.id == monitor_id) {
            tracing::warn!(peer = %label, monitor_id, "no such screen was announced");
            return Err(ActorError::Core(CoreError::Malformed));
        }
        self.send_to(&peer, MessageKind::MonitorSelect { monitor_id });
        Ok(())
    }

    /// Guest side: asks the watched host to cap the picture at
    /// `scale_percent` of its own captured size (§11; D7,
    /// docs/bugs/13-stream-resolution.md task 3).
    ///
    /// The host is the only side that decides what it actually encodes; this
    /// only keeps a value nothing could ever satisfy, or a message a host
    /// that never confirmed it understands, off the wire.
    ///
    /// # Errors
    /// [`ActorError::UnknownPeer`] when this node is not watching `label`;
    /// [`ActorError::Core::Malformed`] for a value outside
    /// `ABR_MIN_SCALE_PERCENT..=STREAM_SCALE_MAX_PERCENT`;
    /// [`ActorError::Unsupported`] when the host never answered with a minor
    /// that carries `MessageKind::StreamScaleRequest`.
    fn on_request_stream_scale(
        &mut self,
        label: &str,
        scale_percent: u32,
    ) -> Result<(), ActorError> {
        let peer = self.resolve(label)?;
        if !(ABR_MIN_SCALE_PERCENT..=STREAM_SCALE_MAX_PERCENT).contains(&scale_percent) {
            return Err(ActorError::Core(CoreError::Malformed));
        }
        if !self.may_request_scale_to(&peer) {
            return Err(ActorError::Unsupported);
        }
        self.send_to(&peer, MessageKind::StreamScaleRequest { scale_percent });
        Ok(())
    }

    /// Guest side: the watched host's own physical display modes, as it last
    /// announced them, or the reason there are none (docs/bugs/
    /// 16-host-display-mode.md #2; ADR 0048).
    ///
    /// Reads what the host announced and nothing else, the same contract
    /// [`Self::on_announced_monitors`] holds: there is no request message, so
    /// this cannot ask, and it must not fall back to enumerating *this*
    /// machine's own modes.
    ///
    /// # Errors
    /// [`ActorError::UnknownPeer`] when this node is not watching `label`.
    fn on_announced_display_modes(&self, label: &str) -> DisplayModesReply {
        let peer = self.resolve(label)?;
        let view = self.views.get(&peer).ok_or(ActorError::UnknownPeer)?;
        Ok((view.display_modes.clone(), view.display_modes_reason))
    }

    /// Guest side: asks the watched host to switch its own physical monitor
    /// to `mode_id` (docs/bugs/16-host-display-mode.md #2; ADR 0048).
    ///
    /// The host is the only side that decides whether its own `display_mode`
    /// grant and its own hardware actually allow it; this only keeps an id
    /// the host never announced, or a message a host that never confirmed it
    /// understands, off the wire.
    ///
    /// # Errors
    /// [`ActorError::UnknownPeer`] when this node is not watching `label`;
    /// [`ActorError::Core::Malformed`] when the host announced no such id;
    /// [`ActorError::Unsupported`] when the host never answered with a minor
    /// that carries `MessageKind::DisplaySetMode`.
    fn on_request_display_set_mode(&mut self, label: &str, mode_id: u32) -> Result<(), ActorError> {
        let peer = self.resolve(label)?;
        let view = self.views.get(&peer).ok_or(ActorError::UnknownPeer)?;
        if !view.display_modes.iter().any(|mode| mode.id == mode_id) {
            tracing::warn!(peer = %label, mode_id, "no such display mode was announced");
            return Err(ActorError::Core(CoreError::Malformed));
        }
        if !self.may_set_display_mode_to(&peer) {
            return Err(ActorError::Unsupported);
        }
        self.send_to(&peer, MessageKind::DisplaySetMode { mode_id });
        Ok(())
    }

    /// Host side: announces this host's monitors to `peer`'s guest
    /// (§11 `MonitorsList`; ADR 0028).
    ///
    /// Fire and forget, from the grant that made `peer` a viewer: the guest
    /// has no way to ask, so this is the only thing that puts the list in
    /// front of it.
    fn announce_monitors(&mut self, peer: NodeId) {
        let label = self.label_of(&peer);
        if self.health.fault().is_some() {
            // A host that cannot produce a picture at all has no meaningful
            // list to give; say nothing rather than offering screens that will
            // never show anything. The guest already has `MediaUnavailable`.
            tracing::debug!(peer = %label, "not announcing screens: no picture to show on them");
            return;
        }
        let monitors = match crate::view::host_monitors() {
            Ok(found) => found
                .into_iter()
                .map(|monitor| MonitorInfo {
                    id: monitor.id,
                    width: monitor.width,
                    height: monitor.height,
                    primary: monitor.primary,
                })
                .collect::<Vec<_>>(),
            Err(error) => {
                tracing::warn!(peer = %label, %error, "cannot enumerate this host's monitors");
                return;
            }
        };
        tracing::debug!(peer = %label, count = monitors.len(), "announcing this host's screens");
        self.send_to(&peer, MessageKind::MonitorsList { monitors });
    }

    /// Host side: announces this host's own physical display modes to
    /// `peer`'s guest (docs/bugs/16-host-display-mode.md #2; ADR 0048).
    ///
    /// Sent at the same two moments `announce_monitors` is — the grant that
    /// makes `peer` a viewer, and again whenever `on_set_grant` moves the
    /// independent `display_mode` grant — because there is no request
    /// message and this is the only thing that puts the list, or an honest
    /// reason for its absence, in front of the guest at all.
    fn announce_display_modes(&mut self, peer: NodeId) {
        let label = self.label_of(&peer);
        if !self
            .display_mode
            .get(&peer)
            .is_some_and(|feature| feature.from_peer)
        {
            // An older peer would decode the unknown discriminant as
            // malformed and close the connection (§9.1); say nothing rather
            // than break it.
            tracing::debug!(peer = %label, "not announcing display modes: peer does not speak the feature");
            return;
        }
        let granted = self.sessions.grants(&peer).is_some_and(|g| g.display_mode);
        let (modes, reason) = if !granted {
            // The one place a lack of permission has to be visible on the
            // wire: nothing else tells this guest's UI why the selector it
            // renders has nothing in it (§18).
            (Vec::new(), Some(DisplayModeUnavailableReason::NotGranted))
        } else if !lumepeer_media::capture::display_modes_supported() {
            (
                Vec::new(),
                Some(DisplayModeUnavailableReason::PlatformUnsupported),
            )
        } else {
            let found: Vec<DisplayModeInfo> = lock_capture(&self.capture)
                .display_modes()
                .into_iter()
                .enumerate()
                .map(|(index, mode)| DisplayModeInfo {
                    id: u32::try_from(index).unwrap_or(0),
                    width: mode.width,
                    height: mode.height,
                    refresh_hz: mode.refresh_hz,
                })
                .collect();
            if found.is_empty() {
                (
                    Vec::new(),
                    Some(DisplayModeUnavailableReason::NoModesReported),
                )
            } else {
                (found, None)
            }
        };
        tracing::debug!(
            peer = %label,
            count = modes.len(),
            granted,
            "announcing this host's display modes"
        );
        self.send_to(&peer, MessageKind::DisplayModesList { modes, reason });
    }

    /// Issues an invite for `role`, refusing while the endpoint has no
    /// dialable address (§7).
    fn on_invite_create(&mut self, role: Role) -> Result<InviteDto, ActorError> {
        let now = unix_now();
        let addr = self.endpoint.addr();
        // An invite is only worth anything if it carries somewhere to
        // dial. Until the endpoint has reached a relay (and, with the
        // direct transports cleared, it has nothing else to offer) the
        // address set is empty and the code would be dead on arrival —
        // which is far worse than saying "not yet": the host reads it
        // out, the guest pastes it, and the failure surfaces on the
        // wrong machine.
        if addr.addrs.is_empty() {
            tracing::warn!("refusing to issue an invite: the endpoint has no dialable address yet");
            return Err(ActorError::Net(NetError::Offline));
        }
        // With direct paths on (ADR 0026) the address set fills up from the
        // local interfaces long before a relay is reached, so an invite issued
        // in that window is dialable across the room and nowhere else. It is
        // still worth issuing — a LAN-only deployment is legitimate — but it
        // is exactly the "works locally, not over the internet" report of ADR
        // 0020, so it is said out loud here rather than discovered on the
        // guest's machine minutes later.
        if addr.relay_urls().next().is_none() {
            tracing::warn!(
                "issuing an invite with no relay address: this code is dialable from the local network only until the endpoint reaches a relay"
            );
        }
        tracing::info!(addrs = ?addr.addrs, "issuing an invite");
        // The obfuscated-transport address/fingerprint are not produced by
        // anything in this actor yet (task 17 increment 3, ADR 0053) — every
        // ticket issued here still carries only the existing iroh address
        // until that wiring lands.
        let issued = InviteTicket::issue(&self.identity, &addr, role, now, None, None);
        match issued {
            Ok(ticket) => match ticket.to_code() {
                Ok(code) => {
                    // Exactly one invite is live at a time: issuing a
                    // replacement is also how the host withdraws the
                    // code it read out earlier (ADR 0016).
                    self.tickets.retire_all();
                    self.tickets.register(&ticket);
                    Ok(InviteDto {
                        code,
                        expires_at: ticket.expires_at,
                    })
                }
                Err(e) => Err(ActorError::Net(e)),
            },
            Err(e) => Err(ActorError::Net(e)),
        }
    }

    /// Transcript of `label`; empty for an unknown label rather than an
    /// error, because a transcript poll races session teardown routinely.
    fn on_chat_transcript(&self, label: &str) -> Vec<ChatEntry> {
        match self.resolve(label) {
            Ok(peer) => self.chat.transcript(&peer).to_vec(),
            Err(_) => Vec::new(),
        }
    }

    /// Takes and clears `label`'s staged inbound clipboard payload.
    fn on_clipboard_pull(&mut self, label: &str) -> Option<String> {
        match self.resolve(label) {
            Ok(peer) => self.clipboard_inbound.remove(&peer),
            Err(_) => None,
        }
    }

    /// Both directions: record and forward one chat message (§9.2).
    ///
    /// The sender's own copy of the grants is advisory here; the receiving
    /// host re-checks what its session state allows before showing anything.
    /// A chat needs no separate grant: it is part of every granted session.
    fn on_chat_send(&mut self, label: &str, text: &str) -> Result<ChatEntry, ActorError> {
        let peer = self.resolve(label)?;
        if !self.connections.contains_key(&peer) {
            return Err(ActorError::UnknownPeer);
        }
        let at = unix_now_secs();
        let stored = self
            .chat
            .record(peer, true, text, at)
            .map_err(ActorError::Core)?
            .clone();
        self.send_to(
            &peer,
            MessageKind::Chat {
                text: text.to_owned(),
            },
        );
        Ok(stored)
    }

    /// Push the local clipboard to `label` (§9.2).
    ///
    /// On the host this is a grant decision: handing this desktop's clipboard
    /// to a guest is exactly what `clipboard_read` means, and the session has
    /// to hold it. On the guest there is no grant to consult, because a guest
    /// is given none (ADR 0029). What stands in for one is that this path
    /// runs only for a host this node currently has an open view onto
    /// (docs/bugs/10-clipboard-auto.md #1; ADR 0046): the guest's clipboard
    /// is the guest's to offer, automatically the moment it changes, and
    /// whether the host *accepts* it is decided on arrival, against
    /// `clipboard_write`, by the only core entitled to decide it.
    fn on_clipboard_push(&mut self, label: &str, text: &str) -> Result<(), ActorError> {
        let peer = self.resolve(label)?;
        let permitted = if self.views.contains_key(&peer) {
            true
        } else {
            self.sessions
                .grants(&peer)
                .is_some_and(|g| clip::permits(g, ClipboardFlow::HostToGuest))
        };
        if !permitted {
            return Err(ActorError::Core(CoreError::NotPermitted));
        }
        clip::validate_payload(text.as_bytes()).map_err(ActorError::Core)?;
        let sync = self.clipboard.entry(peer).or_default();
        if sync
            .local_changed(text.as_bytes())
            .map_err(ActorError::Core)?
            .is_none()
        {
            // Echo of a remote update we applied ourselves: do not send.
            return Ok(());
        }
        self.send_to(
            &peer,
            MessageKind::ClipboardSync {
                data: text.as_bytes().to_vec(),
            },
        );
        Ok(())
    }

    /// This desktop's clipboard changed and at least one session or view is
    /// allowed to be offered it (§8.2, §9.2).
    ///
    /// Two kinds of recipient, and both run through `on_clipboard_push`,
    /// which already knows how to authorize each:
    ///
    /// - **Host side.** A guest whose session carries `clipboard_read`: a
    ///   session that does not carry it is skipped without a word, the
    ///   change is not an error, it simply is not that session's to receive.
    /// - **Guest side** (docs/bugs/10-clipboard-auto.md #1). Every host this
    ///   node currently has an open view onto. There is no grant to check
    ///   here — this side may always *offer* — so every one of them is a
    ///   recipient; the host's own core is what decides on arrival.
    fn on_local_clipboard(&mut self, text: &str) {
        let mut recipients: Vec<NodeId> = self
            .sessions
            .active()
            .into_iter()
            .filter(|(peer, _, grants)| {
                self.sessions.state(peer) == SessionState::Active
                    && clip::permits(*grants, ClipboardFlow::HostToGuest)
            })
            .map(|(peer, _, _)| peer)
            .collect();
        recipients.extend(self.views.keys().copied());
        for peer in recipients {
            let label = self.label_of(&peer);
            if let Err(error) = self.on_clipboard_push(&label, text) {
                // Only the fact and the pseudonymized peer are logged; the
                // content of a clipboard never reaches a log line (§15).
                tracing::debug!(peer = %label, ?error, "clipboard change not sent to this session");
            }
        }
    }

    /// This desktop's clipboard now holds files, and they are offered to
    /// every peer allowed to receive them (docs/bugs/14-clipboard-files.md;
    /// ADR 0047).
    ///
    /// The same shape as [`Self::on_local_clipboard`] one function up, and
    /// deliberately so: copying files is copying, and it reaches the peer
    /// without a second gesture, exactly as copying text has since ADR 0046.
    /// What differs is only which grant decides — files are gated on
    /// `file_transfer` and never on the clipboard grants, because a file
    /// transfer is what this is (`lumepeer_core::clipboard::permits_files`).
    ///
    /// An offer is not a transfer: what crosses is a name and a size, and
    /// the receiving side still answers it in its own file panel before a
    /// single byte moves.
    fn on_local_clipboard_files(&mut self, paths: &[std::path::PathBuf]) {
        let mut recipients: Vec<NodeId> = self
            .sessions
            .active()
            .into_iter()
            .map(|(peer, _, _)| peer)
            .collect();
        recipients.extend(self.views.keys().copied());
        recipients.sort_unstable();
        recipients.dedup();
        for peer in recipients {
            // A peer too old to understand a clipboard file offer is skipped
            // rather than sent one it would decode as malformed (§9.1).
            if !self.may_transfer_files(&peer) || !self.speaks_clipboard_files.contains(&peer) {
                continue;
            }
            self.spawn_clipboard_offer(peer, paths.to_vec());
        }
    }

    // ---------------------------------------------------------------------
    // File transfer (§9.2, §4; ADR 0032)
    // ---------------------------------------------------------------------

    /// Whether this node may exchange files with `peer` right now.
    ///
    /// The same asymmetry as the clipboard, and for the same reason (§2.3;
    /// ADR 0029, ADR 0030). A host holds the `file_transfer` grant and is the
    /// only side whose answer decides anything. A guest holds no grants of
    /// its own, so all it can check is that it still has an open view with
    /// that host — it may *offer*, and the host refuses on arrival if the
    /// grant is not live.
    fn may_transfer_files(&self, peer: &NodeId) -> bool {
        if self.views.contains_key(peer) {
            return true;
        }
        self.sessions.state(peer) == SessionState::Active
            && self
                .sessions
                .grants(peer)
                .is_some_and(|grants| grants.file_transfer)
    }

    /// The shared receiver-side state for `peer`, created on first use.
    fn file_channel(&mut self, peer: NodeId) -> FileChannel {
        self.file_channels
            .entry(peer)
            .or_insert_with(FileChannel::new)
            .clone()
    }

    /// Records one file action in the audit log (§15).
    ///
    /// The tag and the pseudonymized peer, and nothing else: a file name is
    /// exactly what §15 keeps out of this log.
    fn audit_file(&mut self, peer: &NodeId, action: &'static str) {
        tracing::info!(peer = %self.label_of(peer), action, "file transfer");
        self.audit(
            peer,
            lumepeer_core::audit::AuditEvent::FileAction { action },
        );
    }

    /// Measures `paths` off the actor loop and, if any survive, announces
    /// them to `peer` (docs/bugs/14-clipboard-files.md #2).
    ///
    /// Nothing is read or stat'd here. The per-file pass runs on its own
    /// task rather than on the actor loop, and lands back as
    /// [`ActorEvent::ClipboardFilesRead`] (ADR 0027). Permission is
    /// checked by the caller and re-checked when that event arrives, because
    /// the pass takes long enough for a revoke to land in the middle of it.
    fn spawn_clipboard_offer(&self, peer: NodeId, paths: Vec<std::path::PathBuf>) {
        let events = self.events_tx.clone();
        let tag = self.label_of(&peer);
        tokio::spawn(async move {
            let mut files = Vec::new();
            let mut kept_paths = Vec::new();
            for path in paths {
                match stat_offer(&path).await {
                    Ok((name, size)) => {
                        files.push(ClipboardFileEntry { name, size });
                        kept_paths.push(path);
                    }
                    Err(error) => {
                        // The path is this machine's own and stays out of
                        // the log, like every other file name (§15).
                        tracing::debug!(peer = %tag, %error, "a clipboard entry could not be offered");
                    }
                }
            }
            if files.is_empty() {
                return;
            }
            let _ = events
                .send(ActorEvent::ClipboardFilesRead {
                    peer,
                    files,
                    paths: kept_paths,
                })
                .await;
        });
    }

    /// This node's own clipboard file list finished being read and measured,
    /// and can now be announced (docs/bugs/14-clipboard-files.md #2).
    fn on_clipboard_files_read(
        &mut self,
        peer: NodeId,
        files: Vec<ClipboardFileEntry>,
        paths: Vec<std::path::PathBuf>,
    ) {
        // Re-checked rather than assumed: the clipboard read and the stat
        // pass both take real time, long enough for a revoke to land.
        if !self.may_transfer_files(&peer) || !self.speaks_clipboard_files.contains(&peer) {
            tracing::warn!(peer = %self.label_of(&peer), "dropping a read clipboard file list");
            return;
        }
        let queue = self.clipboard_offers_out.entry(peer).or_default();
        queue.extend(paths);
        self.send_to(&peer, MessageKind::ClipboardFileOffer { files });
        self.audit_file(&peer, "clipboard-offer-sent");
        let _ = self.notify.send(ActorNotification::FileTransferChanged);
    }

    /// Either side: answer the oldest offer `label` made (§9.2).
    fn on_file_accept(
        &mut self,
        label: &str,
        accept: bool,
        directory: Option<String>,
    ) -> Result<(), ActorError> {
        let peer = self.resolve(label)?;
        let offer = self
            .file_offers_in
            .get_mut(&peer)
            .and_then(VecDeque::pop_front)
            .ok_or(ActorError::UnknownPeer)?;
        match offer {
            IncomingOffer::Direct(offer) => {
                self.on_direct_offer_accept(peer, offer, accept, directory)
            }
            IncomingOffer::Clipboard(entry) => self.on_clipboard_offer_accept(peer, entry, accept),
        }
    }

    /// Answers one offer that arrived through the peer's file picker (§9.2).
    fn on_direct_offer_accept(
        &mut self,
        peer: NodeId,
        offer: PendingOffer,
        accept: bool,
        directory: Option<String>,
    ) -> Result<(), ActorError> {
        if !accept {
            // §4: a refused offer opens nothing at all. There is no
            // connection to tear down afterwards because there never was one.
            self.send_to(&peer, MessageKind::FileAccept(false));
            self.audit_file(&peer, "offer-declined");
            let _ = self.notify.send(ActorNotification::FileTransferChanged);
            return Ok(());
        }
        if !self.may_transfer_files(&peer) {
            self.send_to(&peer, MessageKind::FileAccept(false));
            return Err(ActorError::Core(CoreError::NotPermitted));
        }
        let directory = directory
            .map(std::path::PathBuf::from)
            .ok_or(ActorError::Core(CoreError::Malformed))?;
        let destination = unique_destination(&directory, &offer.name);
        self.file_accepted
            .entry(peer)
            .or_default()
            .push_back(AcceptedOffer {
                name: offer.name,
                size: offer.size,
                hash: Some(offer.hash),
                destination,
                from_clipboard: false,
            });
        self.send_to(&peer, MessageKind::FileAccept(true));
        self.audit_file(&peer, "offer-accepted");
        // The receiving side opens the connection when it is the guest; a
        // host has no address to dial and waits for the guest's (§4, ADR
        // 0026). Either way this is the first moment `rd/file/1` may exist.
        self.ensure_file_connection(peer);
        let _ = self.notify.send(ActorNotification::FileTransferChanged);
        Ok(())
    }

    /// Answers one entry of the peer's last `ClipboardFileOffer` (docs/bugs/
    /// 14-clipboard-files.md #3).
    ///
    /// The receiving user's directory picker never runs for this path: a
    /// clipboard receive always lands in this node's own clipboard-receive
    /// directory, so a paste actually has something to point at
    /// (`crate::config::clipboard_files_dir`). Accepting is this session's
    /// one human decision; everything after — measuring, hashing, chunking —
    /// is the existing engine, unchanged.
    fn on_clipboard_offer_accept(
        &mut self,
        peer: NodeId,
        entry: ClipboardFileEntry,
        accept: bool,
    ) -> Result<(), ActorError> {
        if !accept {
            self.send_to(&peer, MessageKind::ClipboardFileAccept(false));
            self.audit_file(&peer, "clipboard-offer-declined");
            let _ = self.notify.send(ActorNotification::FileTransferChanged);
            return Ok(());
        }
        if !self.may_transfer_files(&peer) {
            self.send_to(&peer, MessageKind::ClipboardFileAccept(false));
            return Err(ActorError::Core(CoreError::NotPermitted));
        }
        let tag = self.label_of(&peer);
        let base = crate::config::clipboard_files_dir()
            .ok_or_else(|| ActorError::Net(NetError::Io("no data directory".to_owned())))?;
        let directory = base.join(&tag);
        std::fs::create_dir_all(&directory)
            .map_err(|error| ActorError::Net(NetError::Io(error.to_string())))?;
        let destination = unique_destination(&directory, &entry.name);
        self.file_accepted
            .entry(peer)
            .or_default()
            .push_back(AcceptedOffer {
                name: entry.name,
                size: entry.size,
                hash: None,
                destination,
                from_clipboard: true,
            });
        self.send_to(&peer, MessageKind::ClipboardFileAccept(true));
        self.audit_file(&peer, "clipboard-offer-accepted");
        self.ensure_file_connection(peer);
        let _ = self.notify.send(ActorNotification::FileTransferChanged);
        Ok(())
    }

    /// Either side: stop one running transfer (§9.2).
    fn on_file_abort(&mut self, label: &str, transfer_id: TransferId) -> Result<(), ActorError> {
        let peer = self.resolve(label)?;
        if !self.file_transfers.contains_key(&(peer, transfer_id)) {
            return Err(ActorError::UnknownPeer);
        }
        self.send_to(&peer, MessageKind::FileAbort { transfer_id });
        self.cancel_transfer(peer, transfer_id);
        Ok(())
    }

    /// Ends one transfer on this side: the send task stops, staging is
    /// removed, and nothing is exported (§9.2).
    fn cancel_transfer(&mut self, peer: NodeId, transfer_id: TransferId) {
        if let Some(task) = self.file_send_tasks.remove(&(peer, transfer_id)) {
            task.abort();
        }
        if let Some(row) = self.file_transfers.get_mut(&(peer, transfer_id))
            && row.state == TransferState::Running
        {
            row.state = TransferState::Cancelled;
        }
        if let Some(channel) = self.file_channels.get(&peer).cloned() {
            tokio::spawn(async move {
                let mut inbox = channel.inbox.lock().await;
                inbox.tracker.cancel(transfer_id);
                inbox.expected.remove(&transfer_id);
                if let Some(staged) = inbox.staged.remove(&transfer_id) {
                    staged.discard().await;
                }
            });
        }
        self.audit_file(&peer, "transfer-cancelled");
        let _ = self.notify.send(ActorNotification::FileTransferChanged);
    }

    /// The offers and transfers the UI draws.
    fn file_transfers_dto(&self) -> FileTransfersDto {
        let mut offers: Vec<OfferRow> = Vec::new();
        for (peer, queue) in &self.file_offers_in {
            let peer_label = self.label_of(peer);
            for offer in queue {
                offers.push(OfferRow {
                    peer_label: peer_label.clone(),
                    name: offer.name().to_owned(),
                    size: offer.size(),
                    from_clipboard: matches!(offer, IncomingOffer::Clipboard(_)),
                });
            }
        }
        let mut transfers: Vec<TransferRow> = self.file_transfers.values().cloned().collect();
        transfers.sort_by_key(|row| row.transfer_id);
        FileTransfersDto { offers, transfers }
    }

    /// The peer's `rd/file/1` connection, if there is still one that is up.
    ///
    /// A connection the far side closed — or that a withdrawn `file_transfer`
    /// grant closed on this side — stays in the map until something looks at
    /// it: QUIC reports the close on use, not through a callback the actor
    /// could listen to. So every point of use asks here, and a dead entry is
    /// evicted rather than handed out. Without it the first teardown would be
    /// permanent: `ensure_file_connection` would keep finding an entry, never
    /// dial again, and every later transfer would wait on a connection that
    /// accepts nothing.
    fn live_file_connection(&mut self, peer: NodeId) -> Option<iroh::endpoint::Connection> {
        let connection = self.file_conns.get(&peer)?;
        if connection.close_reason().is_some() {
            tracing::debug!(peer = %self.label_of(&peer), "dropping a closed file connection");
            self.file_conns.remove(&peer);
            return None;
        }
        Some(connection.clone())
    }

    /// Opens `rd/file/1` towards `peer`, if this side is the one that can.
    ///
    /// Only the node that dialed the control connection dials this one: the
    /// host was dialed and holds no address for the guest, exactly as with
    /// `rd/media/1` (§4.1, ADR 0026). On the host the connection therefore
    /// arrives rather than being made, and anything queued waits for it.
    fn ensure_file_connection(&mut self, peer: NodeId) {
        if self.live_file_connection(peer).is_some() {
            self.start_pending_sends(peer);
            return;
        }
        if self.file_dialing.contains(&peer) {
            return;
        }
        let Some(addr) = self.host_addrs.get(&peer).cloned() else {
            return;
        };
        self.file_dialing.insert(peer);
        let endpoint = self.endpoint.clone();
        let events = self.events_tx.clone();
        let tag = self.label_of(&peer);
        tokio::spawn(async move {
            let event = match endpoint.connect(addr, lumepeer_net::ALPN_FILE).await {
                Ok(connection) => FileEvent::Connected {
                    peer,
                    connection: Box::new(connection),
                },
                Err(error) => {
                    tracing::warn!(peer = %tag, %error, "could not open the file connection");
                    FileEvent::ConnectFailed { peer }
                }
            };
            let _ = events.send(ActorEvent::File(event)).await;
        });
    }

    /// Starts every send that was waiting for the connection.
    fn start_pending_sends(&mut self, peer: NodeId) {
        let Some(connection) = self.live_file_connection(peer) else {
            return;
        };
        for job in self.file_pending_sends.remove(&peer).unwrap_or_default() {
            self.spawn_send(peer, connection.clone(), job);
        }
    }

    /// Pushes one file onto its own unidirectional stream.
    ///
    /// One stream per transfer, so three concurrent transfers cannot
    /// interleave into a single ordered stream and make the slowest of them
    /// the speed of all three. Progress goes back through `try_send`: a
    /// dropped progress update costs a UI frame, and blocking a transfer on
    /// the actor's mailbox would cost the transfer.
    fn spawn_send(&mut self, peer: NodeId, connection: iroh::endpoint::Connection, job: SendJob) {
        let events = self.events_tx.clone();
        let progress = self.events_tx.clone();
        let tag = self.label_of(&peer);
        let id = job.id;
        let task = tokio::spawn(async move {
            let mut send = match connection.open_uni().await {
                Ok(send) => send,
                Err(error) => {
                    tracing::warn!(peer = %tag, %error, "could not open a file stream");
                    let _ = events
                        .send(ActorEvent::File(FileEvent::Finished {
                            peer,
                            id,
                            state: TransferState::Failed,
                        }))
                        .await;
                    return;
                }
            };
            let result = send_file(&mut send, id, &job.path, job.from, |at| {
                let _ = progress.try_send(ActorEvent::File(FileEvent::Progress {
                    peer,
                    id,
                    moved: at,
                }));
            })
            .await;
            match result {
                Ok(()) => {
                    // Finishing the stream is what tells the far side there
                    // are no more chunks. Completion itself is *not* claimed
                    // here: only the receiver can say the hash matched, and
                    // it says so with a final `FileChunkAck` at the full size.
                    let _ = send.finish();
                }
                Err(error) => {
                    tracing::warn!(peer = %tag, %error, "a file send ended early");
                    let _ = events
                        .send(ActorEvent::File(FileEvent::Finished {
                            peer,
                            id,
                            state: TransferState::Failed,
                        }))
                        .await;
                }
            }
        });
        self.file_send_tasks.insert((peer, id), task);
    }

    /// Reads every chunk stream a peer opens on its file connection.
    fn spawn_file_reader(&mut self, peer: NodeId, connection: iroh::endpoint::Connection) {
        let channel = self.file_channel(peer);
        let events = self.events_tx.clone();
        let tag = self.label_of(&peer);
        tokio::spawn(async move {
            loop {
                let Ok(recv) = connection.accept_uni().await else {
                    tracing::debug!(peer = %tag, "the file connection ended");
                    return;
                };
                tokio::spawn(read_transfer_stream(
                    recv,
                    channel.clone(),
                    peer,
                    tag.clone(),
                    events.clone(),
                ));
            }
        });
    }

    /// One thing that happened to a transfer, back on the actor's own thread.
    fn on_file_event(&mut self, event: FileEvent) {
        match event {
            FileEvent::ConnectFailed { peer } => {
                self.file_dialing.remove(&peer);
                self.file_pending_sends.remove(&peer);
                let _ = self.notify.send(ActorNotification::FileTransferChanged);
            }
            FileEvent::Connected { peer, connection } => self.on_file_connected(peer, *connection),
            FileEvent::Progress { peer, id, moved } => self.on_file_progress(peer, id, moved),
            FileEvent::Finished { peer, id, state } => self.on_file_finished(peer, id, state),
            FileEvent::ClipboardTransferReady {
                peer,
                path,
                name,
                size,
                hash,
            } => self.on_clipboard_transfer_ready(peer, path, name, size, hash),
            FileEvent::ClipboardPrepareFailed { peer } => {
                tracing::warn!(
                    peer = %self.label_of(&peer),
                    "a clipboard file could not be prepared after acceptance"
                );
                let _ = self.notify.send(ActorNotification::FileTransferChanged);
            }
        }
    }

    /// `rd/file/1` towards `peer` is up (§4).
    ///
    /// Authorized here and nowhere else, on the one thread that can read
    /// `SessionManager`. A connection from a peer with no live granted
    /// session carrying `file_transfer` is closed on the spot, which is what
    /// keeps the accept path safe now that it no longer refuses every file
    /// connection unconditionally.
    fn on_file_connected(&mut self, peer: NodeId, connection: iroh::endpoint::Connection) {
        self.file_dialing.remove(&peer);
        let tag = self.label_of(&peer);
        if !self.connections.contains_key(&peer) || !self.may_transfer_files(&peer) {
            tracing::warn!(peer = %tag, "refusing a file connection without a granted session");
            connection.close(
                lumepeer_net::connection::CLOSE_MALFORMED.into(),
                lumepeer_net::error::close_code::MALFORMED.as_bytes(),
            );
            return;
        }
        if let Some(previous) = self.file_conns.insert(peer, connection.clone()) {
            previous.close(0u32.into(), b"replaced");
        }
        self.spawn_file_reader(peer, connection);
        self.start_pending_sends(peer);
    }

    /// Bytes moved on a transfer this node is receiving or sending.
    fn on_file_progress(&mut self, peer: NodeId, id: TransferId, moved: u64) {
        let Some(row) = self.file_transfers.get_mut(&(peer, id)) else {
            return;
        };
        row.moved = moved;
        let incoming = row.incoming;
        let size = row.size;
        if incoming && moved < size {
            // The receiver's running ack, which is also the resume point the
            // sender picks up from after a reconnect (§10). The ack at the
            // full size is deliberately not sent here: it is sent once the
            // hash has been verified, so that "the sender saw size" and "the
            // file is on disk" cannot come apart.
            self.send_to(
                &peer,
                MessageKind::FileChunkAck {
                    transfer_id: id,
                    offset: moved,
                },
            );
        }
        let _ = self.notify.send(ActorNotification::FileTransferChanged);
    }

    /// A transfer ended on this side.
    fn on_file_finished(&mut self, peer: NodeId, id: TransferId, state: TransferState) {
        let Some(row) = self.file_transfers.get_mut(&(peer, id)) else {
            return;
        };
        row.state = state;
        let incoming = row.incoming;
        let size = row.size;
        if state == TransferState::Completed {
            row.moved = size;
        }
        self.file_send_tasks.remove(&(peer, id));
        // A verified, on-disk clipboard receive is put back on this
        // machine's own clipboard, so the paste it exists to serve actually
        // works (docs/bugs/14-clipboard-files.md #3).
        if let Some((destination, from_clipboard)) =
            self.file_receive_destinations.remove(&(peer, id))
            && from_clipboard
            && state == TransferState::Completed
        {
            self.clipboard_worker.write_files(vec![destination]);
        }
        if incoming {
            match state {
                // Sent only now: this ack means "verified and on disk", which
                // is the only completion the sender can honestly report.
                TransferState::Completed => self.send_to(
                    &peer,
                    MessageKind::FileChunkAck {
                        transfer_id: id,
                        offset: size,
                    },
                ),
                TransferState::Failed | TransferState::Cancelled => {
                    self.send_to(&peer, MessageKind::FileAbort { transfer_id: id });
                }
                TransferState::Running => {}
            }
        }
        self.audit_file(
            &peer,
            match state {
                TransferState::Completed => "transfer-completed",
                TransferState::Cancelled => "transfer-cancelled",
                TransferState::Failed => "transfer-failed",
                TransferState::Running => "transfer-running",
            },
        );
        let _ = self.notify.send(ActorNotification::FileTransferChanged);
    }

    /// Inbound `FileOffer`: someone wants to send this node a file (§9.2).
    fn on_file_offer_inbound(&mut self, peer: NodeId, name: &str, size: u64, hash: [u8; 32]) {
        let tag = self.label_of(&peer);
        if !self.may_transfer_files(&peer) {
            tracing::warn!(peer = %tag, "a file offer without a grant; declined");
            self.send_to(&peer, MessageKind::FileAccept(false));
            return;
        }
        // The name is the sender's, which is to say an attacker's. Anything
        // that is not a plain basename is declined rather than repaired
        // (§18): a rewritten name is a file the user did not agree to, under
        // a name neither side chose.
        let Some(name) = safe_file_name(name) else {
            tracing::warn!(peer = %tag, "a file offer whose name is not a plain basename; declined");
            self.send_to(&peer, MessageKind::FileAccept(false));
            return;
        };
        if size > FILE_OFFER_MAX_BYTES {
            self.send_to(&peer, MessageKind::FileAccept(false));
            return;
        }
        let queue = self.file_offers_in.entry(peer).or_default();
        if queue.len() >= MAX_PENDING_FILE_OFFERS {
            tracing::warn!(peer = %tag, "declining an offer past the pending limit");
            self.send_to(&peer, MessageKind::FileAccept(false));
            return;
        }
        queue.push_back(IncomingOffer::Direct(PendingOffer { name, size, hash }));
        self.audit_file(&peer, "offer-received");
        let _ = self.notify.send(ActorNotification::FileTransferChanged);
    }

    /// Inbound `FileAccept`: the peer answered this node's oldest offer.
    fn on_file_accept_inbound(&mut self, peer: NodeId, accepted: bool) {
        let tag = self.label_of(&peer);
        let Some(offer) = self
            .file_offers_out
            .get_mut(&peer)
            .and_then(VecDeque::pop_front)
        else {
            tracing::warn!(peer = %tag, "a file answer with no offer outstanding");
            return;
        };
        if !accepted {
            self.audit_file(&peer, "offer-refused");
            let _ = self.notify.send(ActorNotification::FileTransferChanged);
            return;
        }
        if !self.may_transfer_files(&peer) {
            return;
        }
        let id = self.next_transfer_id;
        self.next_transfer_id = self.next_transfer_id.wrapping_add(1);
        self.send_to(
            &peer,
            MessageKind::FileTransferStart {
                transfer_id: id,
                name: offer.name.clone(),
                size: offer.size,
                hash: offer.hash,
            },
        );
        self.file_transfers.insert(
            (peer, id),
            TransferRow {
                peer_label: tag,
                transfer_id: id,
                name: offer.name,
                size: offer.size,
                moved: 0,
                incoming: false,
                state: TransferState::Running,
                from_clipboard: false,
            },
        );
        self.file_pending_sends
            .entry(peer)
            .or_default()
            .push(SendJob {
                id,
                path: offer.path,
                from: 0,
            });
        self.ensure_file_connection(peer);
        self.audit_file(&peer, "transfer-started");
        let _ = self.notify.send(ActorNotification::FileTransferChanged);
    }

    /// Inbound `FileTransferStart`: the sender names the transfer it is about
    /// to push (§9.2; ADR 0032).
    fn on_file_transfer_start(
        &mut self,
        peer: NodeId,
        transfer_id: TransferId,
        name: &str,
        size: u64,
        hash: [u8; 32],
    ) {
        let tag = self.label_of(&peer);
        if !self.may_transfer_files(&peer) {
            self.send_to(&peer, MessageKind::FileAbort { transfer_id });
            return;
        }
        let Some(accepted) = self
            .file_accepted
            .get_mut(&peer)
            .and_then(VecDeque::pop_front)
        else {
            tracing::warn!(peer = %tag, "a transfer start with no accepted offer behind it");
            self.send_to(&peer, MessageKind::FileAbort { transfer_id });
            return;
        };
        // This is why the start restates the offer: without it the id would
        // mean "whichever offer we both believe was accepted last", and a
        // sender could start a different file under an answer given for this
        // one. A clipboard-sourced accept carries no hash to check here — it
        // had not been computed yet when the human agreed to it (docs/bugs/
        // 14-clipboard-files.md #2) — so that half of the check is skipped
        // for exactly that case; `ReceiveTracker::finish` still verifies the
        // real bytes against the hash this message carries, regardless.
        if accepted.name != name
            || accepted.size != size
            || accepted.hash.is_some_and(|expected| expected != hash)
        {
            tracing::warn!(peer = %tag, "a transfer start that does not describe the accepted offer");
            self.send_to(&peer, MessageKind::FileAbort { transfer_id });
            return;
        }
        self.file_transfers.insert(
            (peer, transfer_id),
            TransferRow {
                peer_label: tag.clone(),
                transfer_id,
                name: accepted.name,
                size,
                moved: 0,
                incoming: true,
                state: TransferState::Running,
                from_clipboard: accepted.from_clipboard,
            },
        );
        let channel = self.file_channel(peer);
        let events = self.events_tx.clone();
        let destination = accepted.destination;
        self.file_receive_destinations.insert(
            (peer, transfer_id),
            (destination.clone(), accepted.from_clipboard),
        );
        tokio::spawn(async move {
            let mut inbox = channel.inbox.lock().await;
            if let Err(error) = inbox.tracker.begin_with(transfer_id, size) {
                tracing::warn!(peer = %tag, %error, "refusing a transfer");
                drop(inbox);
                let _ = events
                    .send(ActorEvent::File(FileEvent::Finished {
                        peer,
                        id: transfer_id,
                        state: TransferState::Failed,
                    }))
                    .await;
                return;
            }
            // Staging lives beside the destination, so exporting is a rename
            // on one volume rather than a second pass over the whole file —
            // and an unwritable destination fails at the first chunk instead
            // of after the last one (§9.2).
            let directory = destination.parent().map_or_else(
                || std::path::PathBuf::from("."),
                std::path::Path::to_path_buf,
            );
            match StagedReceive::create(&directory, transfer_id).await {
                Ok(staged) => {
                    inbox.staged.insert(transfer_id, staged);
                    inbox.expected.insert(transfer_id, (hash, destination));
                    drop(inbox);
                    channel.announce_start();
                }
                Err(error) => {
                    tracing::warn!(peer = %tag, %error, "no staging file for this transfer");
                    inbox.tracker.cancel(transfer_id);
                    drop(inbox);
                    let _ = events
                        .send(ActorEvent::File(FileEvent::Finished {
                            peer,
                            id: transfer_id,
                            state: TransferState::Failed,
                        }))
                        .await;
                }
            }
        });
        self.ensure_file_connection(peer);
        let _ = self.notify.send(ActorNotification::FileTransferChanged);
    }

    /// Inbound `ClipboardFileOffer`: the peer's clipboard holds these files
    /// (docs/bugs/14-clipboard-files.md #2). Each entry is queued exactly
    /// like an ordinary `FileOffer`, tagged so the panel can say where it
    /// came from — the same grant, the same queue, the same FIFO answer
    /// order.
    fn on_clipboard_file_offer_inbound(&mut self, peer: NodeId, files: &[ClipboardFileEntry]) {
        let tag = self.label_of(&peer);
        if !self.may_transfer_files(&peer) {
            tracing::warn!(peer = %tag, "a clipboard file offer without a grant; declined");
            self.send_to(&peer, MessageKind::ClipboardFileAccept(false));
            return;
        }
        let queue = self.file_offers_in.entry(peer).or_default();
        let mut accepted_any = false;
        for entry in files {
            if queue.len() >= MAX_PENDING_FILE_OFFERS {
                tracing::warn!(peer = %tag, "declining a clipboard entry past the pending limit");
                break;
            }
            let Some(name) = safe_file_name(&entry.name) else {
                tracing::warn!(
                    peer = %tag,
                    "a clipboard file entry whose name is not a plain basename; skipped"
                );
                continue;
            };
            queue.push_back(IncomingOffer::Clipboard(ClipboardFileEntry {
                name,
                size: entry.size,
            }));
            accepted_any = true;
        }
        if accepted_any {
            self.audit_file(&peer, "clipboard-offer-received");
            let _ = self.notify.send(ActorNotification::FileTransferChanged);
        }
    }

    /// Inbound `ClipboardFileAccept`: the peer answered the oldest entry of
    /// this node's last `ClipboardFileOffer` (docs/bugs/
    /// 14-clipboard-files.md #2).
    ///
    /// A decline ends here. An accept is the receiver's one human decision;
    /// everything after runs on its own task exactly like an ordinary offer
    /// does (ADR 0027) and lands as [`FileEvent::ClipboardTransferReady`],
    /// which starts the transfer without asking again.
    fn on_clipboard_file_accept_inbound(&mut self, peer: NodeId, accepted: bool) {
        let tag = self.label_of(&peer);
        let Some(path) = self
            .clipboard_offers_out
            .get_mut(&peer)
            .and_then(VecDeque::pop_front)
        else {
            tracing::warn!(peer = %tag, "a clipboard file answer with no offer outstanding");
            return;
        };
        if !accepted {
            self.audit_file(&peer, "clipboard-offer-refused");
            let _ = self.notify.send(ActorNotification::FileTransferChanged);
            return;
        }
        if !self.may_transfer_files(&peer) {
            return;
        }
        let events = self.events_tx.clone();
        tokio::spawn(async move {
            let event = match prepare_offer(&path).await {
                Ok((name, size, hash)) => FileEvent::ClipboardTransferReady {
                    peer,
                    path,
                    name,
                    size,
                    hash,
                },
                Err(error) => {
                    // The path is this machine's own and stays out of the
                    // log, like every other file name (§15).
                    tracing::warn!(peer = %tag, %error, "an accepted clipboard file could not be prepared");
                    FileEvent::ClipboardPrepareFailed { peer }
                }
            };
            let _ = events.send(ActorEvent::File(event)).await;
        });
    }

    /// A file accepted through `ClipboardFileAccept` finished measuring and
    /// hashing and can start (docs/bugs/14-clipboard-files.md #2).
    ///
    /// The tail of this mirrors [`Self::on_file_accept_inbound`] exactly,
    /// from the point an ordinary offer's acceptance is known: allocate the
    /// id, announce `FileTransferStart`, queue the send. What is missing on
    /// purpose is the `FileOffer`/`FileAccept` round trip in front of it —
    /// the human already answered, when they accepted the clipboard entry.
    fn on_clipboard_transfer_ready(
        &mut self,
        peer: NodeId,
        path: std::path::PathBuf,
        name: String,
        size: u64,
        hash: [u8; 32],
    ) {
        if !self.may_transfer_files(&peer) {
            tracing::warn!(peer = %self.label_of(&peer), "dropping a prepared clipboard transfer");
            return;
        }
        let tag = self.label_of(&peer);
        let id = self.next_transfer_id;
        self.next_transfer_id = self.next_transfer_id.wrapping_add(1);
        self.send_to(
            &peer,
            MessageKind::FileTransferStart {
                transfer_id: id,
                name: name.clone(),
                size,
                hash,
            },
        );
        self.file_transfers.insert(
            (peer, id),
            TransferRow {
                peer_label: tag,
                transfer_id: id,
                name,
                size,
                moved: 0,
                incoming: false,
                state: TransferState::Running,
                from_clipboard: true,
            },
        );
        self.file_pending_sends
            .entry(peer)
            .or_default()
            .push(SendJob { id, path, from: 0 });
        self.ensure_file_connection(peer);
        self.audit_file(&peer, "clipboard-transfer-started");
        let _ = self.notify.send(ActorNotification::FileTransferChanged);
    }

    /// Inbound `FileChunkAck`: the receiver reports its resume point, and at
    /// the full size, its verified completion (§9.2, §10).
    fn on_file_chunk_ack(&mut self, peer: NodeId, transfer_id: TransferId, offset: u64) {
        let Some(row) = self.file_transfers.get_mut(&(peer, transfer_id)) else {
            return;
        };
        if row.incoming {
            return;
        }
        row.moved = offset.min(row.size);
        if offset >= row.size {
            row.state = TransferState::Completed;
            self.file_send_tasks.remove(&(peer, transfer_id));
            self.audit_file(&peer, "transfer-completed");
        }
        let _ = self.notify.send(ActorNotification::FileTransferChanged);
    }

    /// Every file connection, offer, staging file and transfer of one peer,
    /// gone (§8.1).
    ///
    /// Called from every path that ends a session or the connection under it.
    /// Nothing is exported on the way out: a transfer that was still running
    /// when the grant behind it ended is a transfer that was never finished
    /// being allowed. The peer's `FEATURE_FILE_TRANSFER` advertisement goes
    /// too, because it is a fact about a connection that is on its way out;
    /// [`Self::abandon_file_transfers`] is the variant for a grant that ends
    /// while the connection lives on.
    fn drop_file_state(&mut self, peer: NodeId) {
        self.abandon_file_transfers(peer);
        self.speaks_file_transfer.remove(&peer);
        self.speaks_clipboard_files.remove(&peer);
    }

    /// The same teardown, minus the peer's feature advertisement (§8.1).
    ///
    /// This is what withdrawing `file_transfer` from a *live* session runs:
    /// the connection stays up and the guest still speaks the feature, so
    /// only what the grant paid for goes — the `rd/file/1` connection, the
    /// offers on both sides, the running sends and every staging file, none
    /// of it exported.
    fn abandon_file_transfers(&mut self, peer: NodeId) {
        if let Some(connection) = self.file_conns.remove(&peer) {
            connection.close(0u32.into(), b"session ended");
        }
        self.file_dialing.remove(&peer);
        self.file_offers_in.remove(&peer);
        self.file_offers_out.remove(&peer);
        self.file_accepted.remove(&peer);
        self.file_pending_sends.remove(&peer);
        self.clipboard_offers_out.remove(&peer);
        self.file_receive_destinations
            .retain(|(p, _), _| *p != peer);
        self.file_send_tasks.retain(|(p, _), task| {
            if *p == peer {
                task.abort();
                false
            } else {
                true
            }
        });
        self.file_transfers.retain(|(p, _), _| *p != peer);
        if let Some(channel) = self.file_channels.remove(&peer) {
            tokio::spawn(async move {
                let mut inbox = channel.inbox.lock().await;
                let ids: Vec<TransferId> = inbox.staged.keys().copied().collect();
                for id in ids {
                    inbox.tracker.cancel(id);
                    if let Some(staged) = inbox.staged.remove(&id) {
                        staged.discard().await;
                    }
                }
                inbox.expected.clear();
            });
        }
        // The temp directory this peer's clipboard-file receives landed in,
        // gone with the session — leftovers from a past session are a leak
        // (docs/bugs/14-clipboard-files.md #3).
        if let Some(base) = crate::config::clipboard_files_dir() {
            let dir = base.join(self.label_of(&peer));
            tokio::spawn(async move {
                let _ = tokio::fs::remove_dir_all(&dir).await;
            });
        }
    }

    /// Starts or stops the clipboard watcher to match what the live sessions
    /// currently allow (§8.1 applied to §9.2).
    ///
    /// The rule is the one that keeps capture off when nobody is watching:
    /// with no session holding `clipboard_read` and no open view of this
    /// node's own, this desktop's clipboard is not read at all — not read
    /// and discarded. Called from every place a grant, a session, a view or
    /// a connection can change.
    ///
    /// Two independent reasons can justify the read, and either is enough:
    ///
    /// - **Host side.** A session this node granted holds `clipboard_read`,
    ///   so this desktop's own copies are due to that guest — or it holds
    ///   `file_transfer`, which is the grant a *file* copied here travels
    ///   under (`permits_files`; docs/bugs/14-clipboard-files.md #4). Both
    ///   are read from the same poll, so leaving the second one out is what
    ///   would make copying a file on a host that shares files but not text
    ///   do nothing at all.
    /// - **Guest side** (docs/bugs/10-clipboard-auto.md #1). `self.views` is
    ///   non-empty: this node is watching at least one host, and its own
    ///   clipboard changes are worth *offering* there the moment they
    ///   happen — the same offer a manual toolbar press used to make, now
    ///   made automatically. The host decides on arrival whether to accept
    ///   it, against its own `clipboard_write`; this side holds no grant to
    ///   consult (ADR 0029, ADR 0030), so an open view is the only local
    ///   fact that can gate the read at all.
    fn refresh_clipboard_watch(&self) {
        let host_side = self.sessions.active().into_iter().any(|(peer, _, grants)| {
            self.sessions.state(&peer) == SessionState::Active
                && (clip::permits(grants, ClipboardFlow::HostToGuest)
                    || clip::permits_files(grants))
        });
        let guest_side = !self.views.is_empty();
        self.clipboard_worker.set_watching(host_side || guest_side);
    }

    /// Host side: grants `role` and, if it carries `view`, registers the peer
    /// as a viewer — which is what starts capture (§8.1, §11).
    ///
    /// A platform with no capture backend still grants: consent, input and the
    /// control channel work regardless, and the guest is told there is no
    /// picture rather than being refused a session it did ask for (§18).
    fn on_grant(&mut self, label: &str, role: Role) -> Result<(), ActorError> {
        let peer = self.resolve(label)?;
        self.grant_role(peer, role)
    }

    /// Starts a granted session, whichever decision produced it.
    ///
    /// Split out of [`Self::on_grant`] so the unattended path of §8 reaches
    /// exactly the same code as the host user's own click: one place decides
    /// (`lumepeer-core`), one place acts on the decision, and an admission
    /// that skipped the dialog cannot end up with a different session shape
    /// than one that did not (ADR 0033).
    fn grant_role(&mut self, peer: NodeId, role: Role) -> Result<(), ActorError> {
        let label = self.label_of(&peer);
        let label = label.as_str();
        self.sessions.grant(peer, role).map_err(ActorError::Core)?;
        self.send_to(&peer, MessageKind::ConsentGrant(role));
        if self.sessions.grants(&peer).is_some_and(|g| g.view) {
            if let Err(error) = lock_capture(&self.capture).add_viewer(peer) {
                tracing::warn!(
                    peer = %label,
                    %error,
                    "consent granted but this platform cannot capture"
                );
            }
            // Announced here rather than waiting for the guest's media dial:
            // the guest opens its window on the `ConsentGrant` this function
            // just queued, and the control stream is reliable and ordered, so
            // the reason arrives as that window's first news instead of a
            // reconnect window later (§18).
            if let Some(reason) = self.health.fault() {
                self.announce_media_fault(peer, reason);
            }
            // Same reasoning, and the same moment: the guest's picker has no
            // way to ask for this list — there is no request message and
            // adding one would be a protocol change — so the announcement is
            // what puts it there, and it goes out with the grant that made the
            // guest a viewer in the first place (§11; ADR 0028).
            self.announce_monitors(peer);
            // Same trigger again, for the host's own display modes
            // (docs/bugs/16-host-display-mode.md #2; ADR 0048). What goes out
            // is whatever the session's own `display_mode` grant says right
            // now — the real list for a full-control guest, the "not granted"
            // reason for a lesser one — and `on_set_grant` re-announces
            // whenever the host moves the switch afterwards.
            self.announce_display_modes(peer);
        }
        tracing::info!(peer = %label, ?role, "consent granted");
        self.audit(
            &peer,
            lumepeer_core::audit::AuditEvent::ConsentGranted { role },
        );
        // The role is the only thing that moves `input` — it is not one of the
        // four independent grants (ADR 0029) — so a grant is exactly when the
        // §15 `InputToggled` event happens, and the value is the session's own.
        let input = self.sessions.grants(&peer).is_some_and(|g| g.input);
        self.audit(
            &peer,
            lumepeer_core::audit::AuditEvent::InputToggled { enabled: input },
        );
        self.refresh_clipboard_watch();
        // A role change moves the `input` grant, and that is one of the two
        // things the cursor channel is decided on (§11).
        self.refresh_cursor_embedding();
        Ok(())
    }

    /// Both directions of "end this session now".
    ///
    /// On the host it is the revoke of §8.1. On the guest, where the watched
    /// host has no entry in this node's own `SessionManager`, it is the view
    /// window closing: the control connection is dropped, which the host sees
    /// as a disconnect and answers with its own `remove_viewer`.
    fn on_revoke(&mut self, label: &str) -> Result<(), ActorError> {
        let peer = self.resolve(label)?;
        if self.views.contains_key(&peer) {
            tracing::info!(peer = %label, "leaving a session from the view window");
            self.settle_connect(peer, ConnectPhase::Idle);
            self.drop_file_state(peer);
            // Records the host into the remembered list on its way out.
            self.stop_view(peer);
            self.host_addrs.remove(&peer);
            // The user closing the window on purpose, not a protocol fault —
            // the malformed code close_connection sends would otherwise make
            // an ordinary exit look like an error in the host's own log
            // (docs/bugs/03-connection-list.md, task 3).
            self.close_connection_normal(peer);
            return Ok(());
        }
        // Nothing is written to the remembered-hosts list here. This branch is
        // the *host* ending a session it granted, and a host keeps no record of
        // who visited it: it decided once, the decision ended with the session,
        // and a list it never asked for is not the app's to build (ADR 0016).
        self.sessions.revoke(peer).map_err(ActorError::Core)?;
        self.send_to(&peer, MessageKind::ConsentRevoke);
        self.stop_media(peer);
        // A revoked session cannot be one of the reasons the clipboard is
        // being watched, and the last one leaving stops the poll outright.
        self.clipboard.remove(&peer);
        self.clipboard_inbound.remove(&peer);
        self.refresh_clipboard_watch();
        // §4 in the direction that matters: a revoke must not have to wait
        // for a 500 MiB transfer to finish before it takes effect, so the
        // file connection is closed here and now.
        self.drop_file_state(peer);
        tracing::info!(peer = %label, "consent revoked");
        self.audit(&peer, lumepeer_core::audit::AuditEvent::ConsentRevoked);
        Ok(())
    }

    /// Host side: turns one independent grant of `label`'s session on or off
    /// (§8.2; ADR 0029).
    ///
    /// Nothing is decided here and nothing goes out on the wire: grants are
    /// not a wire concept, each side checks its own copy, and the guest simply
    /// finds the next clipboard or file attempt permitted or refused. The
    /// audit event the core hands back is logged rather than dropped so the
    /// host has a record of widening its own session (§15).
    ///
    /// Switching a grant *off* is the half that has to reach further than the
    /// next attempt. A grant already being spent — a clipboard being polled,
    /// a transfer moving bytes, a recording being written — has to stop when
    /// the switch does, or the host would watch the permission go and the
    /// activity continue. That is §4's rule about a revoke not queueing behind
    /// a 500 MiB transfer, applied to the finer switch beside it.
    fn on_set_grant(
        &mut self,
        label: &str,
        grant: IndependentGrant,
        allowed: bool,
    ) -> Result<(), ActorError> {
        let peer = self.resolve(label)?;
        let event = self
            .sessions
            .set_grant(peer, grant, allowed)
            .map_err(ActorError::Core)?;
        // The label is already the pseudonymized tag; the raw `NodeId` never
        // reaches a log line (§15).
        tracing::info!(peer = %label, ?event, "an independent grant changed");
        self.audit(&peer, event);
        // `clipboard_read` is what decides whether this desktop's clipboard
        // is read at all, so the watcher follows the switch immediately
        // rather than on the next session change (ADR 0030).
        self.refresh_clipboard_watch();
        // The guest has no way to ask for this list, so a transition either
        // way has to be re-announced here — the same `display_mode` grant
        // moving is what turns an empty list with `NotGranted` into a real
        // one, or back (docs/bugs/16-host-display-mode.md #2; ADR 0048).
        if grant == IndependentGrant::DisplayMode {
            self.announce_display_modes(peer);
        }
        // `secure_desktop` moves both on and off here, unlike the others
        // below: the encode loop's own `EncodeControl` copy is the "one
        // value in one place" it checks before every attempt
        // (`apps/desktop/src-tauri/src/view.rs`), so a grant switched on
        // mid-stall must reach it just as promptly as a revoke does
        // (ADR 0049). Nothing to update if no media session exists yet —
        // `on_media_accepted` seeds the flag from the live grant when the
        // stream opens.
        if grant == IndependentGrant::SecureDesktop
            && let Some(session) = self.media.get(&peer)
        {
            session.control.set_secure_desktop_allowed(allowed);
        }
        if !allowed {
            // Exhaustive on purpose, with no `_` arm, for the reason
            // `Grants::get` gives: a seventh independent grant must not be
            // able to appear and quietly keep running after it is switched
            // off.
            match grant {
                IndependentGrant::FileTransfer => self.abandon_file_transfers(peer),
                // Idempotent, and the guest is told the recording stopped by
                // the same `RecordAck(false)` a manual stop sends.
                IndependentGrant::Recording => {
                    let _ = self.on_record_toggle(label, false)?;
                }
                // `clipboard_read` is already handled above: the watcher is
                // what "spending" that grant looks like, and it has stopped.
                // `clipboard_write` is spent by the peer, not here, and every
                // inbound payload is checked against the live grant as it
                // arrives. `secure_desktop` is already handled above too.
                IndependentGrant::ClipboardRead
                | IndependentGrant::ClipboardWrite
                | IndependentGrant::SecureDesktop => {}
                // Withdrawing the grant from whoever is actually holding the
                // monitor in a switched mode restores it on the spot, the
                // same way turning `recording` off stops the file rather
                // than waiting for the session to end (docs/bugs/
                // 16-host-display-mode.md #3; ADR 0048).
                // `announce_display_modes` above has already told the guest
                // the permission is gone.
                IndependentGrant::DisplayMode => {
                    if self
                        .display_mode_state
                        .is_some_and(|state| state.owner == peer)
                    {
                        self.restore_display_mode();
                    }
                }
            }
        }
        Ok(())
    }

    /// Host side: the address book as the UI sees it (§8; ADR 0034).
    ///
    /// The raw `NodeId` each entry is keyed on stays here: what goes out is
    /// the same pseudonymized label every other panel names a peer by (§15).
    /// An entry whose key no longer decodes is skipped by `peers()` rather
    /// than shown as a device nothing can act on.
    fn on_address_book_list(&self) -> Vec<AddressBookRow> {
        self.address_book
            .book()
            .peers()
            .map(|(peer, entry)| AddressBookRow {
                peer_label: self.label_of(&peer),
                name: entry.label.clone(),
                tags: entry.tags.clone(),
                notes: entry.notes.clone(),
                trusted: entry.trusted,
                connected: self.connections.contains_key(&peer),
            })
            .collect()
    }

    /// Host side: saves or updates one device, keeping its trust flag.
    ///
    /// Trust is never set here, not even for a device that already has it
    /// withdrawn and back: editing a name must not be a path to a permission,
    /// so this preserves whatever `set_trusted` last decided and changes
    /// nothing else (§2.1).
    fn on_address_book_upsert(
        &mut self,
        label: &str,
        name: String,
        tags: Vec<String>,
        notes: String,
    ) -> Result<(), ActorError> {
        let peer = self.resolve(label)?;
        let trusted = self.address_book.book().is_trusted(&peer);
        self.address_book.upsert(
            &peer,
            AddressEntry {
                label: name,
                tags,
                notes,
                trusted,
            },
        );
        tracing::info!(peer = %label, trusted, "address book entry saved");
        Ok(())
    }

    /// Host side: forgets one device, and with it any trust it held.
    fn on_address_book_remove(&mut self, label: &str) -> Result<(), ActorError> {
        let peer = self.resolve(label)?;
        if !self.address_book.remove(&peer) {
            return Err(ActorError::UnknownPeer);
        }
        tracing::info!(peer = %label, "address book entry removed");
        Ok(())
    }

    /// Host side: marks a device trusted, or withdraws that (§8; ADR 0034).
    ///
    /// The only place in the process where the trust flag moves, and it is
    /// reachable only from the host's own main window. Nothing about a
    /// successful connection, an invite or a login ever calls it: trust is
    /// something the host user decides in advance, never something a peer can
    /// earn by turning up.
    fn on_address_book_set_trusted(
        &mut self,
        label: &str,
        trusted: bool,
    ) -> Result<(), ActorError> {
        let peer = self.resolve(label)?;
        let Some(now) = self.address_book.set_trusted(&peer, trusted) else {
            return Err(ActorError::UnknownPeer);
        };
        // Widening the host's own exposure, so it is logged like a grant
        // change (§15); the label is already the pseudonymized tag, and the
        // device's name and notes stay out of the log as host-identifying
        // free text.
        tracing::info!(peer = %label, trusted = now, "device trust changed");
        self.audit(&peer, AuditEvent::DeviceTrustChanged { trusted: now });
        Ok(())
    }

    /// Host side: sets or replaces the device password (§8; ADR 0033).
    ///
    /// The policy check is `lumepeer-core`'s, and the hash never leaves it in
    /// any other form: what lands in the keystore is the PHC string the core
    /// produced. A keystore that refuses the write undoes the change rather
    /// than leaving the host with a password only this process knows —
    /// discovering that after a restart is the worst moment to discover it.
    fn on_unattended_set_password(&mut self, password: &str) -> Result<(), ActorError> {
        let previous = self.unattended.stored_secret().map(ToOwned::to_owned);
        self.unattended
            .set_password(password)
            .map_err(ActorError::Unattended)?;
        if let Err(error) = self.unattended_store.save_password(&self.unattended) {
            match previous {
                Some(phc) => self.unattended.restore_password_hash(&phc),
                None => self.unattended.disable(),
            }
            return Err(ActorError::Net(error));
        }
        tracing::info!("the unattended device password was set");
        Ok(())
    }

    /// Host side: turns unattended access off and forgets both factors.
    ///
    /// Sessions already granted through it are left alone: they were granted,
    /// and ending them is what `session_revoke` is for. What stops is any
    /// *further* admission — including one already in flight, which is why the
    /// pending set is cleared here too.
    fn on_unattended_disable(&mut self) -> Result<(), ActorError> {
        self.unattended.disable();
        self.unattended_pending.clear();
        self.unattended_store.clear().map_err(ActorError::Net)?;
        tracing::info!("unattended access turned off");
        Ok(())
    }

    /// Host side: turns the second factor on or off (§8).
    ///
    /// Turning it on mints a fresh 20-byte secret from the platform CSPRNG and
    /// hands it back once, because an authenticator app cannot be provisioned
    /// without seeing it. Nothing keeps a copy for the UI to ask for again.
    fn on_unattended_set_totp(
        &mut self,
        enabled: bool,
    ) -> Result<Option<TotpProvisioning>, ActorError> {
        if !enabled {
            self.unattended.clear_totp_secret();
            self.unattended_store
                .save_totp(&self.unattended)
                .map_err(ActorError::Net)?;
            tracing::info!("the unattended second factor was turned off");
            return Ok(None);
        }
        if !self.unattended.enabled() {
            // A second factor without a first is not a gate: refuse rather
            // than store a secret that could never be reached (§2.1).
            return Err(ActorError::Unattended(UnattendedError::NotConfigured));
        }
        let previous = self.unattended.stored_totp_secret().copied();
        let mut secret = [0u8; 20];
        rand::rng().fill_bytes(&mut secret);
        self.unattended.set_totp_secret(secret);
        if let Err(error) = self.unattended_store.save_totp(&self.unattended) {
            match previous {
                Some(old) => self.unattended.set_totp_secret(old),
                None => self.unattended.clear_totp_secret(),
            }
            return Err(ActorError::Net(error));
        }
        let totp = self
            .unattended
            .totp()
            .ok_or(ActorError::Unattended(UnattendedError::NotConfigured))?;
        tracing::info!("the unattended second factor was turned on");
        Ok(Some(TotpProvisioning {
            secret_base32: totp.secret_base32(),
            // A fixed account name, never this machine's: the URI is meant to
            // be shown on screen and photographed (§15).
            uri: totp.provisioning_uri("device"),
        }))
    }

    /// Host side: chooses the role a successful admission is granted (§8.2).
    ///
    /// Takes effect on the next admission. A session already running keeps the
    /// snapshot it was granted under, the same rule that stops a policy edit
    /// widening a live session.
    fn on_unattended_set_role(&mut self, role: Role) -> Result<(), ActorError> {
        let previous = self.unattended.role();
        self.unattended.set_role(role);
        if let Err(error) = self.unattended_store.save_role(&self.unattended) {
            self.unattended.set_role(previous);
            return Err(ActorError::Net(error));
        }
        tracing::info!(?role, "the unattended role was set");
        Ok(())
    }

    /// Guest side: abandons this node's own outgoing connect attempt, at
    /// whatever stage it is at (docs/bugs/02-connect-form.md, task 3).
    ///
    /// Clearing `connect_peer` is the whole fix for a dial still in flight:
    /// `on_dialed`'s own staleness check (`self.connect_peer != Some(peer)`)
    /// discards the result once it lands, the same way an already-superseded
    /// attempt is discarded today. A connection already open and waiting on
    /// the far side is closed outright, with the normal close code — the user
    /// walked away, which is not a protocol error and must not be reported as
    /// one (see `03`, task 3, for the sibling case on the view-window side).
    fn on_connect_cancel(&mut self) {
        let Some(peer) = self.connect_peer.take() else {
            return;
        };
        if matches!(
            self.connect_phase,
            ConnectPhase::AwaitingConsent | ConnectPhase::AwaitingCredentials
        ) {
            self.close_connection_normal(peer);
        }
        self.connect_phase = ConnectPhase::Idle;
        self.connect_failure = None;
        self.connect_code_required = false;
        self.connect_retry_secs = None;
        self.pending_remember = None;
        self.connect_credentials_auto = false;
    }

    /// Guest side: answers the host's credential challenge (§8; ADR 0033).
    ///
    /// The password reaches the wire and stops there as far as this method is
    /// concerned; the reply says only that it went out. Whether it was right
    /// comes back as a grant or a rejection, which is also where `remember`
    /// takes effect — see `pending_remember` (docs/bugs/02-connect-form.md,
    /// task 6).
    fn on_unattended_submit(
        &mut self,
        password: &str,
        code: Option<String>,
        remember: bool,
    ) -> Result<(), ActorError> {
        if self.connect_phase != ConnectPhase::AwaitingCredentials {
            return Err(ActorError::Core(CoreError::NotPermitted));
        }
        let peer = self.connect_peer.ok_or(ActorError::UnknownPeer)?;
        self.connect_failure = None;
        self.connect_retry_secs = None;
        self.pending_remember = remember.then(|| password.to_owned());
        self.send_to(
            &peer,
            MessageKind::UnattendedAuth {
                password: password.to_owned(),
                code,
            },
        );
        Ok(())
    }

    /// Guest side: forwards one input event, gated on this node's own copy of
    /// the grant. The host re-checks authoritatively (§2.3).
    fn on_input(&mut self, label: &str, event: InputEventPayload) -> Result<(), ActorError> {
        let peer = self.resolve(label)?;
        let permitted = self
            .views
            .get(&peer)
            .ok_or(ActorError::UnknownPeer)?
            .grants
            .input;
        if !permitted {
            return Err(ActorError::Core(CoreError::NotPermitted));
        }
        self.send_to(&peer, MessageKind::InputEvent(event));
        Ok(())
    }

    /// Guest side: validate the invite here, then run the dial and the
    /// handshake **off** the actor loop (ADR 0027).
    ///
    /// Everything this can decide without touching the network is decided
    /// synchronously, so a bad code or a duplicate still fails the IPC call
    /// outright and the user is told which it was. The part that talks to the
    /// network is the part that can take fifteen seconds, and it must not run
    /// here: `run` awaits `handle_command`, so a dial on this thread stops the
    /// actor accepting incoming connections, delivering `ConsentGrant`s,
    /// serving the status the UI polls once a second and answering the view
    /// window's frame poll. That is the "the app freezes and then says it
    /// could not connect" report this fixes.
    fn spawn_dial(&mut self, raw: &str) -> Result<(), ActorError> {
        let ticket = InviteTicket::from_code(raw).map_err(ActorError::Net)?;
        let addr = ticket.endpoint_addr().map_err(ActorError::Net)?;
        // Dialing a host this node is already talking to would replace the live
        // connection in `connections`, and the replacement's own teardown would
        // then close the session that was working. Refusing here is what makes
        // a second Connect harmless rather than destructive (§21 punch-list
        // item 6).
        if self.connections.contains_key(&addr.id) {
            tracing::info!(peer = %self.label_of(&addr.id), "already connected to this host");
            return Err(ActorError::Net(NetError::AlreadyConnected));
        }
        // A dial already in flight owns `connect_phase`. Letting a second one
        // start would leave two tasks racing to report an outcome into one
        // slot, and the loser would overwrite the winner.
        if self.connect_phase == ConnectPhase::Dialing {
            tracing::info!("a connect attempt is already in flight");
            return Err(ActorError::Net(NetError::AlreadyConnected));
        }
        let proof = postcard::to_allocvec(&ticket)
            .map_err(|_| ActorError::Net(NetError::MalformedTicket))?;

        self.connect_phase = ConnectPhase::Dialing;
        self.connect_peer = Some(addr.id);
        self.connect_failure = None;
        // A previous attempt that never reached a grant or a refusal — the
        // transport dropped mid-credential-exchange, say — could otherwise
        // leave a stale password or auto-submit flag behind for this new,
        // possibly different, host to inherit (docs/bugs/02-connect-form.md,
        // task 6).
        self.pending_remember = None;
        self.connect_credentials_auto = false;

        let endpoint = self.endpoint.clone();
        let tx = self.events_tx.clone();
        let tag = self.label_of(&addr.id);
        let code = raw.to_owned();
        let role = ticket.allowed_request;
        let target = addr.clone();
        tokio::spawn(async move {
            let result = dial_with_retries(&endpoint, &target, role, proof, &tag).await;
            let _ = tx
                .send(ActorEvent::Dialed {
                    peer: target.id,
                    code,
                    addr: Box::new(target),
                    result: result.map(Box::new),
                })
                .await;
        });
        Ok(())
    }

    /// Guest side: takes the outcome of [`Self::spawn_dial`] on the actor's own
    /// thread, which is the only one allowed to store a connection.
    fn on_dialed(
        &mut self,
        peer: NodeId,
        code: String,
        addr: iroh::EndpointAddr,
        result: Result<Box<ControlConnection>, NetError>,
    ) {
        let tag = self.label_of(&peer);
        // The attempt this reports on has been superseded — the user started
        // another one, or the session it belonged to is already gone. Dropping
        // the connection here closes it, which is what we want: nothing is
        // waiting for it.
        if self.connect_peer != Some(peer) {
            tracing::info!(peer = %tag, "discarding the result of a superseded dial");
            return;
        }
        let control = match result {
            Ok(control) => *control,
            Err(error) => {
                tracing::warn!(peer = %tag, %error, "invite connect failed");
                self.connect_failure = Some(crate::commands::net_error_code(&error));
                self.connect_phase = ConnectPhase::Failed;
                self.connect_peer = None;
                return;
            }
        };
        // The handshake proves who answered; the ticket only claimed it.
        let peer = control.peer();
        tracing::info!(peer = %self.label_of(&peer), "connected to a host, awaiting consent");
        // Remembered for the media dial that follows a `ConsentGrant`: the
        // ticket is the only place this address is known without discovery.
        self.host_addrs.insert(peer, addr);
        // Remembered so the history row written when this session ends can dial
        // the same host again (ADR 0016).
        self.host_invites.insert(peer, code);
        self.connect_phase = ConnectPhase::AwaitingConsent;
        self.connect_peer = Some(peer);
        // Guest side: this node is the one that *receives* `MediaUnavailable`,
        // so there is no peer capability to remember here.
        //
        // File transfer is the exception, and the minor is all there is to go
        // on: `HelloAck` carries no feature list, so what a *host* understands
        // can only be read off its protocol minor (§9.1; ADR 0032). Either
        // side may offer a file, so the guest does have to know.
        if control.peer_minor() >= FILE_TRANSFER_MINOR {
            self.speaks_file_transfer.insert(peer);
        } else {
            self.speaks_file_transfer.remove(&peer);
        }
        // Same reasoning for clipboard files: either side may have some, so
        // the guest needs the same floor `FILE_TRANSFER_MINOR` reads off the
        // minor rather than a feature string (docs/bugs/
        // 14-clipboard-files.md #2; ADR 0047).
        if control.peer_minor() >= CLIPBOARD_FILES_MINOR {
            self.speaks_clipboard_files.insert(peer);
        } else {
            self.speaks_clipboard_files.remove(&peer);
        }
        // Same reasoning for the receiver reports this node is about to start
        // producing: only the host's minor says whether it can decode one, and
        // sending one it cannot would close the connection over a diagnostic
        // (§9.1; ADR 0037).
        self.receiver_reports.entry(peer).or_default().to_peer =
            control.peer_minor() >= RECEIVER_REPORT_MINOR;
        // Same reasoning for a manual scale ceiling this node might ask for
        // (D7, docs/bugs/13-stream-resolution.md).
        self.stream_scale.entry(peer).or_default().to_peer =
            control.peer_minor() >= STREAM_SCALE_MINOR;
        // Same reasoning for the host's own display modes this node might ask
        // to change (docs/bugs/16-host-display-mode.md; ADR 0048).
        self.display_mode.entry(peer).or_default().to_peer =
            control.peer_minor() >= DISPLAY_MODE_MINOR;
        self.adopt(control, peer, false, false, false);
    }
}

/// Measures, names and hashes one local file for a `FileOffer` (§9.2).
///
/// Runs on its own task: hashing `FILE_OFFER_MAX_BYTES` is a full disk pass,
/// and the actor loop is not where a disk pass belongs (ADR 0027).
async fn prepare_offer(path: &std::path::Path) -> Result<(String, u64, [u8; 32]), NetError> {
    let meta = tokio::fs::metadata(path)
        .await
        .map_err(|e| NetError::Io(e.to_string()))?;
    if !meta.is_file() {
        return Err(NetError::Io("not a regular file".to_owned()));
    }
    let size = meta.len();
    if size > FILE_OFFER_MAX_BYTES {
        return Err(NetError::Io("over the offer size limit".to_owned()));
    }
    // The same normalization the receiver will apply. Refusing here means a
    // file this machine cannot name safely is never offered, rather than
    // being declined on the far side for reasons the sender cannot see.
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(safe_file_name)
        .ok_or_else(|| NetError::Io("the file has no name that can be offered".to_owned()))?;
    let hash = hash_file(path).await?;
    Ok((name, size, hash))
}

/// Measures and names one local file for a `ClipboardFileOffer` entry
/// (docs/bugs/14-clipboard-files.md #2), without hashing it.
///
/// A clipboard file list is announced before anyone has agreed to receive
/// any of it, so hashing every entry up front — a full disk pass each,
/// exactly what `prepare_offer` does for a file the user already chose to
/// send — would be a disk pass nobody asked for yet. The hash is computed
/// once, in `prepare_offer`, only for the specific file the peer accepts.
async fn stat_offer(path: &std::path::Path) -> Result<(String, u64), NetError> {
    let meta = tokio::fs::metadata(path)
        .await
        .map_err(|e| NetError::Io(e.to_string()))?;
    if !meta.is_file() {
        return Err(NetError::Io("not a regular file".to_owned()));
    }
    let size = meta.len();
    if size > FILE_OFFER_MAX_BYTES {
        return Err(NetError::Io("over the offer size limit".to_owned()));
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(safe_file_name)
        .ok_or_else(|| NetError::Io("the file has no name that can be offered".to_owned()))?;
    Ok((name, size))
}

/// Picks a path in `directory` for `name` that is not already taken.
///
/// A transfer must never quietly replace a file the user already had. The
/// suffix goes before the extension so the result still opens in the same
/// application.
fn unique_destination(directory: &std::path::Path, name: &str) -> std::path::PathBuf {
    let candidate = directory.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let path = std::path::Path::new(name);
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(name)
        .to_owned();
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_default();
    for n in 1..1000u32 {
        let candidate = directory.join(format!("{stem} ({n}){extension}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    // A directory with a thousand copies of one name is not a case worth more
    // code: the last candidate is returned and the export fails loudly.
    directory.join(format!("{stem} (1000){extension}"))
}

/// Reads one transfer's chunks off a unidirectional file stream.
///
/// Everything here runs away from the actor: a 256 KiB chunk is written to
/// staging under the peer's own lock, and only a byte count goes back through
/// the mailbox.
async fn read_transfer_stream(
    mut recv: iroh::endpoint::RecvStream,
    channel: FileChannel,
    peer: NodeId,
    tag: String,
    events: mpsc::Sender<ActorEvent>,
) {
    let mut current: Option<TransferId> = None;
    loop {
        let (id, offset, bytes) = match read_chunk(&mut recv).await {
            Ok(chunk) => chunk,
            Err(NetError::TruncatedStream(_) | NetError::Io(_)) => break,
            Err(error) => {
                tracing::warn!(peer = %tag, %error, "a file chunk was refused");
                break;
            }
        };
        if !wait_for_start(&channel, id).await {
            tracing::warn!(peer = %tag, "chunks arrived for a transfer that was never announced");
            return;
        }
        let mut inbox = channel.inbox.lock().await;
        // Accounting first, bytes second: `apply_chunk` is what refuses a gap,
        // an overrun and a transfer that already ended, and nothing reaches
        // the disk until it has said yes (§3.2, §9.2).
        if let Err(error) = inbox.tracker.apply_chunk(id, offset, bytes.len()) {
            tracing::warn!(peer = %tag, %error, "a file chunk did not fit its transfer");
            drop(inbox);
            let _ = events
                .send(ActorEvent::File(FileEvent::Finished {
                    peer,
                    id,
                    state: TransferState::Failed,
                }))
                .await;
            return;
        }
        if let Some(staged) = inbox.staged.get_mut(&id)
            && let Err(error) = staged.append(&bytes).await
        {
            tracing::warn!(peer = %tag, %error, "a chunk could not be staged");
            inbox.tracker.cancel(id);
            drop(inbox);
            let _ = events
                .send(ActorEvent::File(FileEvent::Finished {
                    peer,
                    id,
                    state: TransferState::Failed,
                }))
                .await;
            return;
        }
        inbox.tracker.hash_chunk(id, &bytes);
        let received = inbox.tracker.state(id).map_or(0, |state| state.received);
        let complete = inbox
            .tracker
            .state(id)
            .is_some_and(|state| state.received == state.total);
        drop(inbox);
        current = Some(id);
        let _ = events
            .send(ActorEvent::File(FileEvent::Progress {
                peer,
                id,
                moved: received,
            }))
            .await;
        if complete {
            finish_transfer(&channel, id, peer, &events).await;
            return;
        }
    }
    // The stream ended without the transfer completing. That is a drop, not a
    // failure: the tracker keeps what arrived, and a sender coming back picks
    // up from the last ack rather than from zero (§10).
    if let Some(id) = current {
        tracing::debug!(peer = %tag, transfer = id, "a file stream ended mid-transfer");
    }
}

/// Waits until `id` has been announced by a `FileTransferStart`.
///
/// The control channel and `rd/file/1` are separate QUIC connections (§4), so
/// nothing orders the announcement against the first chunk. Returns `false`
/// when the wait ran out, which aborts this stream and nothing else.
async fn wait_for_start(channel: &FileChannel, id: TransferId) -> bool {
    let mut starts = channel.starts.subscribe();
    let deadline = tokio::time::Instant::now()
        + std::time::Duration::from_secs(FILE_TRANSFER_START_TIMEOUT_SECS);
    loop {
        if channel.inbox.lock().await.expected.contains_key(&id) {
            return true;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return false;
        }
        // `changed()` is edge-triggered from what this receiver last saw, so a
        // start landing between the check above and this wait cannot be lost.
        if tokio::time::timeout(remaining, starts.changed())
            .await
            .is_err()
        {
            return false;
        }
    }
}

/// Verifies a completed transfer and exports it, or leaves nothing behind.
async fn finish_transfer(
    channel: &FileChannel,
    id: TransferId,
    peer: NodeId,
    events: &mpsc::Sender<ActorEvent>,
) {
    let mut inbox = channel.inbox.lock().await;
    let Some((expected, destination)) = inbox.expected.remove(&id) else {
        return;
    };
    let verified = inbox.tracker.finish(id, expected);
    let staged = inbox.staged.remove(&id);
    drop(inbox);

    let state = match (verified, staged) {
        (true, Some(staged)) => match staged.export(&destination).await {
            Ok(()) => TransferState::Completed,
            Err(error) => {
                tracing::warn!(%error, "a verified transfer could not be exported");
                TransferState::Failed
            }
        },
        // The hash did not match the offer. Nothing leaves staging (§9.2):
        // a file under the expected name that is not the expected file is
        // worse than no file at all.
        (false, Some(staged)) => {
            staged.discard().await;
            TransferState::Failed
        }
        (_, None) => TransferState::Failed,
    };
    let _ = events
        .send(ActorEvent::File(FileEvent::Finished { peer, id, state }))
        .await;
}

/// The wire form of an unattended refusal (§8, §18).
///
/// Deliberately lossy in one direction only: "a password was required but not
/// presented" and "the password was wrong" collapse into the same answer, as
/// do the two code cases, because the difference is only useful to somebody
/// probing the gate. Nothing collapses that the guest needs in order to act —
/// which factor to retype, and how long a lockout has left, both survive.
const fn rejection_of(error: &UnattendedError) -> UnattendedRejection {
    match *error {
        UnattendedError::BadPassword | UnattendedError::MissingPassword => {
            UnattendedRejection::BadPassword
        }
        UnattendedError::BadCode | UnattendedError::MissingCode => UnattendedRejection::BadCode,
        UnattendedError::LockedOut { remaining_secs } => {
            UnattendedRejection::LockedOut { remaining_secs }
        }
        // Not a verdict on the guest: this host cannot decide at all. The
        // password policy error cannot reach here — it is raised only when the
        // *host* sets a password — but it is mapped rather than left to a
        // catch-all so a new variant has to be thought about.
        UnattendedError::NotConfigured
        | UnattendedError::CorruptStore
        | UnattendedError::SaltGeneration
        | UnattendedError::PasswordPolicy { .. } => UnattendedRejection::Unavailable,
    }
}

/// Sorts one accepted connection by ALPN and, if it is control, runs the host
/// handshake and verifies the invite it carries (§4.1, §9.1, §18).
///
/// Authenticates; authorizes nothing. Whether a media connection may exist,
/// and whether a verified ticket may become a session, are questions only the
/// actor can answer, because only the actor can read `SessionManager` (§2.3).
async fn classify_incoming(
    connection: Option<iroh::endpoint::Connection>,
    verifying_key: &ed25519_dalek::VerifyingKey,
    salt: &[u8; 32],
) -> Option<Accepted> {
    let connection = connection?;
    let peer = connection.remote_id();
    let tag = peer_tag(salt, &peer);
    match Channel::from_alpn(connection.alpn()) {
        Some(Channel::Control) => {}
        // Media is authenticated here and authorized by the actor: this task
        // cannot see whether the peer holds a live, granted control session,
        // and guessing would be a way to widen a grant outside
        // `lumepeer-core` (§2.3).
        Some(Channel::Media) => {
            return Some(Accepted::Media {
                connection: Box::new(connection),
                peer,
            });
        }
        // Authenticated here and authorized by the actor, exactly like
        // media: this task cannot see whether the peer holds a live session
        // carrying `file_transfer`, and guessing would widen a grant outside
        // `lumepeer-core` (§2.3). What has not changed is the invariant the
        // old unconditional close protected — an unauthenticated peer must
        // not be able to park a file connection in the control handshake's
        // read (§4.1). It still cannot: the ALPN is decided before any read,
        // this arm never runs the handshake, and the actor closes the
        // connection on the spot unless a granted session already exists.
        Some(Channel::File) => {
            return Some(Accepted::File {
                connection: Box::new(connection),
                peer,
            });
        }
        None => {
            tracing::warn!(peer = %tag, "closing a connection on an unknown ALPN");
            connection.close(
                lumepeer_net::connection::CLOSE_MALFORMED.into(),
                lumepeer_net::error::close_code::MALFORMED.as_bytes(),
            );
            return None;
        }
    }
    let (control, hello) = match lumepeer_net::host_handshake(connection).await {
        Ok(pair) => pair,
        Err(error) => {
            tracing::warn!(peer = %tag, %error, "control handshake failed");
            return None;
        }
    };
    let Ok(ticket) = postcard::from_bytes::<InviteTicket>(&hello.invite_proof) else {
        tracing::warn!(peer = %tag, "invite proof is not a ticket");
        control.close_with(&NetError::InvalidTicket);
        return None;
    };
    if let Err(error) = ticket.verify(verifying_key, unix_now()) {
        tracing::warn!(peer = %tag, %error, "invite ticket did not verify");
        control.close_with(&NetError::InvalidTicket);
        return None;
    }
    Some(Accepted::Control {
        speaks_receiver_report: hello
            .features
            .iter()
            .any(|feature| feature == FEATURE_RECEIVER_REPORT),
        speaks_cursor_shape: hello
            .features
            .iter()
            .any(|feature| feature == FEATURE_CURSOR_SHAPE),
        announces_media_faults: hello
            .features
            .iter()
            .any(|feature| feature == FEATURE_MEDIA_UNAVAILABLE),
        speaks_remote_sas: hello
            .features
            .iter()
            .any(|feature| feature == lumepeer_core::protocol::FEATURE_REMOTE_SAS),
        speaks_file_transfer: hello
            .features
            .iter()
            .any(|feature| feature == FEATURE_FILE_TRANSFER),
        speaks_unattended: hello
            .features
            .iter()
            .any(|feature| feature == FEATURE_UNATTENDED),
        speaks_stream_scale: hello
            .features
            .iter()
            .any(|feature| feature == FEATURE_STREAM_SCALE),
        speaks_clipboard_files: hello
            .features
            .iter()
            .any(|feature| feature == FEATURE_CLIPBOARD_FILES),
        speaks_display_mode: hello
            .features
            .iter()
            .any(|feature| feature == FEATURE_DISPLAY_MODE),
        connection: Box::new(control),
        peer,
        ticket: Box::new(ticket),
    })
}

/// One outgoing control connection — dial and handshake — retried up to
/// [`DIAL_ATTEMPTS`] times.
///
/// The retry is not decoration. A ticket carries the address set the host had
/// when it read the code out, and by the time a human has pasted it the host
/// may have moved to another relay, its NAT binding may have changed, or its
/// discovery record may not have propagated yet. iroh repairs all of that by
/// itself — but only if something connects again, and until now the only thing
/// that did was the user, who cannot tell a stale address from a dead host
/// (ADR 0027).
///
/// The handshake is retried too, and that is the case this was actually
/// written for. Between these two machines the failure is not a dial that
/// never lands: it is a connection that comes up over the host's relay link
/// and then loses it mid-`Hello` —
///
/// ```text
/// host   21:36:41  issuing an invite  addrs={Relay(euc1-1), …}
/// host   21:36:45  home is now relay use1-1, was Some(euc1-1)
/// host   21:36:52  dropping an incoming connection that did not finish its control handshake in time
/// host   21:36:57  Lost connection to relay server: Ping timeout
/// guest  21:36:59  invite connect failed: stream i/o failed: connection lost
/// ```
///
/// — which is [`NetError::Io`]: this side's own observation that a stream
/// stopped, not anybody's decision, and the very thing a second attempt fixes
/// because by then the host is on a relay it can hold, or a direct path has
/// been punched. What is *not* retried is an answer: a bad ticket, a version
/// mismatch or a refusal is a verdict, and asking again only collects it twice.
async fn dial_with_retries(
    endpoint: &PeerEndpoint,
    addr: &iroh::EndpointAddr,
    role: Role,
    proof: Vec<u8>,
    tag: &str,
) -> Result<ControlConnection, NetError> {
    let attempt_budget = std::time::Duration::from_secs(CONNECT_ATTEMPT_TIMEOUT_SECS);
    let mut last = NetError::Dial("no attempt was made".to_owned());
    for attempt in 1..=DIAL_ATTEMPTS {
        let outcome = tokio::time::timeout(
            attempt_budget,
            connect_once(endpoint, addr, role, proof.clone()),
        )
        .await
        .unwrap_or_else(|_| {
            Err(NetError::Dial(format!(
                "no answer within {CONNECT_ATTEMPT_TIMEOUT_SECS}s"
            )))
        });
        let error = match outcome {
            Ok(control) => return Ok(control),
            Err(error) => error,
        };
        let retryable = matches!(error, NetError::Dial(_) | NetError::Io(_));
        tracing::warn!(
            peer = %tag,
            %error,
            attempt,
            of = DIAL_ATTEMPTS,
            retryable,
            "connect attempt failed"
        );
        if !retryable {
            return Err(error);
        }
        last = error;
        if attempt < DIAL_ATTEMPTS {
            // Jittered so a run of attempts sweeps across a periodically
            // flapping relay link instead of staying locked in step with it
            // (ADR 0050).
            let jitter = rand::rng().random_range(0..=DIAL_RETRY_BACKOFF_JITTER_MS);
            tokio::time::sleep(std::time::Duration::from_millis(
                DIAL_RETRY_BACKOFF_MS + jitter,
            ))
            .await;
        }
    }
    Err(last)
}

/// One attempt: dial the control ALPN and run the guest half of the handshake.
async fn connect_once(
    endpoint: &PeerEndpoint,
    addr: &iroh::EndpointAddr,
    role: Role,
    proof: Vec<u8>,
) -> Result<ControlConnection, NetError> {
    let connection = endpoint.connect_control(addr.clone()).await?;
    // What this build understands, for a host to decide what it may send.
    // An older host ignores an unknown string (§9.1) and simply never sends
    // the message behind it.
    let features = vec![
        FEATURE_MEDIA_UNAVAILABLE.to_owned(),
        FEATURE_FILE_TRANSFER.to_owned(),
        FEATURE_CLIPBOARD_FILES.to_owned(),
        FEATURE_UNATTENDED.to_owned(),
        FEATURE_RECEIVER_REPORT.to_owned(),
        FEATURE_CURSOR_SHAPE.to_owned(),
        FEATURE_STREAM_SCALE.to_owned(),
        FEATURE_DISPLAY_MODE.to_owned(),
    ];
    lumepeer_net::guest_handshake(connection, role, proof, features).await
}

/// Binds the endpoint from the OS keystore identity and spawns the actor.
///
/// Reaching a relay is **not** awaited here: on a LAN-only machine that wait
/// never finishes, and `main` blocks on this call before Tauri creates a
/// window, so blocking it would leave the app with no window and no error at
/// all. Ticket pairing does not need a relay (§7), so the wait runs in the
/// background and only logs.
///
/// # Errors
/// [`NetError`] if the keystore or the endpoint bind fails — surfaced as a
/// startup failure rather than silently degrading (§11.2, §24.5).
/// Opens the keystore, honouring the `LUMEPEER_KEYSTORE=file` override.
///
/// Default is the OS-native backend (`crates/net::keystore::open`). The
/// override selects the encrypted-file store — the documented fallback for
/// headless environments (CI, SSH-run E2E) where no secret-service prompter
/// exists to unlock the keyring. `LUMEPEER_KEYSTORE_PATH` chooses the file
/// location; it defaults to the app data directory.
///
/// # Errors
/// [`NetError`] as [`keystore::open`], or when `LUMEPEER_KEYSTORE=file` is
/// set but no usable path can be derived.
fn open_keystore() -> Result<Box<dyn lumepeer_net::keystore::Keystore>, NetError> {
    const KEYSTORE_ENV: &str = "LUMEPEER_KEYSTORE";
    if std::env::var(KEYSTORE_ENV).as_deref() != Ok("file") {
        return lumepeer_net::keystore::open();
    }
    let path = std::env::var("LUMEPEER_KEYSTORE_PATH").map_err(|_| {
        NetError::Keystore("LUMEPEER_KEYSTORE=file also needs LUMEPEER_KEYSTORE_PATH".to_owned())
    })?;
    let path = std::path::PathBuf::from(path);
    tracing::info!(path = %path.display(), "using the encrypted-file keystore (LUMEPEER_KEYSTORE=file)");
    // The user secret mixes the machine id with the user name: stable for
    // this user on this machine, never written anywhere (§11.2).
    let machine = machine_id();
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_default();
    Ok(Box::new(lumepeer_net::keystore::FileKeystore::new(
        path,
        format!("{machine}:{user}").as_bytes(),
    )))
}

/// Reads `/etc/machine-id` (or the fallback `DBUS` path) for the file-keystore
/// user secret. A missing file is not fatal: an empty id only weakens the
/// secret to the user name, matching the fallback's documented threat model.
fn machine_id() -> String {
    for path in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
        if let Ok(id) = std::fs::read_to_string(path) {
            return id.trim().to_owned();
        }
    }
    String::new()
}

pub async fn spawn_actor(
    app: tauri::AppHandle,
    settings: &crate::config::Settings,
) -> Result<ActorHandle, NetError> {
    let store = open_keystore()?;
    let secret_key = load_or_create(store.as_ref())?;
    let identity = SigningKey::from_bytes(&secret_key.to_bytes());
    let relay = settings.relay_url();
    // Relay-only is a WAN test, never the default: with the IP transports
    // cleared every session lives or dies with one relay link, and a client
    // whose relay flaps cannot connect at all (ADR 0026).
    let endpoint = if settings.relay_only() {
        tracing::info!(
            "transport: relay only — direct IP paths are off, so every session goes over the internet"
        );
        PeerEndpoint::bind_relay_only(secret_key, relay).await?
    } else {
        tracing::info!("transport: direct IP paths preferred, relay as the fallback");
        PeerEndpoint::bind_with_lan(secret_key, relay).await?
    };
    let audit = open_audit_log(&app, store.as_ref()).await;
    // A second, independent handle on the same keystore: every native backend
    // opens its own connection per operation rather than holding one open
    // (see e.g. `SecretServiceKeystore`'s own doc comment), so this costs
    // nothing beyond what `UnattendedStore` below already pays, and it is
    // what lets the two stores own their `Box<dyn Keystore>` outright instead
    // of sharing one behind an `Arc` (docs/bugs/02-connect-form.md, task 6).
    let remembered_password_keystore = open_keystore()?;
    let stores = ActorStores {
        history_path: connection_history_path(&app),
        address_book_path: address_book_path(),
        // The same keystore the identity came from: the unattended password
        // hash and TOTP secret are secret material and `CLAUDE.md` keeps
        // secrets out of `config/*.toml` (§11.2; ADR 0033).
        keystore: store,
        remembered_password_keystore,
        audit,
    };

    let handle = spawn_actor_with(
        endpoint.clone(),
        identity,
        Arc::new(crate::view::TauriViewWindows::new(app)),
        default_capture(),
        crate::clipboard_os::platform_clipboard(),
        stores,
    );

    tokio::spawn({
        let online = Arc::clone(&handle.online);
        async move {
            endpoint.online().await;
            online.store(true, Ordering::Relaxed);
            tracing::info!("endpoint reached a relay; invites are dialable from outside the LAN");
        }
    });

    Ok(handle)
}

/// Where the connection history file lives, if the app data directory can be
/// resolved at all. `None` degrades the feature to in-memory-only for this
/// run rather than failing startup over a convenience list (§18).
/// Where the host's address book lives, if the config directory resolves at
/// all. `None` degrades the book to in-memory-only for this run, which trusts
/// nobody — the safe direction (§18; ADR 0034).
///
/// Alongside the other configuration rather than in the app data directory:
/// it is host-owned policy, in the same place `config/control_policy.toml`
/// lives, and it holds no secrets (a `NodeId` is a public key).
fn address_book_path() -> Option<std::path::PathBuf> {
    let Some(dir) = crate::config::config_dir() else {
        tracing::warn!("cannot resolve the config directory; the address book will not persist");
        return None;
    };
    Some(dir.join("address_book.json"))
}

/// Opens the audit log and starts its daily retention sweep (§15; ADR 0041).
///
/// Every failure here is a warning and a `None`, never a refusal to start: §18
/// says a storage fault degrades the feature that needs the storage. A host
/// that cannot write an audit trail is still a host, and refusing to run would
/// hand anyone who can break the database a way to take the machine offline.
///
/// The one failure worth its own message is a lost install salt over a
/// non-empty log: minting a new one would silently split every peer's history
/// in two, so the log is left untouched and unwritten instead.
async fn open_audit_log(
    app: &tauri::AppHandle,
    keystore: &dyn Keystore,
) -> Option<crate::audit_store::AuditStore> {
    use tauri::Manager as _;

    let path = match app.path().app_local_data_dir() {
        Ok(dir) => dir.join("audit.db"),
        Err(error) => {
            tracing::warn!(%error, "cannot resolve the app data directory; no audit log this run");
            return None;
        }
    };
    let store = match crate::audit_store::AuditStore::open(path, keystore).await {
        Ok(store) => store,
        Err(error) => {
            tracing::warn!(%error, "audit log unavailable; the host runs without an audit trail");
            return None;
        }
    };

    // Once a day, not once per record: the sweep is a table scan and the
    // cutoff moves by seconds. `AuditStore::open` already swept once.
    let daily = store.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(
            lumepeer_core::constants::AUDIT_RETENTION_SWEEP_SECS,
        ));
        ticker.tick().await; // fires immediately; the open already pruned
        loop {
            ticker.tick().await;
            if let Err(error) = daily.prune().await {
                tracing::warn!(%error, "audit log: retention sweep failed");
            }
        }
    });
    Some(store)
}

fn connection_history_path(app: &tauri::AppHandle) -> Option<std::path::PathBuf> {
    use tauri::Manager as _;
    match app.path().app_local_data_dir() {
        Ok(dir) => Some(dir.join("connection_history.json")),
        Err(error) => {
            tracing::warn!(%error, "cannot resolve the app data directory; connection history will not persist");
            None
        }
    }
}

/// Builds the host's media side: the capture controller, whether this platform
/// gave a capture backend at all, and the injector when it can only come from
/// the same session as the pixels (ADR 0010).
///
/// A missing backend must not stop the app from starting or from accepting a
/// session: consent, input and the rest of the control channel work regardless
/// (§18). What it must not do either is stay a log line — the fallback
/// capturer produces nothing forever, which is exactly the silent degradation
/// §18 forbids. So the absence is recorded in [`MediaHealth`], where this
/// host's own status screen and the guest's `MediaUnavailable` both read it
/// (docs/adr/0024).
#[must_use]
pub fn default_capture() -> HostMedia {
    fn controller(capturer: Box<dyn lumepeer_media::capture::ScreenCapturer>) -> SharedCapture {
        Arc::new(std::sync::Mutex::new(CaptureController::new(
            capturer,
            CaptureTarget::PrimaryDisplay,
        )))
    }

    match platform_backend() {
        Ok((capturer, injector)) => HostMedia {
            capture: controller(capturer),
            health: Arc::new(MediaHealth::healthy()),
            injector,
        },
        Err(error) => {
            tracing::warn!(%error, "no capture backend on this platform: sessions stay blank");
            HostMedia {
                capture: controller(Box::new(StubCapturer::default())),
                health: Arc::new(MediaHealth::without_capture()),
                injector: None,
            }
        }
    }
}

/// Everything an actor keeps between runs, in one argument.
///
/// Grouped rather than passed one by one so the seam stays one thing: a test
/// builds an actor that persists nothing ([`ActorStores::in_memory`]), and the
/// real application builds one that persists everything, without either of
/// them growing an argument list nobody can read.
pub struct ActorStores {
    /// Guest side: where the remembered-hosts list lives (ADR 0016).
    pub history_path: Option<std::path::PathBuf>,
    /// Host side: where the address book lives (§8; ADR 0034).
    pub address_book_path: Option<std::path::PathBuf>,
    /// Where the unattended credentials live (§8; ADR 0033). The OS keystore
    /// in the real application, an in-memory stand-in in tests — never a
    /// config file, which `CLAUDE.md` rules out for secrets.
    pub keystore: Box<dyn Keystore>,
    /// Where remembered device passwords live (docs/bugs/02-connect-form.md,
    /// task 6). A second, independent keystore handle rather than a second
    /// owner of `keystore` above: every native backend opens its own
    /// connection per operation and holds no state between calls, so a second
    /// `open()` is exactly as cheap and needs no shared ownership.
    pub remembered_password_keystore: Box<dyn Keystore>,
    /// The audit log, when one could be opened (§15; ADR 0041).
    ///
    /// `None` is the ordinary degraded state: no data directory, or a database
    /// that refused to open. The caller has already said so in the log.
    pub audit: Option<crate::audit_store::AuditStore>,
}

impl std::fmt::Debug for ActorStores {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActorStores")
            .field("history_path", &self.history_path)
            .field("address_book_path", &self.address_book_path)
            .finish_non_exhaustive()
    }
}

impl ActorStores {
    /// Stores that keep nothing after the process ends: no history file, no
    /// address book file, and in-memory keystores.
    #[cfg(test)]
    #[must_use]
    pub fn in_memory() -> Self {
        Self {
            history_path: None,
            address_book_path: None,
            keystore: Box::new(lumepeer_net::keystore::MemoryKeystore::new()),
            remembered_password_keystore: Box::new(lumepeer_net::keystore::MemoryKeystore::new()),
            audit: None,
        }
    }
}

/// Removes what a *past* run received through the clipboard path and never
/// got to clean up (a crash, a kill) — docs/bugs/14-clipboard-files.md #3.
///
/// Once per process, not once per actor. A running actor's receives live in
/// this tree, so a second actor starting up alongside it would delete a
/// transfer the first one is halfway through, which is exactly what "a past
/// run" is not. Production spawns one actor, so this fires once there either
/// way; the tests spawn many, and it is they that would otherwise sweep each
/// other's files away mid-transfer.
fn sweep_clipboard_files() {
    static SWEEP: std::sync::Once = std::sync::Once::new();
    SWEEP.call_once(|| {
        if let Some(dir) = crate::config::clipboard_files_dir() {
            let _ = std::fs::remove_dir_all(&dir);
        }
    });
}

/// Spawns the actor over an already bound endpoint. Split out of
/// [`spawn_actor`] so the loop can be driven in tests without a keystore, a
/// relay or a Tauri window: `windows`, `media` and `clipboard` are the three
/// seams that would otherwise need one.
#[must_use]
#[allow(
    clippy::too_many_lines,
    reason = "one struct literal naming every field of the actor's state; \
              splitting it would only move field initializers out of the one \
              place that shows what the actor owns"
)]
pub fn spawn_actor_with(
    endpoint: PeerEndpoint,
    identity: SigningKey,
    windows: Arc<dyn ViewWindows>,
    media: HostMedia,
    clipboard: crate::clipboard_os::ClipboardFactory,
    stores: ActorStores,
) -> ActorHandle {
    let ActorStores {
        history_path,
        address_book_path,
        keystore,
        remembered_password_keystore,
        audit,
    } = stores;
    let remembered_passwords = RememberedPasswordStore::new(remembered_password_keystore);
    // One log, two handles: the actor writes through the `AuditSink` contract,
    // the IPC commands read through the handle below (§15; ADR 0041).
    let audit_reader = audit.clone();
    let audit = audit.map(|store| AuditContext {
        salt: *store.salt(),
        sink: Box::new(store) as Box<dyn lumepeer_core::audit::AuditSink>,
    });
    let unattended_store = UnattendedStore::new(keystore);
    let mut unattended = UnattendedAccess::new();
    unattended_store.restore(&mut unattended);
    if unattended.enabled() {
        tracing::info!(
            code_required = unattended.code_required(),
            role = ?unattended.role(),
            "unattended access is on: trusted devices may log in with the device password"
        );
    }
    let HostMedia {
        capture,
        health,
        injector,
    } = media;
    let (tx, rx) = mpsc::channel(32);
    let (events_tx, events_rx) = mpsc::channel(32);
    let view_feeds: ViewFeeds = Arc::new(std::sync::RwLock::new(HashMap::new()));
    let (faults_tx, faults_rx) = mpsc::channel(8);
    let (reports_tx, reports_rx) = mpsc::channel(REPORT_CAPACITY);
    let (cursors_tx, cursors_rx) = mpsc::channel(REPORT_CAPACITY);
    // Deliberately shallow: a backed-up actor should re-detect the current
    // clipboard on the next poll, not work through a queue of everything the
    // user copied while it was busy.
    let (clipboard_tx, clipboard_changes) = mpsc::channel(4);
    let clipboard_worker = crate::clipboard_os::spawn(clipboard, clipboard_tx);
    // Files a past run received through the clipboard path and never got to
    // clean up (a crash, a kill) are a leak, not something to keep serving
    // (docs/bugs/14-clipboard-files.md #3). Swept once, here, rather than on
    // every session end, which already removes its own peer's subdirectory.
    //
    // Once per *process*, not once per actor — see `sweep_clipboard_files`.
    sweep_clipboard_files();
    let (notify, _) = broadcast::channel(NOTIFY_CAPACITY);
    let mut install_salt = [0u8; 32];
    rand::rng().fill_bytes(&mut install_salt);
    let actor = Actor {
        rx,
        sessions: SessionManager::new(),
        install_salt,
        audit,
        labels: std::collections::HashMap::new(),
        endpoint,
        identity,
        tickets: TicketRegistry::new(),
        connections: std::collections::HashMap::new(),
        next_connection_id: 0,
        handshake_slots: Arc::new(Semaphore::new(MAX_INFLIGHT_HANDSHAKES)),
        events_tx,
        events_rx,
        faults_tx,
        faults_rx,
        reports_tx,
        reports_rx,
        rtt: std::collections::HashMap::new(),
        reception: std::collections::HashMap::new(),
        last_keyframe: std::collections::HashMap::new(),
        receiver_reports: std::collections::HashMap::new(),
        stream_scale: std::collections::HashMap::new(),
        display_mode: std::collections::HashMap::new(),
        display_mode_state: None,
        display_mode_generation: 0,
        speaks_cursor_shape: std::collections::HashSet::new(),
        cursors_tx,
        cursors_rx,
        health: Arc::clone(&health),
        notify: notify.clone(),
        capture,
        media: std::collections::HashMap::new(),
        audio: std::collections::HashMap::new(),
        guest_mic: std::collections::HashMap::new(),
        speaks_remote_sas: std::collections::HashSet::new(),
        recorders: HashMap::new(),
        record_requests: std::collections::HashSet::new(),
        record_request_rate: ConsentRateLimiter::new(),
        injector,
        views: std::collections::HashMap::new(),
        view_feeds: Arc::clone(&view_feeds),
        host_addrs: std::collections::HashMap::new(),
        host_invites: std::collections::HashMap::new(),
        chat: ChatLog::new(),
        speaks_file_transfer: std::collections::HashSet::new(),
        speaks_clipboard_files: std::collections::HashSet::new(),
        clipboard_offers_out: std::collections::HashMap::new(),
        file_conns: std::collections::HashMap::new(),
        file_dialing: std::collections::HashSet::new(),
        file_offers_out: std::collections::HashMap::new(),
        file_offers_in: std::collections::HashMap::new(),
        file_accepted: std::collections::HashMap::new(),
        file_channels: std::collections::HashMap::new(),
        file_pending_sends: std::collections::HashMap::new(),
        file_transfers: std::collections::HashMap::new(),
        file_receive_destinations: std::collections::HashMap::new(),
        file_send_tasks: std::collections::HashMap::new(),
        next_transfer_id: 0,
        clipboard: std::collections::HashMap::new(),
        clipboard_inbound: std::collections::HashMap::new(),
        clipboard_worker,
        clipboard_changes,
        windows,
        host_bar_up: false,
        history: ConnectionHistory::open(history_path),
        unattended,
        unattended_store,
        unattended_pending: std::collections::HashSet::new(),
        speaks_unattended: std::collections::HashSet::new(),
        address_book: AddressBookStore::open(address_book_path),
        connect_code_required: false,
        connect_retry_secs: None,
        connect_phase: ConnectPhase::Idle,
        connect_peer: None,
        connect_failure: None,
        remembered_passwords,
        pending_remember: None,
        connect_credentials_auto: false,
    };
    tokio::spawn(actor.run());
    ActorHandle {
        tx,
        notify,
        online: Arc::new(AtomicBool::new(false)),
        health,
        views: view_feeds,
        audit: audit_reader,
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "a failed assumption must fail the test"
    )]

    use std::time::Duration;

    use lumepeer_media::capture::{Frame, InputCapability, ScreenCapturer};
    use lumepeer_media::error::{MediaError, Result as MediaResult};

    use super::*;
    use crate::clipboard_os::testing::{SharedTestClipboard, test_clipboard};
    use crate::view::DetachedViewWindows;

    /// Anything slower than this on loopback means the test is stuck.
    const TIMEOUT: Duration = Duration::from_secs(20);

    /// The whole of what `RTT_EWMA_ALPHA` means, checked without a clock.
    #[test]
    fn the_first_sample_is_the_average_and_later_ones_are_blended_into_it() {
        assert!((ewma_rtt(None, 100.0) - 100.0).abs() < f32::EPSILON);
        // One quarter of the way from 100 to 200 at an alpha of 0.25.
        let blended = ewma_rtt(Some(100.0), 200.0);
        assert!(
            (blended - (100.0 + RTT_EWMA_ALPHA * 100.0)).abs() < 0.01,
            "smoothing did not follow RTT_EWMA_ALPHA: {blended}"
        );
        // A steady link converges on its own value rather than drifting.
        let mut average = 50.0;
        for _ in 0..64 {
            average = ewma_rtt(Some(average), 50.0);
        }
        assert!((average - 50.0).abs() < 0.01);
    }

    /// A round trip is only a round trip for the nonce this side is waiting
    /// on. Anything else — a stale echo, a value the peer made up — is not a
    /// measurement, and must neither be counted nor treated as an error.
    #[test]
    fn a_foreign_nonce_is_ignored_without_disturbing_the_average() {
        let mut tracker = RttTracker::default();
        tracker.sent(7);
        assert_eq!(tracker.pong(9), None, "a foreign nonce must not measure");
        assert_eq!(tracker.smoothed(), None, "and must not produce an average");
        // The outstanding ping is still outstanding: the right answer still
        // works after the wrong one.
        assert!(tracker.pong(7).is_some());
        let after_the_real_one = tracker.smoothed();
        assert!(after_the_real_one.is_some());
        // And a second echo of an already-answered nonce changes nothing.
        assert_eq!(tracker.pong(7), None);
        assert_eq!(tracker.smoothed(), after_the_real_one);
    }

    /// A guest that asks on every frame must not be able to decide what the
    /// host's uplink is spent on: a keyframe is the most expensive frame in
    /// the stream, so a burst of requests buys exactly one (§11).
    #[test]
    fn a_burst_of_keyframe_requests_is_honoured_once() {
        let mut last_honoured: Option<std::time::Instant> = None;
        let mut honoured = 0u32;
        for _ in 0..1_000 {
            if keyframe_budget_allows(last_honoured) {
                honoured += 1;
                last_honoured = Some(std::time::Instant::now());
            }
        }
        assert_eq!(honoured, 1, "the keyframe budget let a burst through");
    }

    /// And the budget is a wall clock, not a one-shot: a decoder that loses
    /// its reference a minute later must still be able to ask.
    #[test]
    fn the_keyframe_budget_reopens_once_the_interval_has_passed() {
        let long_ago = std::time::Instant::now()
            .checked_sub(Duration::from_millis(KEYFRAME_MIN_INTERVAL_MS * 2))
            .expect("the process has not been running since the epoch");
        assert!(keyframe_budget_allows(Some(long_ago)));
        assert!(
            keyframe_budget_allows(None),
            "a first request is never refused"
        );
    }

    /// docs/bugs/16-host-display-mode.md #3: the auto-revert decision, pure
    /// and checked without a runtime, a capture backend or a real 10-second
    /// wait — only a timeout that still names the live generation, with
    /// capture never confirmed healthy since it was armed, reverts.
    #[test]
    fn auto_revert_only_fires_for_the_live_generation_and_only_when_unconfirmed() {
        // The common case: this is still the switch that armed the timer,
        // and nothing confirmed it — revert.
        assert!(should_auto_revert_display_mode(Some(1), 1, false));
        // Confirmed within the window: keep it.
        assert!(!should_auto_revert_display_mode(Some(1), 1, true));
        // A later switch already moved the generation past this timeout.
        assert!(!should_auto_revert_display_mode(Some(2), 1, false));
        // Already resolved entirely (restored, or the owner disconnected)
        // by the time this timeout fired.
        assert!(!should_auto_revert_display_mode(None, 1, false));
    }

    /// A `Pong` for a ping that was never sent is the same non-event.
    #[test]
    fn a_pong_nobody_asked_for_measures_nothing() {
        let mut tracker = RttTracker::default();
        assert_eq!(tracker.pong(1), None);
        assert_eq!(tracker.smoothed(), None);
    }

    /// A relay URL identifies the *host's* network, and §15 keeps that off a
    /// screen the host does not control: the region is enough to say where
    /// someone is, and a bare address has no region at all.
    #[test]
    fn only_a_relay_region_is_ever_exposed_never_an_address() {
        let region = |raw: &str| {
            raw.parse::<iroh::RelayUrl>()
                .ok()
                .and_then(|url| relay_region(&url))
        };
        assert_eq!(
            region("https://euw1-1.relay.example./"),
            Some("euw1-1".to_owned())
        );
        assert_eq!(
            region("https://192.0.2.10:4433/"),
            None,
            "a bare IPv4 has no region"
        );
        assert_eq!(
            region("https://[2001:db8::1]:4433/"),
            None,
            "nor a bare IPv6"
        );
        assert_eq!(
            region("https://localhost/"),
            None,
            "nor a single-label name"
        );
    }

    /// Capturer that never produces a picture but does start and stop, so the
    /// viewer bookkeeping of `CaptureController` is exercised on a machine with
    /// no capture backend compiled in.
    #[derive(Debug, Default)]
    struct SilentCapturer {
        running: bool,
    }

    impl ScreenCapturer for SilentCapturer {
        fn start(&mut self, _target: CaptureTarget) -> MediaResult<()> {
            self.running = true;
            Ok(())
        }

        fn next_frame(&mut self) -> MediaResult<Option<Frame>> {
            if self.running {
                Ok(None)
            } else {
                Err(MediaError::CaptureUnavailable("stopped".to_owned()))
            }
        }

        fn stop(&mut self) {
            self.running = false;
        }

        fn input_capability(&self) -> InputCapability {
            InputCapability::None
        }
    }

    fn test_capture() -> SharedCapture {
        Arc::new(std::sync::Mutex::new(CaptureController::new(
            Box::new(SilentCapturer::default()),
            CaptureTarget::PrimaryDisplay,
        )))
    }

    /// A host that has a capture backend, like every machine the other tests
    /// pretend to run on.
    fn test_media(capture: &SharedCapture) -> HostMedia {
        HostMedia {
            capture: Arc::clone(capture),
            health: Arc::new(MediaHealth::healthy()),
            injector: None,
        }
    }

    /// Every mode a [`ScriptedModesCapturer`] has actually been asked to
    /// switch to, oldest first (docs/bugs/16-host-display-mode.md #2).
    type SharedAppliedModes = Arc<std::sync::Mutex<Vec<lumepeer_media::capture::DisplayMode>>>;

    /// Capturer whose display modes and set-mode outcome are entirely
    /// scripted, so the grant-gating and wiring of docs/bugs/
    /// 16-host-display-mode.md can be exercised deterministically — without
    /// depending on real hardware, and without depending on which platform
    /// happens to run the test (`display_modes_supported` genuinely differs
    /// across them).
    #[derive(Debug)]
    struct ScriptedModesCapturer {
        running: bool,
        modes: Vec<lumepeer_media::capture::DisplayMode>,
        /// What this "monitor" is set up with right now, changed by every
        /// successful `set_display_mode` — the same way real hardware
        /// behaves, and what lets a test assert a restore actually happened
        /// (docs/bugs/16-host-display-mode.md #3).
        current: lumepeer_media::capture::DisplayMode,
        applied: SharedAppliedModes,
    }

    impl ScreenCapturer for ScriptedModesCapturer {
        fn start(&mut self, _target: CaptureTarget) -> MediaResult<()> {
            self.running = true;
            Ok(())
        }

        fn next_frame(&mut self) -> MediaResult<Option<Frame>> {
            if self.running {
                Ok(None)
            } else {
                Err(MediaError::CaptureUnavailable("stopped".to_owned()))
            }
        }

        fn stop(&mut self) {
            self.running = false;
        }

        fn input_capability(&self) -> InputCapability {
            InputCapability::None
        }

        fn display_modes(
            &self,
            _target: CaptureTarget,
        ) -> Vec<lumepeer_media::capture::DisplayMode> {
            self.modes.clone()
        }

        fn set_display_mode(
            &mut self,
            _target: CaptureTarget,
            mode: lumepeer_media::capture::DisplayMode,
        ) -> MediaResult<()> {
            self.current = mode;
            self.applied
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(mode);
            Ok(())
        }

        fn current_display_mode(
            &self,
            _target: CaptureTarget,
        ) -> Option<lumepeer_media::capture::DisplayMode> {
            Some(self.current)
        }
    }

    /// A host whose "monitor" is set up with `current` and reports `modes`
    /// as switchable, for docs/bugs/16-host-display-mode.md.
    fn scripted_display_mode_media(
        current: lumepeer_media::capture::DisplayMode,
        modes: Vec<lumepeer_media::capture::DisplayMode>,
    ) -> (HostMedia, SharedAppliedModes) {
        let applied: SharedAppliedModes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let capturer = ScriptedModesCapturer {
            running: false,
            modes,
            current,
            applied: Arc::clone(&applied),
        };
        let capture = Arc::new(std::sync::Mutex::new(CaptureController::new(
            Box::new(capturer),
            CaptureTarget::PrimaryDisplay,
        )));
        (test_media(&capture), applied)
    }

    /// A host granting `ViewOnly` to a guest that opened a view window,
    /// where the host's own "monitor" starts at `current` and reports
    /// `modes` as switchable (docs/bugs/16-host-display-mode.md #2, #3).
    async fn display_mode_pair(
        current: lumepeer_media::capture::DisplayMode,
        modes: Vec<lumepeer_media::capture::DisplayMode>,
    ) -> (ActorHandle, ActorHandle, String, String, SharedAppliedModes) {
        let (media, applied) = scripted_display_mode_media(current, modes);
        let (host, _host_endpoint) = actor_with_media(Arc::new(DetachedViewWindows), media).await;
        let recorder = Arc::new(RecordingWindows::default());
        let (guest, _guest_endpoint, _guest_capture, _windows) =
            actor_with_windows(Arc::clone(&recorder) as Arc<dyn ViewWindows>).await;

        let invite = host.invite_create(Role::ViewOnly).await.unwrap();
        guest.invite_connect(invite.code).await.unwrap();
        let guest_label = tokio::time::timeout(TIMEOUT, wait_for_pending(&host))
            .await
            .unwrap();
        host.grant(guest_label.clone(), Role::ViewOnly)
            .await
            .unwrap();
        wait_until("the guest never opened a view", || {
            !recorder.opened().is_empty()
        })
        .await;
        let (_window, host_label, _input) = recorder.opened().remove(0);
        (host, guest, guest_label, host_label, applied)
    }

    /// Polls `host_display_modes` the way the toolbar does, until `ready`
    /// holds (docs/bugs/16-host-display-mode.md #2).
    async fn wait_for_display_modes(
        guest: &ActorHandle,
        host_label: &str,
        mut ready: impl FnMut(&(Vec<DisplayModeInfo>, Option<DisplayModeUnavailableReason>)) -> bool,
    ) -> (Vec<DisplayModeInfo>, Option<DisplayModeUnavailableReason>) {
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        loop {
            let current = guest
                .host_display_modes(host_label.to_owned())
                .await
                .unwrap();
            if ready(&current) {
                return current;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for the announced display modes"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// [`ViewWindows`] that records what the actor asked for, so the guest side
    /// of a grant can be asserted without a Tauri runtime.
    #[derive(Debug, Default)]
    struct RecordingWindows {
        opened: std::sync::Mutex<Vec<(String, String, bool)>>,
        closed: std::sync::Mutex<Vec<String>>,
        host_bar: std::sync::atomic::AtomicBool,
    }

    impl RecordingWindows {
        fn opened(&self) -> Vec<(String, String, bool)> {
            self.opened.lock().unwrap().clone()
        }

        fn closed(&self) -> Vec<String> {
            self.closed.lock().unwrap().clone()
        }

        fn host_bar(&self) -> bool {
            self.host_bar.load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    impl ViewWindows for RecordingWindows {
        fn open(&self, label: &str, peer_label: &str, input: bool) {
            self.opened
                .lock()
                .unwrap()
                .push((label.to_owned(), peer_label.to_owned(), input));
        }

        fn close(&self, label: &str) {
            self.closed.lock().unwrap().push(label.to_owned());
        }

        fn set_host_bar(&self, visible: bool) {
            self.host_bar
                .store(visible, std::sync::atomic::Ordering::Relaxed);
        }
    }

    async fn actor() -> (ActorHandle, PeerEndpoint, SharedCapture) {
        let (handle, endpoint, capture, _windows) =
            actor_with_windows(Arc::new(DetachedViewWindows)).await;
        (handle, endpoint, capture)
    }

    async fn actor_with_windows(
        windows: Arc<dyn ViewWindows>,
    ) -> (
        ActorHandle,
        PeerEndpoint,
        SharedCapture,
        Arc<dyn ViewWindows>,
    ) {
        let capture = test_capture();
        let media = test_media(&capture);
        let (handle, endpoint) = actor_with_media(Arc::clone(&windows), media).await;
        (handle, endpoint, capture, windows)
    }

    /// An actor whose machine clipboard the test drives and inspects.
    async fn actor_with_clipboard(
        windows: Arc<dyn ViewWindows>,
    ) -> (ActorHandle, SharedTestClipboard) {
        let capture = test_capture();
        let media = test_media(&capture);
        let (factory, clipboard) = test_clipboard();
        let secret = iroh::SecretKey::generate();
        let identity = SigningKey::from_bytes(&secret.to_bytes());
        let endpoint = PeerEndpoint::bind_local(secret).await.unwrap();
        let handle = spawn_actor_with(
            endpoint,
            identity,
            windows,
            media,
            factory,
            ActorStores::in_memory(),
        );
        (handle, clipboard)
    }

    /// The seam every actor in these tests is built through: `media` is what
    /// decides whether this host believes it can produce a picture at all.
    async fn actor_with_media(
        windows: Arc<dyn ViewWindows>,
        media: HostMedia,
    ) -> (ActorHandle, PeerEndpoint) {
        let secret = iroh::SecretKey::generate();
        let identity = SigningKey::from_bytes(&secret.to_bytes());
        let endpoint = PeerEndpoint::bind_local(secret).await.unwrap();
        let handle = spawn_actor_with(
            endpoint.clone(),
            identity,
            windows,
            media,
            crate::clipboard_os::no_clipboard(),
            ActorStores::in_memory(),
        );
        (handle, endpoint)
    }

    /// Poll rounds to wait through before concluding that nothing happened.
    /// Three, because one is the baseline read and one more is the earliest a
    /// change could be noticed.
    const QUIET_ROUNDS: u64 = 4;

    async fn a_few_poll_rounds() {
        tokio::time::sleep(Duration::from_millis(
            lumepeer_core::constants::CLIPBOARD_POLL_INTERVAL_MS * QUIET_ROUNDS,
        ))
        .await;
    }

    fn clipboard_writes(state: &SharedTestClipboard) -> Vec<String> {
        state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .writes
            .clone()
    }

    fn set_clipboard(state: &SharedTestClipboard, text: &str) {
        state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .text = Some(text.to_owned());
    }

    /// docs/bugs/14-clipboard-files.md #1: puts a file list on a test
    /// clipboard, the same seam a real platform read goes through.
    fn set_clipboard_files(state: &SharedTestClipboard, paths: &[std::path::PathBuf]) {
        state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .files = Some(paths.to_vec());
    }

    /// Brings up a host and a guest with a live `ViewOnly` session, and
    /// returns both handles, both clipboards and the labels each side knows
    /// the other by.
    struct ClipboardPair {
        host: ActorHandle,
        host_clipboard: SharedTestClipboard,
        guest: ActorHandle,
        guest_clipboard: SharedTestClipboard,
        /// Label the host knows the guest by.
        guest_label: String,
        /// Label the guest knows the host by.
        host_label: String,
    }

    async fn clipboard_pair() -> ClipboardPair {
        let (host, host_clipboard) = actor_with_clipboard(Arc::new(DetachedViewWindows)).await;
        let recorder = Arc::new(RecordingWindows::default());
        let (guest, guest_clipboard) =
            actor_with_clipboard(Arc::clone(&recorder) as Arc<dyn ViewWindows>).await;

        let invite = host.invite_create(Role::ViewOnly).await.unwrap();
        guest.invite_connect(invite.code).await.unwrap();
        let guest_label = tokio::time::timeout(TIMEOUT, wait_for_pending(&host))
            .await
            .unwrap();
        host.grant(guest_label.clone(), Role::ViewOnly)
            .await
            .unwrap();

        wait_until("the guest never opened a view", || {
            !recorder.opened().is_empty()
        })
        .await;
        let (_window, host_label, _input) = recorder.opened().remove(0);

        ClipboardPair {
            host,
            host_clipboard,
            guest,
            guest_clipboard,
            guest_label,
            host_label,
        }
    }

    /// A scratch directory for one test, removed when it goes out of scope.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(what: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "lumepeer-actor-{what}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn join(&self, name: &str) -> std::path::PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A host and a guest with a live `ViewOnly` session, plus the labels each
    /// knows the other by.
    ///
    /// The endpoints are not returned: the actor owns a clone of the one it
    /// was spawned with, so this side's copy has nothing left to hold.
    async fn file_pair() -> (
        ActorHandle,
        ActorHandle,
        String,
        String,
        SharedTestClipboard,
    ) {
        let (host, host_clipboard) = actor_with_clipboard(Arc::new(DetachedViewWindows)).await;
        let recorder = Arc::new(RecordingWindows::default());
        let (guest, _guest_endpoint, _guest_capture, _windows) =
            actor_with_windows(Arc::clone(&recorder) as Arc<dyn ViewWindows>).await;

        let invite = host.invite_create(Role::ViewOnly).await.unwrap();
        guest.invite_connect(invite.code).await.unwrap();
        let guest_label = tokio::time::timeout(TIMEOUT, wait_for_pending(&host))
            .await
            .unwrap();
        host.grant(guest_label.clone(), Role::ViewOnly)
            .await
            .unwrap();
        wait_until("the guest never opened a view", || {
            !recorder.opened().is_empty()
        })
        .await;
        let (_window, host_label, _input) = recorder.opened().remove(0);
        (host, guest, guest_label, host_label, host_clipboard)
    }

    /// Copies `paths` on that machine, the way a user pressing Ctrl+C does.
    ///
    /// The wait is the point rather than politeness: the watch takes its
    /// baseline on the first poll after a grant turns it on, so a copy made
    /// in the same instant as the grant is the starting point and not news
    /// (`clipboard_os::run`). A real hand cannot copy that fast; a test can.
    async fn copy_files(state: &SharedTestClipboard, paths: &[std::path::PathBuf]) {
        a_few_poll_rounds().await;
        set_clipboard_files(state, paths);
    }

    /// [`file_pair`], keeping the host's capture controller.
    ///
    /// What the host is pointed at is not visible from either handle, so a
    /// test about the monitor picker has to hold the controller itself.
    async fn session_pair() -> (ActorHandle, ActorHandle, String, String, SharedCapture) {
        let (host, _host_endpoint, host_capture) = actor().await;
        let recorder = Arc::new(RecordingWindows::default());
        let (guest, _guest_endpoint, _guest_capture, _windows) =
            actor_with_windows(Arc::clone(&recorder) as Arc<dyn ViewWindows>).await;

        let invite = host.invite_create(Role::ViewOnly).await.unwrap();
        guest.invite_connect(invite.code).await.unwrap();
        let guest_label = tokio::time::timeout(TIMEOUT, wait_for_pending(&host))
            .await
            .unwrap();
        host.grant(guest_label.clone(), Role::ViewOnly)
            .await
            .unwrap();
        wait_until("the guest never opened a view", || {
            !recorder.opened().is_empty()
        })
        .await;
        let (_window, host_label, _input) = recorder.opened().remove(0);
        (host, guest, guest_label, host_label, host_capture)
    }

    /// Polls `file_transfers` the way the panel does, until `ready` holds.
    async fn wait_for_files(
        handle: &ActorHandle,
        what: &str,
        mut ready: impl FnMut(&FileTransfersDto) -> bool,
    ) {
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        loop {
            if ready(&handle.file_transfers().await.unwrap()) {
                return;
            }
            assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// §11 and ADR 0028 through the actor: the guest sees the *host's* screens
    /// and picking one retargets the *host's* capture.
    ///
    /// Both halves used to run backwards. The IPC commands are reachable only
    /// from the guest's view window, but the actor handled them as a host
    /// would: `monitors_list` enumerated the guest's own displays and
    /// announced them to the host, and `monitor_select` retargeted the guest's
    /// own capture. Nothing ever crossed the wire, and the toolbar's argument
    /// shapes were wrong on top of that, so the popover only ever showed its
    /// empty note and nobody found out.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_guest_picks_a_screen_and_the_host_is_the_one_that_moves() {
        let (_host, guest, _guest_label, host_label, host_capture) = session_pair().await;

        // The host's own enumeration. Both actors share this process, so this
        // is the same call the host makes — which is exactly what hid the old
        // code's mistake: it enumerated on the wrong side and got the same
        // answer.
        let Ok(expected) = crate::view::host_monitors() else {
            // This build has no way to enumerate displays at all (a Windows
            // build without `capture-windows`). There is then nothing honest
            // to announce, and the picker's empty note is the right answer —
            // never this machine's own screens dressed up as the host's.
            tokio::time::sleep(Duration::from_millis(250)).await;
            assert!(
                guest
                    .monitors_list(host_label.clone())
                    .await
                    .unwrap()
                    .is_empty()
            );
            assert!(
                guest.monitor_select(host_label, 0).await.is_err(),
                "a screen that was never announced was accepted"
            );
            return;
        };

        // Announced with the grant, so it is already there when the picker
        // opens: there is no request message to send.
        let announced = tokio::time::timeout(TIMEOUT, async {
            loop {
                let monitors = guest.monitors_list(host_label.clone()).await.unwrap();
                if !monitors.is_empty() {
                    return monitors;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("the host never announced its screens");
        assert_eq!(announced.len(), expected.len());
        for (got, want) in announced.iter().zip(expected.iter()) {
            assert_eq!(got.id, want.id);
            assert_eq!(got.width, want.width);
            assert_eq!(got.height, want.height);
            assert_eq!(got.primary, want.primary);
        }

        // An id the host never announced never reaches the wire.
        let bogus = announced.iter().map(|m| m.id).max().unwrap_or(0) + 100;
        assert!(
            guest
                .monitor_select(host_label.clone(), bogus)
                .await
                .is_err(),
            "an unannounced screen id was accepted"
        );

        let pick = announced[0].id;
        assert_eq!(
            lock_capture(&host_capture).target(),
            CaptureTarget::PrimaryDisplay,
            "the host started somewhere other than where this test assumes"
        );
        guest.monitor_select(host_label, pick).await.unwrap();
        // The pick travels on the control stream and the host acts on it in
        // its own loop, so the observation is the retarget, not the send.
        wait_until("the host never retargeted its capture", || {
            lock_capture(&host_capture).target() == CaptureTarget::Display(pick)
        })
        .await;
    }

    /// D7, docs/bugs/13-stream-resolution.md task 3: a value outside
    /// `ABR_MIN_SCALE_PERCENT..=STREAM_SCALE_MAX_PERCENT` never reaches the
    /// wire at all — it is refused by this node's own actor, the same shape
    /// [`the_guest_picks_a_screen_and_the_host_is_the_one_that_moves`] checks
    /// for an unannounced monitor id.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stream_scale_request_outside_the_range_never_leaves_this_node() {
        let (_host, guest, _guest_label, host_label, _host_capture) = session_pair().await;

        for scale_percent in [0, ABR_MIN_SCALE_PERCENT - 1, STREAM_SCALE_MAX_PERCENT + 1] {
            assert!(
                matches!(
                    guest
                        .set_stream_scale(host_label.clone(), scale_percent)
                        .await,
                    Err(ActorError::Core(CoreError::Malformed))
                ),
                "scale_percent {scale_percent} outside the range was accepted"
            );
        }

        // The bounds themselves are ordinary requests: two builds of the same
        // software always speak `FEATURE_STREAM_SCALE` to each other, so the
        // only thing left to refuse is the range.
        for scale_percent in [ABR_MIN_SCALE_PERCENT, STREAM_SCALE_MAX_PERCENT] {
            assert!(
                guest
                    .set_stream_scale(host_label.clone(), scale_percent)
                    .await
                    .is_ok(),
                "scale_percent {scale_percent} at the bound was refused"
            );
        }
    }

    /// D7, docs/bugs/13-stream-resolution.md task 2: the host is the one that
    /// decides, and a peer this actor is not watching at all is refused the
    /// same way an unknown label is refused everywhere else in this file.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stream_scale_request_to_an_unwatched_peer_is_refused() {
        let (guest, _guest_endpoint, _guest_capture) = actor().await;
        assert!(matches!(
            guest.set_stream_scale("nobody".to_owned(), 100).await,
            Err(ActorError::UnknownPeer)
        ));
    }

    /// docs/bugs/16-host-display-mode.md #2; ADR 0048: `view` is not enough,
    /// and neither is `input` — a `FullControl` session still sees
    /// `NotGranted` until the host explicitly hands out the independent
    /// `display_mode` grant, and only then does the real list, and a
    /// requested switch, actually reach the capturer.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn display_mode_needs_its_own_grant_and_the_real_list_arrives_once_granted() {
        use lumepeer_media::capture::DisplayMode;

        let original = DisplayMode {
            width: 1024,
            height: 768,
            refresh_hz: 60,
        };
        let modes = vec![
            DisplayMode {
                width: 1920,
                height: 1080,
                refresh_hz: 60,
            },
            DisplayMode {
                width: 2560,
                height: 1440,
                refresh_hz: 144,
            },
        ];
        let (host, guest, guest_label, host_label, applied) =
            display_mode_pair(original, modes.clone()).await;

        // ViewOnly alone, from the pairing above: not granted.
        let (found, reason) =
            wait_for_display_modes(&guest, &host_label, |(_, reason)| reason.is_some()).await;
        assert!(found.is_empty());
        assert_eq!(reason, Some(DisplayModeUnavailableReason::NotGranted));

        // FullControl widens input too, but `display_mode` stays its own
        // independent grant (§2.2; ADR 0048): view and input together are
        // still not enough.
        host.grant(guest_label.clone(), Role::FullControl)
            .await
            .unwrap();
        let (found, reason) =
            wait_for_display_modes(&guest, &host_label, |(_, reason)| reason.is_some()).await;
        assert!(found.is_empty());
        assert_eq!(reason, Some(DisplayModeUnavailableReason::NotGranted));

        // The independent grant, explicitly: the real list arrives.
        host.set_grant(guest_label.clone(), IndependentGrant::DisplayMode, true)
            .await
            .unwrap();
        let (found, reason) =
            wait_for_display_modes(&guest, &host_label, |(modes, _)| !modes.is_empty()).await;
        assert_eq!(reason, None);
        assert_eq!(found.len(), 2);
        assert_eq!(
            (found[0].width, found[0].height, found[0].refresh_hz),
            (1920, 1080, 60)
        );
        assert_eq!(
            (found[1].width, found[1].height, found[1].refresh_hz),
            (2560, 1440, 144)
        );

        // The guest asks for the second mode; the host's own capturer sees
        // exactly that mode, not merely "some" call.
        guest
            .host_display_set_mode(host_label.clone(), found[1].id)
            .await
            .unwrap();
        wait_until("the host never applied the requested mode", || {
            applied
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .last()
                == Some(&modes[1])
        })
        .await;

        // Turning the grant back off tells the guest so over the wire, and
        // — task 3's reversibility — restores the monitor to what it was
        // before the first switch, not to whatever the most recent switch
        // happened to be.
        host.set_grant(guest_label.clone(), IndependentGrant::DisplayMode, false)
            .await
            .unwrap();
        let (found_after, reason_after) =
            wait_for_display_modes(&guest, &host_label, |(_, reason)| {
                *reason == Some(DisplayModeUnavailableReason::NotGranted)
            })
            .await;
        assert!(found_after.is_empty());
        assert_eq!(reason_after, Some(DisplayModeUnavailableReason::NotGranted));
        wait_until("the original mode was never restored", || {
            applied
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .last()
                == Some(&original)
        })
        .await;
        // A stale id from before the grant went away is refused by the
        // guest's own cache before anything reaches the host again.
        assert!(matches!(
            guest.host_display_set_mode(host_label, found[1].id).await,
            Err(ActorError::Core(CoreError::Malformed))
        ));
        // Exactly the one switch and the one restore ever reached the
        // capturer.
        assert_eq!(
            applied
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            2
        );
    }

    /// §9.1: an id the host never announced is refused before it reaches the
    /// capturer at all — the guest-side check `on_pick_monitor` already
    /// applies to `MonitorSelect`, mirrored here for `DisplaySetMode`
    /// (docs/bugs/16-host-display-mode.md #2).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_display_set_mode_request_for_an_unannounced_id_never_reaches_the_capturer() {
        use lumepeer_media::capture::DisplayMode;

        let original = DisplayMode {
            width: 1024,
            height: 768,
            refresh_hz: 60,
        };
        let modes = vec![DisplayMode {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
        }];
        let (host, guest, guest_label, host_label, applied) =
            display_mode_pair(original, modes).await;
        host.set_grant(guest_label, IndependentGrant::DisplayMode, true)
            .await
            .unwrap();
        wait_for_display_modes(&guest, &host_label, |(modes, _)| !modes.is_empty()).await;

        assert!(matches!(
            guest.host_display_set_mode(host_label, 99).await,
            Err(ActorError::Core(CoreError::Malformed))
        ));
        assert!(
            applied
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
            "an unannounced id reached the capturer"
        );
    }

    /// D7 point 2, docs/bugs/16-host-display-mode.md #2: the host is the one
    /// that decides, and a peer this actor is not watching at all is refused
    /// the same way an unknown label is refused everywhere else in this
    /// file.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_display_set_mode_request_to_an_unwatched_peer_is_refused() {
        let (guest, _guest_endpoint, _guest_capture) = actor().await;
        assert!(matches!(
            guest.host_display_set_mode("nobody".to_owned(), 0).await,
            Err(ActorError::UnknownPeer)
        ));
    }

    /// docs/bugs/16-host-display-mode.md #3, the most important property in
    /// the file: the host's own monitor is restored when the session that
    /// changed it ends. The guest leaving is simulated here as its own
    /// `revoke` — the view window closing — which `on_revoke`'s own doc
    /// comment says the host sees exactly like an ungraceful disconnect
    /// would: both funnel into the same `on_closed` teardown every other
    /// kind of per-peer state (media, file transfers, clipboard) already
    /// goes through, so what this proves holds for a crash or a dropped
    /// connection too, not only a clean exit.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_display_mode_switch_is_restored_when_the_owning_session_ends() {
        use lumepeer_media::capture::DisplayMode;

        let original = DisplayMode {
            width: 1024,
            height: 768,
            refresh_hz: 60,
        };
        let modes = vec![DisplayMode {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
        }];
        let (host, guest, guest_label, host_label, applied) =
            display_mode_pair(original, modes.clone()).await;
        host.set_grant(guest_label.clone(), IndependentGrant::DisplayMode, true)
            .await
            .unwrap();
        let (found, _) =
            wait_for_display_modes(&guest, &host_label, |(modes, _)| !modes.is_empty()).await;

        guest
            .host_display_set_mode(host_label.clone(), found[0].id)
            .await
            .unwrap();
        wait_until("the host never applied the requested mode", || {
            applied
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .last()
                == Some(&modes[0])
        })
        .await;

        // The owning guest leaves without ever touching the grant.
        guest.revoke(host_label).await.unwrap();

        wait_until(
            "the original mode was never restored once the owning session ended",
            || {
                applied
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .last()
                    == Some(&original)
            },
        )
        .await;
    }

    /// §9.2 and §4 through the actor: a granted host copies a file, the
    /// offer that reaches the guest by itself is accepted into a directory of
    /// the guest's own choosing, `rd/file/1` opens for the first time, and the
    /// file lands verified under the offered basename.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_granted_session_moves_a_file_end_to_end() {
        let scratch = Scratch::new("e2e");
        let source = scratch.join("report.pdf");
        let bytes: Vec<u8> = (0..40_000u32)
            .map(|i| u8::try_from(i % 251).unwrap_or(0))
            .collect();
        std::fs::write(&source, &bytes).unwrap();
        let (host, guest, guest_label, host_label, host_clipboard) = file_pair().await;
        host.set_grant(guest_label, IndependentGrant::FileTransfer, true)
            .await
            .unwrap();

        copy_files(&host_clipboard, std::slice::from_ref(&source)).await;
        wait_for_files(&guest, "the offer never reached the guest", |files| {
            files.offers.len() == 1 && files.offers[0].name == "report.pdf"
        })
        .await;
        // It arrived tagged as a clipboard offer, which is what tells the
        // panel — and the accept below — that no directory is being chosen.
        assert!(guest.file_transfers().await.unwrap().offers[0].from_clipboard);

        // The directory argument is ignored on this path: a clipboard receive
        // lands in this node's own clipboard-receive directory so the paste it
        // exists to serve has something to point at (docs/bugs/
        // 14-clipboard-files.md #3).
        guest.file_accept(host_label, true, None).await.unwrap();

        // The sender only claims completion once the receiver's final ack says
        // the hash matched and the file is on disk, so a `Completed` row on
        // both sides is the verification, not a hopeful label.
        wait_for_files(&host, "the transfer never completed", |files| {
            files
                .transfers
                .iter()
                .any(|row| row.state == TransferState::Completed)
        })
        .await;
        let received = guest.file_transfers().await.unwrap();
        let row = received
            .transfers
            .iter()
            .find(|row| row.name == "report.pdf")
            .expect("the guest has no row for the file it received");
        assert_eq!(row.state, TransferState::Completed);
        assert!(row.incoming && row.from_clipboard);
        assert_eq!(row.size, bytes.len() as u64);
        assert_eq!(row.moved, row.size);
    }

    /// §8.1 and §4: switching `file_transfer` back off takes the file
    /// connection and every transfer with it, rather than leaving what the
    /// grant paid for running until the session happens to end.
    ///
    /// The second half matters as much as the first: the withdrawal must cost
    /// nothing permanent, so the host can switch the grant on again and the
    /// next file still moves. Only the grant went — not the peer's
    /// `FEATURE_FILE_TRANSFER` advertisement, which is a fact about a
    /// connection that is still up.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn withdrawing_the_grant_ends_the_transfers_and_can_be_given_back() {
        let scratch = Scratch::new("withdrawn");
        let first = scratch.join("first.pdf");
        std::fs::write(&first, b"the one that was allowed").unwrap();
        let second = scratch.join("second.pdf");
        std::fs::write(&second, b"copied while the switch was off").unwrap();
        let third = scratch.join("third.pdf");
        std::fs::write(&third, b"the one after the switch came back").unwrap();

        let (host, guest, guest_label, host_label, host_clipboard) = file_pair().await;
        host.set_grant(guest_label.clone(), IndependentGrant::FileTransfer, true)
            .await
            .unwrap();
        copy_files(&host_clipboard, std::slice::from_ref(&first)).await;
        wait_for_files(&guest, "the offer never reached the guest", |files| {
            files.offers.len() == 1
        })
        .await;
        guest
            .file_accept(host_label.clone(), true, None)
            .await
            .unwrap();
        wait_for_files(&host, "the transfer never started", |files| {
            !files.transfers.is_empty()
        })
        .await;

        host.set_grant(guest_label.clone(), IndependentGrant::FileTransfer, false)
            .await
            .unwrap();

        // The host's whole file world for this peer is gone the moment the
        // switch moves — not on the next offer, and not when the session ends.
        let after = host.file_transfers().await.unwrap();
        assert!(
            after.transfers.is_empty() && after.offers.is_empty(),
            "the withdrawal left transfers behind"
        );
        // And a new copy offers nothing, as it would not have before the
        // grant: the watch is off with no grant left to justify reading this
        // desktop's clipboard at all (§8.1).
        copy_files(&host_clipboard, std::slice::from_ref(&second)).await;
        a_few_poll_rounds().await;
        assert!(
            guest.file_transfers().await.unwrap().offers.is_empty(),
            "a withdrawn grant still offered a copied file"
        );

        // Given back, the path works again: the teardown took the grant, not
        // the guest's ability to speak the feature. A file not yet copied,
        // because what is already on the clipboard when the watch turns back
        // on is its baseline rather than news.
        host.set_grant(guest_label, IndependentGrant::FileTransfer, true)
            .await
            .unwrap();
        copy_files(&host_clipboard, std::slice::from_ref(&third)).await;
        wait_for_files(&guest, "the second offer never arrived", |files| {
            files.offers.iter().any(|row| row.name == "third.pdf")
        })
        .await;
        guest.file_accept(host_label, true, None).await.unwrap();
        wait_for_files(&host, "the second transfer never completed", |files| {
            files
                .transfers
                .iter()
                .any(|row| row.name == "third.pdf" && row.state == TransferState::Completed)
        })
        .await;
    }

    /// §8.2: without the `file_transfer` grant nothing is offered at all —
    /// and, since the grant is also what justifies reading this desktop's
    /// clipboard, the copy is not even looked at. The same file goes through
    /// once the host decides.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_copy_without_the_grant_offers_nothing() {
        let scratch = Scratch::new("nogrant");
        let source = scratch.join("report.pdf");
        std::fs::write(&source, b"anything at all").unwrap();

        let (host, guest, guest_label, _host_label, host_clipboard) = file_pair().await;
        copy_files(&host_clipboard, std::slice::from_ref(&source)).await;
        a_few_poll_rounds().await;
        assert!(guest.file_transfers().await.unwrap().offers.is_empty());

        host.set_grant(guest_label, IndependentGrant::FileTransfer, true)
            .await
            .unwrap();
        // The list already on the clipboard when the grant arrives is the
        // watch's baseline, not news: it was copied while nobody was allowed
        // to look. Copying again is what a user does, and what is offered.
        copy_files(&host_clipboard, &[]).await;
        copy_files(&host_clipboard, std::slice::from_ref(&source)).await;
        wait_for_files(&guest, "the offer never reached the guest", |files| {
            files.offers.len() == 1
        })
        .await;
    }

    /// §9.2: a declined offer leaves nothing behind on either side, and the
    /// receiving directory stays exactly as empty as it was (§4: no file
    /// connection is opened for an offer nobody took).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_declined_offer_moves_no_bytes() {
        let scratch = Scratch::new("declined");
        let source = scratch.join("report.pdf");
        std::fs::write(&source, b"never travels").unwrap();
        let inbox = scratch.join("inbox");
        std::fs::create_dir_all(&inbox).unwrap();

        let (host, guest, guest_label, host_label, host_clipboard) = file_pair().await;
        host.set_grant(guest_label, IndependentGrant::FileTransfer, true)
            .await
            .unwrap();
        copy_files(&host_clipboard, std::slice::from_ref(&source)).await;
        wait_for_files(&guest, "the offer never reached the guest", |files| {
            files.offers.len() == 1
        })
        .await;

        guest.file_accept(host_label, false, None).await.unwrap();

        a_few_poll_rounds().await;
        assert!(guest.file_transfers().await.unwrap().offers.is_empty());
        assert!(
            host.file_transfers().await.unwrap().transfers.is_empty(),
            "a declined offer started a transfer"
        );
        assert_eq!(
            std::fs::read_dir(&inbox).unwrap().count(),
            0,
            "a declined offer wrote something"
        );
    }

    /// docs/bugs/14-clipboard-files.md #4 (ADR 0047): files through the
    /// clipboard run under `file_transfer` alone, never under
    /// `clipboard_read`/`clipboard_write`. A host that turns both clipboard
    /// grants on but leaves `file_transfer` off must still refuse a
    /// clipboard file offer — proved over two real actors and a real
    /// connection, not by reasoning about the grant check in isolation.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn clipboard_files_need_file_transfer_not_the_clipboard_grants() {
        let scratch = Scratch::new("clipboard-grant");
        let source = scratch.join("photo.png");
        std::fs::write(&source, b"not a real image, just bytes").unwrap();

        let pair = clipboard_pair().await;
        // Both clipboard grants on, `file_transfer` conspicuously left off.
        pair.host
            .set_grant(
                pair.guest_label.clone(),
                IndependentGrant::ClipboardRead,
                true,
            )
            .await
            .unwrap();
        pair.host
            .set_grant(
                pair.guest_label.clone(),
                IndependentGrant::ClipboardWrite,
                true,
            )
            .await
            .unwrap();

        copy_files(&pair.guest_clipboard, std::slice::from_ref(&source)).await;

        a_few_poll_rounds().await;
        assert!(
            pair.host.file_transfers().await.unwrap().offers.is_empty(),
            "clipboard grants alone let a clipboard file offer through"
        );

        // The positive control: the next copy goes through once
        // `file_transfer` is the grant that is actually on, which is what
        // proves the refusal above was the grant check and not, say, a
        // feature negotiation that silently never happened. A fresh copy,
        // because the list already on the clipboard is no longer news to the
        // watch that has seen it.
        pair.host
            .set_grant(pair.guest_label, IndependentGrant::FileTransfer, true)
            .await
            .unwrap();
        let second = scratch.join("scan.png");
        std::fs::write(&second, b"also just bytes").unwrap();
        copy_files(&pair.guest_clipboard, std::slice::from_ref(&second)).await;
        wait_for_files(
            &pair.host,
            "the clipboard offer never arrived once file_transfer was on",
            |files| {
                files
                    .offers
                    .iter()
                    .any(|row| row.name == "scan.png" && row.from_clipboard)
            },
        )
        .await;
    }

    /// ADR 0055: the host's always-on-top session bar is up exactly while a
    /// guest is connected, and is put there by the actor rather than by any
    /// window asking for it — there is no setting and no IPC command that
    /// opens it, because "somebody is connected to this machine" is not a
    /// thing the untrusted presentation layer gets to decide (§2.2, §2.3).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_host_bar_goes_up_with_the_first_guest_and_down_with_the_last() {
        let windows = Arc::new(RecordingWindows::default());
        let (host, _host_endpoint, _host_capture, _windows) =
            actor_with_windows(Arc::clone(&windows) as Arc<dyn ViewWindows>).await;
        let (guest, _guest_endpoint, _guest_capture) = actor().await;
        assert!(!windows.host_bar(), "the bar was up with nobody connected");

        let invite = host.invite_create(Role::ViewOnly).await.unwrap();
        guest.invite_connect(invite.code).await.unwrap();
        let label = tokio::time::timeout(TIMEOUT, wait_for_pending(&host))
            .await
            .unwrap();
        // A guest waiting to be let in is not a guest who is watching.
        assert!(!windows.host_bar(), "a pending consent put the bar up");

        host.grant(label.clone(), Role::ViewOnly).await.unwrap();
        wait_until("the bar never went up for a live session", || {
            windows.host_bar()
        })
        .await;

        host.revoke(label).await.unwrap();
        wait_until("the bar never came down after the revoke", || {
            !windows.host_bar()
        })
        .await;
    }

    /// §8.1 applied to §9.2: a granted session is not a licence to read the
    /// host user's clipboard. Until `clipboard_read` is on, the clipboard is
    /// not read at all — the assertion is on the read count, not on what was
    /// sent, because "read it and threw it away" is not the rule.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_granted_session_alone_never_reads_the_hosts_clipboard() {
        let pair = clipboard_pair().await;
        set_clipboard(&pair.host_clipboard, "a password, probably");

        a_few_poll_rounds().await;
        let reads = pair
            .host_clipboard
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .reads;
        assert_eq!(reads, 0, "the clipboard was read without a grant");
        assert!(clipboard_writes(&pair.guest_clipboard).is_empty());
    }

    /// The host-to-guest direction end to end: `clipboard_read` turns the
    /// watcher on, a copy on the host lands on the guest's own clipboard, and
    /// turning the grant back off stops it again mid-session.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_read_grant_carries_the_hosts_clipboard_to_the_guest() {
        let pair = clipboard_pair().await;
        set_clipboard(&pair.host_clipboard, "before the grant");

        pair.host
            .set_grant(
                pair.guest_label.clone(),
                IndependentGrant::ClipboardRead,
                true,
            )
            .await
            .unwrap();
        // Whatever predates the decision stays put: the host allowed the guest
        // to see what it copies, not what it had copied.
        a_few_poll_rounds().await;
        assert!(clipboard_writes(&pair.guest_clipboard).is_empty());

        set_clipboard(&pair.host_clipboard, "after the grant");
        wait_until("the guest never received the host clipboard", || {
            clipboard_writes(&pair.guest_clipboard) == vec!["after the grant".to_owned()]
        })
        .await;

        pair.host
            .set_grant(
                pair.guest_label.clone(),
                IndependentGrant::ClipboardRead,
                false,
            )
            .await
            .unwrap();
        set_clipboard(&pair.host_clipboard, "after the switch went off");
        a_few_poll_rounds().await;
        assert_eq!(
            clipboard_writes(&pair.guest_clipboard),
            vec!["after the grant".to_owned()],
            "the clipboard kept flowing after the grant was withdrawn"
        );
    }

    /// The guest-to-host direction: the guest may always *offer*, and the
    /// host's core is what decides. Without `clipboard_write` the payload is
    /// dropped on arrival; with it, it reaches the host's real clipboard.
    ///
    /// This is the check that used to be reversed: the host was reading
    /// `clipboard_read` for a payload written *to* it, so a host that had
    /// turned exactly the right switch on saw nothing happen.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_write_grant_is_what_lets_a_guest_change_the_hosts_clipboard() {
        let pair = clipboard_pair().await;

        pair.guest
            .clipboard_push(pair.host_label.clone(), "from the guest".to_owned())
            .await
            .unwrap();
        a_few_poll_rounds().await;
        assert!(
            clipboard_writes(&pair.host_clipboard).is_empty(),
            "a guest changed the host clipboard with no grant"
        );
        assert_eq!(
            pair.host
                .clipboard_pull(pair.guest_label.clone())
                .await
                .unwrap(),
            None
        );

        pair.host
            .set_grant(
                pair.guest_label.clone(),
                IndependentGrant::ClipboardWrite,
                true,
            )
            .await
            .unwrap();
        pair.guest
            .clipboard_push(pair.host_label.clone(), "now allowed".to_owned())
            .await
            .unwrap();
        wait_until("the host clipboard never changed", || {
            clipboard_writes(&pair.host_clipboard) == vec!["now allowed".to_owned()]
        })
        .await;
        assert_eq!(
            pair.host.clipboard_pull(pair.guest_label).await.unwrap(),
            Some("now allowed".to_owned()),
            "the UI has no way to say a clipboard arrived"
        );
    }

    /// Each direction needs its own grant, and holding one is not holding the
    /// other (§2.2). `clipboard_write` alone must not start the host's
    /// watcher, and must not carry the host's clipboard anywhere.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_write_grant_does_not_let_the_guest_read_the_host() {
        let pair = clipboard_pair().await;
        pair.host
            .set_grant(
                pair.guest_label.clone(),
                IndependentGrant::ClipboardWrite,
                true,
            )
            .await
            .unwrap();

        set_clipboard(&pair.host_clipboard, "host side secret");
        a_few_poll_rounds().await;
        let reads = pair
            .host_clipboard
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .reads;
        assert_eq!(reads, 0, "clipboard_write started a read watcher");
        assert!(clipboard_writes(&pair.guest_clipboard).is_empty());
    }

    /// A revoke takes the watcher with it, without waiting for the session's
    /// transport to notice (§8.1).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_revoke_stops_the_clipboard_watcher() {
        let pair = clipboard_pair().await;
        set_clipboard(&pair.host_clipboard, "before the grant");
        pair.host
            .set_grant(
                pair.guest_label.clone(),
                IndependentGrant::ClipboardRead,
                true,
            )
            .await
            .unwrap();
        // Let the baseline round happen before changing anything, so the
        // change below is unambiguously a change.
        a_few_poll_rounds().await;
        set_clipboard(&pair.host_clipboard, "while granted");
        wait_until("the guest never received the host clipboard", || {
            !clipboard_writes(&pair.guest_clipboard).is_empty()
        })
        .await;

        pair.host.revoke(pair.guest_label).await.unwrap();
        let before = pair
            .host_clipboard
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .reads;
        a_few_poll_rounds().await;
        let after = pair
            .host_clipboard
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .reads;
        assert_eq!(
            before, after,
            "the clipboard was still being read after a revoke"
        );
    }

    /// docs/bugs/10-clipboard-auto.md #1: the guest-to-host direction no
    /// longer needs a toolbar press. Opening a view starts this node's own
    /// clipboard watcher, a local change is *offered* to the host it is
    /// watching automatically, and the host's `clipboard_write` grant is
    /// still what decides whether it lands — exactly the same authorization
    /// `the_write_grant_is_what_lets_a_guest_change_the_hosts_clipboard`
    /// covers for a manual push.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_guest_clipboard_change_reaches_the_host_only_with_the_write_grant() {
        let pair = clipboard_pair().await;
        // The view is already open by the time `clipboard_pair` returns, so
        // the guest's watcher is already on (docs/bugs/10-clipboard-auto.md
        // #1). Let its baseline round land on the empty starting clipboard
        // before this test's own change exists to be mistaken for one.
        a_few_poll_rounds().await;

        set_clipboard(&pair.guest_clipboard, "typed on the guest");
        a_few_poll_rounds().await;
        assert!(
            clipboard_writes(&pair.host_clipboard).is_empty(),
            "the guest's clipboard reached the host with no grant"
        );

        pair.host
            .set_grant(
                pair.guest_label.clone(),
                IndependentGrant::ClipboardWrite,
                true,
            )
            .await
            .unwrap();
        set_clipboard(&pair.guest_clipboard, "typed after the grant");
        wait_until("the host never received the guest's clipboard", || {
            clipboard_writes(&pair.host_clipboard) == vec!["typed after the grant".to_owned()]
        })
        .await;
    }

    /// §8.1 applied to the new guest-side watcher (docs/bugs/10-clipboard-
    /// auto.md #1): closing the view is what stops it, the same as a
    /// host-side revoke stops the host's own.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn closing_the_view_stops_the_guests_own_clipboard_watch() {
        let pair = clipboard_pair().await;
        a_few_poll_rounds().await;
        let before = pair
            .guest_clipboard
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .reads;

        // Guest side: leaving the session from the view window, exactly what
        // closing it does (`on_revoke`'s `self.views.contains_key` branch).
        pair.guest.revoke(pair.host_label.clone()).await.unwrap();
        set_clipboard(&pair.guest_clipboard, "after closing the view");
        a_few_poll_rounds().await;
        let after = pair
            .guest_clipboard
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .reads;
        assert_eq!(
            before, after,
            "the guest's own clipboard was still being read after the view closed"
        );
    }

    /// Polls until `predicate` holds, or fails the test.
    async fn wait_until(what: &str, mut predicate: impl FnMut() -> bool) {
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        loop {
            if predicate() {
                return;
            }
            assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    fn viewers(capture: &SharedCapture) -> usize {
        lock_capture(capture).viewer_count()
    }

    fn pointer_event(x: u16, y: u16) -> InputEventPayload {
        use lumepeer_core::protocol::InputDetail;
        InputEventPayload {
            logical: 0,
            scancode: 0,
            modifiers: 0,
            detail: InputDetail::PointerMove { x, y },
        }
    }

    /// Polls the remembered-hosts list until it has an entry, or fails.
    async fn wait_for_history(handle: &ActorHandle, what: &str) -> Vec<HistoryEntry> {
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        loop {
            let entries = handle.history().await.unwrap();
            if !entries.is_empty() {
                return entries;
            }
            assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Polls the connect phase the way the connect form does, until it reaches
    /// `want`.
    async fn wait_for_phase(handle: &ActorHandle, want: ConnectPhase) {
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        loop {
            let phase = handle.connect_state().await.unwrap().phase;
            if phase == want {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "connect phase stuck at {phase:?}, wanted {want:?}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Polls `status` the way the frontend does until a pending row shows up.
    async fn wait_for_pending(handle: &ActorHandle) -> String {
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        loop {
            let rows = handle.status().await.unwrap();
            if let Some(row) = rows.iter().find(|r| r.state == SessionStateDto::Pending) {
                return row.label.clone();
            }
            assert!(tokio::time::Instant::now() < deadline, "no pending session");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Brings a guest to the point of holding a session with `host`, so its
    /// pseudonymized label is known and it can be put in the address book.
    ///
    /// The first visit always goes through the ordinary consent path: a
    /// device cannot be trusted before the host has ever seen it, which is
    /// the whole shape of §8's model (ADR 0034).
    async fn introduce(host: &ActorHandle, guest: &ActorHandle) -> String {
        let invite = host.invite_create(Role::ViewOnly).await.unwrap();
        guest.invite_connect(invite.code).await.unwrap();
        let label = tokio::time::timeout(TIMEOUT, wait_for_pending(host))
            .await
            .unwrap();
        host.grant(label.clone(), Role::ViewOnly).await.unwrap();
        wait_for_phase(guest, ConnectPhase::Connected).await;
        // Saved while the session is live, because that is the only moment the
        // host knows this device at all: a label is minted from a pending or
        // active session, and the book is what keeps it resolvable afterwards.
        host.address_book_upsert(
            label.clone(),
            "office".to_owned(),
            Vec::new(),
            String::new(),
        )
        .await
        .unwrap();
        // End the visit; what stays behind is the book entry, still untrusted.
        host.revoke(label.clone()).await.unwrap();
        wait_for_phase(guest, ConnectPhase::Idle).await;
        label
    }

    /// The password every unattended test in this module logs in with. Over
    /// `UNATTENDED_PASSWORD_MIN_BYTES`, because the core refuses anything
    /// shorter (ADR 0033).
    const DEVICE_PASSWORD: &str = "correct horse battery staple";

    /// A host with unattended access on and `guest` saved and trusted.
    async fn trusted_host(host: &ActorHandle, guest: &ActorHandle) -> String {
        let label = introduce(host, guest).await;
        host.unattended_set_password(DEVICE_PASSWORD.to_owned())
            .await
            .unwrap();
        host.address_book_set_trusted(label.clone(), true)
            .await
            .unwrap();
        label
    }

    /// §8: a trusted device reaching a host with nobody at it is asked for the
    /// device password, and a correct one starts the session without any human
    /// ever seeing a dialog (ADR 0033).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_trusted_device_signs_in_with_the_device_password() {
        let (host, _host_endpoint, _host_capture) = actor().await;
        let (guest, _guest_endpoint, _guest_capture) = actor().await;
        let label = trusted_host(&host, &guest).await;

        let invite = host.invite_create(Role::ViewOnly).await.unwrap();
        guest.invite_connect(invite.code).await.unwrap();

        // The challenge, not a consent dialog.
        wait_for_phase(&guest, ConnectPhase::AwaitingCredentials).await;
        let state = guest.connect_state().await.unwrap();
        assert!(!state.code_required, "no second factor was configured");
        assert!(
            host.status()
                .await
                .unwrap()
                .iter()
                .all(|row| row.state != SessionStateDto::Pending),
            "a trusted device must not also queue a consent request"
        );

        guest
            .unattended_submit(DEVICE_PASSWORD.to_owned(), None, false)
            .await
            .unwrap();
        wait_for_phase(&guest, ConnectPhase::Connected).await;

        let rows = host.status().await.unwrap();
        let session = rows
            .iter()
            .find(|row| row.label == label && row.state == SessionStateDto::Active)
            .expect("the admitted session is active on the host");
        assert_eq!(session.role, Role::ViewOnly, "the host's configured role");
    }

    /// docs/bugs/02-connect-form.md, task 6 (D2): a password remembered with
    /// "remember" checked is submitted automatically on the next connect to
    /// the same host — the guest never has to answer the credential form a
    /// second time.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_remembered_password_signs_in_again_without_a_second_submission() {
        let (host, _host_endpoint, _host_capture) = actor().await;
        let (guest, _guest_endpoint, _guest_capture) = actor().await;
        let label = trusted_host(&host, &guest).await;

        let invite = host.invite_create(Role::ViewOnly).await.unwrap();
        guest.invite_connect(invite.code).await.unwrap();
        wait_for_phase(&guest, ConnectPhase::AwaitingCredentials).await;
        guest
            .unattended_submit(DEVICE_PASSWORD.to_owned(), None, true)
            .await
            .unwrap();
        wait_for_phase(&guest, ConnectPhase::Connected).await;

        // End the session so the guest is free to dial the same host again.
        host.revoke(label).await.unwrap();
        wait_for_phase(&guest, ConnectPhase::Idle).await;

        let invite = host.invite_create(Role::ViewOnly).await.unwrap();
        guest.invite_connect(invite.code).await.unwrap();
        // No second `unattended_submit` call anywhere in this test: if the
        // remembered password were not tried automatically, the guest would
        // sit in `AwaitingCredentials` forever and this wait would time out.
        wait_for_phase(&guest, ConnectPhase::Connected).await;
    }

    /// docs/bugs/02-connect-form.md, task 6: an auto-submitted password the
    /// host no longer accepts must not be retried silently (that would burn
    /// the consent-rate budget on a password already known to be wrong) — it
    /// falls back to the ordinary credential form for a human to answer, and
    /// the stale entry is forgotten rather than tried again next time.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stale_remembered_password_falls_back_to_the_modal_instead_of_retrying() {
        const NEW_PASSWORD: &str = "a completely different passphrase";
        let (host, _host_endpoint, _host_capture) = actor().await;
        let (guest, _guest_endpoint, _guest_capture) = actor().await;
        let label = trusted_host(&host, &guest).await;

        let invite = host.invite_create(Role::ViewOnly).await.unwrap();
        guest.invite_connect(invite.code).await.unwrap();
        wait_for_phase(&guest, ConnectPhase::AwaitingCredentials).await;
        guest
            .unattended_submit(DEVICE_PASSWORD.to_owned(), None, true)
            .await
            .unwrap();
        wait_for_phase(&guest, ConnectPhase::Connected).await;

        host.revoke(label).await.unwrap();
        wait_for_phase(&guest, ConnectPhase::Idle).await;

        // The host's password changes; the guest's remembered copy is now
        // stale.
        host.unattended_set_password(NEW_PASSWORD.to_owned())
            .await
            .unwrap();

        let invite = host.invite_create(Role::ViewOnly).await.unwrap();
        guest.invite_connect(invite.code).await.unwrap();
        let state = wait_for_refusal(&guest, "UNATTENDED_BAD_PASSWORD").await;
        assert_eq!(state.phase, ConnectPhase::AwaitingCredentials);
        assert!(
            !state.credentials_auto,
            "a refused auto-submit must fall back to showing the modal, not retry unseen"
        );

        // A human can still sign in with the new password — the failed
        // auto-attempt did not consume the credential form's own retry.
        guest
            .unattended_submit(NEW_PASSWORD.to_owned(), None, false)
            .await
            .unwrap();
        wait_for_phase(&guest, ConnectPhase::Connected).await;
    }

    /// §8.2: passing the password is admission at the role the host
    /// configured, and nothing more — an unattended login lands on exactly
    /// the grants a consent dialog at that same role would have produced,
    /// never on a wider set for having skipped the dialog.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unattended_login_lands_on_the_configured_roles_grants() {
        let (host, _host_endpoint, _host_capture) = actor().await;
        let (guest, _guest_endpoint, _guest_capture) = actor().await;
        let label = trusted_host(&host, &guest).await;
        host.unattended_set_role(Role::FullControl).await.unwrap();

        let invite = host.invite_create(Role::ViewOnly).await.unwrap();
        guest.invite_connect(invite.code).await.unwrap();
        wait_for_phase(&guest, ConnectPhase::AwaitingCredentials).await;
        guest
            .unattended_submit(DEVICE_PASSWORD.to_owned(), None, false)
            .await
            .unwrap();
        wait_for_phase(&guest, ConnectPhase::Connected).await;

        let rows = host.status().await.unwrap();
        let session = rows
            .iter()
            .find(|row| row.label == label && row.state == SessionStateDto::Active)
            .expect("the admitted session is active");
        // The role the host chose is honored, not the one the invite asked
        // for, and it brings exactly what that role brings.
        assert_eq!(session.role, Role::FullControl);
        assert_eq!(session.grants, Grants::from_role(Role::FullControl));
        assert!(session.input, "FullControl implies input");
    }

    /// The gate of ADR 0034: trust decides who may *try* the password. A saved
    /// but untrusted device gets the ordinary consent dialog, exactly as if
    /// unattended access were off.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_untrusted_device_gets_the_consent_dialog_not_the_password_prompt() {
        let (host, _host_endpoint, _host_capture) = actor().await;
        let (guest, _guest_endpoint, _guest_capture) = actor().await;
        let label = trusted_host(&host, &guest).await;
        // Trust withdrawn; the password is still set.
        host.address_book_set_trusted(label.clone(), false)
            .await
            .unwrap();

        let invite = host.invite_create(Role::ViewOnly).await.unwrap();
        guest.invite_connect(invite.code).await.unwrap();

        let pending = tokio::time::timeout(TIMEOUT, wait_for_pending(&host))
            .await
            .unwrap();
        assert_eq!(pending, label);
        // Waited for rather than sampled: the host queueing the request and
        // the guest settling into its phase are two nodes' worth of scheduling
        // apart, and a single read here is a race that only shows up on a busy
        // machine. A guest sent down the credential path never arrives, so the
        // wait fails the test rather than hiding the fault.
        wait_for_phase(&guest, ConnectPhase::AwaitingConsent).await;
    }

    /// A device that was never saved at all is never trusted, whatever else is
    /// configured: `is_trusted` answers `false` for everything absent.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_device_that_is_not_in_the_book_gets_the_consent_dialog() {
        let (host, _host_endpoint, _host_capture) = actor().await;
        let (guest, _guest_endpoint, _guest_capture) = actor().await;
        host.unattended_set_password(DEVICE_PASSWORD.to_owned())
            .await
            .unwrap();

        let invite = host.invite_create(Role::ViewOnly).await.unwrap();
        guest.invite_connect(invite.code).await.unwrap();

        tokio::time::timeout(TIMEOUT, wait_for_pending(&host))
            .await
            .unwrap();
        wait_for_phase(&guest, ConnectPhase::AwaitingConsent).await;
    }

    /// §18: a wrong password is refused with the coarse code and nothing else,
    /// the form stays up, and the session never starts.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_wrong_password_is_refused_and_starts_no_session() {
        let (host, _host_endpoint, _host_capture) = actor().await;
        let (guest, _guest_endpoint, _guest_capture) = actor().await;
        let label = trusted_host(&host, &guest).await;

        let invite = host.invite_create(Role::ViewOnly).await.unwrap();
        guest.invite_connect(invite.code).await.unwrap();
        wait_for_phase(&guest, ConnectPhase::AwaitingCredentials).await;

        guest
            .unattended_submit("not the password at all".to_owned(), None, false)
            .await
            .unwrap();

        let deadline = tokio::time::Instant::now() + TIMEOUT;
        loop {
            let state = guest.connect_state().await.unwrap();
            if state.code == Some("UNATTENDED_BAD_PASSWORD") {
                // Still on the form: a mistyped password is retryable, and the
                // lockout inside the core is what bounds the retries.
                assert_eq!(state.phase, ConnectPhase::AwaitingCredentials);
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "no refusal arrived: {state:?}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        assert!(
            host.status()
                .await
                .unwrap()
                .iter()
                .all(|row| row.label != label || row.state != SessionStateDto::Active),
            "a refused login must not leave a session behind"
        );
    }

    use lumepeer_core::constants::UNATTENDED_MAX_FAILED_ATTEMPTS;

    /// Polls the connect state until the host's refusal carries `want`.
    async fn wait_for_refusal(handle: &ActorHandle, want: &str) -> ConnectSnapshot {
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        loop {
            let state = handle.connect_state().await.unwrap();
            if state.code == Some(want) {
                return state;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "wanted {want}, stuck at {state:?}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// The lockout is the only thing bounding guesses, so it has to hold
    /// across attempts and then refuse the correct password too.
    ///
    /// The counter lives in the host's `UnattendedAccess`, which outlives
    /// every connection; the only per-connection state the credential path
    /// keeps is `unattended_pending`, a membership set that grants a peer the
    /// right to be *heard*, never a budget. So hanging up and dialing again
    /// buys an attacker nothing, and there is no second counter anywhere to
    /// disagree with this one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_lockout_refuses_even_the_right_password() {
        let (host, _host_endpoint, _host_capture) = actor().await;
        let (guest, _guest_endpoint, _guest_capture) = actor().await;
        let label = trusted_host(&host, &guest).await;

        let invite = host.invite_create(Role::ViewOnly).await.unwrap();
        guest.invite_connect(invite.code).await.unwrap();
        wait_for_phase(&guest, ConnectPhase::AwaitingCredentials).await;

        for _ in 0..UNATTENDED_MAX_FAILED_ATTEMPTS {
            guest
                .unattended_submit("not the password".to_owned(), None, false)
                .await
                .unwrap();
            // Each refusal leaves the guest on the form: a mistyped password
            // is retryable, and this is the budget being spent.
            wait_for_refusal(&guest, "UNATTENDED_BAD_PASSWORD").await;
        }

        guest
            .unattended_submit(DEVICE_PASSWORD.to_owned(), None, false)
            .await
            .unwrap();
        let state = wait_for_refusal(&guest, "UNATTENDED_LOCKED_OUT").await;
        assert!(
            state.retry_secs.is_some_and(|secs| {
                secs > 0 && secs <= lumepeer_core::constants::UNATTENDED_LOCKOUT_DURATION_SECS
            }),
            "the guest is told how long to wait, within the configured lockout: {state:?}"
        );
        assert!(
            host.status()
                .await
                .unwrap()
                .iter()
                .all(|row| row.label != label || row.state != SessionStateDto::Active),
            "the right password must not admit anyone during a lockout"
        );
    }

    /// Turning unattended access off puts the host back on the consent path
    /// even for a device that is still marked trusted.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn turning_unattended_access_off_returns_the_host_to_the_consent_path() {
        let (host, _host_endpoint, _host_capture) = actor().await;
        let (guest, _guest_endpoint, _guest_capture) = actor().await;
        let label = trusted_host(&host, &guest).await;
        host.unattended_disable().await.unwrap();

        let status = host.unattended_status().await.unwrap();
        assert!(!status.enabled);
        assert!(!status.totp_enabled);

        let invite = host.invite_create(Role::ViewOnly).await.unwrap();
        guest.invite_connect(invite.code).await.unwrap();
        let pending = tokio::time::timeout(TIMEOUT, wait_for_pending(&host))
            .await
            .unwrap();
        assert_eq!(pending, label);
    }

    /// The second factor cannot be armed without a first, and the settings a
    /// host does make are what `unattended_status` reports back — never the
    /// password itself, which the type cannot carry.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_settings_surface_reports_state_and_never_credentials() {
        let (host, _host_endpoint, _host_capture) = actor().await;

        let status = host.unattended_status().await.unwrap();
        assert!(!status.enabled);
        assert_eq!(status.role, Role::ViewOnly, "deny-by-default");

        // A second factor before a password is refused.
        assert!(host.unattended_set_totp(true).await.is_err());

        // A password below the policy floor is refused, and changes nothing.
        assert!(
            host.unattended_set_password("short".to_owned())
                .await
                .is_err()
        );
        assert!(!host.unattended_status().await.unwrap().enabled);

        host.unattended_set_password(DEVICE_PASSWORD.to_owned())
            .await
            .unwrap();
        let provisioning = host.unattended_set_totp(true).await.unwrap().unwrap();
        assert!(!provisioning.secret_base32.is_empty());
        assert!(provisioning.uri.starts_with("otpauth://totp/Lumepeer:"));

        let status = host.unattended_status().await.unwrap();
        assert!(status.enabled);
        assert!(status.totp_enabled);
    }

    /// A trusted device with the second factor on is told to bring a code, and
    /// a password alone is not enough.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_second_factor_is_announced_and_enforced() {
        let (host, _host_endpoint, _host_capture) = actor().await;
        let (guest, _guest_endpoint, _guest_capture) = actor().await;
        let _label = trusted_host(&host, &guest).await;
        host.unattended_set_totp(true).await.unwrap();

        let invite = host.invite_create(Role::ViewOnly).await.unwrap();
        guest.invite_connect(invite.code).await.unwrap();
        wait_for_phase(&guest, ConnectPhase::AwaitingCredentials).await;
        assert!(
            guest.connect_state().await.unwrap().code_required,
            "the challenge has to say a code is expected, or the guest cannot supply one"
        );

        guest
            .unattended_submit(DEVICE_PASSWORD.to_owned(), None, false)
            .await
            .unwrap();
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        loop {
            let state = guest.connect_state().await.unwrap();
            if state.code == Some("UNATTENDED_BAD_CODE") {
                assert_eq!(state.phase, ConnectPhase::AwaitingCredentials);
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "a missing code was not refused: {state:?}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Saving a device is not trusting it (§2.1): the entry lands untrusted
    /// and only `address_book_set_trusted` moves the flag.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn saving_a_device_never_trusts_it() {
        let (host, _host_endpoint, _host_capture) = actor().await;
        let (guest, _guest_endpoint, _guest_capture) = actor().await;
        let label = introduce(&host, &guest).await;

        host.address_book_upsert(
            label.clone(),
            "office".to_owned(),
            vec!["work".to_owned()],
            "note".to_owned(),
        )
        .await
        .unwrap();
        let rows = host.address_book_list().await.unwrap();
        let row = rows.iter().find(|r| r.peer_label == label).unwrap();
        assert!(!row.trusted, "a saved device starts untrusted");
        assert_eq!(row.name, "office");
        assert_eq!(row.tags, vec!["work".to_owned()]);

        host.address_book_set_trusted(label.clone(), true)
            .await
            .unwrap();
        // Editing the entry afterwards must not disturb the flag either way.
        host.address_book_upsert(
            label.clone(),
            "office 2".to_owned(),
            Vec::new(),
            String::new(),
        )
        .await
        .unwrap();
        let rows = host.address_book_list().await.unwrap();
        let row = rows.iter().find(|r| r.peer_label == label).unwrap();
        assert!(row.trusted, "editing a name must not withdraw trust");
        assert_eq!(row.name, "office 2");

        // Forgetting takes the trust with it.
        host.address_book_remove(label.clone()).await.unwrap();
        assert!(host.address_book_list().await.unwrap().is_empty());
        assert!(host.address_book_remove(label).await.is_err());
    }

    /// C1: the guest actor must keep its `ControlConnection` after the
    /// handshake and keep reading it, or the host's `ConsentGrant` is never
    /// observed. Both sides run as real actors, driven only through the same
    /// handle the Tauri commands use.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_guest_actor_observes_the_hosts_grant_and_revoke() {
        let (host, _host_endpoint, _host_capture) = actor().await;
        let (guest, _guest_endpoint, _guest_capture) = actor().await;
        let mut events = guest.subscribe();

        let invite = host.invite_create(Role::ViewOnly).await.unwrap();
        guest.invite_connect(invite.code).await.unwrap();

        let label = tokio::time::timeout(TIMEOUT, wait_for_pending(&host))
            .await
            .unwrap();
        host.grant(label.clone(), Role::ViewOnly).await.unwrap();

        let granted = tokio::time::timeout(TIMEOUT, events.recv()).await.unwrap();
        assert_eq!(
            granted.unwrap(),
            ActorNotification::ConsentGranted {
                role: Role::ViewOnly
            }
        );

        let rows = host.status().await.unwrap();
        assert!(rows.iter().any(|r| r.state == SessionStateDto::Active));

        host.revoke(label).await.unwrap();
        let revoked = tokio::time::timeout(TIMEOUT, events.recv()).await.unwrap();
        assert_eq!(revoked.unwrap(), ActorNotification::ConsentRevoked);
    }

    /// The host's UI has to learn about an incoming request without polling:
    /// `main.rs` subscribes to this stream to raise the window, and the guest
    /// otherwise waits in `awaiting_consent` until the host happens to look
    /// (docs/bugs/01). The notification was broadcast to nobody for a while,
    /// so this test exists to keep it read.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_pending_request_reaches_a_host_side_subscriber() {
        let (host, _host_endpoint, _host_capture) = actor().await;
        let (guest, _guest_endpoint, _guest_capture) = actor().await;
        let mut events = host.subscribe();

        let invite = host.invite_create(Role::ViewOnly).await.unwrap();
        guest.invite_connect(invite.code).await.unwrap();

        let requested = tokio::time::timeout(TIMEOUT, events.recv()).await.unwrap();
        assert_eq!(requested.unwrap(), ActorNotification::ConsentRequested);
    }

    /// I3: when the guest goes away before the host decides, the host's reader
    /// task must notice and the pending row must disappear.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_guest_that_leaves_before_the_grant_stops_being_pending() {
        let (host, _host_endpoint, _host_capture) = actor().await;
        let (guest, guest_endpoint, _guest_capture) = actor().await;

        let invite = host.invite_create(Role::ViewOnly).await.unwrap();
        guest.invite_connect(invite.code).await.unwrap();
        let label = tokio::time::timeout(TIMEOUT, wait_for_pending(&host))
            .await
            .unwrap();

        guest_endpoint.close().await;

        let deadline = tokio::time::Instant::now() + TIMEOUT;
        loop {
            if host.status().await.unwrap().is_empty() {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the pending entry for {label} survived the disconnect"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// I7: a connection on a non-control ALPN must never reach the control
    /// handshake, and it must not become a pending consent request.
    ///
    /// Media is now an accepted ALPN, so this also pins the rule that makes
    /// accepting it safe: without a granted control session behind the same
    /// `NodeId` the connection is still closed, and no capture starts.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_media_alpn_connection_is_refused_without_a_handshake() {
        let (host, host_endpoint, capture) = actor().await;
        let stranger = PeerEndpoint::bind_local(iroh::SecretKey::generate())
            .await
            .unwrap();

        // Issued so the host has a live ticket: the ALPN must be what stops
        // this, not the absence of an invite.
        let _invite = host.invite_create(Role::ViewOnly).await.unwrap();
        let connection = stranger
            .connect(host_endpoint.addr(), lumepeer_net::ALPN_MEDIA)
            .await
            .unwrap();
        // The host closes it; the stranger never gets a media stream.
        assert!(
            tokio::time::timeout(TIMEOUT, connection.closed())
                .await
                .is_ok(),
            "the host left a media connection open"
        );
        assert!(host.status().await.unwrap().is_empty());
        assert!(
            !lock_capture(&capture).is_capturing(),
            "a refused media dial must never start capture"
        );
    }

    /// The whole point of this phase: granting `view` registers the guest as a
    /// viewer, which is what starts capture, and revoking takes it away again
    /// (§8.1, §11).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_grant_adds_a_viewer_and_a_revoke_removes_it() {
        let (host, _host_endpoint, capture) = actor().await;
        let (guest, _guest_endpoint, _guest_capture) = actor().await;

        assert_eq!(viewers(&capture), 0);
        assert!(!lock_capture(&capture).is_capturing());

        let invite = host.invite_create(Role::ViewOnly).await.unwrap();
        guest.invite_connect(invite.code).await.unwrap();
        let label = tokio::time::timeout(TIMEOUT, wait_for_pending(&host))
            .await
            .unwrap();

        host.grant(label.clone(), Role::ViewOnly).await.unwrap();
        assert_eq!(viewers(&capture), 1, "a view grant must start capture");
        assert!(lock_capture(&capture).is_capturing());

        host.revoke(label).await.unwrap();
        assert_eq!(viewers(&capture), 0, "a revoke must stop capture");
        assert!(!lock_capture(&capture).is_capturing());
    }

    /// docs/bugs/03-connection-list.md, task 4: the host is remembered as
    /// soon as the session actually starts, not only once it ends — a
    /// session that never reaches an explicit end (a crash, a lost machine)
    /// must not lose the invite it took to get here.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_guest_remembers_the_host_as_soon_as_consent_is_granted() {
        let (host, _host_endpoint, _capture) = actor().await;
        let (guest, _guest_endpoint, _guest_capture) = actor().await;

        let invite = host.invite_create(Role::FullControl).await.unwrap();
        guest.invite_connect(invite.code).await.unwrap();
        let label = tokio::time::timeout(TIMEOUT, wait_for_pending(&host))
            .await
            .unwrap();
        host.grant(label, Role::FullControl).await.unwrap();
        wait_for_phase(&guest, ConnectPhase::Connected).await;

        // Not disconnected, let alone revoked — the row must already exist.
        let remembered = guest.history().await.unwrap();
        assert_eq!(remembered.len(), 1);
        assert_eq!(remembered[0].role, Role::FullControl);
        assert!(
            host.history().await.unwrap().is_empty(),
            "a host must not build a record of the guests it let in"
        );
    }

    /// §21 punch-list item 5 / ADR 0016: an ended session is remembered by the
    /// side that dialed, so it can go back, and by nobody on the side that was
    /// dialed. The host decided once; it keeps no roster of who visited.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_guest_remembers_the_host_and_the_host_remembers_nobody() {
        let (host, host_endpoint, _capture) = actor().await;
        let (guest, _guest_endpoint, _guest_capture) = actor().await;

        assert!(host.history().await.unwrap().is_empty());
        assert!(guest.history().await.unwrap().is_empty());

        let invite = host.invite_create(Role::FullControl).await.unwrap();
        guest.invite_connect(invite.code).await.unwrap();
        let label = tokio::time::timeout(TIMEOUT, wait_for_pending(&host))
            .await
            .unwrap();
        host.grant(label.clone(), Role::FullControl).await.unwrap();
        host.revoke(label.clone()).await.unwrap();

        let remembered = wait_for_history(&guest, "the guest never remembered the host").await;
        assert_eq!(remembered.len(), 1);
        assert_eq!(remembered[0].peer_label, host_tag(&host_endpoint.addr().id));
        assert_eq!(remembered[0].role, Role::FullControl);
        assert!(
            host.history().await.unwrap().is_empty(),
            "a host must not build a record of the guests it let in"
        );
    }

    /// The same has to hold when nobody revokes anything: a host that simply
    /// disappears is still a host worth remembering on the guest side, since
    /// its view window closing is the only signal either side gets.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_disconnect_records_the_host_without_an_explicit_revoke() {
        let (host, host_endpoint, _capture) = actor().await;
        let (guest, _guest_endpoint, _guest_capture) = actor().await;

        let invite = host.invite_create(Role::ViewOnly).await.unwrap();
        guest.invite_connect(invite.code).await.unwrap();
        let label = tokio::time::timeout(TIMEOUT, wait_for_pending(&host))
            .await
            .unwrap();
        host.grant(label, Role::ViewOnly).await.unwrap();
        // `grant` returns once the message is queued, not once it lands. Tear
        // the host down before the guest has the grant and there is no session
        // to lose, which would make this test pass or fail on timing rather
        // than on the behaviour it is about.
        wait_for_phase(&guest, ConnectPhase::Connected).await;

        let host_id = host_endpoint.addr().id;
        host_endpoint.close().await;

        let remembered =
            wait_for_history(&guest, "a vanished host was never recorded on the guest").await;
        assert_eq!(remembered[0].peer_label, host_tag(&host_id));
        assert_eq!(remembered[0].role, Role::ViewOnly);
    }

    /// The reason the guest keeps the list at all (ADR 0016): a remembered row
    /// dials the host again on its own, without the operator hunting down a
    /// code, and the host is asked for consent again exactly as the first time.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_remembered_host_can_be_dialed_again_and_still_needs_consent() {
        let (host, _host_endpoint, _capture) = actor().await;
        let (guest, _guest_endpoint, _guest_capture) = actor().await;

        let invite = host.invite_create(Role::ViewOnly).await.unwrap();
        guest.invite_connect(invite.code).await.unwrap();
        let label = tokio::time::timeout(TIMEOUT, wait_for_pending(&host))
            .await
            .unwrap();
        host.grant(label.clone(), Role::ViewOnly).await.unwrap();
        host.revoke(label).await.unwrap();
        let remembered = wait_for_history(&guest, "nothing to reconnect to").await;

        // Nothing is retyped: the row carries the code, and the code stays in
        // Rust — the caller only names the host.
        guest
            .history_connect(remembered[0].peer_label.clone())
            .await
            .unwrap();

        let again = tokio::time::timeout(TIMEOUT, wait_for_pending(&host))
            .await
            .expect("a reconnect must queue a fresh consent request, not walk straight in");
        assert!(
            host.status()
                .await
                .unwrap()
                .iter()
                .any(|row| row.label == again && row.state == SessionStateDto::Pending),
            "the host decides again on every reconnect (§2.3)"
        );

        assert!(
            matches!(
                guest.history_connect("no-such-host".to_owned()).await,
                Err(ActorError::UnknownPeer)
            ),
            "a label that names no remembered host must dial nothing"
        );
    }

    use lumepeer_core::constants::CONSENT_RATE_PER_MINUTE;

    /// docs/bugs/03-connection-list.md, tasks 1 and 2. H1 was confirmed by
    /// this exact scenario failing before the fix: five connect/close-window
    /// cycles inside a minute succeeded, and a sixth against the same host
    /// in the same window was refused — `connect_phase` landed on `Failed`
    /// with no §18 code at all, which `invite-view.ts::failureKey` renders
    /// as "The connection ended before it was accepted", the user's exact
    /// report. `on_closed` now forgets the consent-rate counter for a peer
    /// whose session actually ran (`SessionManager::on_disconnect` returns
    /// `Ok` only then — never for one only ever queued, which keeps the
    /// limiter's protection against a peer that floods requests without
    /// ever being granted a session), so reconnecting stays possible however
    /// many times this cycle repeats within the minute.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn h1_reconnecting_past_the_rate_limit_keeps_working_after_a_clean_session() {
        let (host, _host_endpoint, _host_capture) = actor().await;
        let recorder = Arc::new(RecordingWindows::default());
        let (guest, _guest_endpoint, _guest_capture, _windows) =
            actor_with_windows(Arc::clone(&recorder) as Arc<dyn ViewWindows>).await;

        // Twice CONSENT_RATE_PER_MINUTE, comfortably past where the bug used
        // to bite on the very next cycle after the fifth.
        for attempt in 1..=2 * CONSENT_RATE_PER_MINUTE {
            let invite = host.invite_create(Role::ViewOnly).await.unwrap();
            guest.invite_connect(invite.code).await.unwrap();
            let host_side_label = tokio::time::timeout(TIMEOUT, wait_for_pending(&host))
                .await
                .unwrap_or_else(|_| panic!("attempt {attempt} never reached the host"));
            host.grant(host_side_label, Role::ViewOnly).await.unwrap();
            wait_for_phase(&guest, ConnectPhase::Connected).await;
            wait_until("no view window was opened", || {
                !recorder.opened().is_empty()
            })
            .await;
            // The guest's own label for the host it is viewing — not the
            // host's label for the guest, which `on_revoke` cannot resolve
            // (each side labels the other from its own per-run salt).
            let guest_side_label = recorder.opened().last().unwrap().1.clone();
            // Closing the view window — the user's exact reported action.
            guest.revoke(guest_side_label).await.unwrap();
            wait_for_phase(&guest, ConnectPhase::Idle).await;
            assert_eq!(
                guest.connect_state().await.unwrap().phase,
                ConnectPhase::Idle,
                "attempt {attempt} did not settle cleanly"
            );
        }
    }

    /// §21 punch-list item 6: the connect form has to know that a dial which
    /// returned is not a session yet, or it re-enables itself while the host
    /// is still looking at the consent dialog.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_connect_phase_waits_for_the_host_to_decide() {
        let (host, _host_endpoint, _capture) = actor().await;
        let (guest, _guest_endpoint, _guest_capture) = actor().await;

        assert_eq!(
            guest.connect_state().await.unwrap().phase,
            ConnectPhase::Idle
        );

        let invite = host.invite_create(Role::ViewOnly).await.unwrap();
        guest.invite_connect(invite.code).await.unwrap();
        // `invite_connect` returns when the attempt starts, not when it lands
        // (ADR 0027), so the form has to stay disabled through both waits —
        // the dial and then the host's decision.
        assert!(
            guest.connect_state().await.unwrap().phase.is_pending(),
            "the attempt is in flight from the moment it is started"
        );
        wait_for_phase(&guest, ConnectPhase::AwaitingConsent).await;
        assert!(
            guest.connect_state().await.unwrap().phase.is_pending(),
            "the handshake is done but nobody has decided yet"
        );

        let label = tokio::time::timeout(TIMEOUT, wait_for_pending(&host))
            .await
            .unwrap();
        host.grant(label.clone(), Role::ViewOnly).await.unwrap();
        wait_for_phase(&guest, ConnectPhase::Connected).await;
        assert!(!guest.connect_state().await.unwrap().phase.is_pending());

        host.revoke(label).await.unwrap();
        wait_for_phase(&guest, ConnectPhase::Idle).await;
    }

    /// ADR 0027: a dial that is going nowhere must not take the app with it.
    ///
    /// Before the dial moved off the actor loop, `handle_command` awaited it,
    /// so for as long as iroh kept trying — up to fifteen seconds per attempt
    /// — nothing else the actor owns could make progress: no incoming
    /// connection was accepted, no `ConsentGrant` was delivered, and the four
    /// commands the UI polls once a second all queued behind it. The app
    /// looked frozen and then said it could not connect.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dial_in_flight_does_not_stall_the_actor() {
        let (guest, _guest_endpoint, _guest_capture) = actor().await;
        // A host that exists on paper and answers nothing: the ticket verifies,
        // the address is in a range that goes nowhere, so the dial runs its
        // full budget.
        let (unreachable, _endpoint, _capture) = actor().await;
        let invite = unreachable.invite_create(Role::ViewOnly).await.unwrap();
        drop(unreachable);

        guest.invite_connect(invite.code).await.unwrap();
        assert_eq!(
            guest.connect_state().await.unwrap().phase,
            ConnectPhase::Dialing,
            "the attempt is in flight, and saying so is what keeps the form disabled"
        );

        // The assertion that matters: every other command still answers while
        // that dial is outstanding.
        for _ in 0..5 {
            tokio::time::timeout(Duration::from_secs(2), guest.status())
                .await
                .expect("the actor must still answer while a dial is in flight")
                .unwrap();
            tokio::time::timeout(Duration::from_secs(2), guest.history())
                .await
                .expect("the actor must still answer while a dial is in flight")
                .unwrap();
        }
    }

    /// A host that says no has to say it in a way the guest's form can show:
    /// a denial is not the same as a dial that never landed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_denied_request_leaves_the_connect_phase_denied() {
        let (host, _host_endpoint, _capture) = actor().await;
        let (guest, _guest_endpoint, _guest_capture) = actor().await;

        let invite = host.invite_create(Role::ViewOnly).await.unwrap();
        guest.invite_connect(invite.code).await.unwrap();
        let label = tokio::time::timeout(TIMEOUT, wait_for_pending(&host))
            .await
            .unwrap();

        // Deny is a revoke of a session that was never granted (§8.1).
        host.revoke(label).await.unwrap();
        wait_for_phase(&guest, ConnectPhase::Denied).await;
    }

    /// The bug behind §21 punch-list item 6: a second Connect against a host
    /// this node is already talking to used to replace the live connection,
    /// and the replacement's own teardown then killed the working session.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_second_connect_to_the_same_host_is_refused_and_leaves_the_session_alone() {
        let (host, _host_endpoint, _capture) = actor().await;
        let (guest, _guest_endpoint, _guest_capture) = actor().await;

        let invite = host.invite_create(Role::ViewOnly).await.unwrap();
        guest.invite_connect(invite.code.clone()).await.unwrap();
        let label = tokio::time::timeout(TIMEOUT, wait_for_pending(&host))
            .await
            .unwrap();
        host.grant(label.clone(), Role::ViewOnly).await.unwrap();
        wait_for_phase(&guest, ConnectPhase::Connected).await;

        assert!(
            matches!(
                guest.invite_connect(invite.code).await,
                Err(ActorError::Net(NetError::AlreadyConnected))
            ),
            "a duplicate dial must be refused, not raced"
        );

        assert_eq!(
            guest.connect_state().await.unwrap().phase,
            ConnectPhase::Connected,
            "the refused second dial must not disturb the live session"
        );
        assert!(
            host.status()
                .await
                .unwrap()
                .iter()
                .any(|row| row.label == label && row.state == SessionStateDto::Active)
        );
    }

    /// ADR 0016 end to end: the same code the host read out once has to work
    /// again after the session ends, or "remembered host" is a list of dead
    /// links and the operator is back to asking for a new code every time.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_same_invite_code_still_works_after_a_session_has_ended() {
        let (host, _host_endpoint, _capture) = actor().await;
        let (guest, _guest_endpoint, _guest_capture) = actor().await;

        let invite = host.invite_create(Role::ViewOnly).await.unwrap();
        guest.invite_connect(invite.code.clone()).await.unwrap();
        let label = tokio::time::timeout(TIMEOUT, wait_for_pending(&host))
            .await
            .unwrap();
        host.grant(label.clone(), Role::ViewOnly).await.unwrap();
        host.revoke(label).await.unwrap();
        // Granted and then ended is not a denial: the form goes quiet rather
        // than reporting a refusal that never happened.
        wait_for_phase(&guest, ConnectPhase::Idle).await;

        guest
            .invite_connect(invite.code)
            .await
            .expect("a live invite is a way back in, not a one-shot");
        tokio::time::timeout(TIMEOUT, wait_for_pending(&host))
            .await
            .expect("the second connection must reach the host's consent queue");
    }

    /// The other half of ADR 0016: "refresh invite" is the host's withdrawal
    /// switch, so the code it replaced has to stop working immediately.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refreshing_the_invite_retires_the_code_it_replaced() {
        let (host, _host_endpoint, _capture) = actor().await;
        let (guest, _guest_endpoint, _guest_capture) = actor().await;

        let first = host.invite_create(Role::ViewOnly).await.unwrap();
        let second = host.invite_create(Role::ViewOnly).await.unwrap();
        assert_ne!(first.code, second.code);

        // Whether the dial itself reports success is a race and not the point:
        // the host refuses the retired ticket *after* the handshake, by
        // closing, and that close can land on either side of the guest's own
        // handshake completing. What must never happen either way is a consent
        // request reaching the host.
        let _ = guest.invite_connect(first.code).await;
        assert!(
            tokio::time::timeout(Duration::from_secs(3), wait_for_pending(&host))
                .await
                .is_err(),
            "a replaced invite must not reach the host's consent queue"
        );
    }

    /// A guest that disappears without a revoke must not leave the host
    /// capturing its own screen for nobody.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_disconnect_removes_the_viewer_too() {
        let (host, _host_endpoint, capture) = actor().await;
        let (guest, guest_endpoint, _guest_capture) = actor().await;

        let invite = host.invite_create(Role::ViewOnly).await.unwrap();
        guest.invite_connect(invite.code).await.unwrap();
        let label = tokio::time::timeout(TIMEOUT, wait_for_pending(&host))
            .await
            .unwrap();
        host.grant(label, Role::ViewOnly).await.unwrap();
        assert_eq!(viewers(&capture), 1);

        guest_endpoint.close().await;
        wait_until("the viewer survived the disconnect", || {
            viewers(&capture) == 0
        })
        .await;
        assert!(!lock_capture(&capture).is_capturing());
    }

    /// The guest side of a grant: a view window opens by itself, is pollable
    /// through the same pseudonymized-label IPC as everything else, and closing
    /// it ends the session.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_granted_guest_gets_a_view_window_and_closing_it_ends_the_session() {
        let (host, _host_endpoint, host_capture) = actor().await;
        let recorder = Arc::new(RecordingWindows::default());
        let (guest, _guest_endpoint, _guest_capture, _windows) =
            actor_with_windows(Arc::clone(&recorder) as Arc<dyn ViewWindows>).await;

        let invite = host.invite_create(Role::FullControl).await.unwrap();
        guest.invite_connect(invite.code).await.unwrap();
        let label = tokio::time::timeout(TIMEOUT, wait_for_pending(&host))
            .await
            .unwrap();
        host.grant(label, Role::FullControl).await.unwrap();

        wait_until("no view window was opened", || {
            !recorder.opened().is_empty()
        })
        .await;
        let (window_label, peer_label, input) = recorder.opened().remove(0);
        assert_eq!(window_label, crate::view::window_label(&peer_label));
        assert!(input, "FullControl carries a live input grant");

        let frame = guest.view_frame(&peer_label, 0).unwrap();
        assert_eq!(
            frame.len(),
            crate::view::VIEW_RESPONSE_HEADER_BYTES,
            "no picture has been decoded yet, so only the header comes back"
        );
        assert_eq!(frame[1], 1, "the live input grant rides on every frame");

        // Input is forwarded while the grant carries it.
        guest
            .input(peer_label.clone(), pointer_event(1, 2))
            .await
            .unwrap();

        // Closing the window is the guest's revoke: the session ends from this
        // side and the host stops capturing.
        guest.revoke(peer_label.clone()).await.unwrap();
        assert!(recorder.closed().contains(&window_label));
        wait_until("the host kept capturing after the guest left", || {
            viewers(&host_capture) == 0
        })
        .await;
        assert!(matches!(
            guest.view_frame(&peer_label, 0),
            Err(ActorError::UnknownPeer)
        ));
    }

    /// §18, docs/adr/0024: a host with no capture backend must say so, on the
    /// wire and on its own status, instead of leaving the guest to sit out the
    /// reconnect window and then be told the connection failed.
    ///
    /// The session itself is untouched: this is not a revoke.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_host_with_no_capture_backend_tells_the_guest_why() {
        let capture = test_capture();
        let (host, _host_endpoint) = actor_with_media(
            Arc::new(DetachedViewWindows),
            HostMedia {
                capture: Arc::clone(&capture),
                health: Arc::new(MediaHealth::without_capture()),
                injector: None,
            },
        )
        .await;
        let recorder = Arc::new(RecordingWindows::default());
        let (guest, _guest_endpoint, _guest_capture, _windows) =
            actor_with_windows(Arc::clone(&recorder) as Arc<dyn ViewWindows>).await;

        let invite = host.invite_create(Role::ViewOnly).await.unwrap();
        guest.invite_connect(invite.code).await.unwrap();
        let label = tokio::time::timeout(TIMEOUT, wait_for_pending(&host))
            .await
            .unwrap();
        host.grant(label, Role::ViewOnly).await.unwrap();

        wait_until("no view window was opened", || {
            !recorder.opened().is_empty()
        })
        .await;
        let (_window, peer_label, _input) = recorder.opened().remove(0);

        // Well inside `RECONNECT_WINDOW_SECS`, which is what the guest used to
        // spend waiting before being told the wrong thing.
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        loop {
            let frame = guest.view_frame(&peer_label, 0).unwrap();
            if frame[0] == ViewStatus::NoCapture.code() {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the guest was never told why there is no picture (status {})",
                frame[0]
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        // Not a revoke: the grant, the session and the viewer registration all
        // stand, and the guest can still drive the session it was given.
        let rows = host.status().await.unwrap();
        assert!(
            rows.iter().any(|r| r.state == SessionStateDto::Active),
            "announcing a missing backend must not end the session"
        );
        assert_eq!(viewers(&capture), 1);

        // The operator's own screen says it too, before anyone else reports it.
        assert!(!host.media_health().can_capture());
        assert!(host.media_health().can_encode());
        assert!(
            guest.media_health().can_capture(),
            "the fault belongs to the host that has it, not to whoever heard about it"
        );
    }

    /// §11, ADR 0028: a guest holding the `input` grant can put the
    /// Ctrl+Alt+Del request on the wire at all.
    ///
    /// The regression this pins: `on_sas_request` gated on `self.sessions`,
    /// which is the *host* role's register of sessions this node has granted
    /// to its own guests. A guest has no entry there for the host it watches,
    /// so every request was refused before it could be sent and the toolbar
    /// button and the Ctrl+Alt+Shift+D chord both did nothing — on a live
    /// session with `input` granted. The gate belongs on `views`, which is
    /// where this side keeps the grants the host announced, exactly as
    /// `on_input` reads them.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_guest_with_the_input_grant_can_send_the_sas_request() {
        let (host, _host_endpoint, _host_capture) = actor().await;
        let recorder = Arc::new(RecordingWindows::default());
        let (guest, _guest_endpoint, _guest_capture, _windows) =
            actor_with_windows(Arc::clone(&recorder) as Arc<dyn ViewWindows>).await;

        let invite = host.invite_create(Role::FullControl).await.unwrap();
        guest.invite_connect(invite.code).await.unwrap();
        let label = tokio::time::timeout(TIMEOUT, wait_for_pending(&host))
            .await
            .unwrap();
        host.grant(label, Role::FullControl).await.unwrap();

        wait_until("no view window was opened", || {
            !recorder.opened().is_empty()
        })
        .await;
        let (_window, peer_label, input) = recorder.opened().remove(0);
        assert!(input, "FullControl carries a live input grant");

        // The same grant that carries an ordinary keystroke carries this one.
        guest
            .input(peer_label.clone(), pointer_event(1, 2))
            .await
            .unwrap();
        guest.sas_request(peer_label).await.unwrap();
    }

    /// The other half of the gate: without the grant nothing reaches the wire,
    /// which is what the broken version got right by accident.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_view_only_guest_cannot_send_the_sas_request() {
        let (host, _host_endpoint, _host_capture) = actor().await;
        let recorder = Arc::new(RecordingWindows::default());
        let (guest, _guest_endpoint, _guest_capture, _windows) =
            actor_with_windows(Arc::clone(&recorder) as Arc<dyn ViewWindows>).await;

        let invite = host.invite_create(Role::ViewOnly).await.unwrap();
        guest.invite_connect(invite.code).await.unwrap();
        let label = tokio::time::timeout(TIMEOUT, wait_for_pending(&host))
            .await
            .unwrap();
        host.grant(label, Role::ViewOnly).await.unwrap();

        wait_until("no view window was opened", || {
            !recorder.opened().is_empty()
        })
        .await;
        let (_window, peer_label, _input) = recorder.opened().remove(0);
        assert!(matches!(
            guest.sas_request(peer_label).await,
            Err(ActorError::Core(CoreError::NotPermitted))
        ));
    }

    /// A `ViewOnly` session must not be able to forward input, and the guest
    /// drops it before it ever reaches the wire (the host checks again).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_view_only_guest_cannot_forward_input() {
        let (host, _host_endpoint, _host_capture) = actor().await;
        let recorder = Arc::new(RecordingWindows::default());
        let (guest, _guest_endpoint, _guest_capture, _windows) =
            actor_with_windows(Arc::clone(&recorder) as Arc<dyn ViewWindows>).await;

        let invite = host.invite_create(Role::ViewOnly).await.unwrap();
        guest.invite_connect(invite.code).await.unwrap();
        let label = tokio::time::timeout(TIMEOUT, wait_for_pending(&host))
            .await
            .unwrap();
        host.grant(label, Role::ViewOnly).await.unwrap();

        wait_until("no view window was opened", || {
            !recorder.opened().is_empty()
        })
        .await;
        let (_window, peer_label, input) = recorder.opened().remove(0);
        assert!(!input, "ViewOnly must never imply input (§2.2, §8.2)");
        assert!(matches!(
            guest.input(peer_label.clone(), pointer_event(3, 4)).await,
            Err(ActorError::Core(CoreError::NotPermitted))
        ));
        let frame = guest.view_frame(&peer_label, 0).unwrap();
        assert_eq!(frame[1], 0);
    }

    /// One session row of `handle`, waited for rather than assumed.
    async fn wait_for_row(
        handle: &ActorHandle,
        label: &str,
        what: &str,
        mut predicate: impl FnMut(&SessionSnapshot) -> bool,
    ) -> SessionSnapshot {
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        loop {
            let rows = handle.status().await.unwrap();
            if let Some(row) = rows.iter().find(|r| r.label == label)
                && predicate(row)
            {
                return row.clone();
            }
            assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// §8.2 applied to §17: a granted session is not a licence to record it.
    /// The `recording` grant is a separate decision, and without it the toggle
    /// is refused before any file is opened.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn recording_without_the_grant_is_refused_before_a_file_is_opened() {
        let pair = clipboard_pair().await;

        let refused = pair
            .host
            .record_toggle(pair.guest_label.clone(), true)
            .await;
        assert!(matches!(
            refused,
            Err(ActorError::Core(CoreError::NotPermitted))
        ));
        let row = wait_for_row(&pair.host, &pair.guest_label, "no session row", |_| true).await;
        assert!(
            !row.recording_active,
            "a refused toggle started a recording"
        );
        // The guest is told nothing changed, so its indicator stays dark.
        let frame = pair.guest.view_frame(&pair.host_label, 0).unwrap();
        assert_eq!(frame[1] & crate::view::VIEW_FLAG_RECORDING, 0);
    }

    /// The whole §17 host path: the grant, the toggle, the file Rust chose,
    /// the indicator on both sides, and a second press that changes nothing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_granted_recording_lands_where_rust_chose_and_toggling_twice_is_idempotent() {
        let pair = clipboard_pair().await;
        pair.host
            .set_grant(pair.guest_label.clone(), IndependentGrant::Recording, true)
            .await
            .unwrap();

        let path = pair
            .host
            .record_toggle(pair.guest_label.clone(), true)
            .await
            .unwrap()
            .expect("a started recording answers with its path");
        let path = std::path::PathBuf::from(path);
        assert!(path.exists(), "no file at the path the actor reported");
        assert_eq!(
            path.extension().and_then(std::ffi::OsStr::to_str),
            Some("lmrc")
        );
        // Chosen by Rust, under this app's own data directory (§2.3).
        assert!(path.starts_with(crate::config::recordings_dir().unwrap()));

        let again = pair
            .host
            .record_toggle(pair.guest_label.clone(), true)
            .await
            .unwrap();
        assert_eq!(
            again.map(std::path::PathBuf::from),
            Some(path.clone()),
            "a second start opened a second file"
        );

        let row = wait_for_row(
            &pair.host,
            &pair.guest_label,
            "recording never showed",
            |r| r.recording_active,
        )
        .await;
        assert!(row.grants.recording);
        // No hidden capture (§2.2): the guest is told, over the wire, without
        // having asked.
        wait_until("the guest was never told it is being recorded", || {
            pair.guest
                .view_frame(&pair.host_label, 0)
                .is_ok_and(|frame| frame[1] & crate::view::VIEW_FLAG_RECORDING != 0)
        })
        .await;

        assert!(
            pair.host
                .record_toggle(pair.guest_label.clone(), false)
                .await
                .unwrap()
                .is_none(),
            "stopping reports no path"
        );
        wait_until("the guest was never told the recording stopped", || {
            pair.guest
                .view_frame(&pair.host_label, 0)
                .is_ok_and(|frame| frame[1] & crate::view::VIEW_FLAG_RECORDING == 0)
        })
        .await;

        // The flushed file is a readable container carrying both events.
        let bytes = std::fs::read(&path).unwrap();
        let (_, records) = lumepeer_media::record::read_recording(bytes.as_slice()).unwrap();
        let lines: Vec<String> = records
            .iter()
            .filter_map(|r| match r {
                lumepeer_media::record::Record::Event { line, .. } => Some(line.clone()),
                _ => None,
            })
            .collect();
        assert!(
            lines.iter().any(|l| l.contains("record-start")),
            "{lines:?}"
        );
        assert!(lines.iter().any(|l| l.contains("record-stop")), "{lines:?}");
        let _ = std::fs::remove_file(&path);
    }

    /// §2.2 applied to §17: taking the `recording` grant back stops the
    /// recording that was running under it, and the guest's indicator goes
    /// dark with it.
    ///
    /// Anything else would be the hidden capture the whole feature is written
    /// against: a host user who moves the switch off has to be able to read
    /// "nothing is being recorded" off the same panel, not "nothing new will
    /// be allowed to start".
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn withdrawing_the_grant_stops_a_running_recording() {
        let pair = clipboard_pair().await;
        pair.host
            .set_grant(pair.guest_label.clone(), IndependentGrant::Recording, true)
            .await
            .unwrap();
        let path = pair
            .host
            .record_toggle(pair.guest_label.clone(), true)
            .await
            .unwrap()
            .expect("a started recording answers with its path");
        let path = std::path::PathBuf::from(path);
        wait_until("the guest was never told it is being recorded", || {
            pair.guest
                .view_frame(&pair.host_label, 0)
                .is_ok_and(|frame| frame[1] & crate::view::VIEW_FLAG_RECORDING != 0)
        })
        .await;

        pair.host
            .set_grant(pair.guest_label.clone(), IndependentGrant::Recording, false)
            .await
            .unwrap();

        let row = wait_for_row(
            &pair.host,
            &pair.guest_label,
            "the recording outlived its grant",
            |r| !r.recording_active,
        )
        .await;
        assert!(!row.grants.recording);
        wait_until("the guest was never told the recording stopped", || {
            pair.guest
                .view_frame(&pair.host_label, 0)
                .is_ok_and(|frame| frame[1] & crate::view::VIEW_FLAG_RECORDING == 0)
        })
        .await;
        // Stopped, not abandoned: the file was closed properly and carries the
        // stop event, so what was recorded while it was allowed stays readable.
        let bytes = std::fs::read(&path).unwrap();
        let (_, records) = lumepeer_media::record::read_recording(bytes.as_slice()).unwrap();
        assert!(
            records.iter().any(|r| matches!(
                r,
                lumepeer_media::record::Record::Event { line, .. } if line.contains("record-stop")
            )),
            "the withdrawn recording was never closed off"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// §17's request half: a guest may *ask*, and asking decides nothing. The
    /// host user answers, and a refusal is an ordinary answer the guest is
    /// told about rather than an error or a silence.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_guests_request_waits_for_the_host_and_never_records_by_itself() {
        let pair = clipboard_pair().await;

        pair.guest
            .record_request(pair.host_label.clone())
            .await
            .unwrap();
        let row = wait_for_row(
            &pair.host,
            &pair.guest_label,
            "the request never arrived",
            |r| r.record_request,
        )
        .await;
        assert!(
            !row.grants.recording && !row.recording_active,
            "asking granted itself a recording"
        );

        // Asking again does not queue a second dialog.
        for _ in 0..3 {
            pair.guest
                .record_request(pair.host_label.clone())
                .await
                .unwrap();
        }
        a_few_poll_rounds().await;
        let rows = pair.host.status().await.unwrap();
        assert_eq!(
            rows.iter()
                .filter(|r| r.label == pair.guest_label && r.record_request)
                .count(),
            1
        );

        // The host declines: the request clears, nothing was recorded, and the
        // guest's indicator never lit.
        assert!(
            pair.host
                .record_toggle(pair.guest_label.clone(), false)
                .await
                .unwrap()
                .is_none()
        );
        let row = wait_for_row(
            &pair.host,
            &pair.guest_label,
            "the request never cleared",
            |r| !r.record_request,
        )
        .await;
        assert!(!row.recording_active);
        let frame = pair.guest.view_frame(&pair.host_label, 0).unwrap();
        assert_eq!(frame[1] & crate::view::VIEW_FLAG_RECORDING, 0);
    }

    /// Unknown labels are refused cleanly, with no panic and no peer parsing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unknown_label_is_refused() {
        let (host, _endpoint, _capture) = actor().await;
        assert!(matches!(
            host.grant("deadbeefdeadbeef".to_owned(), Role::ViewOnly)
                .await,
            Err(ActorError::UnknownPeer)
        ));
        assert!(matches!(
            host.revoke("deadbeefdeadbeef".to_owned()).await,
            Err(ActorError::UnknownPeer)
        ));
        assert!(matches!(
            host.view_frame("deadbeefdeadbeef", 0),
            Err(ActorError::UnknownPeer)
        ));
        assert!(matches!(
            host.input("deadbeefdeadbeef".to_owned(), pointer_event(1, 1))
                .await,
            Err(ActorError::UnknownPeer)
        ));
    }
}
