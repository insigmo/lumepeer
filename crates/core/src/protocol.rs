//! Control protocol wire types (design doc §9.1).
//!
//! Serialization is `postcard`; framing (`u32_be length || payload`) lives in
//! `lumepeer-net::framing`, which enforces the length check before allocating.

use serde::{Deserialize, Serialize};

use crate::consent::Role;
use crate::constants::{
    ABR_MIN_SCALE_PERCENT, CHAT_MAX_BYTES, CLIPBOARD_FILE_LIST_MAX_ENTRIES, CLIPBOARD_MAX_BYTES,
    FILE_NAME_MAX_BYTES, FILE_OFFER_MAX_BYTES, MAX_CONTROL_FRAME_BYTES, MAX_CURSOR_SHAPE_PIXELS,
    MAX_MONITORS_PER_HOST, STREAM_SCALE_MAX_PERCENT, UNATTENDED_CODE_MAX_BYTES,
    UNATTENDED_PASSWORD_MAX_BYTES,
};
use crate::error::{CoreError, Result};

/// Logical identifiers at or above this value denote pointer buttons rather
/// than keys.
///
/// §9.1 fixes the field but not the namespace; splitting it here lets the host
/// policy of §8.2 distinguish `PointerClick` from `KeyPress` without a platform
/// scancode table. See docs/adr/0007.
pub const POINTER_BUTTON_LOGICAL_BASE: u32 = 0xF000_0000;

/// `Shift` bit of [`InputEventPayload::modifiers`].
///
/// §9.1 fixes the field but not its bit layout, the same gap `POINTER_BUTTON_
/// LOGICAL_BASE` fills for `logical`. The guest webview's `modifiersOf`
/// (`apps/desktop/src/view-window.ts`) writes exactly these four bits, and the
/// platform injectors of §11 read them back — the X11 one has to, since it
/// needs to know whether the character it was asked for is reachable at the
/// current shift level of the host's own layout.
pub const MODIFIER_SHIFT: u32 = 1 << 0;
/// `Control` bit of [`InputEventPayload::modifiers`].
pub const MODIFIER_CTRL: u32 = 1 << 1;
/// `Alt` bit of [`InputEventPayload::modifiers`].
pub const MODIFIER_ALT: u32 = 1 << 2;
/// `Meta`/`Super`/`Command` bit of [`InputEventPayload::modifiers`].
pub const MODIFIER_META: u32 = 1 << 3;

/// Protocol major version. A mismatch closes the connection before consent (§9.1).
pub const PROTOCOL_MAJOR: u16 = 1;
/// Protocol minor version. Unknown optional features are ignored (§9.1).
///
/// 1: appended `Chat`, `KeyframeRequest`, `CursorShape`, `MonitorsList`,
/// `MonitorSelect`, `PrivacyMode`/`PrivacyModeAck`, `AudioStart`/`AudioStop`,
/// `FileAbort` and `FileChunkAck` after `ResumeHello`; added per-message
/// limit checks in `MessageEnvelope::decode`. Existing variants kept their
/// discriminants, so a MINOR 0 peer decodes every old message unchanged.
///
/// 2: appended [`MessageKind::MediaUnavailable`] after `FileChunkAck`. A host
/// only sends it to a guest whose `Hello` advertised
/// [`FEATURE_MEDIA_UNAVAILABLE`], so an older peer — which would treat the
/// unknown discriminant as malformed and close the connection (§9.1) — never
/// sees it. See `docs/adr/0024-host-media-unavailable-wire-message.md`.
///
/// 3: appended [`MessageKind::SasRequest`] after `MediaUnavailable`. A guest
/// sends it only when it holds the `input` grant, and the host only acts on
/// it from a session whose `input` grant is live — the same per-event
/// re-check every injected key gets (§8.1). A host that cannot honor it
/// answers [`MessageKind::SasAck`] with `false` rather than staying silent.
/// See `docs/adr/0028-remote-sas-and-view-toolbar.md`.
///
/// 4: appended [`MessageKind::UnattendedChallenge`],
/// [`MessageKind::UnattendedAuth`] and [`MessageKind::UnattendedReject`] after
/// `SasAck`, for the unattended-access admission path of §8 — the one way into
/// a session that has no human at the host to answer a consent dialog. A host
/// offers the challenge only to a guest whose `Hello` advertised
/// [`FEATURE_UNATTENDED`], so an older peer keeps seeing the ordinary
/// `ConsentRequest` path. See
/// `docs/adr/0033-unattended-admission-and-keystore-secret-slots.md`.
///
/// 5: appended [`MessageKind::FileTransferStart`] after `UnattendedReject`.
/// `FileAbort` and `FileChunkAck` have always been documented as naming a
/// `transfer_id` "announced in `FileTransferStart`", and no such message
/// existed — so the two sides had no way to agree on one, and the transfer
/// engine had nothing to be reached by. A peer sends it only to one whose
/// `Hello` advertised [`FEATURE_FILE_TRANSFER`], for the same minor-version
/// reason `MediaUnavailable` rides behind its feature string. See
/// `docs/adr/0032-file-transfer-start-and-the-lazy-file-connection.md`.
///
/// 6: appended [`MessageKind::ReceiverReport`] after `FileTransferStart`.
/// `QualityAdjust` says what the encoder should do; nothing said what the
/// receiver actually saw, so the host's adaptation had to invent a congestion
/// signal out of its own write latency (ADR 0015). A guest sends it only to a
/// host whose `HelloAck` minor is at least this one, and a host only reads it
/// from a guest whose `Hello` advertised [`FEATURE_RECEIVER_REPORT`]. See
/// `docs/adr/0037-receiver-reports-and-the-degradation-ladder.md`.
///
/// 7: appended [`MessageKind::StreamScaleRequest`] after `ReceiverReport`.
/// The host-side downscale mechanism of ADR 0018 and the adaptive ladder of
/// ADR 0037 already existed in full; nothing let a guest ask for a picture
/// smaller than its own screen wants. A guest sends it only to a host whose
/// `HelloAck` minor is at least this one, and a host only reads it from a
/// guest whose `Hello` advertised [`FEATURE_STREAM_SCALE`] — the same shape
/// [`FEATURE_RECEIVER_REPORT`] uses, since this is also a guest-to-host
/// message and `HelloAck` carries no feature list of its own. See
/// `docs/bugs/13-stream-resolution.md` and `docs/bugs/DECISIONS.md` D7.
///
/// 8: appended [`MessageKind::ClipboardFileOffer`] and
/// [`MessageKind::ClipboardFileAccept`] after `StreamScaleRequest`. §9.2's
/// file transfer engine (ADR 0032) could already move a file once both sides
/// had agreed to it; nothing let either side say "these files are on my
/// clipboard" in the first place. Either side may hold such a clipboard, so
/// either side may send `ClipboardFileOffer`, exactly like `FileOffer` — sent
/// only to a peer whose `Hello` advertised [`FEATURE_CLIPBOARD_FILES`]. See
/// `docs/bugs/14-clipboard-files.md` and `docs/adr/0047-clipboard-files-are-a-
/// file-transfer-not-a-clipboard-extension.md`.
pub const PROTOCOL_MINOR: u16 = 8;

/// `Hello.features` string a guest sends to say it understands
/// [`MessageKind::MediaUnavailable`].
///
/// §9.1 makes unknown feature strings ignorable, which is what lets a guest
/// advertise this to an older host with no risk; the host side of the same
/// rule is that it must not send the message unless it saw this string.
pub const FEATURE_MEDIA_UNAVAILABLE: &str = "media-unavailable";

