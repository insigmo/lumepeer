//! Single point of truth for every numeric constant of the project
//! (design doc §14). Magic numbers duplicating these values are a defect.

/// Maximum size of one control frame payload, checked before allocation (§9.1).
pub const MAX_CONTROL_FRAME_BYTES: usize = 65_536;
/// Maximum size of one media frame payload on `rd/media/1`, checked before
/// allocation exactly as `MAX_CONTROL_FRAME_BYTES` is on the control channel
/// (§3.2, §9.1). Encoded video needs far more room than a control message, but
/// still a bound: at the `ABR_MAX_BITRATE_KBPS` ceiling a single keyframe stays
/// orders of magnitude below this. It is deliberately at or under
/// `lumepeer_media::decode::SLOT_PAYLOAD_BYTES`, so a frame that passed this
/// check always fits the decoder's shared-memory slot.
pub const MAX_MEDIA_FRAME_BYTES: usize = 4 * 1024 * 1024;
/// Largest picture, in pixels, that travels through the media pipeline in one
/// frame (§11, §15; ADR 0018).
///
/// A host screen bigger than this is downscaled before it is encoded, so the
/// wire, the sandboxed decoder's shared-memory slot and the guest's canvas all
/// stay inside the memory budget of `ACTIVE_SESSION_EXTRA_RAM_BUDGET_MIB`.
/// One RGBA8 picture of this size is 8 MiB, which is exactly what
/// `lumepeer_media::decode::SLOT_PAYLOAD_BYTES` holds — the two are asserted
/// against each other at compile time there, so raising one without the other
/// does not build.
pub const MAX_PICTURE_PIXELS: usize = 1920 * 1080;
/// Pause between redial attempts inside the one media recovery pass bounded by
/// [`RECONNECT_WINDOW_SECS`]. Not a second reconnect window: it only keeps a
/// host that refuses instantly from turning that window into a busy loop.
pub const MEDIA_REDIAL_BACKOFF_MS: u64 = 500;
/// Per-`NodeId` rate limit on `ConsentRequest` (§9.2).
pub const CONSENT_RATE_PER_MINUTE: u32 = 5;
/// Total size of the host-side consent queue across all guests (§8.1).
pub const MAX_PENDING_CONSENTS: usize = 3;
/// Window in which a dropped session may be resumed by the same peer (§10).
pub const RECONNECT_WINDOW_SECS: u64 = 60;
/// Pause between attempts of `WindowsCapturer` to reopen a Desktop
/// Duplication lost to the secure desktop (lock screen, UAC prompt or fast
/// user switch), in milliseconds (docs/bugs/11-uac-degradation.md).
pub const SECURE_DESKTOP_RECOVERY_BACKOFF_MS: u64 = 1_000;
/// Consecutive reopen attempts `WindowsCapturer` makes before giving up and
/// reporting the session as interrupted for good
/// (docs/bugs/11-uac-degradation.md). At
/// [`SECURE_DESKTOP_RECOVERY_BACKOFF_MS`] this is a two-minute window —
/// generous enough for someone to notice and answer a UAC prompt, bounded
/// enough that a host with a permanently unattended monitor does not spin
/// forever.
pub const SECURE_DESKTOP_RECOVERY_MAX_ATTEMPTS: u32 = 120;
/// How often the encode loop asks the privileged helper for a fresh frame of
/// the secure desktop while it holds the `secure_desktop` grant and capture
/// is stuck behind one, in milliseconds
/// (`docs/bugs/15-secure-desktop-capture.md`, ADR 0046).
///
/// Deliberately much slower than the ordinary encode cadence: a UAC prompt
/// or a lock screen is largely static, and a fresh pipe round trip plus a GDI
/// capture on a `LocalSystem` process is a real cost to spend on every frame
/// interval for a picture that mostly is not changing.
pub const SECURE_DESKTOP_CAPTURE_INTERVAL_MS: u64 = 500;
/// Control-channel keepalive interval (§9.1).
pub const PING_INTERVAL_SECS: u64 = 20;
/// Smoothing factor of the exponentially weighted moving average that turns
/// individual `Ping`/`Pong` round trips into the RTT the UI shows (§11, §18).
///
/// Not in the design doc: §9.1 fixes the keepalive interval but says nothing
/// about how the measurement it carries is reported. A raw sample jumps with
/// every retransmit and scheduling hiccup, and a number that jumps is a number
/// nobody can act on. 0.25 keeps roughly the last four probes in view — about
/// a minute and a half at [`PING_INTERVAL_SECS`] — which is slow enough to be
/// readable and fast enough to notice a path that just got worse.
pub const RTT_EWMA_ALPHA: f32 = 0.25;
/// Largest round trip, in milliseconds, that is taken as a measurement rather
/// than as a broken clock (§18).
///
/// A `Pong` that comes back after this either crossed a suspended machine or
/// was never really a round trip at all, and folding it into the
/// [`RTT_EWMA_ALPHA`] average would poison the reading for minutes. Above it
/// the sample is dropped and the previous average stands.
pub const RTT_MAX_PLAUSIBLE_MS: u32 = 60_000;
/// Deadline for one accepted connection to complete the control handshake
/// before the host drops it, so a peer that connects and then goes silent
/// cannot tie up a task (§9.1, §18).
///
/// This covers only the `Hello`/`HelloAck` exchange, which is one round trip
/// on a connection that is already up. Getting the connection up is bounded
/// separately by [`INCOMING_ACCEPT_TIMEOUT_SECS`], because the two are not the
/// same kind of wait at all (ADR 0027).
pub const CONTROL_HANDSHAKE_TIMEOUT_SECS: u64 = 10;
/// Deadline for an accepted incoming connection to finish its **QUIC**
/// handshake, before any control frame is expected of it (§9.1, §18).
///
/// Deliberately longer than a guest's own dial budget. A guest that is still
/// hole-punching has not gone silent — it is working — and a host that gives
/// up first turns a slow path into a failed session while the far side is
/// still trying (ADR 0027).
pub const INCOMING_ACCEPT_TIMEOUT_SECS: u64 = 20;
/// Attempts one outgoing connect makes before it is reported as failed
/// (§7, ADR 0027).
///
/// A first attempt races whatever the host's address set said when the invite
/// was issued: a relay the host has since moved off, a hole punch that has not
/// landed, a discovery record that is a few seconds stale. Each of those is
/// gone by the next attempt, and a user who has to re-paste the code cannot
/// tell any of them from a dead host.
pub const DIAL_ATTEMPTS: u32 = 3;
/// Pause between the attempts of [`DIAL_ATTEMPTS`], long enough for iroh's own
/// discovery to have republished, short enough not to read as a hang.
pub const DIAL_RETRY_BACKOFF_MS: u64 = 750;
/// Bound on one attempt of [`DIAL_ATTEMPTS`] — dial *and* handshake together.
///
/// Without it a single attempt can hold the whole budget: a connection that
/// comes up over a relay link which then dies takes iroh's full dial timeout
/// plus the handshake's own stall before it gives up, and the retry that would
/// have worked never runs (ADR 0027).
pub const CONNECT_ATTEMPT_TIMEOUT_SECS: u64 = 20;
/// Handshakes the host will run concurrently. Beyond this, further incoming
/// connections are closed immediately rather than queued (§3.2).
pub const MAX_INFLIGHT_HANDSHAKES: usize = 8;
/// Cumulative active session time granted to the trial plan (§12.3).
pub const TRIAL_SESSION_LIMIT_SECS: u64 = 30 * 60;
/// Thresholds before license expiry at which `LicenseWarn` is sent (§9.1).
pub const LICENSE_WARN_BEFORE_SECS: [u64; 2] = [300, 60];
/// Client heartbeat interval towards the broker (§12.2).
pub const HEARTBEAT_INTERVAL_SECS: u64 = 180;
/// Offline grace without a successful heartbeat, Pro plan (§12.3).
pub const OFFLINE_GRACE_PRO_DAYS: u64 = 7;
/// Offline grace without a successful heartbeat, Team plan (§12.3).
pub const OFFLINE_GRACE_TEAM_DAYS: u64 = 3;
/// Concurrent guest connections allowed on the trial plan (§8.2).
pub const MAX_CONCURRENT_GUESTS_TRIAL: u8 = 1;
/// Concurrent guest connections allowed on the Pro plan (§8.2).
pub const MAX_CONCURRENT_GUESTS_PRO: u8 = 1;
/// Concurrent guest connections allowed on the Team plan, controller
/// included in the ceiling (§8.2).
pub const MAX_CONCURRENT_GUESTS_TEAM: u8 = 5;
/// TTL of a short-link entry (§7).
pub const SHORT_LINK_TTL_SECS: u64 = 600;
/// Width of the opaque short-link identifier (§7).
pub const SHORT_LINK_ID_BITS: usize = 128;
/// Maximum clipboard payload, text/plain UTF-8 only (§9.2).
pub const CLIPBOARD_MAX_BYTES: usize = 64 * 1024;
/// How often the desktop host re-reads its own clipboard while at least one
/// session holds a clipboard grant (§9.2; ADR 0030).
///
/// Not in the design doc: §9.2 assumes a clipboard *change* is observable,
/// and no cross-platform API delivers one. Polling is the substitute, so the
/// number is a latency/cost trade rather than a protocol value — fast enough
/// that copy-then-paste feels immediate, slow enough that an idle granted
/// session is not reading the user's clipboard hundreds of times a minute.
/// The poll runs only while a grant is live; without one the clipboard is
/// never read at all.
pub const CLIPBOARD_POLL_INTERVAL_MS: u64 = 500;
/// Maximum size of a single offered file (§9.2).
pub const FILE_OFFER_MAX_BYTES: u64 = 500 * 1024 * 1024;
/// Maximum byte length of the file name in a `FileOffer` or a
/// `FileTransferStart` (§9.2; ADR 0032).
///
/// Not in the design doc: §9.2 bounds the file, not its name. 255 is the
/// per-component limit of every filesystem this ships on, so a longer name
/// could not be written down anyway — and a name is untrusted input that ends
/// up as a path, which is the one place a missing bound is worth having.
pub const FILE_NAME_MAX_BYTES: usize = 255;
/// Maximum number of pending file offers per session (§9.2).
pub const MAX_PENDING_FILE_OFFERS: usize = 3;
/// Idle desktop RSS budget (§15).
pub const IDLE_RAM_BUDGET_MIB: u32 = 60;
/// Extra RSS budget for an active session with hardware encode (§15).
pub const ACTIVE_SESSION_EXTRA_RAM_BUDGET_MIB: u32 = 150;
/// Width of the random invite identifier (§7).
pub const INVITE_ID_BITS: usize = 128;
/// TTL of a one-shot invite ticket (§7).
pub const INVITE_TICKET_TTL_SECS: u64 = 10 * 60;
/// Short-link creation rate limit per IP (§7).
pub const SHORT_LINK_CREATE_RATE_PER_MIN: u32 = 10;
/// Short-link resolution rate limit per IP (§7).
pub const SHORT_LINK_RESOLVE_RATE_PER_MIN: u32 = 30;
/// Hard cutoff for an already active session after a wall-clock rollback (§12.3).
pub const CLOCK_ROLLBACK_ACTIVE_SESSION_CUTOFF_SECS: u64 = 10 * 60;
/// Default encoder frame rate (§11).
pub const ENCODE_DEFAULT_FPS: u8 = 30;
/// Ceiling on the worker threads the software H.264 encoder may use (§11,
/// ADR 0027).
///
/// Only the fallback encoder needs this: a hardware MFT does its own
/// scheduling. What it bounds is a host with no hardware encoder, where
/// `openh264` on one thread produced single-digit frame rates at 1080p while
/// the rest of the machine sat idle. Capped rather than "all cores" because
/// the host is someone's working machine, not a transcoding farm, and §15
/// budgets the session, not the box.
pub const ENCODE_MAX_SOFTWARE_THREADS: u16 = 4;
/// Default encoder bitrate (§11).
pub const ENCODE_DEFAULT_BITRATE_KBPS: u32 = 4_000;
/// Lower bound of the adaptive bitrate range (§11).
pub const ABR_MIN_BITRATE_KBPS: u32 = 300;
/// Upper bound of the adaptive bitrate range (§11).
pub const ABR_MAX_BITRATE_KBPS: u32 = 12_000;
/// Receiver feedback interval sent by the guest (§11).
pub const ABR_FEEDBACK_INTERVAL_MS: u32 = 500;
/// How long the host keeps treating a guest's last
/// [`ABR_FEEDBACK_INTERVAL_MS`] report as the truth about the link before it
/// falls back to judging congestion by its own write latency (§11; ADR 0015,
/// ADR 0037).
///
/// Deliberately several report intervals: reports ride the control channel
/// while pictures ride `rd/media/1`, so one late report is ordinary and must
/// not flip the host between two disagreeing congestion signals every second.
/// Long enough for that, short enough that a guest which stops reporting
/// entirely — an old peer, a wedged view — gets the host-local estimate back
/// rather than a quality target frozen where it happened to be.
pub const ABR_FEEDBACK_STALE_AFTER_MS: u64 = 2_000;
/// Maximum rate at which the host applies quality changes (§11).
///
/// Named for the bitrate because that is the only knob §11 has, and it now
/// covers frame rate and resolution as well (ADR 0037): the ceiling is one
/// change of the *whole* target per second, not one per knob. Ripple from
/// three knobs moving independently reads worse than a steadily lower
/// picture.
pub const ABR_ADJUST_MAX_RATE_PER_SEC: u32 = 1;
/// Lower bound of the adaptive frame rate, the second rung of the degradation
/// ladder (§11; ADR 0037).
///
/// Not in the design doc: §11 adapts the bitrate only. Below this a desktop
/// stops reading as a live screen and starts reading as a broken one, which is
/// the failure `ABR_MIN_BITRATE_KBPS` exists to prevent on its own axis.
pub const ABR_MIN_FPS: u8 = 10;
/// Lower bound of the adaptive picture scale, in percent of the captured
/// size — the third and last rung of the ladder (§11; ADR 0037).
///
/// Half of each axis is a quarter of the pixels, which is as far as a remote
/// desktop can be reduced and still have readable text.
pub const ABR_MIN_SCALE_PERCENT: u32 = 50;
/// Step, in percent, by which the adaptive picture scale moves (§11; ADR 0037).
pub const ABR_SCALE_STEP_PERCENT: u32 = 25;
/// Step, in frames per second, by which the adaptive frame rate moves
/// (§11; ADR 0037).
pub const ABR_FPS_STEP: u8 = 5;
/// Fraction of the current bitrate target, in percent, that observed goodput
/// must fall under before the host treats the link as unable to carry what it
/// is sending (§11; ADR 0037).
///
/// `rd/media/1` is a reliable ordered QUIC stream, so a guest never reports
/// lost bytes — the honest congestion signal it *can* report is that less
/// arrived per second than was sent. The margin keeps an idle screen, which
/// legitimately produces far less than the target, from reading as congestion:
/// goodput is only consulted while frames are actually flowing.
pub const ABR_GOODPUT_SHORTFALL_PERCENT: u32 = 70;
/// Shortest interval between two keyframes the host will force on a guest's
/// request (§11).
///
/// Not in the design doc: §11 has the request, not a budget for it. A keyframe
/// is the most expensive frame in the stream, so a guest that asks on every
/// frame would turn the request into a way to make the host send nothing else.
/// The host honours at most one request per interval and drops the rest.
pub const KEYFRAME_MIN_INTERVAL_MS: u64 = 1_000;
/// Log rotation by age (§16.1).
pub const LOG_ROTATION_DAYS: u32 = 7;
/// Log rotation by size (§16.1).
pub const LOG_ROTATION_MAX_MIB: u32 = 100;
/// Bound on how long the Windows Media Foundation hardware encoder waits for
/// an async MFT event (`METransformNeedInput`/`METransformHaveOutput`) before
/// treating the encoder as stalled. Not in the design doc: added so a wedged
/// GPU driver fails one `encode()` call instead of hanging the session
/// forever, per the "degrade towards safety, tell the user" rule of §24.5
/// (ADR 0011).
pub const ENCODE_HW_EVENT_TIMEOUT_MS: u64 = 2_000;