/// `Hello.features` string a guest sends to say it understands
/// [`MessageKind::SasAck`].
///
/// Same compatibility shape as [`FEATURE_MEDIA_UNAVAILABLE`]: a host must
/// never send the ack to a peer that did not advertise the string, because
/// that peer decodes the unknown discriminant as malformed and closes the
/// connection (§9.1).
pub const FEATURE_REMOTE_SAS: &str = "remote-sas";

/// `Hello.features` string a guest sends to say it understands the unattended
/// credential exchange ([`MessageKind::UnattendedChallenge`] and
/// [`MessageKind::UnattendedReject`]).
///
/// Same compatibility shape as [`FEATURE_MEDIA_UNAVAILABLE`], and one step
/// stronger in consequence: a host that offered the challenge to a guest which
/// cannot answer it would leave that guest waiting on a consent dialog nobody
/// is going to see. A host that does not see this string falls back to the
/// ordinary consent path of §8.1, which asks a human — never the other way
/// round.
pub const FEATURE_UNATTENDED: &str = "unattended";

/// `Hello.features` string a peer sends to say it understands
/// [`MessageKind::FileTransferStart`], and therefore that a file transfer
/// with it can name a `transfer_id` both sides agree on.
///
/// Both sides advertise it and both sides check it: either end of a session
/// may offer a file (§9.2), so either end may be the one that has to send the
/// message. Without the string on the far side, an offer is simply never
/// made — better than a transfer that starts and then cannot be acked,
/// aborted or resumed.
pub const FEATURE_FILE_TRANSFER: &str = "file-transfer";

/// `Hello.features` string a guest sends to say it will report what it
/// actually received ([`MessageKind::ReceiverReport`]), and therefore that
/// this host's adaptation can be driven by measurements rather than by a
/// stand-in (ADR 0015, ADR 0037).
///
/// Same compatibility shape as [`FEATURE_MEDIA_UNAVAILABLE`], read in the
/// other direction: it is the *guest* that sends the new message, so the host
/// uses the string to know whether the reports it is not receiving mean "this
/// link is quiet" or "this peer cannot speak". Without it the host keeps
/// deriving congestion from its own writes, which is what every peer before
/// minor 6 gets.
pub const FEATURE_RECEIVER_REPORT: &str = "receiver-report";

/// `Hello.features` string a guest sends to say it will draw the host's cursor
/// itself, from [`MessageKind::CursorShape`], rather than expecting it in the
/// picture (§11).
///
/// Unlike the other feature strings this one does not gate a *new*
/// discriminant — `CursorShape` has existed since minor 1 — it gates a change
/// of behaviour on the **host**: a host that sees it, and whose capture
/// backend can separate the two, stops compositing the cursor into the frame.
/// Without the string the host keeps drawing it in, because a guest that
/// cannot draw the cursor and is no longer sent one has no cursor at all.
///
/// Both halves have to hold. A host whose platform cannot stop compositing
/// (Wayland's `CursorMode::Embedded`, macOS's `setShowsCursor(true)`) sends no
/// `CursorShape` at all, which is what tells the guest to keep its own overlay
/// off: two cursors are worse than one that lags.
pub const FEATURE_CURSOR_SHAPE: &str = "cursor-shape";

/// `Hello.features` string a guest sends to say it understands
/// [`MessageKind::StreamScaleRequest`], and therefore that this host may read
/// one from it as an authorized ceiling rather than an unknown discriminant
/// (D7, docs/bugs/13-stream-resolution.md).
///
/// Same compatibility shape as [`FEATURE_RECEIVER_REPORT`]: this is a
/// guest-to-host message, so it is the *guest* that advertises the string in
/// its own `Hello`, and the host reads it off that — `HelloAck` carries no
/// feature list for the guest to read the other way.
pub const FEATURE_STREAM_SCALE: &str = "stream-scale";

/// `Hello.features` string a peer sends to say it understands
/// [`MessageKind::ClipboardFileOffer`] and [`MessageKind::ClipboardFileAccept`]
/// (docs/bugs/14-clipboard-files.md; ADR 0047).
///
/// Same compatibility shape as [`FEATURE_FILE_TRANSFER`]: either end of a
/// session may have files on its own clipboard, so either end may be the one
/// that has to send the offer, and both sides advertise and check the string.
/// Without it on the far side, a clipboard file list is simply never
/// announced — the same choice `FEATURE_FILE_TRANSFER` makes for an ordinary
/// offer, and for the same reason (§18: a transfer that starts and cannot be
/// acked or resumed is worse than one never offered).
pub const FEATURE_CLIPBOARD_FILES: &str = "clipboard-files";

/// Direction of a control message, part of the anti-replay tuple (§9.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Direction {
    /// Host to guest.
    HostToGuest,
    /// Guest to host.
    GuestToHost,
}

/// Envelope carried by every control frame (§9.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageEnvelope {
    /// Session identifier generated by the host via CSPRNG after handshake.
    pub session_id: [u8; 16],
    /// Direction of this message.
    pub direction: Direction,
    /// Strictly monotonic per `(session_id, direction)`, starts at 0.
    pub seq: u64,
    /// Message discriminant.
    pub kind: MessageKind,
    /// Opaque body, at most `MAX_CONTROL_FRAME_BYTES`.
    pub body: Vec<u8>,
}

/// Control message kinds (§9.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageKind {
    /// First message of a connection, carries the invite proof.
    Hello {
        /// Protocol major of the sender.
        major: u16,
        /// Protocol minor of the sender.
        minor: u16,
        /// Role the guest asks for; the host still decides (§2.3).
        role_request: Role,
        /// Optional feature strings; unknown ones are ignored.
        features: Vec<String>,
        /// Proof of possession of a valid invite (§7).
        invite_proof: Vec<u8>,
    },
    /// Answer to `Hello`.
    HelloAck {
        /// Protocol major of the responder.
        major: u16,
        /// Protocol minor of the responder.
        minor: u16,
    },
    /// Guest asks the host user for consent.
    ConsentRequest,
    /// Host grants the given role.
    ConsentGrant(Role),
    /// Host revokes all grants.
    ConsentRevoke,
    /// Media session may start.
    SessionStart,
    /// Media session must stop.
    SessionStop,
    /// License expiry warning (§12).
    LicenseWarn {
        /// Seconds left in the current license window.
        seconds_left: u64,
    },
    /// License check failed; session must end (§12.4).
    LicenseDeny {
        /// Reason, free of secrets (§15).
        reason: String,
    },
    /// Input event from guest to host.
    InputEvent(InputEventPayload),
    /// Clipboard payload, text/plain UTF-8 only (§9.2).
    ClipboardSync {
        /// Clipboard bytes, at most `CLIPBOARD_MAX_BYTES`.
        data: Vec<u8>,
    },
    /// Guest asks to record the session.
    RecordRequest,
    /// Host answer to `RecordRequest`.
    RecordAck(bool),
    /// Offer of a single file (§9.2).
    FileOffer {
        /// Basename after normalization, no path separators.
        name: String,
        /// File size, at most `FILE_OFFER_MAX_BYTES`.
        size: u64,
        /// BLAKE3 of the whole file, verified before export from staging.
        hash: [u8; 32],
    },
    /// Answer to `FileOffer`; `true` opens the `rd/file/1` connection (§4).
    FileAccept(bool),
    /// Encoder target adjustment derived from receiver feedback (§11).
    QualityAdjust {
        /// Requested frame rate.
        target_fps: u8,
        /// Requested bitrate.
        target_bitrate_kbps: u32,
    },
    /// Keepalive, sent every `PING_INTERVAL_SECS` (§9.1).
    Ping(u64),
    /// Answer to `Ping`.
    Pong(u64),
    /// Resume attempt inside the reconnect window (§10).
    ResumeHello {
        /// Session being resumed.
        session_id: [u8; 16],
        /// Highest sequence number the sender has processed.
        last_received_seq: u64,
    },
    /// One chat message of the in-session text chat (§9.2).
    ///
    /// Appended after `ResumeHello` so every earlier variant keeps its
    /// postcard discriminant; the golden vectors of §17.2 depend on that
    /// (`PROTOCOL_MINOR 1`).
    Chat {
        /// UTF-8 message, at most `CHAT_MAX_BYTES` bytes.
        text: String,
    },
    /// Guest asks the host encoder for an instant keyframe (§11): decoder
    /// joined mid-stream or packets were lost beyond what the jitter buffer
    /// can conceal. The host produces a keyframe at the next opportunity.
    ///
    /// Rate-limited by *both* sides, at most one honoured request per
    /// `KEYFRAME_MIN_INTERVAL_MS`. The caller limiting itself is politeness;
    /// the host limiting the caller is the part that matters, because a
    /// keyframe is the most expensive frame in the stream and a guest that
    /// asked on every frame would otherwise decide what the host spends its
    /// uplink on.
    KeyframeRequest,
    /// Host announces the shape of its cursor (§11). Sent when the cursor
    /// changes; the guest draws it locally over the video for minimum input
    /// latency feedback.
    ///
    /// Sent only to a guest whose `Hello` advertised
    /// [`FEATURE_CURSOR_SHAPE`], and only by a host that has stopped
    /// compositing the cursor into the picture. Receiving one is therefore
    /// the guest's signal that the picture no longer contains a cursor and
    /// that drawing one is now its job.
    ///
    /// No position accompanies it, and that is the point: a cursor drawn from
    /// the guest's own pointer moves with the hand instead of with the video,
    /// which is the whole reason this channel exists. A position from the host
    /// matters only when something other than the guest moved the pointer, and
    /// paying a message per mouse move for that case would cost more than it
    /// buys (§11).
    CursorShape {
        /// Shape payload: geometry plus premultiplied BGRA pixels.
        shape: CursorShapeData,
    },
    /// Host lists the monitors a guest may target (§11).
    MonitorsList {
        /// One entry per monitor, at most `MAX_MONITORS_PER_HOST`.
        monitors: Vec<MonitorInfo>,
    },
    /// Guest picks which monitor to capture; the host re-targets capture.
    MonitorSelect {
        /// Monitor id as announced in `MonitorsList`.
        monitor_id: u32,
    },
    /// Guest asks the host to enable or disable privacy mode (§11): blank
    /// the host's physical monitors and block local input while the session
    /// is controlled. The host user must have enabled this capability.
    ///
    /// **Reserved, not pending.** The feature was decided against; nothing
    /// implements this message or its ack, and nothing is going to. The
    /// discriminant stays because removing it renumbers every variant after
    /// it, which the golden vectors of §17.2 exist to prevent outside a major
    /// version. A host that receives it answers nothing.
    PrivacyMode {
        /// `true` to blank and lock, `false` to restore.
        enabled: bool,
    },
    /// Host answers `PrivacyMode`: whether the mode actually became active.
    PrivacyModeAck {
        /// Whether privacy mode is now active on the host.
        active: bool,
    },
    /// Host offers to start streaming audio (§11); parameters are fixed by
    /// constants (`AUDIO_SAMPLE_RATE_HZ` etc.) so none travel on the wire.
    AudioStart {
        /// Sample rate, always `AUDIO_SAMPLE_RATE_HZ` today.
        sample_rate_hz: u32,
        /// Channel count, always `AUDIO_CHANNELS` today.
        channels: u8,
    },
    /// Either side stops the audio channel (§11).
    AudioStop,
    /// Sender aborts one file transfer mid-flight (§9.2). Unlike
    /// `FileAccept(false)` this applies after chunking has already begun.
    FileAbort {
        /// Transfer being aborted, as announced in `FileTransferStart`.
        transfer_id: u64,
    },
    /// Receiver acknowledges contiguous bytes received (§9.2), driving the
    /// sender's resume point after a reconnect.
    FileChunkAck {
        /// Transfer being acknowledged.
        transfer_id: u64,
        /// Number of leading bytes now durably received.
        offset: u64,
    },
    /// Host to guest: this session will never carry a picture, and why (§18).
    ///
    /// Not a revoke and not an error: the control session and every grant on
    /// it stay exactly as they are. What it ends is the guest's *waiting* —
    /// a media pipeline that cannot start has nothing to reconnect to, so the
    /// guest stops its recovery pass instead of sitting out the reconnect
    /// window and then blaming the connection.
    ///
    /// New in minor 2, and appended last on purpose: every discriminant
    /// before it keeps the value the golden vectors of §17.2 froze.
    MediaUnavailable(MediaUnavailableReason),
    /// Guest to host: deliver the Secure Attention Sequence (Ctrl+Alt+Del)
    /// to the host user (§11). New in minor 3.
    ///
    /// The host treats this like any other input event: it is acted on only
    /// from a session whose `input` grant is live right now, and the host
    /// answers [`MessageKind::SasAck`] saying whether it actually managed to
    /// synthesize the sequence — on a platform with no SAS mechanism the
    /// answer is an honest `false`, never a silent success.
    SasRequest,
    /// Host to guest: the answer to [`MessageKind::SasRequest`] (§11). New
    /// in minor 3.
    ///
    /// `true` means the sequence was delivered to the host OS; `false` means
    /// it was refused or is impossible here (no SAS mechanism, no `input`
    /// grant, non-Windows host). Sent only to a guest whose `Hello`
    /// advertised [`FEATURE_REMOTE_SAS`], for the same minor-version reason
    /// `MediaUnavailable` rides behind its feature string.
    SasAck {
        /// Whether the sequence reached the host OS.
        delivered: bool,
    },
    /// Host to guest: this host is configured for unattended access, so there
    /// is nobody here to answer a consent dialog — present credentials
    /// instead (§8). New in minor 4.
    ///
    /// Sent in place of waiting on `ConsentRequest`, and only to a guest whose
    /// `Hello` advertised [`FEATURE_UNATTENDED`]. Saying whether a second
    /// factor is expected is a UI necessity, not a disclosure the host is
    /// careless about: the peer already had to present a valid invite and be
    /// marked trusted in the host's address book before the challenge is
    /// offered at all.
    UnattendedChallenge {
        /// Whether a one-time code must accompany the password.
        code_required: bool,
    },
    /// Guest to host: the answer to [`MessageKind::UnattendedChallenge`]
    /// (§8). New in minor 4.
    ///
    /// Verified in the host's `crates/core` — `unattended::UnattendedAccess::
    /// admit` — and nowhere else. The password is at most
    /// `UNATTENDED_PASSWORD_MAX_BYTES` and the code at most
    /// `UNATTENDED_CODE_MAX_BYTES`, both checked before allocation on decode.
    UnattendedAuth {
        /// The device password, in the clear inside the QUIC/TLS tunnel; the
        /// host hashes it and never stores it.
        password: String,
        /// The one-time code, present only when the challenge asked for one.
        code: Option<String>,
    },
    /// Host to guest: the credentials were refused (§8, §18). New in minor 4.
    ///
    /// A success is not answered with this message but with the ordinary
    /// `ConsentGrant`, so the guest's admission path is the same one an
    /// attended session takes.
    UnattendedReject(UnattendedRejection),
    /// Sender to receiver: the file just accepted is this transfer (§9.2).
    /// New in minor 5.
    ///
    /// Sent after a [`MessageKind::FileAccept`] of `true` and before the
    /// first chunk reaches `rd/file/1`. It exists because `FileAbort` and
    /// `FileChunkAck` both name a `transfer_id` that nothing had ever
    /// assigned: `FileOffer` carries no identifier, so until now the two
    /// sides could not refer to the same transfer at all.
    ///
    /// The offer's three fields are restated rather than implied. An id whose
    /// meaning is "whichever offer we both believe was accepted last" is
    /// shared state with no way to check itself, and a receiver has to be
    /// able to refuse a start that does not describe the file it agreed to —
    /// which it can only do if the start says what it is starting.
    FileTransferStart {
        /// Identifier both sides use for this transfer from here on.
        transfer_id: u64,
        /// Basename, as in the offer this starts.
        name: String,
        /// Size in bytes, as in the offer this starts.
        size: u64,
        /// BLAKE3 of the whole file, as in the offer this starts.
        hash: [u8; 32],
    },
    /// Guest to host: what this receiver actually saw over the last
    /// `ABR_FEEDBACK_INTERVAL_MS` (§11). New in minor 6.
    ///
    /// The input the adaptation of §11 was always specified to have and never
    /// had: without it the host can only watch how long its own writes take
    /// and call that congestion (ADR 0015). Sent only towards a host that
    /// speaks minor 6, and read only from a guest whose `Hello` advertised
    /// [`FEATURE_RECEIVER_REPORT`].
    ///
    /// Every field is a claim by an untrusted peer, so none of them is
    /// range-checked here: an absurd report is a feedback frame to drop at the
    /// point of use, not a malformed frame that should tear the session down.
    /// The one thing this side owes it is that no arithmetic on it can panic,
    /// which is why loss is an integer permille rather than a float.
    ReceiverReport {
        /// Share of frames in the window the decoder could not turn into a
        /// picture, in permille. Values above 1000 are nonsense and dropped.
        loss_permille: u16,
        /// The guest's own smoothed control-channel round trip, in
        /// milliseconds, as it measured it.
        rtt_ms: u32,
        /// Media bytes that arrived in the window, as kilobits per second.
        goodput_kbps: u32,
    },
    /// Guest to host: cap the picture at `scale_percent` of the host's own
    /// captured size (§11; D7, docs/bugs/13-stream-resolution.md). New in
    /// minor 7.
    ///
    /// A ceiling, not a target: the host's adaptive controller (ADR 0037)
    /// stays free to sit below it when the link cannot carry it, and stays
    /// free to recover only up to it, never past it. Sent only towards a host
    /// that speaks minor 7, and read only from a guest whose `Hello`
    /// advertised [`FEATURE_STREAM_SCALE`].
    ///
    /// Bounded here, on decode, because a static range is exactly what §9.1
    /// exists to check before anything downstream believes an untrusted
    /// peer's number: `ABR_MIN_SCALE_PERCENT` is the floor below which text
    /// stops being readable, and `STREAM_SCALE_MAX_PERCENT` is simply the
    /// whole picture.
    StreamScaleRequest {
        /// Requested ceiling, `ABR_MIN_SCALE_PERCENT..=STREAM_SCALE_MAX_PERCENT`.
        scale_percent: u32,
    },
    /// Sender to receiver: "these files are on my clipboard" (docs/bugs/
    /// 14-clipboard-files.md #2; ADR 0047). New in minor 8.
    ///
    /// Names and sizes only, **never paths**: a full path leaks information
    /// about the sending machine that the receiver has no use for (§15). This
    /// message announces nothing more than "there is something to paste" —
    /// no content crosses the wire here, and no bytes move until the receiver
    /// accepts and the existing `FileOffer`/`FileTransferStart` machinery
    /// carries them over `rd/file/1`, exactly as it would for a file offered
    /// through the ordinary picker. Sent only to a peer whose `Hello`
    /// advertised [`FEATURE_CLIPBOARD_FILES`].
    ClipboardFileOffer {
        /// One entry per file currently on the sender's clipboard, at most
        /// `CLIPBOARD_FILE_LIST_MAX_ENTRIES`.
        files: Vec<ClipboardFileEntry>,
    },
    /// Receiver to sender: answers the oldest outstanding
    /// [`MessageKind::ClipboardFileOffer`] entry (§9.2; ADR 0047). New in
    /// minor 8.
    ///
    /// Shaped exactly like [`MessageKind::FileAccept`] — a bare bool — for
    /// the same reason: the entries are answered one at a time, oldest first,
    /// so nothing beyond "yes" or "no" has to travel back. `true` is the
    /// receiving user's single decision to paste; the sender then measures,
    /// hashes and starts the transfer through the same
    /// `FileTransferStart` a regular offer would use — the receiver never
    /// re-confirms the individual file that follows.
    ClipboardFileAccept(bool),
}