/// Maximum UTF-8 byte length of one chat message (§9.2). Chat rides the
/// control channel, so it must always stay well under
/// `MAX_CONTROL_FRAME_BYTES`.
pub const CHAT_MAX_BYTES: usize = 4_096;

/// Maximum pixel area of one cursor shape update (§11). A cursor is UI
/// chrome, never a second video channel; anything larger is malformed.
pub const MAX_CURSOR_SHAPE_PIXELS: usize = 128 * 128;

/// Maximum number of monitors one host may report in `MonitorsList` (§11).
pub const MAX_MONITORS_PER_HOST: usize = 8;

/// Audio sample rate of the Opus audio channel (§11). Opus internally
/// supports 8/12/16/24/48 kHz; 48 kHz is the only full-band rate and the
/// single fixed value keeps the negotiation trivial.
pub const AUDIO_SAMPLE_RATE_HZ: u32 = 48_000;
/// Audio channel count of the Opus audio channel (§11).
pub const AUDIO_CHANNELS: u8 = 2;
/// Duration of one encoded audio frame in milliseconds (§11). 20 ms is the
/// Opus default and keeps latency under one video frame at 30 fps.
pub const AUDIO_FRAME_MS: u32 = 20;
/// Default audio bitrate (§11): 96 kbit/s, the Opus sweet spot for a
/// stereo desktop-audio mix.
pub const AUDIO_DEFAULT_BITRATE_BPS: i32 = 96_000;
/// Maximum size of one encoded audio frame on the wire (§11). 20 ms of
/// uncompressed 48 kHz stereo s16 is 3 840 bytes; Opus output is smaller,
/// but the bound stays for the length check before allocation.
pub const AUDIO_MAX_FRAME_BYTES: usize = 8 * 1024;