/// Why an unattended admission was refused (§8, §18).
///
/// A closed set, and deliberately coarse in the same way
/// `unattended::UnattendedError` is: it never says how close a guess was, and
/// it never distinguishes "the password was right but the code was not" from
/// the other way round. `LockedOut` carries only the remaining seconds, which
/// the guest needs in order to stop retrying.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UnattendedRejection {
    /// The password did not verify, or none was presented.
    BadPassword,
    /// The one-time code did not verify, or none was presented.
    BadCode,
    /// Every attempt is refused until the lockout expires (§18).
    LockedOut {
        /// Seconds until attempts are accepted again.
        remaining_secs: u64,
    },
    /// This host cannot decide right now: unattended access is not configured,
    /// or its stored credentials are unusable. Not a verdict on the guest.
    Unavailable,
}

/// Why a host cannot produce media (§18).
///
/// A closed set, not a free-text reason: this crosses onto a screen the
/// host's operator does not control, and §15 keeps host-identifying detail
/// (device names, driver versions, paths) off the wire. The guest turns it
/// into localized text of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MediaUnavailableReason {
    /// The host has no screen-capture backend for its platform.
    NoCaptureBackend,
    /// The host has no video encoder it can build.
    NoEncoder,
    /// The host's Windows capture is currently blocked by a secure desktop
    /// (lock screen, UAC prompt or fast user switch) and is retrying on its
    /// own (`docs/bugs/11-uac-degradation.md`). Unlike the two reasons
    /// above, this is not permanent: the session stays open, and the guest
    /// should expect it to clear without a reconnect.
    SecureDesktopActive,
}

/// Pixel geometry plus pixel payload of one cursor shape (§11).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorShapeData {
    /// Width in pixels, at least 1.
    pub width: u16,
    /// Height in pixels, at least 1.
    pub height: u16,
    /// Hotspot x within the shape, strictly less than `width`.
    pub hotspot_x: u16,
    /// Hotspot y within the shape, strictly less than `height`.
    pub hotspot_y: u16,
    /// Premultiplied **BGRA** pixels, exactly `width * height * 4` bytes and
    /// at most `MAX_CURSOR_SHAPE_PIXELS * 4`.
    ///
    /// The field name says RGBA and the format is BGRA, which is a
    /// contradiction that had to be resolved in one direction or the other.
    /// It is resolved here, in the comment, because the *bytes* are settled:
    /// every backend that can report a cursor at all produces BGRA — Windows
    /// hands back `DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR` as a 32bpp BGRA
    /// bitmap, and X11's XFIXES `GetCursorImage` returns premultiplied ARGB
    /// packed into a `CARD32`, which is the same four bytes in little-endian
    /// order. Renaming the field would change the postcard-encoded *shape* of
    /// this message and break the golden vectors of §17.2, so the name stays
    /// and this comment is the authority.
    pub rgba: Vec<u8>,
}

/// One file named in a [`MessageKind::ClipboardFileOffer`] (docs/bugs/
/// 14-clipboard-files.md #2; ADR 0047).
///
/// Deliberately the same two fields [`MessageKind::FileOffer`] restates on
/// the wire, minus the hash: hashing every file on a clipboard just to
/// announce it would be a full disk pass nobody asked for yet (ADR 0027), so
/// the hash is computed only once the specific file is actually accepted,
/// exactly as it already is for the first offer a user makes through the
/// picker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardFileEntry {
    /// Basename, already normalized on the sending side the same way
    /// `FileOffer` is — no path separators, nothing a receiver would have to
    /// repair.
    pub name: String,
    /// File size, at most `FILE_OFFER_MAX_BYTES`.
    pub size: u64,
}

/// One monitor of the host, as reported in `MonitorsList` (§11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorInfo {
    /// Host-assigned stable id a guest passes back in `MonitorSelect`.
    pub id: u32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Whether this is the host's primary display.
    pub primary: bool,
}

/// Input event payload: logical key plus physical scancode, never raw OS
/// handles (§11).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputEventPayload {
    /// Platform-independent logical key or button identifier.
    pub logical: u32,
    /// Physical scancode as reported by the guest.
    pub scancode: u32,
    /// Modifier bitmask.
    pub modifiers: u32,
    /// Event-specific data (pointer coordinates, wheel delta, press/release).
    pub detail: InputDetail,
}

/// Discriminates the kind of input carried by an [`InputEventPayload`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputDetail {
    /// Key or button press.
    Press,
    /// Key or button release.
    Release,
    /// Absolute pointer motion, normalized to the captured surface.
    PointerMove {
        /// Horizontal position in 0..=65535 of the captured surface width.
        x: u16,
        /// Vertical position in 0..=65535 of the captured surface height.
        y: u16,
    },
    /// Scroll wheel movement.
    Wheel {
        /// Horizontal delta.
        dx: i16,
        /// Vertical delta.
        dy: i16,
    },
}

impl MessageEnvelope {
    /// Serializes the envelope.
    ///
    /// # Errors
    /// Returns [`CoreError::FrameSize`] if the encoding exceeds
    /// `MAX_CONTROL_FRAME_BYTES`, and [`CoreError::Malformed`] if `postcard`
    /// cannot encode the value.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let bytes = postcard::to_allocvec(self).map_err(|_| CoreError::Malformed)?;
        if bytes.is_empty() || bytes.len() > MAX_CONTROL_FRAME_BYTES {
            return Err(CoreError::FrameSize { size: bytes.len() });
        }
        Ok(bytes)
    }

    /// Deserializes an envelope from an already length-checked frame.
    ///
    /// # Errors
    /// Returns [`CoreError::FrameSize`] if `bytes` is empty or longer than
    /// `MAX_CONTROL_FRAME_BYTES`, and [`CoreError::Malformed`] on any decoding
    /// failure. The size check happens before `postcard` touches the input
    /// (§9.1: allocation-DoS protection).
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.is_empty() || bytes.len() > MAX_CONTROL_FRAME_BYTES {
            return Err(CoreError::FrameSize { size: bytes.len() });
        }
        // `postcard::from_bytes` stops at the end of the value and ignores
        // whatever follows. A frame is length-prefixed, so trailing bytes mean
        // the two sides disagree about what the frame contains; that is a
        // malformed frame, not a decodable one with padding. Rejecting it also
        // keeps the encoding canonical, which the interop vectors of §17.2 rely
        // on.
        let (envelope, rest) =
            postcard::take_from_bytes::<Self>(bytes).map_err(|_| CoreError::Malformed)?;
        if !rest.is_empty() {
            return Err(CoreError::Malformed);
        }
        envelope.check_limits()?;
        Ok(envelope)
    }

    /// Enforces the per-message limits of §14 that are narrower than the
    /// frame bound (§9.1 allocation-DoS protection at the type level).
    ///
    /// # Errors
    /// [`CoreError::Malformed`] when any payload exceeds its constant:
    /// chat text over `CHAT_MAX_BYTES`, clipboard over `CLIPBOARD_MAX_BYTES`,
    /// a cursor whose pixel count contradicts its geometry or exceeds
    /// `MAX_CURSOR_SHAPE_PIXELS`, more than `MAX_MONITORS_PER_HOST` monitors,
    /// a `FileOffer` or `FileTransferStart` over `FILE_OFFER_MAX_BYTES` or
    /// naming a file over `FILE_NAME_MAX_BYTES`.
    fn check_limits(&self) -> Result<()> {
        match &self.kind {
            MessageKind::Chat { text } => {
                if text.len() > CHAT_MAX_BYTES {
                    return Err(CoreError::Malformed);
                }
            }
            MessageKind::ClipboardSync { data } => {
                if data.len() > CLIPBOARD_MAX_BYTES {
                    return Err(CoreError::Malformed);
                }
            }
            MessageKind::CursorShape { shape } => {
                let pixels = usize::from(shape.width) * usize::from(shape.height);
                if pixels == 0 || pixels > MAX_CURSOR_SHAPE_PIXELS || shape.rgba.len() != pixels * 4
                {
                    return Err(CoreError::Malformed);
                }
                if usize::from(shape.hotspot_x) >= usize::from(shape.width)
                    || usize::from(shape.hotspot_y) >= usize::from(shape.height)
                {
                    return Err(CoreError::Malformed);
                }
            }
            MessageKind::MonitorsList { monitors } => {
                if monitors.len() > MAX_MONITORS_PER_HOST {
                    return Err(CoreError::Malformed);
                }
                for monitor in monitors {
                    if monitor.width == 0 || monitor.height == 0 {
                        return Err(CoreError::Malformed);
                    }
                }
            }
            MessageKind::FileOffer { name, size, .. }
            | MessageKind::FileTransferStart { name, size, .. } => {
                if *size > FILE_OFFER_MAX_BYTES || name.len() > FILE_NAME_MAX_BYTES {
                    return Err(CoreError::Malformed);
                }
            }
            // A clipboard file list is untrusted input from a peer that has
            // not necessarily proven anything about the files it claims to
            // hold, so it gets the same two bounds `FileOffer` gets, checked
            // before anything downstream allocates per entry (§9.1;
            // docs/bugs/14-clipboard-files.md #2).
            MessageKind::ClipboardFileOffer { files } => {
                if files.len() > CLIPBOARD_FILE_LIST_MAX_ENTRIES {
                    return Err(CoreError::Malformed);
                }
                for entry in files {
                    if entry.size > FILE_OFFER_MAX_BYTES || entry.name.len() > FILE_NAME_MAX_BYTES {
                        return Err(CoreError::Malformed);
                    }
                }
            }
            // Credentials arrive from a peer that has not been admitted yet,
            // which makes this the least trusted payload on the channel: both
            // fields are bounded before anything downstream allocates (§9.1).
            MessageKind::UnattendedAuth { password, code } => {
                if password.len() > UNATTENDED_PASSWORD_MAX_BYTES {
                    return Err(CoreError::Malformed);
                }
                if code
                    .as_ref()
                    .is_some_and(|c| c.len() > UNATTENDED_CODE_MAX_BYTES)
                {
                    return Err(CoreError::Malformed);
                }
            }
            // A guest's manual ceiling is a claim from an untrusted peer
            // (§9.1), and the range is static — unlike `MonitorSelect`,
            // which can only be checked against a host's own runtime monitor
            // count — so it is checked here rather than at the point of use.
            MessageKind::StreamScaleRequest { scale_percent }
                if *scale_percent < ABR_MIN_SCALE_PERCENT
                    || *scale_percent > STREAM_SCALE_MAX_PERCENT =>
            {
                return Err(CoreError::Malformed);
            }
            _ => {}
        }
        Ok(())
    }
}