/// Maximum payload of one file-transfer chunk on `rd/file/1` (§9.2). Chunks
/// ride the media framing bound, so they must stay strictly under
/// `MAX_MEDIA_FRAME_BYTES` with room for the chunk header.
pub const FILE_CHUNK_MAX_BYTES: usize = 256 * 1024;
/// Maximum concurrent file transfers per session (§9.2). Mirrors
/// `MAX_PENDING_FILE_OFFERS` for the transfer phase that follows an offer.
pub const MAX_CONCURRENT_FILE_TRANSFERS: usize = 3;
/// How long a chunk stream on `rd/file/1` waits for the `FileTransferStart`
/// that names its transfer before giving up (§9.2; ADR 0032).
///
/// Not in the design doc: it exists because the control channel and the file
/// channel are separate QUIC connections (§4), so nothing orders the start
/// message against the first chunk. The wait is short — this is two messages
/// the same peer sent at the same moment, not a network round trip — and
/// timing out aborts one transfer rather than the connection.
pub const FILE_TRANSFER_START_TIMEOUT_SECS: u64 = 15;

/// Time step of the RFC 6238 TOTP second factor (§8; ADR 0023 §2). 30 s is
/// what every mainstream authenticator app defaults to.
pub const UNATTENDED_TOTP_STEP_SECS: u64 = 30;
/// Consecutive failed unattended verifications before the host locks out
/// brute force (§18).
pub const UNATTENDED_MAX_FAILED_ATTEMPTS: u32 = 5;
/// How long the host refuses every unattended verification once the failure
/// limit is reached (§18).
pub const UNATTENDED_LOCKOUT_DURATION_SECS: u64 = 300;

/// Shortest device password the host will accept when setting one (§8).
///
/// §8 fixes the lockout but not a strength policy, and leaving one out would
/// be a policy decision made by omission: five attempts per
/// `UNATTENDED_LOCKOUT_DURATION_SECS` is only a meaningful defence if the
/// secret has enough room in it to be worth guessing at that rate. Eight bytes
/// is the floor, not a recommendation (ADR 0033).
pub const UNATTENDED_PASSWORD_MIN_BYTES: usize = 8;
/// Longest device password an unattended credential message may carry (§8;
/// §9.1 allocation-DoS check at the parse boundary). Generous enough for a
/// passphrase, far below `MAX_CONTROL_FRAME_BYTES`.
pub const UNATTENDED_PASSWORD_MAX_BYTES: usize = 1024;
/// Longest one-time code an unattended credential message may carry (§8).
///
/// Codes are six digits today (`unattended::Totp`), and verification insists
/// on exactly that. The wire limit leaves a little room on purpose, so a peer
/// sending a longer code fails verification with a coarse `BadCode` instead of
/// having its connection torn down as a malformed frame.
pub const UNATTENDED_CODE_MAX_BYTES: usize = 8;

/// How long the host keeps an audit record before deleting it (§15).
///
/// §15 fixes the retention, and that is the whole policy: records older than
/// this are removed unconditionally, protocol violations included. "Keep the
/// interesting ones longer" would be a retention decision §15 did not make,
/// and a log that quietly outlives its stated retention is worse than no log.
pub const AUDIT_RETENTION_DAYS: u64 = 30;

/// How often the host sweeps records past [`AUDIT_RETENTION_DAYS`] (§15).
///
/// Once a day, plus once at startup. Sweeping per append would turn every
/// consent decision into a table scan, and the cutoff moves by seconds — a
/// record can outlive its retention by up to this long, which is the price of
/// not paying for a scan the host has no reason to make.
pub const AUDIT_RETENTION_SWEEP_SECS: u64 = 24 * 60 * 60;