/// Checks that `remote_major` matches [`PROTOCOL_MAJOR`].
///
/// # Errors
/// Returns [`CoreError::IncompatibleVersion`] on mismatch; the caller closes
/// the connection before any consent decision (§9.1, §18).
pub const fn check_version(remote_major: u16) -> Result<()> {
    if remote_major == PROTOCOL_MAJOR {
        Ok(())
    } else {
        Err(CoreError::IncompatibleVersion {
            local: PROTOCOL_MAJOR,
            remote: remote_major,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::constants::{
        AUDIO_CHANNELS, AUDIO_SAMPLE_RATE_HZ, CLIPBOARD_FILE_LIST_MAX_ENTRIES, CLIPBOARD_MAX_BYTES,
        FILE_NAME_MAX_BYTES, FILE_OFFER_MAX_BYTES, UNATTENDED_LOCKOUT_DURATION_SECS,
    };

    fn envelope(kind: MessageKind) -> MessageEnvelope {
        MessageEnvelope {
            session_id: [7u8; 16],
            direction: Direction::GuestToHost,
            seq: 0,
            kind,
            body: Vec::new(),
        }
    }

    #[test]
    fn roundtrip_preserves_envelope() {
        let original = envelope(MessageKind::ConsentGrant(Role::ViewOnly));
        let bytes = original.encode().unwrap();
        assert_eq!(MessageEnvelope::decode(&bytes).unwrap(), original);
    }

    #[test]
    fn empty_frame_is_rejected() {
        assert!(matches!(
            MessageEnvelope::decode(&[]),
            Err(CoreError::FrameSize { size: 0 })
        ));
    }

    #[test]
    fn trailing_bytes_make_a_frame_malformed() {
        let envelope = MessageEnvelope {
            session_id: [1u8; 16],
            direction: Direction::HostToGuest,
            seq: 0,
            kind: MessageKind::ConsentRevoke,
            body: Vec::new(),
        };
        let mut bytes = envelope.encode().unwrap();
        assert_eq!(MessageEnvelope::decode(&bytes).unwrap(), envelope);
        bytes.push(0xff);
        assert!(matches!(
            MessageEnvelope::decode(&bytes),
            Err(CoreError::Malformed)
        ));
    }

    #[test]
    fn oversized_frame_is_rejected_before_decoding() {
        let oversized = vec![0u8; MAX_CONTROL_FRAME_BYTES + 1];
        assert!(matches!(
            MessageEnvelope::decode(&oversized),
            Err(CoreError::FrameSize { .. })
        ));
    }

    #[test]
    fn media_unavailable_survives_a_roundtrip() {
        for reason in [
            MediaUnavailableReason::NoCaptureBackend,
            MediaUnavailableReason::NoEncoder,
        ] {
            let original = envelope(MessageKind::MediaUnavailable(reason));
            let bytes = original.encode().unwrap();
            assert_eq!(MessageEnvelope::decode(&bytes).unwrap(), original);
        }
    }

    /// The new kind of minor 2 must sit *after* every kind an earlier minor
    /// froze: the golden vectors of §17.2 encode those discriminants
    /// literally.
    #[test]
    fn the_new_kind_did_not_move_an_older_discriminant() {
        // Byte 18 is the kind discriminant: 16 session id bytes, direction,
        // then a single-byte `seq` of 0.
        let kind_byte = |kind| envelope(kind).encode().unwrap()[18];

        assert_eq!(
            kind_byte(MessageKind::ResumeHello {
                session_id: [0u8; 16],
                last_received_seq: 0,
            }),
            18,
            "the last minor-0 kind moved"
        );
        assert_eq!(
            kind_byte(MessageKind::FileChunkAck {
                transfer_id: 0,
                offset: 0,
            }),
            29,
            "the last minor-1 kind moved"
        );
        assert_eq!(
            kind_byte(MessageKind::MediaUnavailable(
                MediaUnavailableReason::NoEncoder
            )),
            30,
        );
        assert_eq!(
            kind_byte(MessageKind::SasRequest),
            31,
            "the minor-3 kind must be appended after MediaUnavailable"
        );
        assert_eq!(
            kind_byte(MessageKind::SasAck { delivered: false }),
            32,
            "the last minor-3 kind moved"
        );
        assert_eq!(
            kind_byte(MessageKind::UnattendedChallenge {
                code_required: false
            }),
            33,
            "the minor-4 kinds must be appended after SasAck"
        );
        assert_eq!(
            kind_byte(MessageKind::FileTransferStart {
                transfer_id: 0,
                name: String::new(),
                size: 0,
                hash: [0u8; 32],
            }),
            36,
            "the minor-5 kind must be appended after UnattendedReject"
        );
        assert_eq!(
            kind_byte(MessageKind::UnattendedAuth {
                password: String::new(),
                code: None,
            }),
            34,
        );
        assert_eq!(
            kind_byte(MessageKind::UnattendedReject(
                UnattendedRejection::BadPassword
            )),
            35,
        );
        assert_eq!(
            kind_byte(MessageKind::ReceiverReport {
                loss_permille: 0,
                rtt_ms: 0,
                goodput_kbps: 0,
            }),
            37,
            "the minor-6 kind must be appended after FileTransferStart"
        );
        assert_eq!(
            kind_byte(MessageKind::StreamScaleRequest { scale_percent: 100 }),
            38,
            "the minor-7 kind must be appended after ReceiverReport"
        );
        assert_eq!(
            kind_byte(MessageKind::ClipboardFileOffer { files: Vec::new() }),
            39,
            "the minor-8 kinds must be appended after StreamScaleRequest"
        );
        assert_eq!(kind_byte(MessageKind::ClipboardFileAccept(false)), 40,);
    }

    /// A receiver report is a claim, not a fact, so decoding never judges it:
    /// even a nonsense one round-trips, and dropping it is the reader's job
    /// (ADR 0037).
    #[test]
    fn a_receiver_report_roundtrips_including_the_nonsense_ones() {
        let cases = [
            MessageKind::ReceiverReport {
                loss_permille: 0,
                rtt_ms: 42,
                goodput_kbps: 3_000,
            },
            MessageKind::ReceiverReport {
                loss_permille: 1_000,
                rtt_ms: 0,
                goodput_kbps: 0,
            },
            MessageKind::ReceiverReport {
                loss_permille: u16::MAX,
                rtt_ms: u32::MAX,
                goodput_kbps: u32::MAX,
            },
        ];
        for kind in cases {
            let original = envelope(kind);
            let bytes = original.encode().unwrap();
            assert_eq!(MessageEnvelope::decode(&bytes).unwrap(), original);
        }
    }

    #[test]
    fn the_unattended_exchange_roundtrips() {
        let cases = [
            MessageKind::UnattendedChallenge {
                code_required: true,
            },
            MessageKind::UnattendedChallenge {
                code_required: false,
            },
            MessageKind::UnattendedAuth {
                password: "correct horse battery staple".to_owned(),
                code: Some("123456".to_owned()),
            },
            MessageKind::UnattendedAuth {
                password: "no second factor here".to_owned(),
                code: None,
            },
            MessageKind::UnattendedReject(UnattendedRejection::BadPassword),
            MessageKind::UnattendedReject(UnattendedRejection::BadCode),
            MessageKind::UnattendedReject(UnattendedRejection::LockedOut {
                remaining_secs: UNATTENDED_LOCKOUT_DURATION_SECS,
            }),
            MessageKind::UnattendedReject(UnattendedRejection::Unavailable),
        ];
        for kind in cases {
            let original = envelope(kind);
            let bytes = original.encode().unwrap();
            assert_eq!(MessageEnvelope::decode(&bytes).unwrap(), original);
        }
    }

    /// Credentials arrive from a peer that has not been admitted yet, so both
    /// fields are bounded at the parse boundary rather than downstream (§9.1).
    #[test]
    fn oversized_credentials_are_malformed_not_a_big_allocation() {
        let long_password = envelope(MessageKind::UnattendedAuth {
            password: "p".repeat(UNATTENDED_PASSWORD_MAX_BYTES + 1),
            code: None,
        });
        assert!(matches!(
            MessageEnvelope::decode(&long_password.encode().unwrap()),
            Err(CoreError::Malformed)
        ));

        let long_code = envelope(MessageKind::UnattendedAuth {
            password: "fine".to_owned(),
            code: Some("1".repeat(UNATTENDED_CODE_MAX_BYTES + 1)),
        });
        assert!(matches!(
            MessageEnvelope::decode(&long_code.encode().unwrap()),
            Err(CoreError::Malformed)
        ));

        // Exactly at the limit is still a valid frame: the bound is inclusive.
        let at_limit = envelope(MessageKind::UnattendedAuth {
            password: "p".repeat(UNATTENDED_PASSWORD_MAX_BYTES),
            code: Some("1".repeat(UNATTENDED_CODE_MAX_BYTES)),
        });
        assert_eq!(
            MessageEnvelope::decode(&at_limit.encode().unwrap()).unwrap(),
            at_limit
        );
    }

    /// §9.2: the start message carries the offer back so the receiver can
    /// check it, and both of the bounds it repeats are enforced on decode.
    #[test]
    fn a_transfer_start_roundtrips_and_is_bounded_like_the_offer_it_repeats() {
        let original = envelope(MessageKind::FileTransferStart {
            transfer_id: u64::MAX,
            name: "report.pdf".to_owned(),
            size: FILE_OFFER_MAX_BYTES,
            hash: [9u8; 32],
        });
        let bytes = original.encode().unwrap();
        assert_eq!(MessageEnvelope::decode(&bytes).unwrap(), original);

        // A start claiming more than any offer could have carried is
        // malformed, exactly as the offer itself would be.
        let oversized = envelope(MessageKind::FileTransferStart {
            transfer_id: 1,
            name: "big".to_owned(),
            size: FILE_OFFER_MAX_BYTES + 1,
            hash: [0u8; 32],
        })
        .encode()
        .unwrap();
        assert!(matches!(
            MessageEnvelope::decode(&oversized),
            Err(CoreError::Malformed)
        ));

        // A name no filesystem could write is refused before anyone tries.
        let long_name = envelope(MessageKind::FileTransferStart {
            transfer_id: 1,
            name: "n".repeat(FILE_NAME_MAX_BYTES + 1),
            size: 1,
            hash: [0u8; 32],
        })
        .encode()
        .unwrap();
        assert!(matches!(
            MessageEnvelope::decode(&long_name),
            Err(CoreError::Malformed)
        ));
        // Exactly at the bound still passes.
        let at_bound = envelope(MessageKind::FileTransferStart {
            transfer_id: 1,
            name: "n".repeat(FILE_NAME_MAX_BYTES),
            size: 1,
            hash: [0u8; 32],
        })
        .encode()
        .unwrap();
        assert!(MessageEnvelope::decode(&at_bound).is_ok());
    }

    /// The same name bound now covers the offer, which had only ever been
    /// bounded on its size.
    #[test]
    fn an_offer_with_an_unwritable_name_is_malformed() {
        let bytes = envelope(MessageKind::FileOffer {
            name: "n".repeat(FILE_NAME_MAX_BYTES + 1),
            size: 1,
            hash: [0u8; 32],
        })
        .encode()
        .unwrap();
        assert!(matches!(
            MessageEnvelope::decode(&bytes),
            Err(CoreError::Malformed)
        ));
    }

    #[test]
    fn major_mismatch_is_incompatible() {
        assert!(check_version(PROTOCOL_MAJOR).is_ok());
        assert!(matches!(
            check_version(PROTOCOL_MAJOR + 1),
            Err(CoreError::IncompatibleVersion { .. })
        ));
    }

    #[test]
    fn chat_roundtrips_and_oversize_is_rejected() {
        let long = "x".repeat(CHAT_MAX_BYTES);
        let original = envelope(MessageKind::Chat { text: long });
        let bytes = original.encode().unwrap();
        assert_eq!(MessageEnvelope::decode(&bytes).unwrap(), original);

        // One byte over the chat limit is a limit violation, not a crash.
        let mut oversized_text = String::with_capacity(CHAT_MAX_BYTES + 1);
        for _ in 0..=(CHAT_MAX_BYTES) {
            oversized_text.push('y');
        }
        let oversized = envelope(MessageKind::Chat {
            text: oversized_text,
        });
        assert!(matches!(
            MessageEnvelope::decode(&oversized.encode().unwrap()),
            Err(CoreError::Malformed)
        ));
    }

    #[test]
    fn keyframe_request_roundtrips() {
        let original = envelope(MessageKind::KeyframeRequest);
        let bytes = original.encode().unwrap();
        assert_eq!(MessageEnvelope::decode(&bytes).unwrap(), original);
    }

    #[test]
    fn cursor_shape_roundtrips_and_bad_geometry_is_rejected() {
        let shape = CursorShapeData {
            width: 2,
            height: 2,
            hotspot_x: 1,
            hotspot_y: 1,
            rgba: vec![0xAA; 2 * 2 * 4],
        };
        let original = envelope(MessageKind::CursorShape {
            shape: shape.clone(),
        });
        let bytes = original.encode().unwrap();
        assert_eq!(MessageEnvelope::decode(&bytes).unwrap(), original);

        // Pixel count disagreeing with the geometry is malformed, never a
        // short read further down the pipeline.
        let mut lying = shape;
        lying.rgba.pop();
        let bad = envelope(MessageKind::CursorShape { shape: lying });
        assert!(matches!(
            MessageEnvelope::decode(&bad.encode().unwrap()),
            Err(CoreError::Malformed)
        ));
    }

    /// A cursor is a bitmap from an untrusted peer, and the three ways its
    /// geometry can lie all have to end in `Malformed` rather than in an
    /// allocation, an index past the end, or a divide by zero.
    #[test]
    fn every_impossible_cursor_geometry_is_refused() {
        let shape = |width: u16, height: u16, hotspot_x: u16, hotspot_y: u16, pixels: usize| {
            envelope(MessageKind::CursorShape {
                shape: CursorShapeData {
                    width,
                    height,
                    hotspot_x,
                    hotspot_y,
                    rgba: vec![0x11; pixels],
                },
            })
        };
        let refused = |kind: MessageEnvelope| {
            matches!(
                MessageEnvelope::decode(&kind.encode().unwrap()),
                Err(CoreError::Malformed)
            )
        };

        // A zero axis: `width * height` is 0, and a "shape" of no pixels is
        // not a shape.
        assert!(refused(shape(0, 4, 0, 0, 0)));
        assert!(refused(shape(4, 0, 0, 0, 0)));
        // A hotspot outside its own shape: there is no pixel to put under the
        // pointer.
        assert!(refused(shape(2, 2, 2, 0, 2 * 2 * 4)));
        assert!(refused(shape(2, 2, 0, 2, 2 * 2 * 4)));
        // More pixels than the geometry accounts for, as well as fewer.
        assert!(refused(shape(2, 2, 0, 0, 2 * 2 * 4 + 1)));
        assert!(refused(shape(2, 2, 0, 0, 2 * 2 * 4 - 1)));
        // An ordinary cursor — the 32x32 and 64x64 every desktop actually
        // uses — is untouched by any of this.
        for side in [32u16, 64] {
            let ok = shape(side, side, 0, 0, usize::from(side) * usize::from(side) * 4);
            assert_eq!(MessageEnvelope::decode(&ok.encode().unwrap()).unwrap(), ok);
        }

        // An area past the bound of §14 never reaches a decoder at all: at
        // four bytes a pixel, `MAX_CURSOR_SHAPE_PIXELS` is already the whole
        // of `MAX_CONTROL_FRAME_BYTES`, so the frame bound of §9.1 is the
        // stricter of the two in practice and refuses it while encoding.
        // Both refusals are the same outcome — nothing that size is sent, and
        // nothing that size is believed — and the test asserts the outcome
        // rather than which check happened to get there first.
        let side = 256u16; // 65536 pixels, four times MAX_CURSOR_SHAPE_PIXELS.
        let oversized = shape(side, side, 0, 0, usize::from(side) * usize::from(side) * 4);
        match oversized.encode() {
            Err(CoreError::FrameSize { .. }) => {}
            Err(other) => panic!("unexpected refusal while encoding: {other}"),
            Ok(bytes) => assert!(matches!(
                MessageEnvelope::decode(&bytes),
                Err(CoreError::Malformed)
            )),
        }
    }

    #[test]
    fn monitors_list_roundtrips_and_overflow_is_rejected() {
        let monitors = vec![MonitorInfo {
            id: 0,
            width: 1920,
            height: 1080,
            primary: true,
        }];
        let original = envelope(MessageKind::MonitorsList {
            monitors: monitors.clone(),
        });
        let bytes = original.encode().unwrap();
        assert_eq!(MessageEnvelope::decode(&bytes).unwrap(), original);

        let flood = vec![
            MonitorInfo {
                id: 0,
                width: 1,
                height: 1,
                primary: false,
            };
            MAX_MONITORS_PER_HOST + 1
        ];
        let overflow = envelope(MessageKind::MonitorsList { monitors: flood });
        assert!(matches!(
            MessageEnvelope::decode(&overflow.encode().unwrap()),
            Err(CoreError::Malformed)
        ));
    }

    #[test]
    fn privacy_audio_and_file_abort_roundtrip() {
        let cases = [
            MessageKind::PrivacyMode { enabled: true },
            MessageKind::PrivacyModeAck { active: false },
            MessageKind::AudioStart {
                sample_rate_hz: AUDIO_SAMPLE_RATE_HZ,
                channels: AUDIO_CHANNELS,
            },
            MessageKind::AudioStop,
            MessageKind::FileAbort { transfer_id: 7 },
            MessageKind::FileChunkAck {
                transfer_id: 7,
                offset: 4096,
            },
        ];
        for kind in cases {
            let original = envelope(kind);
            let bytes = original.encode().unwrap();
            assert_eq!(MessageEnvelope::decode(&bytes).unwrap(), original);
        }
    }

    #[test]
    fn legacy_clipboard_limit_is_enforced_on_decode() {
        // The frame bound of §9.1 already keeps a >64 KiB clipboard off the
        // wire entirely: `encode` refuses before anything is sent.
        let oversized = envelope(MessageKind::ClipboardSync {
            data: vec![0; CLIPBOARD_MAX_BYTES + 1],
        });
        assert!(matches!(
            oversized.encode(),
            Err(CoreError::FrameSize { .. })
        ));
    }

    #[test]
    fn legacy_file_offer_size_limit_is_enforced_on_decode() {
        let oversized = envelope(MessageKind::FileOffer {
            name: "big.bin".to_owned(),
            size: FILE_OFFER_MAX_BYTES + 1,
            hash: [0; 32],
        });
        assert!(matches!(
            MessageEnvelope::decode(&oversized.encode().unwrap()),
            Err(CoreError::Malformed)
        ));
    }

    /// D7, docs/bugs/13-stream-resolution.md task 1: the range is
    /// `ABR_MIN_SCALE_PERCENT..=STREAM_SCALE_MAX_PERCENT`, inclusive on both
    /// ends, and a guest's claim outside it is malformed rather than an index
    /// or an allocation trusted from an unauthenticated field.
    #[test]
    fn a_stream_scale_request_roundtrips_within_its_range_and_is_malformed_outside_it() {
        for scale_percent in [ABR_MIN_SCALE_PERCENT, 75, STREAM_SCALE_MAX_PERCENT] {
            let original = envelope(MessageKind::StreamScaleRequest { scale_percent });
            let bytes = original.encode().unwrap();
            assert_eq!(MessageEnvelope::decode(&bytes).unwrap(), original);
        }

        for scale_percent in [
            0,
            ABR_MIN_SCALE_PERCENT - 1,
            STREAM_SCALE_MAX_PERCENT + 1,
            u32::MAX,
        ] {
            let bytes = envelope(MessageKind::StreamScaleRequest { scale_percent })
                .encode()
                .unwrap();
            assert!(
                matches!(MessageEnvelope::decode(&bytes), Err(CoreError::Malformed)),
                "scale_percent {scale_percent} outside the range was accepted"
            );
        }
    }

    /// docs/bugs/14-clipboard-files.md #2: names and sizes travel, an empty
    /// list is ordinary (declining a whole clipboard is still a valid state
    /// to be in), and the accept is a bare bool like `FileAccept`.
    #[test]
    fn a_clipboard_file_offer_roundtrips_including_the_accept() {
        let cases = [
            MessageKind::ClipboardFileOffer { files: Vec::new() },
            MessageKind::ClipboardFileOffer {
                files: vec![
                    ClipboardFileEntry {
                        name: "report.pdf".to_owned(),
                        size: 4096,
                    },
                    ClipboardFileEntry {
                        name: "photo.png".to_owned(),
                        size: 1_048_576,
                    },
                ],
            },
            MessageKind::ClipboardFileAccept(true),
            MessageKind::ClipboardFileAccept(false),
        ];
        for kind in cases {
            let original = envelope(kind);
            let bytes = original.encode().unwrap();
            assert_eq!(MessageEnvelope::decode(&bytes).unwrap(), original);
        }
    }

    /// §9.1: the same two bounds `FileOffer` enforces apply per entry, and to
    /// the whole list's length, before anything downstream believes an
    /// untrusted peer's clipboard announcement (docs/bugs/
    /// 14-clipboard-files.md #2).
    #[test]
    fn a_clipboard_file_offer_past_its_bounds_is_malformed() {
        let too_many = envelope(MessageKind::ClipboardFileOffer {
            files: (0..=CLIPBOARD_FILE_LIST_MAX_ENTRIES)
                .map(|i| ClipboardFileEntry {
                    name: format!("file-{i}.bin"),
                    size: 1,
                })
                .collect(),
        });
        assert!(matches!(
            MessageEnvelope::decode(&too_many.encode().unwrap()),
            Err(CoreError::Malformed)
        ));

        let name_too_long = envelope(MessageKind::ClipboardFileOffer {
            files: vec![ClipboardFileEntry {
                name: "n".repeat(FILE_NAME_MAX_BYTES + 1),
                size: 1,
            }],
        });
        assert!(matches!(
            MessageEnvelope::decode(&name_too_long.encode().unwrap()),
            Err(CoreError::Malformed)
        ));

        let size_too_big = envelope(MessageKind::ClipboardFileOffer {
            files: vec![ClipboardFileEntry {
                name: "big.bin".to_owned(),
                size: FILE_OFFER_MAX_BYTES + 1,
            }],
        });
        assert!(matches!(
            MessageEnvelope::decode(&size_too_big.encode().unwrap()),
            Err(CoreError::Malformed)
        ));

        // Exactly at every bound is still ordinary traffic.
        let at_bounds = envelope(MessageKind::ClipboardFileOffer {
            files: (0..CLIPBOARD_FILE_LIST_MAX_ENTRIES)
                .map(|i| ClipboardFileEntry {
                    name: format!(
                        "{i}{}",
                        "n".repeat(FILE_NAME_MAX_BYTES - i.to_string().len())
                    ),
                    size: FILE_OFFER_MAX_BYTES,
                })
                .collect(),
        });
        assert!(MessageEnvelope::decode(&at_bounds.encode().unwrap()).is_ok());
    }
}
