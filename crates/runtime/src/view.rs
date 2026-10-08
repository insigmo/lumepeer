//! Remote-view pipeline: host capture/encode, guest decode, view windows
//! (design doc §4.1, §8.1, §11, §11.3).
//!
//! Nothing here authorizes anything. The actor in [`crate::network`] decides
//! whether a peer holds a `view` or an `input` grant and only then calls into
//! this module; every function below assumes that decision has already been
//! taken by `lumepeer-core` (§2.3).
//!
//! Two loops live here, one per side of a session:
//!
//! - The **host** loop pulls frames out of the shared [`CaptureController`]
//!   and encodes them; a second task writes them onto that peer's
//!   `rd/media/1` stream, one frame deep, so a slow link cannot stall the
//!   capture of the next picture (ADR 0059). The controller is the gate: with
//!   no viewer it refuses to produce a frame, so the loop ends by itself when
//!   the last viewer leaves (§8.1, §11).
//! - The **guest** loop dials `rd/media/1` and hands what arrives to whichever
//!   side of the window is decoding (ADR 0058). A window whose `WebView` has a
//!   `VideoDecoder` takes the bitstream through [`BitstreamFeed`] and decodes
//!   it itself; one that does not gets the sandboxed worker process of §11.3,
//!   which decodes to RGBA into a single-slot `watch` channel. Reordering is
//!   the jitter buffer's problem upstream of decode either way.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use lumepeer_core::NodeId;
use lumepeer_core::consent::HostAttendance;
use lumepeer_core::constants::{
    ABR_FEEDBACK_INTERVAL_MS, ABR_FEEDBACK_STALE_AFTER_MS, AUDIO_CHANNELS, AUDIO_MAX_FRAME_BYTES,
    AUDIO_SAMPLE_RATE_HZ,
    ENCODE_REFUSALS_BEFORE_FAULT, KEYFRAME_MIN_INTERVAL_MS, MAX_MEDIA_FRAME_BYTES,
    MEDIA_ACK_BEST_WINDOW_MS, MEDIA_QUEUE_JITTER_SLACK_MAX_MS, MEDIA_QUEUE_SLACK_MAX_MS,
    MEDIA_QUEUE_SLACK_MIN_MS, MEDIA_REDIAL_BACKOFF_MS, RECONNECT_WINDOW_SECS,
    SECURE_DESKTOP_CAPTURE_INTERVAL_MS, SOFTWARE_AV1_MAX_FPS, SOFTWARE_AV1_MAX_PIXELS,
    SOFTWARE_AV1_SLOW_WINDOWS, SOFTWARE_AV1_WATCH_FRAMES,
};
use lumepeer_core::protocol::{CursorShapeData, MediaUnavailableReason};
use lumepeer_media::audio_meter::AudioMeter;
use lumepeer_media::abr::{
    AbrController, EncodeSpeed, FULL_SCALE_PERCENT, LinkPressure, QualityTarget, ReceiverFeedback,
    ceiling_fps, defended_fps, pinned_target,
};
use lumepeer_media::capture::{CaptureController, Frame, InputInjector, PixelFormat};
use lumepeer_media::decode::{DecodedFrame, DecoderHandle};
use lumepeer_media::encode::{
    EncodedFrame, EncoderConfig, EncoderKind, VideoCodec, VideoEncoder, select_encoder,
    software_av1,
};
use lumepeer_media::error::MediaError;
use lumepeer_media::playout::AudioPlayer;
use lumepeer_media::scale::{cursor_for_picture, fit_within, fit_within_budget, scale_to_percent};
use lumepeer_net::{
    PeerConnection, STREAM_ACKS, STREAM_MIC, accept_media_stream, decode_frame_ack,
    encode_frame_ack, open_media_stream,
};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::media_stats::{ArrivalStats, EncodeReport, EncodeStats, MediaStatsSnapshot};

/// Microseconds in a second, for turning a frame rate into a delay.
const MICROS_PER_SEC: u64 = 1_000_000;

/// Permille, for turning a share of frames into the integer the wire carries.
const PERMILLE: u64 = 1_000;

/// Bits in a byte, for turning received bytes into kilobits per second.
const BITS_PER_BYTE: u64 = 8;

/// Longest the encode loop waits for an acknowledgement before looking again
/// (ADR 0139). Only a fallback: an acknowledgement, or the end of the stream
/// that carries them, wakes it at once.
const ACK_HOLD_TIMEOUT: Duration = Duration::from_millis(100);

/// Host side: what the actor may say to a running encode loop, and what the
/// loop says back (§11).
///
/// A shared cell rather than a channel for most of it, because none of that is
/// a stream of events: a keyframe request that arrives twice before the next
/// frame is still one keyframe, a receiver report supersedes the one before
/// it, and the current target is a fact the actor reads whenever the UI asks.
/// The cursor is the exception and is a real channel — every shape matters,
/// and one dropped is one the guest draws with until the next change.
///
/// Nothing here authorizes anything: the actor has already decided that this
/// peer holds a live `view` grant before an encode loop exists at all (§2.3).
#[derive(Debug, Clone)]
pub struct EncodeControl {
    /// Who this loop is feeding, so its cursor updates can be named.
    peer: NodeId,
    /// Where cursor shapes go. `None` while the guest has not asked for the
    /// separate channel, which is also what keeps a loop that will never send
    /// one from reading the cursor at all.
    cursors: Option<mpsc::Sender<(NodeId, CursorShapeData)>>,
    /// Raised by the actor when a guest asked for a keyframe *and* the request
    /// passed the host's own `KEYFRAME_MIN_INTERVAL_MS` budget. Cleared by the
    /// loop when it acts on it.
    keyframe: Arc<AtomicBool>,
    /// The guest's newest receiver report, with the moment it landed. Taken by
    /// the loop; the timestamp is what lets it tell "the guest is quiet right
    /// now" from "this guest never reports at all".
    feedback: Arc<Mutex<Option<(ReceiverFeedback, Instant)>>>,
    /// What the loop is encoding at right now, for the connection-quality
    /// panel of §18. Written by the loop, read by the actor.
    target: Arc<Mutex<QualityTarget>>,
    /// The quality preset the guest picked, as a percentage of this host's
    /// captured size, if it picked one (§11; D7,
    /// docs/bugs/13-stream-resolution.md task 2). Turned into the whole
    /// quality target by `lumepeer_media::abr::pinned_target`: while it is
    /// set, the adaptive controller does not move any of the three knobs,
    /// because a preset and a ladder both driving the picture is what a
    /// person sees as the quality changing on its own.
    manual_cap: Arc<Mutex<Option<u32>>>,
    /// The frame rate the same preset names, if the guest sent one
    /// (ADR 0136). Pinned with the preset, and never above the session's own
    /// ceiling: a guest asking for 144 from a 60 Hz host gets 60.
    fps_cap: Arc<Mutex<Option<u8>>>,
    /// The picture size the guest said it will draw, in its own device
    /// pixels, if it asked for one (§11; ADR 0060). `None` is a guest that
    /// never asked, which is the only case that still gets the
    /// `MAX_PICTURE_PIXELS` ceiling of ADR 0018 - it may be decoding into
    /// the RGBA slot of §11.3, and that slot is what the ceiling is for.
    size_cap: Arc<Mutex<Option<(u32, u32)>>>,
    /// Whether this session currently holds the `secure_desktop` grant
    /// (ADR 0049). Written by the actor whenever the host flips the grant
    /// (`Network::on_set_grant`), read by the loop before every attempt to
    /// serve a secure-desktop frame instead of the honest "can't see this"
    /// message — so a revoke reaches the very next frame without the loop
    /// ever touching `lumepeer-core` itself (§2.3).
    secure_desktop_allowed: Arc<AtomicBool>,
    /// Whether the loop is, right now, actually serving secure-desktop
    /// pixels for this session — distinct from the grant above the same way
    /// `recording_active` is distinct from the `recording` grant (§17).
    /// Written by the loop, read by the actor for the host's own
    /// non-removable indicator (ADR 0049).
    secure_desktop_active: Arc<AtomicBool>,
    /// Whether the host's own capture is, right now, blocked behind the
    /// secure desktop — the capturer's report, with no grant in it.
    ///
    /// Distinct from `secure_desktop_active` above, which additionally
    /// requires the `secure_desktop` *viewing* grant, because the two answer
    /// different questions. "Is the guest being shown these pixels" decides
    /// the indicator; "can an ordinary `SendInput` reach the desktop at all"
    /// decides where a guest's click has to be routed (ADR 0057), and that
    /// one is a fact about this machine that no grant changes. Keying the
    /// routing off the viewing flag meant a session without the viewing
    /// grant sent every click to the in-session injector, where `SendInput`
    /// answered `ERROR_ACCESS_DENIED` and the event vanished
    /// (`docs/bugs/15-secure-desktop-capture.md`).
    secure_desktop_blocked: Arc<AtomicBool>,
    /// Raised by the loop once the picture's stream is open (ADR 0137).
    /// Every other stream of the session waits for it: a guest takes the
    /// first stream it accepts as the picture, and the picture's stream
    /// carries no tag that would let it tell otherwise.
    video_stream_open: Arc<watch::Sender<bool>>,
    /// The size of the last picture the loop captured, before any reduction
    /// (ADR 0141). Written by the loop, read by the actor when it chooses
    /// this peer's codec again: software AV1 is chosen only for a picture of
    /// at most 1080p, and this is how the actor learns what the screen is.
    captured_size: Arc<Mutex<Option<(u32, u32)>>>,
    /// Whether the guest picked this loop's encoder by hand (ADR 0143). Set
    /// by the actor before the loop starts. A picked software AV1 encoder is
    /// kept whatever the screen size or the frame time: the loop's own way
    /// out of software AV1 is a redial into what the host would choose, and
    /// the host would choose the same pick again.
    encoder_pinned: Arc<AtomicBool>,
    /// What the guest has acknowledged of the picture's stream, which the
    /// loop waits on rather than queueing frames the link cannot carry yet
    /// (ADR 0139). Fed by [`spawn_guest_streams`].
    acks: Arc<FrameAcks>,
}

impl EncodeControl {
    /// A control surface for `peer`, with the cursor channel on only when the
    /// guest said it will draw the cursor itself (§11; `FEATURE_CURSOR_SHAPE`).
    #[must_use]
    pub fn new(peer: NodeId, cursors: Option<mpsc::Sender<(NodeId, CursorShapeData)>>) -> Self {
        Self {
            acks: Arc::new(FrameAcks::default()),
            peer,
            cursors,
            keyframe: Arc::new(AtomicBool::new(false)),
            feedback: Arc::new(Mutex::new(None)),
            target: Arc::new(Mutex::new(QualityTarget::default())),
            manual_cap: Arc::new(Mutex::new(None)),
            fps_cap: Arc::new(Mutex::new(None)),
            size_cap: Arc::new(Mutex::new(None)),
            // Deny until told otherwise: the actor seeds this from the
            // session's live `secure_desktop` grant the moment it accepts the
            // media connection, and moves it again on every `on_set_grant`
            // (ADR 0049). A control that were to start permissive would be a
            // window in which the core has not been consulted at all.
            secure_desktop_allowed: Arc::new(AtomicBool::new(false)),
            secure_desktop_active: Arc::new(AtomicBool::new(false)),
            secure_desktop_blocked: Arc::new(AtomicBool::new(false)),
            video_stream_open: Arc::new(watch::channel(false).0),
            captured_size: Arc::new(Mutex::new(None)),
            encoder_pinned: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Marks this loop's encoder as the guest's own pick (ADR 0143).
    pub(crate) fn pin_encoder(&self) {
        self.encoder_pinned.store(true, Ordering::Relaxed);
    }

    /// Whether the guest picked this loop's encoder by hand (ADR 0143).
    fn encoder_pinned(&self) -> bool {
        self.encoder_pinned.load(Ordering::Relaxed)
    }

    /// The size of the last picture this session's loop captured, if it has
    /// captured one (ADR 0141).
    #[must_use]
    pub fn captured_size(&self) -> Option<(u32, u32)> {
        *self
            .captured_size
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Records the size of the screen this session shows: the display's own
    /// size when a loop starts, each captured picture's after that, and —
    /// from the actor — what the stream this one replaced last knew, so the
    /// size survives a chain of redials on a screen that never changes.
    pub(crate) fn note_captured(&self, size: (u32, u32)) {
        *self
            .captured_size
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(size);
    }

    /// Where the guest's acknowledgements of the picture go (ADR 0139).
    #[must_use]
    pub fn acks(&self) -> Arc<FrameAcks> {
        Arc::clone(&self.acks)
    }

    /// Resolves once the picture's stream is open on the media connection
    /// (ADR 0137): `false` when the session ended without ever opening it.
    pub fn video_stream_opened(&self) -> impl Future<Output = bool> + Send + 'static {
        let mut open = self.video_stream_open.subscribe();
        async move { open.wait_for(|open| *open).await.is_ok() }
    }

    /// Whether this session carries the cursor on its own channel.
    #[must_use]
    pub const fn cursor_channel(&self) -> bool {
        self.cursors.is_some()
    }

    /// Hands one changed shape to the actor, dropping it rather than stalling
    /// the encode loop when the actor is busy.
    fn send_cursor(&self, shape: CursorShapeData) {
        if let Some(cursors) = &self.cursors
            && cursors.try_send((self.peer, shape)).is_err()
        {
            tracing::debug!("dropping a cursor shape: the actor is backed up");
        }
    }

    /// Asks the loop for an intra frame on its next encode.
    pub fn request_keyframe(&self) {
        self.keyframe.store(true, Ordering::Relaxed);
    }

    /// Hands the loop what the guest says it received.
    pub fn report(&self, feedback: ReceiverFeedback) {
        *self
            .feedback
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((feedback, Instant::now()));
    }

    /// What the loop is encoding at right now.
    #[must_use]
    pub fn target(&self) -> QualityTarget {
        *self
            .target
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Takes a pending keyframe request, if there is one.
    fn take_keyframe_request(&self) -> bool {
        self.keyframe.swap(false, Ordering::Relaxed)
    }

    /// Takes the newest receiver report, if one has not been consumed yet.
    fn take_feedback(&self) -> Option<(ReceiverFeedback, Instant)> {
        self.feedback
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    /// Publishes what the loop settled on.
    fn publish(&self, target: QualityTarget) {
        *self
            .target
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = target;
    }

    /// The guest's chosen preset right now, as a scale percentage, if any
    /// (§11; D7).
    #[must_use]
    pub fn manual_cap(&self) -> Option<u32> {
        *self
            .manual_cap
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Sets the guest's chosen preset, replacing whatever was there.
    ///
    /// Returns whether this actually changed the preset, which is what the
    /// actor uses to decide whether a keyframe is owed: a request that
    /// repeats the value already in effect has nothing new to draw.
    pub fn set_manual_cap(&self, cap: Option<u32>) -> bool {
        let mut current = self
            .manual_cap
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let changed = *current != cap;
        *current = cap;
        changed
    }

    /// The frame rate the guest's preset names right now, if any (ADR 0136).
    #[must_use]
    pub fn fps_cap(&self) -> Option<u8> {
        *self
            .fps_cap
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Sets the frame rate the guest's preset names, replacing whatever was
    /// there.
    pub fn set_fps_cap(&self, cap: Option<u8>) {
        *self
            .fps_cap
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = cap;
    }

    /// The picture size the guest asked for right now, if any (§11; ADR 0060).
    #[must_use]
    pub fn size_cap(&self) -> Option<(u32, u32)> {
        *self
            .size_cap
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Sets the picture size the guest asked for, replacing whatever was
    /// there.
    ///
    /// Returns whether this actually changed it, for the same reason
    /// [`Self::set_manual_cap`] does: a request repeating the size already in
    /// effect has nothing new to draw, and a keyframe is the most expensive
    /// frame in the stream to spend on nothing.
    pub fn set_size_cap(&self, cap: Option<(u32, u32)>) -> bool {
        let mut current = self
            .size_cap
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let changed = *current != cap;
        *current = cap;
        changed
    }

    /// The actor calls this whenever `secure_desktop` moves on this session
    /// (ADR 0049). Revoking it must reach the loop before its next attempt,
    /// which is exactly what a plain atomic store gives for free.
    pub fn set_secure_desktop_allowed(&self, allowed: bool) {
        self.secure_desktop_allowed
            .store(allowed, Ordering::Relaxed);
        if !allowed {
            // A revoke stops the *attempt*; the loop's own next iteration
            // clears `secure_desktop_active` once it notices, but the host's
            // indicator must not keep claiming this is happening in the
            // meantime.
            self.secure_desktop_active.store(false, Ordering::Relaxed);
        }
    }

    /// Whether the loop may currently try the secure-desktop path.
    fn secure_desktop_allowed(&self) -> bool {
        self.secure_desktop_allowed.load(Ordering::Relaxed)
    }

    /// Records whether the loop is, right now, actually serving
    /// secure-desktop pixels for this session (ADR 0049).
    fn set_secure_desktop_active(&self, active: bool) {
        self.secure_desktop_active.store(active, Ordering::Relaxed);
    }

    /// Host side: whether this session's guest is currently seeing the
    /// secure desktop, for the non-removable indicator (ADR 0049, §17).
    #[must_use]
    pub fn secure_desktop_active(&self) -> bool {
        self.secure_desktop_active.load(Ordering::Relaxed)
    }

    /// Records what the capturer just reported about the secure desktop,
    /// grant or no grant.
    fn set_secure_desktop_blocked(&self, blocked: bool) {
        self.secure_desktop_blocked
            .store(blocked, Ordering::Relaxed);
    }

    /// Host side: whether this machine's capture is currently behind the
    /// secure desktop, which is what decides that an input event has to go
    /// through the helper rather than through `SendInput` (ADR 0057).
    #[must_use]
    pub fn secure_desktop_blocked(&self) -> bool {
        self.secure_desktop_blocked.load(Ordering::Relaxed)
    }
}

/// Bytes of the fixed header every `view_cursor` response carries:
/// `seq:u32 | width:u16 | height:u16 | hotspot_x:u16 | hotspot_y:u16`.
pub const CURSOR_RESPONSE_HEADER_BYTES: usize = 12;

/// Guest side: the host's cursor as the view window needs it (§11).
///
/// `seq` is a counter, not a timestamp: the window polls with the one it has
/// and gets pixels back only when the host has since announced a different
/// shape. A cursor is at most `MAX_CURSOR_SHAPE_PIXELS`, but it changes every
/// time a pointer crosses a text field, and re-serializing it into every poll
/// would be a second video channel for a picture nobody asked to move.
#[derive(Debug, Clone)]
pub struct CursorFeed {
    /// Bumped on every shape the host announces; starts at 1, so 0 is "the
    /// window has seen nothing yet" and can never collide with a real shape.
    pub seq: u32,
    /// The newest shape, in the premultiplied BGRA of §11.
    pub shape: CursorShapeData,
}

/// Serializes a cursor for `view_cursor`.
///
/// Pixels are omitted when `since_seq` already names the current shape, and
/// the whole body is just the header when the host has announced none — which
/// is what tells the window to draw nothing, because on that host the cursor
/// is still in the picture.
#[must_use]
pub fn encode_cursor_response(cursor: Option<&CursorFeed>, since_seq: u32) -> Vec<u8> {
    let Some(cursor) = cursor else {
        return vec![0u8; CURSOR_RESPONSE_HEADER_BYTES];
    };
    let mut out = Vec::with_capacity(CURSOR_RESPONSE_HEADER_BYTES + cursor.shape.rgba.len());
    out.extend_from_slice(&cursor.seq.to_le_bytes());
    out.extend_from_slice(&cursor.shape.width.to_le_bytes());
    out.extend_from_slice(&cursor.shape.height.to_le_bytes());
    out.extend_from_slice(&cursor.shape.hotspot_x.to_le_bytes());
    out.extend_from_slice(&cursor.shape.hotspot_y.to_le_bytes());
    if cursor.seq != since_seq {
        out.extend_from_slice(&cursor.shape.rgba);
    }
    out
}

/// Guest side: what the media receiver has to tell the actor, because it can
/// only be said on the *control* channel the actor alone owns (§11).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MediaReport {
    /// What this receiver saw over the last `ABR_FEEDBACK_INTERVAL_MS`.
    /// `rtt_ms` is not in here: the media task has no round trip of its own
    /// to measure, and the actor already has the control channel's.
    Feedback {
        /// Share of frames the decoder could not turn into a picture, in
        /// permille.
        loss_permille: u16,
        /// Media bytes that arrived in the window, as kilobits per second.
        goodput_kbps: u32,
    },
    /// The decoder has nothing to decode against: it just started, or it
    /// failed on a frame that referenced one it never saw.
    KeyframeNeeded,
}

/// One [`CaptureController`] shared by the actor and every encode loop.
///
/// Shared rather than owned by one loop because the controller *is* the
/// "capture only with a viewer" rule of §8.1: `add_viewer`/`remove_viewer` are
/// taken on the actor's thread the moment a grant or a revoke happens, while
/// the loops only ever ask it for frames.
pub type SharedCapture = Arc<Mutex<CaptureController>>;

/// Locks the shared controller, recovering from a poisoned mutex.
///
/// A panic in one encode loop must not make the host unable to *stop*
/// capturing — refusing to unlock here would leave the screen being captured
/// with no way to revoke, which is the exact opposite of §2.4's "a failure
/// degrades towards safety".
pub fn lock_capture(capture: &SharedCapture) -> MutexGuard<'_, CaptureController> {
    capture.lock().unwrap_or_else(|poisoned| {
        tracing::warn!("recovering the capture controller from a poisoned lock");
        poisoned.into_inner()
    })
}

/// Host side: every monitor this host can capture, in the order
/// [`CaptureTarget::Display`](lumepeer_media::capture::CaptureTarget::Display)
/// indexes (§11 `MonitorsList`; ADR 0028).
///
/// # Errors
/// [`lumepeer_media::MediaError::CaptureUnavailable`] when the platform
/// cannot enumerate its displays; the caller refuses the announcement rather
/// than sending a list that would not survive a `MonitorSelect`.
pub fn host_monitors() -> lumepeer_media::error::Result<Vec<lumepeer_media::capture::HostMonitor>> {
    lumepeer_media::capture::host_monitors()
}

/// Host side: how many displays this host can capture.
///
/// # Errors
/// Same as [`host_monitors`].
pub fn host_display_count() -> lumepeer_media::error::Result<usize> {
    lumepeer_media::capture::host_display_count()
}

/// What this host can actually do about producing a picture, as far as it
/// knows so far (§18: a missing backend is announced, not silently degraded
/// into a screen that stays blank forever).
///
/// Two facts with two different lifetimes. Whether a capture backend exists
/// is settled at startup by what this build was compiled with and what
/// platform it runs on, so it is known before anyone connects. An encoder, by
/// contrast, is only ever built inside a session, so its absence can only be
/// learned the first time a guest asks for one — until then `can_encode` is
/// the honest "nothing has said otherwise yet".
///
/// Read by the `network_status` IPC command: the operator who is about to
/// share their screen finds out on their own machine, instead of the guest
/// discovering it a reconnect window later as a generic "connection lost".
#[derive(Debug, Default)]
pub struct MediaHealth {
    capture_missing: AtomicBool,
    encoder_missing: AtomicBool,
}

impl MediaHealth {
    /// Health of a host that has a capture backend and has not yet had an
    /// encoder fail on it.
    #[must_use]
    pub fn healthy() -> Self {
        Self::default()
    }

    /// Health of a host whose platform gave no capture backend at all.
    #[must_use]
    pub fn without_capture() -> Self {
        Self {
            capture_missing: AtomicBool::new(true),
            encoder_missing: AtomicBool::new(false),
        }
    }

    /// Records a fault a session just ran into.
    ///
    /// `SecureDesktopActive` is deliberately not recorded here: unlike the
    /// other two, it is not a fact about this host's platform or build — it
    /// is expected to clear on its own, and a future guest must not be
    /// refused a session over a UAC prompt that has since closed
    /// (`docs/bugs/11-uac-degradation.md`). `CaptureDenied` is not recorded
    /// for the same reason: someone at this host can grant capture, and the
    /// next session must ask the operating system again (ADR 0110). Nor is
    /// `EncoderFailed`: this host does have an encoder, and the next session
    /// builds a fresh one rather than being refused over this one (ADR 0135).
    pub fn record(&self, reason: MediaUnavailableReason) {
        match reason {
            MediaUnavailableReason::NoCaptureBackend => {
                self.capture_missing.store(true, Ordering::Relaxed);
            }
            MediaUnavailableReason::NoEncoder => {
                self.encoder_missing.store(true, Ordering::Relaxed);
            }
            MediaUnavailableReason::SecureDesktopActive
            | MediaUnavailableReason::CaptureDenied
            | MediaUnavailableReason::EncoderFailed => {}
        }
    }

    /// Whether this host has a screen-capture backend.
    #[must_use]
    pub fn can_capture(&self) -> bool {
        !self.capture_missing.load(Ordering::Relaxed)
    }

    /// Whether this host has, as far as it has been asked, a video encoder.
    #[must_use]
    pub fn can_encode(&self) -> bool {
        !self.encoder_missing.load(Ordering::Relaxed)
    }

    /// The reason to announce, if this host cannot produce a picture.
    ///
    /// Capture first: with no backend the encoder is never even reached, so
    /// reporting the encoder there would name the wrong cause.
    #[must_use]
    pub fn fault(&self) -> Option<MediaUnavailableReason> {
        if !self.can_capture() {
            Some(MediaUnavailableReason::NoCaptureBackend)
        } else if !self.can_encode() {
            Some(MediaUnavailableReason::NoEncoder)
        } else {
            None
        }
    }
}

/// The host's media side as one value: the shared capture controller, what is
/// known about whether this machine can produce a picture at all, and — on the
/// one platform that cannot build the two apart — the matching injector.
///
/// They travel together because they are decided together: the same
/// `platform_backend()` call that picks the controller's backend is what tells
/// the host it has none, and on the Wayland portal path it is also the only
/// call that can produce an injector for the session capture just negotiated
/// (ADR 0010).
#[derive(Debug)]
pub struct HostMedia {
    /// Controller every encode loop pulls frames from.
    pub capture: SharedCapture,
    /// What this host knows about its own ability to produce a picture.
    pub health: Arc<MediaHealth>,
    /// Injector paired with `capture`, on platforms where input has to come
    /// from the same session as the pixels. `None` everywhere else, which
    /// leaves the actor building one lazily on the first input event (§18).
    pub injector: Option<Box<dyn InputInjector>>,
    /// Where this host stands on encoding AV1 in software (ADR 0141):
    /// [`software_av1::readiness`] — the build, the processor, the hardware
    /// probe and this host's own measurement — on every real host. A seam so
    /// a test can run the software AV1 path without depending on how fast an
    /// unoptimised test build happens to measure.
    pub software_av1: fn() -> software_av1::Readiness,
    /// Starts this host's one software AV1 measurement, soon after the actor
    /// starts (ADR 0143): [`software_av1::start_measurement`] on every real
    /// host, and nothing in a test, which has no use for six seconds of
    /// encoding.
    pub measure_software_av1: fn() -> bool,
}

/// What an encode loop reports back when it cannot produce a picture at all.
///
/// The loop holds only the peer's `rd/media/1` connection; the control stream
/// that has to carry `MediaUnavailable` belongs to the actor, and so does the
/// decision about whether that peer speaks the message at all. So the fault
/// travels back to the actor rather than being written from here.
pub type MediaFault = (NodeId, MediaUnavailableReason);

/// Bytes of the per-frame header on the media wire: keyframe flag plus the
/// capture timestamp the decoder copies back onto the picture.
const MEDIA_PAYLOAD_HEADER_BYTES: usize = 9;

/// Serializes one encoded frame for the media channel.
///
/// Deliberately not `postcard`: the bitstream is already the payload and
/// copying it through a serializer once per picture would cost exactly what
/// §15's latency budget does not have.
///
/// Public because a session agent writes exactly these bytes into the frame
/// mapping (ADR 0085 §1). That is the whole point of it being one function:
/// the agent produces the payload, the privileged host copies it onto the wire
/// without opening it, and there is no second place where a keyframe flag and
/// a timestamp are laid out. A host that parsed a bitstream would be a
/// `LocalSystem` process decoding attacker-influenced bytes.
#[must_use]
pub fn encode_media_payload(frame: &EncodedFrame) -> Vec<u8> {
    let mut out = Vec::with_capacity(MEDIA_PAYLOAD_HEADER_BYTES + frame.data.len());
    out.push(u8::from(frame.keyframe));
    out.extend_from_slice(&frame.timestamp_us.to_le_bytes());
    out.extend_from_slice(&frame.data);
    out
}

/// Parses a media payload, or `None` if the peer sent something malformed.
///
/// Returns rather than panics: this is untrusted input on a network path
/// (§21).
fn decode_media_payload(bytes: &[u8]) -> Option<EncodedFrame> {
    if bytes.len() <= MEDIA_PAYLOAD_HEADER_BYTES {
        return None;
    }
    let (header, data) = bytes.split_at(MEDIA_PAYLOAD_HEADER_BYTES);
    let mut timestamp = [0u8; 8];
    timestamp.copy_from_slice(header.get(1..9)?);
    Some(EncodedFrame {
        keyframe: header.first().copied()? != 0,
        timestamp_us: u64::from_le_bytes(timestamp),
        data: data.to_vec(),
    })
}

/// Frames the guest's bitstream queue holds before it stops believing the
/// window is draining it.
///
/// Two seconds at the default frame rate. A window that is merely busy
/// catches up inside this; a window that has stopped answering is not going
/// to, and holding its backlog forever would trade the picture for memory.
const BITSTREAM_QUEUE_FRAMES: usize = 64;

/// ...and the byte ceiling on the same queue, for the case the frames are
/// keyframes rather than the small inter frames the count above assumes.
const BITSTREAM_QUEUE_BYTES: usize = 8 * 1024 * 1024;

/// How long [`BitstreamFeed::take`] waits for a frame before answering with
/// the header alone.
///
/// Not a frame interval and not a poll rate: the call returns the instant a
/// frame exists. This is only the ceiling that keeps `status`, the `input`
/// grant and the recording indicator refreshing on a session where the
/// picture has stopped — the three things that must reach the window
/// precisely when no frames are arriving.
pub const BITSTREAM_POLL_TIMEOUT_MS: u64 = 250;

/// Who turns this session's bitstream into pictures.
///
/// Decided by the view window, on its first call, by which command it uses —
/// nothing here guesses. The window is the only side that knows whether its
/// own `WebView` has a usable `VideoDecoder`, and it can change its mind later
/// (a decoder that fails repeatedly falls back by simply going back to
/// `view_next_frame`), so this is a cell the newest caller wins rather than a
/// setting fixed at startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodePath {
    /// No window has asked yet. Frames are queued for whoever does; nothing
    /// is decoded, and in particular the sandboxed worker process is not
    /// started for a session that may never need it.
    Undecided,
    /// The window decodes the bitstream itself, in the `WebView` (ADR 0058).
    Native,
    /// The sandboxed worker of §11.3 decodes to RGBA and the window polls for
    /// pixels.
    Worker,
}

impl DecodePath {
    const fn code(self) -> u8 {
        match self {
            Self::Undecided => 0,
            Self::Native => 1,
            Self::Worker => 2,
        }
    }

    const fn from_code(code: u8) -> Self {
        match code {
            1 => Self::Native,
            2 => Self::Worker,
            _ => Self::Undecided,
        }
    }
}

/// Encoded frames on their way from the media receiver to a view window that
/// decodes them itself (ADR 0058).
///
/// The alternative this exists to avoid is the one the pipeline used to have:
/// decode to RGBA in a worker process, then hand the *pixels* to the `WebView`
/// over IPC. One 1080p picture is 8 MiB, and moving 8 MiB through `WebView2`'s
/// IPC costs well over a hundred milliseconds — per frame, before anything is
/// drawn. The same picture as H.264 is a few tens of kilobytes, so sending the
/// bitstream instead is not an optimisation of that path, it is three orders
/// of magnitude less work.
///
/// A queue rather than the single slot the RGBA path uses, and that is
/// forced: RGBA pictures are independent, so keeping only the newest is
/// exactly right, while an inter frame is meaningless without the frames it
/// references. Dropping one silently would corrupt everything after it until
/// the next intra frame.
#[derive(Debug, Default)]
pub struct BitstreamFeed {
    queue: Mutex<BitstreamQueue>,
    ready: tokio::sync::Notify,
    path: std::sync::atomic::AtomicU8,
    /// What arrived, for the view window's statistics overlay. Kept here
    /// because this is the one object the media receiver and the window's IPC
    /// both already hold, whichever side decodes.
    stats: Mutex<ArrivalStats>,
}

#[derive(Debug, Default)]
struct BitstreamQueue {
    frames: std::collections::VecDeque<EncodedFrame>,
    bytes: usize,
    /// Whether the stream the window is holding was broken since it last
    /// looked — frames were dropped, or the media connection was redialled.
    /// The window has to throw its decoder state away and wait for an intra
    /// frame; nothing else can put it back in step.
    desync: bool,
}

impl BitstreamFeed {
    /// Who is decoding this session right now.
    pub fn path(&self) -> DecodePath {
        DecodePath::from_code(self.path.load(Ordering::Relaxed))
    }

    /// Records the choice the calling window just made by which command it
    /// used.
    pub fn choose(&self, path: DecodePath) {
        self.path.store(path.code(), Ordering::Relaxed);
    }

    /// Queues one encoded frame and wakes whatever is waiting for it.
    pub fn push(&self, frame: EncodedFrame) {
        {
            let mut queue = self.lock();
            queue.bytes = queue.bytes.saturating_add(frame.data.len());
            queue.frames.push_back(frame);
            if queue.frames.len() > BITSTREAM_QUEUE_FRAMES || queue.bytes > BITSTREAM_QUEUE_BYTES {
                // Nobody is draining this. Everything queued behind the
                // overflow is undecodable anyway once a frame is missing, so
                // the honest answer is to say so once and start clean.
                queue.frames.clear();
                queue.bytes = 0;
                queue.desync = true;
            }
        }
        self.ready.notify_waiters();
    }

    /// Marks the stream broken: the window must reset its decoder and the
    /// host must be asked for an intra frame.
    pub fn desync(&self) {
        {
            let mut queue = self.lock();
            queue.frames.clear();
            queue.bytes = 0;
            queue.desync = true;
        }
        self.ready.notify_waiters();
    }

    /// Everything queued, waiting up to `timeout` for the first frame.
    ///
    /// Returns the frames in order and whether the stream was broken since
    /// the previous call. An empty answer is normal and is what keeps the
    /// window's status and grant flags live while the picture is still.
    pub async fn take(&self, timeout: Duration) -> (Vec<EncodedFrame>, bool) {
        let deadline = Instant::now() + timeout;
        loop {
            // Registered before the queue is inspected, or a frame pushed
            // between the two would be waited out to the timeout.
            let notified = self.ready.notified();
            if let Some(batch) = self.drain() {
                return batch;
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return (Vec::new(), false);
            };
            if tokio::time::timeout(remaining, notified).await.is_err() {
                return self.drain().unwrap_or_default();
            }
        }
    }

    /// Everything queued right now, or `None` when there is nothing to say.
    fn drain(&self) -> Option<(Vec<EncodedFrame>, bool)> {
        let mut queue = self.lock();
        if queue.frames.is_empty() && !queue.desync {
            return None;
        }
        let desync = std::mem::take(&mut queue.desync);
        queue.bytes = 0;
        Some((queue.frames.drain(..).collect(), desync))
    }

    fn lock(&self) -> MutexGuard<'_, BitstreamQueue> {
        self.queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Forgets everything measured: a new media stream has started, and the
    /// host's capture clock with it.
    pub fn restart_stats(&self) {
        *self.lock_stats() = ArrivalStats::default();
    }

    /// One frame arrived from the host, `bytes` long on the wire.
    pub fn note_arrival(&self, frame: &EncodedFrame, bytes: usize) {
        self.lock_stats()
            .record(Instant::now(), frame.timestamp_us, bytes, frame.keyframe);
    }

    /// The latest reading of the media connection's QUIC path.
    pub fn note_path(&self, path: Option<lumepeer_net::PathSnapshot>) {
        self.lock_stats().set_path(path);
    }

    /// Everything measured so far, for the statistics overlay.
    pub fn stats(&self) -> MediaStatsSnapshot {
        self.lock_stats().snapshot(Instant::now())
    }

    fn lock_stats(&self) -> MutexGuard<'_, ArrivalStats> {
        self.stats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Bytes of the header [`encode_chunk_response`] always emits.
///
/// 8 through minor 10; ADR 0067 appended one codec byte for minor 11 (§11).
pub const CHUNK_RESPONSE_HEADER_BYTES: usize = 9;

/// Bytes of the per-frame header inside a chunk response.
pub const CHUNK_FRAME_HEADER_BYTES: usize = 13;

/// Flags byte bit: the stream was broken since the previous chunk, so the
/// window must reset its decoder and wait for an intra frame.
pub const VIEW_FLAG_DESYNC: u8 = 0b0000_0100;

/// Serializes one batch of encoded frames for `view_next_chunk`.
///
/// Layout, little endian:
/// `status:u8 | flags:u8 | count:u16 | reserved:u32 | codec:u8`, then `count`
/// frames of `flags:u8 | timestamp_us:u64 | length:u32 | bitstream`.
///
/// The `status`/`flags` header is the same contract
/// [`encode_view_response`] carries and for the same reasons: the pipeline's
/// health, the live `input` grant (§8.1) and the host's own recording
/// statement (§2.2) all have to ride every answer, including the empty ones a
/// still screen produces. `codec` rides the same way, for the same reason
/// (ADR 0067): a session's negotiated codec can only change alongside a full
/// decoder reset (like `desync`), so the window has to see it on every
/// answer rather than fetch it once and risk missing a change.
#[must_use]
pub fn encode_chunk_response(
    status: ViewStatus,
    input: bool,
    recording: bool,
    frames: &[EncodedFrame],
    desync: bool,
    codec: u8,
) -> Vec<u8> {
    let payload: usize = frames
        .iter()
        .map(|f| CHUNK_FRAME_HEADER_BYTES + f.data.len())
        .sum();
    let mut out = Vec::with_capacity(CHUNK_RESPONSE_HEADER_BYTES + payload);
    out.push(status.code());
    out.push(
        if input { VIEW_FLAG_INPUT } else { 0 }
            | if recording { VIEW_FLAG_RECORDING } else { 0 }
            | if desync { VIEW_FLAG_DESYNC } else { 0 },
    );
    out.extend_from_slice(
        &u16::try_from(frames.len())
            .unwrap_or(u16::MAX)
            .to_le_bytes(),
    );
    out.extend_from_slice(&0u32.to_le_bytes());
    out.push(codec);
    for frame in frames {
        out.push(u8::from(frame.keyframe));
        out.extend_from_slice(&frame.timestamp_us.to_le_bytes());
        out.extend_from_slice(
            &u32::try_from(frame.data.len())
                .unwrap_or(u32::MAX)
                .to_le_bytes(),
        );
        out.extend_from_slice(&frame.data);
    }
    out
}

/// What the guest's view window is showing right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewStatus {
    /// The session is granted but no picture has arrived yet.
    Waiting,
    /// Frames are flowing.
    Live,
    /// The media connection or the decoder failed and the single recovery pass
    /// of the connection-health policy is running. Non-blocking: the last
    /// picture stays on screen underneath.
    Reconnecting,
    /// The recovery pass elapsed without a frame. Terminal.
    Failed,
    /// The host said it has no screen-capture backend, so this session will
    /// never carry a picture. Terminal, and distinct from `Failed`: nothing
    /// was lost and nothing is worth retrying (§18).
    NoCapture,
    /// The host said it has no video encoder. Terminal, same as `NoCapture`.
    NoEncoder,
    /// The host's Windows capture is blocked by a secure desktop (lock
    /// screen, UAC prompt or fast user switch) and is retrying on its own.
    /// Not terminal, and not `Reconnecting`: the media connection itself is
    /// fine, and the picture underneath is only stale, not lost
    /// (`docs/bugs/11-uac-degradation.md`).
    SecureDesktop,
    /// The host's operating system refused it screen capture (ADR 0110).
    /// Terminal, same as `NoCapture`, with text that says who can fix it.
    CaptureDenied,
    /// The host's encoder refused every frame it was given until the host
    /// gave up (ADR 0135). Terminal, same as `NoEncoder`.
    EncoderFailed,
}

impl From<MediaUnavailableReason> for ViewStatus {
    fn from(reason: MediaUnavailableReason) -> Self {
        match reason {
            MediaUnavailableReason::NoCaptureBackend => Self::NoCapture,
            MediaUnavailableReason::NoEncoder => Self::NoEncoder,
            MediaUnavailableReason::SecureDesktopActive => Self::SecureDesktop,
            MediaUnavailableReason::CaptureDenied => Self::CaptureDenied,
            MediaUnavailableReason::EncoderFailed => Self::EncoderFailed,
        }
    }
}

impl ViewStatus {
    /// Wire value carried in the first byte of the IPC frame response.
    ///
    /// Only ever append: this index is `apps/desktop/src/view-window.ts`'s
    /// `STATUS_BY_CODE` position, a byte of the protocol
    /// (`docs/bugs/11-uac-degradation.md`).
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Waiting => 0,
            Self::Live => 1,
            Self::Reconnecting => 2,
            Self::Failed => 3,
            Self::NoCapture => 4,
            Self::NoEncoder => 5,
            Self::SecureDesktop => 6,
            Self::CaptureDenied => 7,
            Self::EncoderFailed => 8,
        }
    }

    /// Whether the pipeline behind this status has stopped for good, so the
    /// guest has nothing left to wait for.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Failed
                | Self::NoCapture
                | Self::NoEncoder
                | Self::CaptureDenied
                | Self::EncoderFailed
        )
    }
}

/// Single-slot contents of one view: the newest picture and the health of the
/// pipeline that produced it.
#[derive(Debug, Clone)]
pub struct ViewSlot {
    /// Current pipeline health.
    pub status: ViewStatus,
    /// Newest decoded picture, if any has arrived at all.
    pub frame: Option<DecodedFrame>,
}

impl ViewSlot {
    /// Slot of a view whose window just opened.
    #[must_use]
    pub const fn waiting() -> Self {
        Self {
            status: ViewStatus::Waiting,
            frame: None,
        }
    }
}

/// Builds the slot `view_next_frame` should actually serialize for one poll.
///
/// `since_us` is the timestamp of the picture the caller already has, or 0
/// for none — the same "no picture" sentinel [`encode_view_response`] itself
/// uses. `status`/`input` must ride on every poll regardless (an overlay
/// transition or a lowered grant must never be missed), but the pixel
/// payload is dropped when it would just be the picture the caller already
/// painted: a guest polling faster than the video updates (its `tick` loop
/// runs on `requestAnimationFrame`, the host encodes at a fixed, often
/// slower, cadence) would otherwise pay a multi-megabyte re-serialization
/// for nothing on most polls.
#[must_use]
pub fn slot_for_poll(current: &ViewSlot, since_us: u64) -> ViewSlot {
    let unchanged = since_us != 0
        && current
            .frame
            .as_ref()
            .is_some_and(|f| f.timestamp_us == since_us);
    ViewSlot {
        status: current.status,
        frame: if unchanged {
            None
        } else {
            current.frame.clone()
        },
    }
}

/// Bytes of the header [`encode_view_response`] always emits.
pub const VIEW_RESPONSE_HEADER_BYTES: usize = 18;

/// Flags byte bit: the session's `input` grant is live right now.
pub const VIEW_FLAG_INPUT: u8 = 0b0000_0001;
/// Flags byte bit: the host says it is recording this session (§17).
pub const VIEW_FLAG_RECORDING: u8 = 0b0000_0010;

/// Serializes a slot for `view_next_frame`'s binary IPC response.
///
/// Layout, little endian: `status | flags | width | height | timestamp_us |
/// RGBA8 pixels`. Binary rather than JSON because a 1080p picture is ~8 MB and
/// base64-ing it per frame would dominate the frame budget of §15.
///
/// The flags byte rides along on every frame instead of being fetched once at
/// window load. `input` has to, because the grant is live: a later
/// `session_grant` that lowers the role must be able to take the guest's input
/// listeners away again (§8.1). `recording` has to for the same reason from
/// the other direction — the indicator §2.2 requires cannot be a thing the
/// window was told once and might now be wrong about.
#[must_use]
pub fn encode_view_response(slot: &ViewSlot, input: bool, recording: bool) -> Vec<u8> {
    let frame = slot.frame.as_ref();
    let pixels = frame.map_or(&[][..], |f| f.data.as_slice());
    let mut out = Vec::with_capacity(VIEW_RESPONSE_HEADER_BYTES + pixels.len());
    out.push(slot.status.code());
    out.push(
        if input { VIEW_FLAG_INPUT } else { 0 } | if recording { VIEW_FLAG_RECORDING } else { 0 },
    );
    out.extend_from_slice(&frame.map_or(0, |f| f.width).to_le_bytes());
    out.extend_from_slice(&frame.map_or(0, |f| f.height).to_le_bytes());
    out.extend_from_slice(&frame.map_or(0, |f| f.timestamp_us).to_le_bytes());
    out.extend_from_slice(pixels);
    out
}

/// Window label of the view onto `peer_label`.
#[must_use]
pub fn window_label(peer_label: &str) -> String {
    format!("view-{peer_label}")
}

/// What a view window has in it (ADR 0101, ADR 0124).
///
/// The session and the role are the same whichever it is; what changes is
/// whether the guest dials `rd/media/1` at all, and which page the window
/// loads. Only [`Self::Screen`] has a picture, and only a window with a
/// picture ever has input or the keyboard grab of ADR 0090 behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ViewSurface {
    /// The host's screen, with the toolbar and everything it opens.
    #[default]
    Screen,
    /// A shell and nothing else (ADR 0101).
    Terminal,
    /// The file manager and nothing else (ADR 0124).
    Files,
}

/// Which of a guest's windows onto one host a shell belongs to (ADR 0131).
///
/// A session's shells share one channel and one queue of things to tell the
/// guest. With the session's own window and a terminal window beside it both
/// asking for shells, each has to be told only about the ones it opened — a
/// poll that drained the other's output would leave it with a shell that
/// never prints anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum TerminalWindow {
    /// The session's own window: the terminal panel of a screen, or the whole
    /// window of a session that is a shell and nothing else (ADR 0101).
    #[default]
    Session,
    /// A terminal window opened beside a session that already has a window
    /// of its own, instead of a second connect to the same host (ADR 0131).
    Beside,
}

impl ViewSurface {
    /// Whether this window shows the host's picture, which is the same
    /// question as whether the guest dials `rd/media/1` for it: a host starts
    /// its encode loop when it accepts that connection and at no other
    /// moment, so a surface without a picture is a host that encodes nothing.
    #[must_use]
    pub const fn has_picture(self) -> bool {
        matches!(self, Self::Screen)
    }
}

/// How the actor opens and closes the guest's remote-view window.
///
/// The seam that keeps this crate free of any idea what a window is
/// (ADR 0085 §1): the runtime says when one should exist and what is in it, a
/// front end says how one is made. `apps/desktop/src-tauri/src/view_windows.rs`
/// holds the production implementation, built from the `AppHandle` that
/// `spawn_actor` receives; the actor's own tests drive the full grant/revoke
/// cycle through a stand-in and never start a webview at all.
pub trait ViewWindows: std::fmt::Debug + Send + Sync {
    /// Opens the view window `label` onto `peer_label`.
    ///
    /// `host_label` is the same host's *stable* pseudonym — the one the
    /// remembered-hosts list is keyed by, which `peer_label` deliberately is
    /// not (it is re-salted every run so a guest cannot be correlated across
    /// them). The window needs both: every IPC command names the session by
    /// `peer_label`, and the picture it keeps for the connection list has to
    /// be findable tomorrow, under the only name that survives a restart.
    ///
    /// `surface` is what the window has in it: the host's screen, or a shell
    /// (ADR 0101) or the file manager (ADR 0124) with no picture at all — the
    /// same session and the same role, opened by a guest that never dialled
    /// `rd/media/1`. It arrives here rather than being inferred from `input`
    /// because the two are independent: a screen session may be granted no
    /// input, and a window without a picture is never given any.
    fn open(
        &self,
        label: &str,
        peer_label: &str,
        host_label: &str,
        input: bool,
        surface: ViewSurface,
    );
    /// Closes the view window `label`, if it is open.
    fn close(&self, label: &str);
    /// Host side: puts the always-on-top session bar up, or takes it down.
    ///
    /// Idempotent, and called on every actor turn whose active-session count
    /// changed. The bar is the host's own controls — who is connected, and
    /// the revoke — kept reachable while the main window is minimized or
    /// away in the tray; §2.2's "the person at this machine can see what is
    /// happening" stops being true the moment the only surface that says so
    /// is behind a taskbar button.
    fn set_host_bar(&self, visible: bool);
    /// Whether anybody is in front of this host to answer a consent dialog
    /// (ADR 0085 §2).
    ///
    /// Asked of this trait rather than carried as a flag on the actor because
    /// this is the seam that *is* the host's own screen: an implementation
    /// that can put a window in front of somebody is the definition of a host
    /// with a person at it. A session-0 host answers
    /// [`HostAttendance::Unattended`] until its session agent attaches, which
    /// is the same moment its indicator goes up.
    ///
    /// Re-read per handshake, never cached: somebody signing in while a
    /// session is already running changes the answer, and the next guest to
    /// arrive gets the dialog the one before it could not have been shown.
    fn attendance(&self) -> HostAttendance;
    /// Whether this host's own screen is the secure desktop right now, for a
    /// host that has no encode loop to notice it (ADR 0088 §1).
    ///
    /// A desktop client learns this per media session, from its own capture
    /// failing with `SecureDesktopActive`, and answers `false` here. A
    /// session-0 host serving the logon screen has no capture of its own to
    /// fail, and this is where it says so — so that a guest's keystroke on
    /// that screen is gated by `secure_desktop_input` exactly as it is on a
    /// UAC prompt (ADR 0057, ADR 0061).
    fn on_secure_desktop(&self) -> bool;
    /// Whether nobody at all is signed in to this machine (ADR 0088 §3).
    ///
    /// Narrower than [`attendance`](Self::attendance): a signed-in desktop
    /// whose owner walked away is unattended, and still has an indicator on it
    /// for them to come back to. A machine at its logon screen has no desktop
    /// to put one on, and an admission to it is audited as its own kind.
    fn nobody_signed_in(&self) -> bool;
}

/// [`ViewWindows`] that does nothing, for driving the actor without a webview.
///
/// Test-only: the shipped binary always has an `AppHandle`, and a build that
/// silently opened no window would be a way to run a session the user cannot
/// see, which is exactly what §2.1 forbids.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct DetachedViewWindows;

#[cfg(test)]
impl ViewWindows for DetachedViewWindows {
    fn open(
        &self,
        label: &str,
        _peer_label: &str,
        _host_label: &str,
        _input: bool,
        _surface: ViewSurface,
    ) {
        tracing::debug!(window = %label, "no webview attached: not opening a view window");
    }

    fn close(&self, _label: &str) {}

    fn set_host_bar(&self, visible: bool) {
        tracing::debug!(visible, "no webview attached: not moving the host bar");
    }

    /// Attended: these tests drive the consent dialog, so the actor they
    /// build has to behave as a host somebody could answer at. A detached
    /// implementation that claimed otherwise would silently move every test
    /// onto ADR 0085's credential-only path.
    fn attendance(&self) -> HostAttendance {
        HostAttendance::Attended
    }

    fn on_secure_desktop(&self) -> bool {
        false
    }

    fn nobody_signed_in(&self) -> bool {
        false
    }
}

/// Shared slot the actor swaps a [`crate::recorder::SessionRecorder`] into
/// while an encode loop is running (§17). `None` records nothing.
pub type SharedRecorder = Arc<std::sync::Mutex<Option<Arc<crate::recorder::SessionRecorder>>>>;

/// Host side: capture, encode and write frames until the last viewer leaves.
///
/// Returns as soon as the controller refuses a frame — which is exactly what
/// `remove_viewer` on the last viewer makes it do — so there is no separate
/// "stop capturing" signal to keep in sync with the grant state (§8.1).
///
/// `recorder` is the recording slot of §17: whatever sits in it when a frame
/// has been written also receives that frame, so starting or stopping a
/// recording mid-session never restarts the pipeline.
///
/// `codec` is the caller's already-settled choice (§11; ADR 0067) — the
/// intersection of what the guest advertised understanding with what this
/// host can actually encode right now, computed once before this loop starts
/// and never revisited: a mid-session codec change is not supported, so a
/// redial simply starts a fresh loop with whatever the actor decides then.
#[allow(
    clippy::too_many_arguments,
    reason = "codec (ADR 0067) is the eighth: a struct just to carry these \
              past the one call site that assembles them would be indirection \
              with no second caller to justify it"
)]
pub fn spawn_encode_loop(
    connection: PeerConnection,
    capture: SharedCapture,
    recorder: SharedRecorder,
    tag: String,
    codec: VideoCodec,
    peer: NodeId,
    faults: mpsc::Sender<MediaFault>,
    control: EncodeControl,
) -> JoinHandle<()> {
    spawn_encode_loop_with(
        connection,
        capture,
        recorder,
        tag,
        codec,
        peer,
        faults,
        control,
        select_encoder,
    )
}

/// [`spawn_encode_loop`] with the building of its encoder handed in, so a test
/// can put an encoder that refuses every frame behind the real loop
/// (ADR 0135). The builder is called again when a preset rebuilds the
/// encoder for its frame rate (ADR 0136).
#[allow(
    clippy::too_many_lines,
    reason = "one uninterrupted pass over one frame — capture, scale, encode, \
              write, record, adapt — and every split would put a step of it \
              behind a call that hides the order the steps must happen in"
)]
#[allow(
    clippy::too_many_arguments,
    reason = "the same eight as `spawn_encode_loop`, and the builder its \
              encoder comes from"
)]
pub(crate) fn spawn_encode_loop_with(
    connection: PeerConnection,
    capture: SharedCapture,
    recorder: SharedRecorder,
    tag: String,
    codec: VideoCodec,
    peer: NodeId,
    faults: mpsc::Sender<MediaFault>,
    control: EncodeControl,
    build_encoder: impl Fn(EncoderConfig) -> lumepeer_media::Result<Box<dyn VideoEncoder>>
    + Send
    + 'static,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let writer = match open_media_stream(&connection).await {
            Ok(writer) => writer,
            Err(error) => {
                tracing::warn!(peer = %tag, %error, "cannot open the media stream");
                return;
            }
        };
        // Audio may open its stream now and be accepted second (ADR 0137).
        control.video_stream_open.send_replace(true);
        // The write goes on its own task, one frame deep (ADR 0059).
        //
        // It used to be the last step of this loop, which made the frame
        // budget the *sum* of capture, scale, encode and network rather than
        // the largest of them - and worse, a link that was momentarily slow
        // stalled the capture of the next picture, so the delay compounded
        // instead of being absorbed. With the write behind a one-slot channel
        // the loop finds out about a busy link by not being able to reserve a
        // slot, which is both the correct back-pressure signal and the moment
        // to skip a *source* frame rather than queue a stale encoded one.
        let (frames_tx, frames_rx) = mpsc::channel::<WriteJob>(1);
        // Held so the writer dies with this loop: it owns the media stream,
        // and a writer outliving the encoder would keep the session's stream
        // open with nothing behind it.
        let _writer_task = AbortOnDrop(spawn_media_writer(writer, frames_rx, tag.clone()));
        // As fast as the host's display refreshes, up to ENCODE_MAX_FPS, and
        // the encoder is told the same figure — it budgets each frame by it
        // (ADR 0136). Asked once: a display-mode or monitor change mid-session
        // keeps the rate the session started with until the next redial. On
        // a blocking thread because on X11 the question is a new connection
        // to the server.
        let shared = Arc::clone(&capture);
        let display_mode =
            tokio::task::spawn_blocking(move || lock_capture(&shared).current_display_mode())
                .await
                .ok()
                .flatten();
        let max_fps = ceiling_fps(display_mode.map(|mode| mode.refresh_hz));
        // Built for the rate of a preset the guest has already named. Built
        // for the display's rate instead, it was rebuilt for the preset's a
        // frame later: two keyframes where one does, and on a slow link's
        // first seconds the second alone held the first picture back for
        // seconds (ADR 0144).
        let built_fps = if control.manual_cap().is_some() {
            control.fps_cap().map_or(max_fps, |fps| fps.min(max_fps))
        } else {
            max_fps
        };
        let mut encoder = match build_encoder(EncoderConfig {
            codec,
            fps: built_fps,
            ..EncoderConfig::default()
        }) {
            Ok(encoder) => encoder,
            Err(error) => {
                // §18: no silent degradation. The log alone was not enough —
                // it left the guest waiting out the whole reconnect window for
                // a frame that could never come, and then blaming the
                // connection. The fault goes back to the actor, which tells
                // both this host's own UI and the guest what is actually
                // wrong (docs/adr/0024).
                tracing::warn!(peer = %tag, %error, "no encoder available: this session stays blank");
                let _ = faults.send((peer, MediaUnavailableReason::NoEncoder)).await;
                return;
            }
        };
        // Software AV1 is chosen for the 30 fps preset and runs at it,
        // whatever the display could do (ADR 0141); the encoder has already
        // held its own budget there. Everything paced off `max_fps` below —
        // the ladder, a preset's rate — inherits the same ceiling.
        let software_av1 = encoder.kind() == EncoderKind::SoftwareAv1;
        // The way out of software AV1 below — a stream that ends so the
        // guest's redial gets what the host would choose — is only for the
        // host's own choice. A guest's pick would come straight back
        // (ADR 0143).
        let chosen_av1 = software_av1 && !control.encoder_pinned();
        let max_fps = if software_av1 {
            max_fps.min(SOFTWARE_AV1_MAX_FPS)
        } else {
            max_fps
        };
        // The screen's own size, known before a single frame is taken where
        // the platform can say (ADR 0141). A software AV1 stream for a screen
        // larger than it is chosen for ends here, before it has taken a
        // picture at all: the guest redials, the actor reads the size from
        // here, and the H.264 stream it gets takes the snapshot below.
        //
        // Only while no frame has said otherwise: a display mode is the
        // CRTC's, and an X11 monitor scaled with `xrandr --scale` or split
        // with `--setmonitor` captures less than that. Read over a size the
        // frames had already reported, it ended every software AV1 stream
        // of a 1080p capture, and the H.264 stream's frames then put the
        // real size back for the next preset to choose AV1 from again. A
        // mode switched since is caught by the first frame below instead.
        if let Some(mode) = display_mode
            && control.captured_size().is_none()
        {
            control.note_captured((mode.width, mode.height));
        }
        if chosen_av1
            && let Some(size) = control.captured_size()
            && picture_pixels(size) > SOFTWARE_AV1_MAX_PIXELS
        {
            tracing::info!(
                peer = %tag,
                width = size.0,
                height = size.1,
                "the screen is larger than software AV1 is chosen for; ending this stream so the guest redials into H.264"
            );
            return;
        }
        // A loop that replaces another starts on a capture that is already
        // running and only reports changes, so on a still screen its guest
        // would wait for something to move before seeing anything. Take the
        // screen as it is now, for this loop alone: the capture is shared,
        // and a picture left for the next poll went to whichever viewer's
        // loop polled first (ADR 0141). The first tick encodes it instead of
        // polling.
        let shared = Arc::clone(&capture);
        let mut snapshot = tokio::task::spawn_blocking(move || lock_capture(&shared).snapshot())
            .await
            .ok()
            .flatten();
        // Whether this host keeps up with it, window by window (ADR 0141).
        let mut software_av1_watch = SoftwareAv1Watch::default();
        // The frame rate the encoder was built for, which a preset can move
        // (ADR 0136).
        let mut encoder_fps = built_fps;
        // Three knobs now, not one: the controller walks bitrate, then frame
        // rate, then picture scale (ADR 0037). The loop's own pacing is where
        // the frame rate lives, so `interval` is derived from the target
        // rather than fixed for the session.
        let mut abr = AbrController::with_max_fps(max_fps);
        let mut target = abr.target();
        control.publish(target);
        let mut interval = frame_interval(target.fps);
        // When the guest last told this side what it actually received. A
        // guest that reports is the authority on its own link; without one —
        // an older peer, or one that has gone quiet — the only congestion
        // signal left is the host's own [`Backlog`], which is what ADR 0015
        // settled for and ADR 0059 made honest.
        let mut last_report: Option<Instant> = None;
        let feedback_stale = Duration::from_millis(ABR_FEEDBACK_STALE_AFTER_MS);
        // What this side actually put on the wire, over the window the guest
        // measures its own arrivals across. Without it the controller has only
        // the bitrate *ceiling* to compare arrivals against, and a desktop
        // nobody is touching encodes to a fraction of that — which read as
        // congestion on a link with no loss at all, and walked the whole
        // degradation ladder down on an idle LAN (docs/bugs/07-video-quality.md).
        let mut sent = SendRate::default();
        // How many of the recent frame slots the link would not take. This is
        // the host-local congestion signal, and it replaces one that measured
        // how long `write_frame().await` happened to block: on a reliable
        // ordered QUIC stream that duration is the congestion window filling,
        // the OS scheduler, and the encoder's own jitter, all read as packet
        // loss. Two frame intervals of it reported *total* loss, which walked
        // the whole degradation ladder down over a hiccup
        // (docs/bugs/07-video-quality.md).
        let mut backlog = Backlog::default();
        // What this loop spent its time on, summarised into the log every
        // `ENCODE_STATS_PERIOD` for whoever is working out why a session is
        // slow or soft. Read by nothing else.
        let mut encode_stats = EncodeStats::new(Instant::now());
        // Session recording (§17): the actor swaps a recorder into the shared
        // `recorder` slot; each written frame is offered to whatever is in
        // there now, so a mid-session start/stop needs no pipeline restart.
        let mut recorder_now: Option<Arc<crate::recorder::SessionRecorder>>;
        // Whether the guest has already been told about the secure desktop
        // this side is currently stuck behind, so a tick that is still stuck
        // does not re-announce every frame interval
        // (docs/bugs/11-uac-degradation.md). Reset as soon as capture is
        // healthy again, so a later recurrence is announced afresh.
        let mut secure_desktop_notified = false;
        // Earliest instant the next secure-desktop capture attempt may run
        // (ADR 0049); due immediately the first time capture gets stuck.
        let mut secure_desktop_next_attempt_at = Instant::now();
        // Reference clock for the timestamp on a secure-desktop frame — this
        // loop's own start, the same role `Active::started_at` plays inside
        // `WindowsCapturer` for its ordinary frames.
        let media_started = Instant::now();
        // Whether the guest's own picture size (ADR 0060) has been withdrawn
        // because the encoder would not take it (ADR 0074), or is on trial
        // for a refusal that may have had nothing to do with it (ADR 0135).
        let mut guest_size = GuestSize::Honoured;
        // Frames the encoder would not take: how many in a row, for the
        // bound that ends this loop, and how many since the last log line.
        let mut refusals = EncodeRefusals::default();
        // The cursor at the scale the guest draws it: the picture's, which the
        // reductions below can make smaller than the screen (ADR 0138).
        let mut cursor = PictureCursor::default();
        // When the last picture was captured. The next one is not taken
        // before a frame interval after it, and that is the whole of the
        // pacing: a screen that has been still is captured the moment it
        // changes, not at the next tick of a clock that started when it was
        // last looked at (ADR 0139).
        let mut last_capture: Option<Instant> = None;
        let acks = control.acks();
        // Whether this host's encoder makes the frame rate, and how much of
        // the captured picture it is handed while it does not (ADR 0144).
        let mut speed = EncodeSpeed::default();
        // Whether the link carries what a preset pinned, and the bitrate the
        // picture is held under while it does not (ADR 0144).
        let mut link = LinkPressure::default();

        loop {
            if let Some(at) = last_capture {
                sleep_for_the_rest_of(interval, at).await;
            }
            let tick_started = Instant::now();
            if let Some(report) = encode_stats.due(tick_started) {
                log_encode_report(
                    &tag,
                    &report,
                    encoder.kind(),
                    codec,
                    target,
                    control.manual_cap().is_some(),
                    connection.path_snapshot(),
                    acks.best(),
                    acks.slack(interval),
                    speed.cap_percent(),
                    link.cap_kbps(),
                );
            }
            // A preset pins the bitrate, and a link that cannot carry it
            // brings it down until it can; without a preset the adaptive
            // controller has the bitrate, and any cap goes (ADR 0144).
            let link_moved = if control.manual_cap().is_some() {
                link.due(tick_started, target.bitrate_kbps)
            } else {
                link.release().then_some(target.bitrate_kbps)
            };
            if let Some(kbps) = link_moved {
                match encoder.set_bitrate(kbps) {
                    Ok(()) => tracing::info!(
                        peer = %tag,
                        kbps,
                        capped = link.cap_kbps().is_some(),
                        "the link moved the picture's bitrate"
                    ),
                    Err(error) => tracing::warn!(
                        peer = %tag,
                        %error,
                        kbps,
                        "encoder refused the link's bitrate"
                    ),
                }
            }
            // Is the link still carrying what was sent before, longer than it
            // takes when nothing is queued? Then a frame made now would only
            // wait behind them, and the picture the guest sees would be that
            // much older on every click. Wait for the guest to say what it has
            // instead, and capture what is on screen then (ADR 0139).
            if acks.hold(acks.slack(interval), ACK_HOLD_TIMEOUT).await {
                link.held(tick_started.elapsed());
                backlog.skipped();
                encode_stats.held();
                continue;
            }
            // Is the link still on the previous frame? Then it cannot take
            // this one, and the honest thing is to not produce it: an encoded
            // frame may not simply be dropped later (everything after it
            // references it), and queueing it would only mean the guest is
            // shown a picture of a moment that has already passed. Skipping
            // the whole tick - capture included - costs nothing and is what
            // the adaptive controller then reads as pressure.
            //
            // Waited for, up to an interval, not merely looked at (ADR 0144).
            // The writer is woken by the frame handed to it and runs only
            // once this task yields, and after an encode that took longer
            // than the interval nothing between the hand-off and here does:
            // the slot was always still taken, every such frame was followed
            // by a skipped interval, and a host whose encoder needed 50 ms a
            // frame made 11 frames a second where the encoder allowed 19.
            let permit = match frames_tx.try_reserve() {
                Ok(permit) => permit,
                Err(mpsc::error::TrySendError::Full(())) => {
                    match tokio::time::timeout(interval, frames_tx.reserve()).await {
                        Ok(Ok(permit)) => {
                            link.held(tick_started.elapsed());
                            permit
                        }
                        Ok(Err(_closed)) => {
                            tracing::info!(peer = %tag, "media stream ended");
                            return;
                        }
                        Err(_elapsed) => {
                            link.held(tick_started.elapsed());
                            backlog.skipped();
                            encode_stats.skipped();
                            continue;
                        }
                    }
                }
                Err(mpsc::error::TrySendError::Closed(())) => {
                    tracing::info!(peer = %tag, "media stream ended");
                    return;
                }
            };
            // Pick up a recorder the actor may have swapped in (or out) since
            // the last frame; a poisoned lock here only means "record nothing
            // this frame", which is the safe direction.
            recorder_now = recorder
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let shared = Arc::clone(&capture);
            // `next_frame` is a blocking platform call; it must not sit on a
            // tokio worker thread.
            let capture_started = Instant::now();
            let (captured, waits_for_change) = if let Some(frame) = snapshot.take() {
                (Ok(Ok(Some(frame))), false)
            } else {
                match tokio::task::spawn_blocking(move || {
                    let mut capture = lock_capture(&shared);
                    (capture.next_frame(), capture.waits_for_change())
                })
                .await
                {
                    Ok((captured, waits)) => (Ok(captured), waits),
                    Err(error) => (Err(error), false),
                }
            };
            let frame = match captured {
                Ok(Ok(Some(frame))) => {
                    encode_stats.captured(capture_started.elapsed());
                    secure_desktop_notified = false;
                    // Ordinary capture just produced a real frame, so
                    // whatever the secure-desktop path was doing a moment
                    // ago is not happening on this tick (ADR 0049): the
                    // desktop is reachable again, for pictures and for input
                    // alike.
                    control.set_secure_desktop_active(false);
                    control.set_secure_desktop_blocked(false);
                    frame
                }
                // The screen has not changed (§11.1): nothing to send. Also
                // the "just reopened, no pixels yet" answer `WindowsCapturer`
                // gives right after a secure-desktop recovery, so this is
                // where that recovery is noticed too.
                Ok(Ok(None)) => {
                    secure_desktop_notified = false;
                    control.set_secure_desktop_active(false);
                    control.set_secure_desktop_blocked(false);
                    // The cursor changes on a screen that does not: crossing
                    // a text field turns the arrow into an I-beam and
                    // repaints nothing. With the cursor out of the picture no
                    // frame comes for it, so the shape is read here too and
                    // goes out at the size of the last picture sent, rather
                    // than waiting for the next repaint (ADR 0150).
                    if control.cursor_channel()
                        && let Some(shape) = lock_capture(&capture).cursor_shape()
                    {
                        cursor.changed(shape);
                    }
                    if let Some(shape) = cursor.due_for_last_picture() {
                        control.send_cursor(shape);
                    }
                    // A backend that waited for a change before saying "none"
                    // is asked again at once: sleeping out the interval here
                    // is time a change would wait unseen — up to 17 ms of
                    // every 33 at 30 fps (ADR 0139).
                    if !waits_for_change {
                        sleep_for_the_rest_of(interval, tick_started).await;
                    }
                    continue;
                }
                // The secure desktop (lock screen, UAC prompt or fast user
                // switch) is in the foreground; `WindowsCapturer` is already
                // retrying its own reopen on a backoff
                // (`crates/media/src/capture/windows.rs`). This is expected
                // to clear, so the loop keeps running instead of returning —
                // the session stays up, and only the guest is told, once per
                // episode rather than every tick (§18,
                // docs/bugs/11-uac-degradation.md).
                //
                // ADR 0049 extends this arm rather than replacing it: if this
                // session holds the `secure_desktop` grant, try to serve the
                // real thing first. Every failure of that attempt — no
                // grant, service unreachable, the far side's session check
                // refusing this caller, or simply "the secure desktop is not
                // showing anything right now" — falls straight through to
                // `docs/bugs/11-uac-degradation.md`'s unmodified fallback
                // below, exactly as it behaved before this ADR existed.
                Ok(Err(MediaError::SecureDesktopActive(reason))) => {
                    // The ordinary capturer just reported the secure desktop is
                    // in the foreground. That — not "a fresh picture landed this
                    // tick" — is the authoritative signal the input path keys on
                    // to route a guest's clicks and keys to the `Winlogon`
                    // injector (ADR 0057). The *picture* is throttled to
                    // `SECURE_DESKTOP_CAPTURE_INTERVAL_MS` (500 ms), so
                    // `secure_desktop_frame` returns `None` on all but ~1 tick in
                    // N; keying the flag off that None dropped it to false ~97%
                    // of the time, and a click arriving in one of those gaps fell
                    // through to the ordinary in-session injector, whose
                    // `SendInput` cannot reach `Winlogon` and is silently dropped
                    // (docs/bugs/11-uac-degradation.md). Latch it for the whole
                    // episode instead, while the guest is actually being shown
                    // the secure desktop (the viewing grant is on) — input is not
                    // throttled just because the picture is.
                    //
                    // The *routing* flag carries no grant at all: whether an
                    // ordinary `SendInput` can reach the desktop is a fact
                    // about this machine, and a session without the viewing
                    // grant used to send its clicks to an injector that could
                    // only answer `ERROR_ACCESS_DENIED`.
                    control.set_secure_desktop_blocked(true);
                    control.set_secure_desktop_active(control.secure_desktop_allowed());
                    if let Some(frame) = secure_desktop_frame(
                        &control,
                        &mut secure_desktop_next_attempt_at,
                        media_started,
                    )
                    .await
                    {
                        secure_desktop_notified = false;
                        frame
                    } else {
                        if !secure_desktop_notified {
                            tracing::info!(
                                peer = %tag,
                                %reason,
                                "capture blocked by the secure desktop; retrying while it clears"
                            );
                            let _ = faults
                                .send((peer, MediaUnavailableReason::SecureDesktopActive))
                                .await;
                            secure_desktop_notified = true;
                        }
                        sleep_for_the_rest_of(interval, tick_started).await;
                        continue;
                    }
                }
                // The operating system said no: the Wayland portal's dialog
                // was dismissed. Ending the loop alone left the guest
                // redialing a media stream that could never carry a frame
                // (ADR 0110).
                Ok(Err(MediaError::PermissionDenied)) => {
                    tracing::warn!(peer = %tag, "the OS refused screen capture: this session stays blank");
                    let _ = faults
                        .send((peer, MediaUnavailableReason::CaptureDenied))
                        .await;
                    return;
                }
                Ok(Err(error)) => {
                    tracing::info!(peer = %tag, %error, "capture ended: stopping the encode loop");
                    return;
                }
                Err(error) => {
                    tracing::warn!(peer = %tag, %error, "the capture task ended unexpectedly");
                    return;
                }
            };
            last_capture = Some(Instant::now());

            // Two reductions, in this order and for different reasons. The
            // target is whatever the guest's preset pinned, or whatever the
            // adaptive controller settled on when it named none — the two
            // are never combined, because they would be two hands on the same
            // variable (D7, docs/bugs/13-stream-resolution.md). The budget
            // reduction is the hard ceiling of §15 that no choice may exceed,
            // so it goes last and has the final say (ADR 0018). An encoder
            // that cannot make the frame rate is handed less than either asks
            // for (ADR 0144).
            let scale_percent = target.scale_percent.min(speed.cap_percent());
            // Ignored while it is on trial, and for the rest of the session
            // once the encoder has refused it: see [`GuestSize`].
            let size_cap = if guest_size.honoured() {
                control.size_cap()
            } else {
                None
            };

            // The cursor rides its own channel when the guest asked for one,
            // and is read only then: a shape the loop would never send is a
            // platform call made for nothing. `cursor_shape` answers `None`
            // for a cursor that has not changed, so this is one comparison on
            // a steady screen (§11). It is sent once the frame is, at that
            // frame's scale.
            if control.cursor_channel()
                && let Some(shape) = lock_capture(&capture).cursor_shape()
            {
                cursor.changed(shape);
            }
            let captured_size = (frame.width, frame.height);
            control.note_captured(captured_size);
            // Software AV1 is chosen for a picture of at most 1080p (ADR
            // 0141). A screen that turns out larger — one whose platform
            // could not say so before the first frame (Wayland), or a
            // display mode switched mid-session — ends this loop rather than
            // encoding something nobody measured: the guest redials, and the
            // actor, which now knows the size from `note_captured`, gives it
            // H.264. Where the backend can take a snapshot (Windows, X11)
            // the redial's loop takes its own, so this frame is not the
            // guest's last chance at one; on Wayland, which cannot, the
            // H.264 stream waits for the screen to change.
            if chosen_av1 && picture_pixels(captured_size) > SOFTWARE_AV1_MAX_PIXELS {
                tracing::info!(
                    peer = %tag,
                    width = captured_size.0,
                    height = captured_size.1,
                    "the screen is larger than software AV1 is chosen for; ending this stream so the guest redials into H.264"
                );
                return;
            }

            // A guest that just started decoding, or that lost more than it
            // could conceal, has nothing to decode against until an intra
            // frame arrives. The budget on how often this may be asked for is
            // the actor's (§11) — by the time the flag is up, the request has
            // already been through it.
            if control.take_keyframe_request()
                && let Err(error) = encoder.request_keyframe()
            {
                tracing::warn!(peer = %tag, %error, "the encoder refused a keyframe request");
            }

            // Scaling and encoding are both blocking work on the whole
            // picture - two CPU passes over several megabytes, then a
            // hardware MFT round trip or a full software encode - and neither
            // has any more business on a tokio worker thread than
            // `next_frame` above does. The encoder travels with the closure
            // and back.
            let finished = tokio::task::spawn_blocking(move || {
                let scale_started = Instant::now();
                let frame = scale_to_percent(frame, scale_percent);
                // The guest's own box when it named one, the ADR 0018
                // ceiling when it did not - never both. `MAX_PICTURE_PIXELS`
                // sizes the RGBA slot of §11.3, so it binds exactly the
                // guests that still decode through it; applying it to a
                // guest that decodes in its own webview is what turned a
                // 1440p host into a resampled 1080p picture the guest then
                // stretched back out (ADR 0060).
                let frame = match size_cap {
                    Some((width, height)) => fit_within(frame, width, height),
                    None => fit_within_budget(frame),
                };
                let scale_took = scale_started.elapsed();
                let mut encoder = encoder;
                let encode_started = Instant::now();
                let bitstream = encoder.encode(&frame);
                let timing = EncodeTiming {
                    scale: scale_took,
                    encode: encode_started.elapsed(),
                    size: (frame.width, frame.height),
                };
                (encoder, bitstream, timing)
            })
            .await;
            let (bitstream, timing) = match finished {
                Ok((returned, bitstream, timing)) => {
                    encoder = returned;
                    match bitstream {
                        Ok(bitstream) => {
                            // A frame at the picture budget, right after one
                            // at the guest's size was refused: that refusal
                            // was the size's (ADR 0074, ADR 0135).
                            if guest_size == GuestSize::OnTrial {
                                tracing::warn!(
                                    peer = %tag,
                                    "the encoder refused the size this guest asked for; \
                                     the rest of this session runs at the picture budget"
                                );
                            }
                            guest_size = guest_size.after_frame();
                            refusals.took();
                            (bitstream, timing)
                        }
                        Err(error) => {
                            // §18: a size the encoder will not take is not a
                            // reason to send nothing at all. The guest's own
                            // size is the one thing in this pipeline that can
                            // exceed what a hardware encoder negotiates —
                            // measured, both MFTs on the reference machine
                            // refuse every size above 4K while
                            // `MAX_STREAM_PIXELS` now allows 5K (ADR 0074) —
                            // so it is the first thing given up, and the next
                            // frame tries the ADR 0018 budget. Only that frame
                            // going through says the size was the cause.
                            guest_size = guest_size.after_refusal(size_cap.is_some());
                            // §18 again: skipping refused frames without end
                            // left a guest on its one picture for minutes,
                            // told nothing, while this log took thirty lines a
                            // second (ADR 0135). A bounded run, then the guest
                            // is told and the loop ends.
                            if refusals.refused() >= ENCODE_REFUSALS_BEFORE_FAULT {
                                // Software AV1 has H.264 behind it on the
                                // same host (ADR 0141): give it up for this
                                // process and let the guest redial into
                                // that, instead of ending the session.
                                if chosen_av1 {
                                    software_av1::demote(&format!(
                                        "the encoder refused {ENCODE_REFUSALS_BEFORE_FAULT} frames in a row: {error}"
                                    ));
                                    return;
                                }
                                tracing::warn!(
                                    peer = %tag,
                                    %error,
                                    in_a_row = ENCODE_REFUSALS_BEFORE_FAULT,
                                    "the encoder keeps refusing frames: this session stays blank"
                                );
                                let _ = faults
                                    .send((peer, MediaUnavailableReason::EncoderFailed))
                                    .await;
                                return;
                            }
                            if let Some(refused) = refusals.due(Instant::now()) {
                                tracing::warn!(peer = %tag, %error, refused, "encoder refused a frame");
                            }
                            sleep_for_the_rest_of(interval, tick_started).await;
                            continue;
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(peer = %tag, %error, "the encode task ended unexpectedly");
                    return;
                }
            };
            // Whether the encoder makes the frame rate the preset asked for,
            // or the share of it the preset defends (ADR 0144).
            let asked_fps = control.fps_cap().unwrap_or(target.fps);
            let picture_percent = u32::try_from(
                u64::from(timing.size.1) * u64::from(FULL_SCALE_PERCENT)
                    / u64::from(captured_size.1.max(1)),
            )
            .unwrap_or(FULL_SCALE_PERCENT);
            if let Some(cap) = speed.frame(
                Instant::now(),
                timing.scale + timing.encode,
                picture_percent,
                frame_interval(defended_fps(asked_fps, target.fps)),
            ) {
                tracing::info!(
                    peer = %tag,
                    scale_percent = cap,
                    "the encoder's speed moved the picture's size"
                );
            }
            // An encoder's own rate control may decide a picture is worth no
            // bits at all: `openh264` skips frames to hold its bitrate, and
            // hands back an empty one. Nothing to send — on the wire it was a
            // nine-byte header the guest could only discard as malformed and
            // count as lost (ADR 0144).
            if bitstream.data.is_empty() {
                continue;
            }
            if let Some(shape) = cursor.due(captured_size, timing.size) {
                control.send_cursor(shape);
            }
            if bitstream.data.len() > MAX_MEDIA_FRAME_BYTES {
                tracing::warn!(
                    peer = %tag,
                    bytes = bitstream.data.len(),
                    "dropping a frame larger than the media frame bound"
                );
                continue;
            }
            encode_stats.encoded(
                timing.scale,
                timing.encode,
                timing.size,
                bitstream.data.len(),
                bitstream.keyframe,
            );
            // A host that passed its measurement can still fall behind in
            // real use — a game where the measurement had text, the person at
            // the host busy with something heavy. Two windows in a row that
            // cannot hold the frame rate software AV1 was chosen for, and it
            // is off for this process; the guest redials into H.264
            // (ADR 0141).
            if chosen_av1 {
                match software_av1_watch.frame(timing.scale + timing.encode) {
                    SoftwareAv1Verdict::Pending => {}
                    SoftwareAv1Verdict::KeepingUp(p95) => tracing::info!(
                        peer = %tag,
                        p95_ms = %format_args!("{:.1}", p95.as_secs_f64() * 1_000.0),
                        frames = SOFTWARE_AV1_WATCH_FRAMES,
                        "software AV1 frame time (scale and encode, p95)"
                    ),
                    SoftwareAv1Verdict::Behind(p95) => {
                        software_av1::demote(&format!(
                            "a session's p95 frame time stayed at {:.1} ms, over the {} fps interval, for {} windows of {} frames",
                            p95.as_secs_f64() * 1_000.0,
                            SOFTWARE_AV1_MAX_FPS,
                            SOFTWARE_AV1_SLOW_WINDOWS,
                            SOFTWARE_AV1_WATCH_FRAMES,
                        ));
                        return;
                    }
                }
            }
            sent.wrote(bitstream.data.len());
            link.sent(bitstream.data.len());
            backlog.offered();
            // Recording rides the frame the guest is being sent (§17): the
            // container stores the same bitstream, written by the same task
            // that writes the wire, so the two cannot diverge.
            permit.send(WriteJob {
                frame: bitstream,
                recorder: recorder_now.clone(),
            });
            acks.sent(Instant::now());
            // The guest's own measurement wins whenever there is a fresh one;
            // the host-local stand-in only speaks for a link nobody is
            // reporting on (ADR 0015, ADR 0037).
            let measured = match control.take_feedback() {
                Some((mut feedback, at)) => {
                    last_report = Some(at);
                    // The guest reports what arrived; only this side knows
                    // what was offered, and one without the other says
                    // nothing about the link.
                    feedback.sent_kbps = sent.take_kbps();
                    // The guest is the authority on its own link while it is
                    // talking; the local window has nothing to add and must
                    // not carry over into the next silence.
                    backlog.reset();
                    Some(feedback)
                }
                None if last_report.is_none_or(|at| at.elapsed() > feedback_stale) => backlog
                    .due()
                    .map(|loss| backlog_feedback(loss, sent.take_kbps())),
                None => None,
            };
            // A guest that named a preset has already said what it wants the
            // picture to be, and the controller is then not a second opinion
            // to weigh against it — it is a second hand on the same three
            // knobs, and the picture drifts between the two for the whole
            // session (docs/bugs/07-video-quality.md). The measurement above
            // still runs while a preset is pinned: it is what keeps the
            // backlog window and the report clock honest for the moment the
            // guest stops naming one.
            // The preset names its frame rate too (ADR 0136): the most this
            // host delivers for `performance`, fewer and sharper frames for
            // the presets that ask for them — never more than the host has.
            let preset_fps = control.fps_cap().map_or(max_fps, |fps| fps.min(max_fps));
            let preset = pinned_target(control.manual_cap(), preset_fps);
            let settled = match preset {
                Some(pinned) => (pinned != target).then_some(pinned),
                None => measured.and_then(|feedback| abr.on_feedback(feedback)),
            };
            if let Some(next) = settled {
                // Under whatever the link holds the bitrate to (ADR 0144).
                let next_kbps = link
                    .cap_kbps()
                    .map_or(next.bitrate_kbps, |cap| cap.min(next.bitrate_kbps));
                if next.bitrate_kbps != target.bitrate_kbps
                    && let Err(error) = encoder.set_bitrate(next_kbps)
                {
                    tracing::warn!(
                        peer = %tag,
                        %error,
                        target_kbps = next_kbps,
                        "encoder refused a bitrate change"
                    );
                }
                // A preset picked anew is a new tradeoff between size and
                // frames, and the encoder's speed is measured against it
                // afresh (ADR 0144).
                if preset.is_some() {
                    speed.reset();
                }
                if next.fps != target.fps {
                    interval = frame_interval(next.fps);
                }
                // A preset's frame rate has to reach the encoder, not only
                // the pace: it budgets every frame by the rate it was built
                // for, so a 30 fps preset on an encoder built for 60 would
                // spend half its bitrate (ADR 0136). Only a preset rebuilds
                // it — one intra frame, which a preset change asks for anyway
                // — where the ladder's 5 fps steps would pay one a second.
                if preset.is_some() && next.fps != encoder_fps {
                    match build_encoder(EncoderConfig {
                        codec,
                        fps: next.fps,
                        bitrate_kbps: next_kbps,
                    }) {
                        Ok(rebuilt) => {
                            encoder = rebuilt;
                            encoder_fps = next.fps;
                        }
                        Err(error) => tracing::warn!(
                            peer = %tag,
                            %error,
                            fps = next.fps,
                            "the encoder could not be rebuilt for the preset's frame rate"
                        ),
                    }
                }
                target = next;
                control.publish(target);
                tracing::debug!(
                    peer = %tag,
                    bitrate_kbps = target.bitrate_kbps,
                    fps = target.fps,
                    scale_percent = target.scale_percent,
                    "quality target moved"
                );
            }
        }
    })
}

/// Pixels in a picture of `size`.
fn picture_pixels((width, height): (u32, u32)) -> usize {
    usize::try_from(u64::from(width) * u64::from(height)).unwrap_or(usize::MAX)
}

/// What one more frame tells a live software AV1 session about keeping up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SoftwareAv1Verdict {
    /// The window is not full yet.
    Pending,
    /// A window closed within the frame interval; its p95.
    KeepingUp(Duration),
    /// The [`SOFTWARE_AV1_SLOW_WINDOWS`]-th window in a row closed over the
    /// frame interval; the last one's p95.
    Behind(Duration),
}

/// A live software AV1 session's check that this host keeps up (ADR 0141):
/// the p95 of scale-and-encode time over windows of
/// [`SOFTWARE_AV1_WATCH_FRAMES`] encoded frames, against the frame interval
/// at [`SOFTWARE_AV1_MAX_FPS`].
///
/// The interval, the same line the measurement holds the host to before
/// choosing it (ADR 0142). In a session the only question left is whether it
/// can keep the frame rate — and on the reference host a game took 22–27 ms a
/// frame in software AV1 against 31–67 ms in `openh264`, which drops 40% of a
/// game's frames besides, so falling back on anything stricter would hand such
/// a session to the encoder that does worse.
#[derive(Debug, Default)]
struct SoftwareAv1Watch {
    window: Vec<Duration>,
    behind: u32,
}

impl SoftwareAv1Watch {
    /// Records one frame's time.
    fn frame(&mut self, took: Duration) -> SoftwareAv1Verdict {
        self.window.push(took);
        if self.window.len() < SOFTWARE_AV1_WATCH_FRAMES {
            return SoftwareAv1Verdict::Pending;
        }
        self.window.sort_unstable();
        let rank = (self.window.len() * 95).div_ceil(100);
        let p95 = self.window[rank.saturating_sub(1)];
        self.window.clear();
        if p95 > frame_interval(SOFTWARE_AV1_MAX_FPS) {
            self.behind += 1;
        } else {
            self.behind = 0;
        }
        if self.behind >= SOFTWARE_AV1_SLOW_WINDOWS {
            SoftwareAv1Verdict::Behind(p95)
        } else {
            SoftwareAv1Verdict::KeepingUp(p95)
        }
    }
}

/// How long the blocking half of one encode tick spent where, and on what.
#[derive(Debug, Clone, Copy)]
struct EncodeTiming {
    /// Reducing the picture — handing the work to the GPU when the frame is
    /// there (ADR 0139), the whole filter on the CPU otherwise; zero when
    /// nothing needed reducing.
    scale: Duration,
    /// The encoder call itself.
    encode: Duration,
    /// The picture the encoder was handed.
    size: (u32, u32),
}

/// One line in the host's log summarising the last `ENCODE_STATS_PERIOD` of
/// an encode loop, for whoever is working out why a session is slow or soft.
///
/// A period in which nothing was encoded, skipped or held is a still screen,
/// and says nothing worth a line.
#[allow(
    clippy::too_many_arguments,
    reason = "one log line, and each argument is a different source's part of it"
)]
fn log_encode_report(
    tag: &str,
    report: &EncodeReport,
    encoder: EncoderKind,
    codec: VideoCodec,
    target: QualityTarget,
    preset: bool,
    path: Option<lumepeer_net::PathSnapshot>,
    link_best: Option<Duration>,
    link_slack: Duration,
    speed_cap_percent: u32,
    link_cap_kbps: Option<u32>,
) {
    if report.frames == 0 && report.skipped == 0 && report.held == 0 {
        return;
    }
    tracing::info!(
        peer = %tag,
        ?encoder,
        ?codec,
        width = report.width,
        height = report.height,
        fps = report.fps,
        kbps = report.kbps,
        keyframes = report.keyframes,
        skipped = report.skipped,
        held = report.held,
        link_best_ms = ?link_best.map(|best| best.as_millis()),
        link_slack_ms = link_slack.as_millis(),
        capture_ms = %format_args!("{:.1}/{:.1}", report.capture_ms_avg, report.capture_ms_max),
        scale_ms = %format_args!("{:.1}/{:.1}", report.scale_ms_avg, report.scale_ms_max),
        encode_ms = %format_args!("{:.1}/{:.1}", report.encode_ms_avg, report.encode_ms_max),
        target_kbps = target.bitrate_kbps,
        target_fps = target.fps,
        target_scale = target.scale_percent,
        preset,
        speed_cap = speed_cap_percent,
        link_cap_kbps = ?link_cap_kbps,
        rtt_ms = ?path.map(|path| path.rtt.as_millis()),
        cwnd = ?path.map(|path| path.cwnd),
        lost_packets = ?path.map(|path| path.lost_packets),
        congestion_events = ?path.map(|path| path.congestion_events),
        relay = ?path.map(|path| path.relay),
        "media: the last period of the encode loop (ms as avg/max)"
    );
}

/// The pacing delay of one frame at `fps`.
///
/// `max(1)` is not defensive noise: the frame rate is clamped by
/// `ABR_MIN_FPS` upstream, and dividing by a zero that cannot occur would
/// still be a panic in a loop that must not have one.
///
/// In microseconds: whole milliseconds made 60 fps a 16 ms interval and 144
/// a 6 ms one — 62.5 and 166 frames a second.
fn frame_interval(fps: u8) -> Duration {
    Duration::from_micros(MICROS_PER_SEC / u64::from(fps.max(1)))
}

/// Sleeps only what's left of `interval` after `tick_started`, instead of
/// unconditionally sleeping the whole interval on top of however long the
/// tick's own work took — the latter compounds every tick that capture,
/// encode or write is not instant, and real throughput falls under
/// `ENCODE_DEFAULT_FPS` under any load at all.
///
/// On a blocking thread with `std`'s sleep, not `tokio::time::sleep` (ADR
/// 0136). Tokio's timer on Windows wakes on the system clock tick: measured
/// on the reference machine, every sleep from 1 to 11 ms took 15.8 ms and
/// 16 to 23 ms took 31.7, so a loop doing 5 ms of work a tick ran at 21 fps
/// when it targeted 30, and at 32 when it targeted 60. `std` sleeps on a
/// high-resolution waitable timer there — the same requests came back within
/// half a millisecond.
async fn sleep_for_the_rest_of(interval: Duration, tick_started: Instant) {
    if let Some(remaining) = interval.checked_sub(tick_started.elapsed()) {
        let _ = tokio::task::spawn_blocking(move || std::thread::sleep(remaining)).await;
    }
}

/// The branch `docs/bugs/11-uac-degradation.md`'s task 3 prepared, extended
/// rather than replaced (ADR 0049): while capture is stuck behind the secure
/// desktop, try to serve the real thing instead of the honest "can't see
/// this" message, but only when this session actually holds the grant.
///
/// `None` for every reason at once — no grant, not yet due for another try,
/// the service unreachable, the session-binding check on the far side
/// refusing this caller, or a malformed answer — which is exactly what tells
/// the caller to fall through to `docs/bugs/11-uac-degradation.md`'s
/// existing behaviour unchanged.
///
/// Throttled to [`SECURE_DESKTOP_CAPTURE_INTERVAL_MS`] rather than tried
/// every tick: a fresh pipe round trip and a GDI capture on a `LocalSystem`
/// process is a real cost, and the secure desktop is largely static.
/// `next_attempt_at` is advanced whether or not this call actually finds a
/// frame, so a run of refusals cannot turn into a busy loop against the
/// service.
///
/// The pipe round trip and the capture it triggers are blocking calls, so
/// they run on `spawn_blocking` rather than on this task's own worker
/// thread — the same treatment `next_frame` already gets a few lines below
/// wherever this is called from.
async fn secure_desktop_frame(
    control: &EncodeControl,
    next_attempt_at: &mut Instant,
    started: Instant,
) -> Option<Frame> {
    if !control.secure_desktop_allowed() {
        return None;
    }
    let now = Instant::now();
    if now < *next_attempt_at {
        return None;
    }
    *next_attempt_at = now + Duration::from_millis(SECURE_DESKTOP_CAPTURE_INTERVAL_MS);

    let captured =
        tokio::task::spawn_blocking(lumepeer_service::client::capture_secure_desktop_frame)
            .await
            .ok()??;
    let expected_len = usize::try_from(captured.width)
        .ok()?
        .checked_mul(usize::try_from(captured.height).ok()?)?
        .checked_mul(4)?;
    if expected_len == 0 || captured.data.len() != expected_len {
        tracing::warn!(
            width = captured.width,
            height = captured.height,
            len = captured.data.len(),
            "the secure-desktop frame mapping did not agree with its own header; discarding it"
        );
        return None;
    }
    Some(Frame::cpu(
        captured.width,
        captured.height,
        PixelFormat::Bgra8,
        u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
        captured.data,
    ))
}

/// One frame on its way to the wire, and whatever recording was running when
/// it was produced (§17).
#[derive(Debug)]
struct WriteJob {
    frame: EncodedFrame,
    recorder: Option<Arc<crate::recorder::SessionRecorder>>,
}

/// Host side: the only task that touches the media stream (ADR 0059).
///
/// Its whole job is to be the thing that can be slow without the capture and
/// encode stages having to wait for it. A frame arrives, it is written, and if
/// a recording is running the same bytes go into the container - which is also
/// why the recorder rides the job rather than being consulted here: the frame
/// the guest is sent and the frame that is recorded must be the same one, and
/// the actor may swap the recorder at any moment.
fn spawn_media_writer(
    mut writer: lumepeer_net::MediaFrameWriter<iroh::endpoint::SendStream>,
    mut jobs: mpsc::Receiver<WriteJob>,
    tag: String,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(job) = jobs.recv().await {
            if let Err(error) = writer.write_frame(&encode_media_payload(&job.frame)).await {
                tracing::info!(peer = %tag, %error, "media stream ended");
                return;
            }
            if let Some(recorder) = job.recorder.as_ref() {
                recorder.write_video(job.frame.timestamp_us, &job.frame.data);
            }
        }
    })
}

/// How many of the recent frame slots the link would not take.
///
/// The host-local congestion signal of ADR 0015, measured honestly
/// (ADR 0059). The
/// version this replaces timed `write_frame().await` and called anything
/// slower than the frame budget "loss": on a reliable ordered QUIC stream that
/// duration is the congestion window filling, the peer's receive window, the
/// OS scheduler and the encoder's own variance, and two frame intervals of it
/// saturated at *total* loss. The controller then walked bitrate, frame rate
/// and resolution all the way to their floors over a hiccup on a link with no
/// loss at all (docs/bugs/07-video-quality.md).
///
/// What this counts instead is the one thing that unambiguously says the link
/// cannot carry the load: the writer was still busy with the previous frame,
/// so this frame was never produced.
#[derive(Debug)]
struct Backlog {
    started: Instant,
    offered: u32,
    skipped: u32,
}

impl Default for Backlog {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            offered: 0,
            skipped: 0,
        }
    }
}

impl Backlog {
    /// A frame was produced and handed to the writer.
    fn offered(&mut self) {
        self.offered = self.offered.saturating_add(1);
    }

    /// A frame was not produced, because the writer was still busy.
    fn skipped(&mut self) {
        self.skipped = self.skipped.saturating_add(1);
    }

    /// Forgets the current window. Called whenever the guest's own report
    /// takes over, so a window that spans the silence and the report is never
    /// attributed to either.
    fn reset(&mut self) {
        *self = Self::default();
    }

    /// The share of slots the link would not take, once the window has run
    /// its course, and starts the next one in the same call.
    ///
    /// A window, not a per-frame reading, and the same
    /// [`ABR_FEEDBACK_INTERVAL_MS`] the guest measures its own arrivals
    /// across. One frame is not a measurement of anything: the controller
    /// halves the bitrate above `HEAVY_LOSS`, so reporting per frame would
    /// mean a single scheduling hiccup halves the picture quality - which is
    /// the exact failure of the timing-based signal this replaced.
    fn due(&mut self) -> Option<f32> {
        if self.started.elapsed() < Duration::from_millis(u64::from(ABR_FEEDBACK_INTERVAL_MS)) {
            return None;
        }
        let total = self.offered.saturating_add(self.skipped);
        let skipped = self.skipped;
        self.reset();
        // A window with nothing in it is an idle screen, not a congested
        // link. No evidence is not evidence.
        Some(if total == 0 {
            0.0
        } else {
            f64_ratio(skipped, total)
        })
    }
}

/// `skipped / total` as the `f32` [`ReceiverFeedback`] wants.
#[expect(
    clippy::cast_possible_truncation,
    reason = "a ratio of two frame counts, in 0.0..=1.0 by construction"
)]
fn f64_ratio(skipped: u32, total: u32) -> f32 {
    (f64::from(skipped) / f64::from(total)) as f32
}

/// Dresses the host-local congestion measurement in the [`ReceiverFeedback`]
/// shape [`AbrController`] expects (see docs/adr/0015-host-local-abr.md).
///
/// `rtt_ms` has no local equivalent, and the goodput half is deliberately
/// self-cancelling: `goodput_kbps` and `sent_kbps` are the same local rate, so
/// the controller's arrival check cannot fire on a measurement that never
/// crossed the network. Congestion on this path is `loss`, and only `loss`.
fn backlog_feedback(loss: f32, sent_kbps: u32) -> ReceiverFeedback {
    ReceiverFeedback {
        loss,
        rtt_ms: 0,
        goodput_kbps: sent_kbps,
        sent_kbps,
    }
}

/// Host side: what the guest has acknowledged of the picture's stream, and
/// whether the link has room for another frame (§11; ADR 0139).
///
/// A frame written to the media stream is not a frame on its way: QUIC takes
/// it into its send buffer as soon as flow control allows, which is over a
/// megabyte, and sends it when congestion control allows. On a link slower
/// than the encoder that buffer filled, and every click waited behind seconds
/// of picture nobody would see. The guest now says how many frames it has
/// received, and the loop produces a frame only while the oldest one still
/// unacknowledged has not been on its way much longer than the quickest
/// recent one took — the link's own round trip, with nothing queued.
///
/// Inert until an acknowledgement stream opens: a guest that predates
/// ADR 0139 sends none, and its sessions run exactly as they did.
#[derive(Debug, Default)]
pub struct FrameAcks {
    state: Mutex<AckState>,
    /// Woken on every acknowledgement and when the stream ends.
    changed: tokio::sync::Notify,
}

#[derive(Debug, Default)]
struct AckState {
    /// An acknowledgement stream is open: the guest speaks ADR 0139.
    live: bool,
    /// Frames handed to the writer since the picture's stream opened.
    sent: u64,
    /// Frames the guest has said it received, cumulatively.
    acked: u64,
    /// The frames not yet acknowledged, oldest first, with when each was
    /// handed to the writer. Only kept while `live`.
    in_flight: std::collections::VecDeque<(u64, Instant)>,
    /// How long acknowledgements took over the last
    /// [`MEDIA_ACK_BEST_WINDOW_MS`], as a monotonic queue: each entry is
    /// quicker than every one after it, so the front is the minimum.
    best: std::collections::VecDeque<(Instant, Duration)>,
    /// The previous acknowledgement's delay, for [`Self::jitter`].
    last_delay: Option<Duration>,
    /// How much one acknowledgement's delay differs from the one before it,
    /// as a moving average over the last eight or so — RFC 3550's
    /// interarrival jitter, on acknowledgements (ADR 0144). A queue that
    /// builds moves every delay up together and barely moves this; a path
    /// whose round trip swings moves it by the swing.
    jitter: Duration,
}

impl AckState {
    fn note_delay(&mut self, at: Instant, delay: Duration) {
        while self.best.back().is_some_and(|&(_, slower)| slower >= delay) {
            self.best.pop_back();
        }
        self.best.push_back((at, delay));
    }

    fn best(&mut self, now: Instant) -> Option<Duration> {
        let window = Duration::from_millis(MEDIA_ACK_BEST_WINDOW_MS);
        while self
            .best
            .front()
            .is_some_and(|&(at, _)| now.saturating_duration_since(at) > window)
        {
            self.best.pop_front();
        }
        self.best.front().map(|&(_, delay)| delay)
    }
}

impl FrameAcks {
    fn lock(&self) -> MutexGuard<'_, AckState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The guest opened its acknowledgement stream.
    pub fn opened(&self) {
        self.lock().live = true;
    }

    /// The acknowledgement stream ended. Nothing more will be acknowledged,
    /// so nothing is held back any longer either.
    pub fn closed(&self) {
        {
            let mut state = self.lock();
            state.live = false;
            state.in_flight.clear();
        }
        self.changed.notify_waiters();
    }

    /// One frame was handed to the writer at `at`.
    fn sent(&self, at: Instant) {
        let mut state = self.lock();
        state.sent = state.sent.saturating_add(1);
        if state.live {
            let seq = state.sent;
            state.in_flight.push_back((seq, at));
        }
    }

    /// The guest has received `count` frames, as of `at`.
    pub fn acked(&self, count: u64, at: Instant) {
        {
            let mut state = self.lock();
            if count <= state.acked {
                return;
            }
            state.acked = count;
            // Only the newest frame this covers is timed: the older ones were
            // waiting for this acknowledgement as much as for the link.
            let mut newest = None;
            while state
                .in_flight
                .front()
                .is_some_and(|&(seq, _)| seq <= count)
            {
                newest = state.in_flight.pop_front();
            }
            if let Some((_, handed_over)) = newest {
                let delay = at.saturating_duration_since(handed_over);
                state.note_delay(at, delay);
                if let Some(previous) = state.last_delay.replace(delay) {
                    let swing = delay.abs_diff(previous);
                    state.jitter = (state.jitter * 7 + swing) / 8;
                }
            }
        }
        self.changed.notify_waiters();
    }

    /// Whether the link has room for another frame at `now`: nothing is
    /// waiting, nothing has been measured yet, or the oldest frame waiting
    /// has been on its way no more than `slack` longer than the best case.
    fn admits(&self, now: Instant, slack: Duration) -> bool {
        let mut state = self.lock();
        let Some(&(_, oldest)) = state.in_flight.front() else {
            return true;
        };
        let Some(best) = state.best(now) else {
            return true;
        };
        now.saturating_duration_since(oldest) <= best + slack
    }

    /// `false` at once when the link has room for another frame; otherwise
    /// waits for the next acknowledgement, at most `timeout`, and says `true`.
    async fn hold(&self, slack: Duration, timeout: Duration) -> bool {
        // Armed before the check, so an acknowledgement landing between the
        // two is not waited out to the timeout.
        let changed = self.changed.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        if self.admits(Instant::now(), slack) {
            return false;
        }
        let _ = tokio::time::timeout(timeout, changed).await;
        true
    }

    /// The link's best case right now, for the log.
    fn best(&self) -> Option<Duration> {
        self.lock().best(Instant::now())
    }

    /// How far behind its best case the link may fall at a frame interval of
    /// `interval` before frames are held back: [`queue_slack`], widened to
    /// twice the link's own jitter, up to [`MEDIA_QUEUE_JITTER_SLACK_MAX_MS`]
    /// (ADR 0144).
    ///
    /// A link whose round trip varies by a hundred milliseconds from one
    /// frame to the next is not falling behind every time a frame lands late,
    /// and holding the picture back for each of them left such a link idle
    /// most of the time.
    fn slack(&self, interval: Duration) -> Duration {
        let jitter = self.lock().jitter;
        queue_slack(interval)
            .max((jitter * 2).min(Duration::from_millis(MEDIA_QUEUE_JITTER_SLACK_MAX_MS)))
    }
}

/// How far behind its best case the media link may fall at a frame interval
/// of `interval`: two intervals, between [`MEDIA_QUEUE_SLACK_MIN_MS`] and
/// [`MEDIA_QUEUE_SLACK_MAX_MS`] (ADR 0139).
fn queue_slack(interval: Duration) -> Duration {
    (interval * 2).clamp(
        Duration::from_millis(MEDIA_QUEUE_SLACK_MIN_MS),
        Duration::from_millis(MEDIA_QUEUE_SLACK_MAX_MS),
    )
}

/// How much this host has written since the guest's previous report.
///
/// The counterpart of the guest's own arrival window, kept on this side
/// because only this side knows what it offered the link. Reset every time it
/// is read, so each report is compared against the frames it was reporting on.
#[derive(Debug, Default)]
struct SendRate {
    bytes: u64,
    since: Option<Instant>,
}

impl SendRate {
    /// One frame went out.
    fn wrote(&mut self, bytes: usize) {
        self.since.get_or_insert_with(Instant::now);
        self.bytes = self.bytes.saturating_add(bytes as u64);
    }

    /// The rate over the window just ended, and starts the next one.
    ///
    /// `0` for a window with no elapsed time in it, which is what the
    /// controller already reads as "nobody measured this".
    fn take_kbps(&mut self) -> u32 {
        let millis = self.since.map_or(0, |at| {
            u64::try_from(at.elapsed().as_millis()).unwrap_or(u64::MAX)
        });
        let bits = self.bytes.saturating_mul(8);
        *self = Self::default();
        if millis == 0 {
            return 0;
        }
        u32::try_from(bits / millis).unwrap_or(u32::MAX)
    }
}

/// Where the guest's own picture size (ADR 0060) stands with the encoder.
///
/// ADR 0074 gives the size up the first time the encoder refuses a frame,
/// because it is the one thing in this pipeline that can exceed what a
/// hardware encoder negotiates. A refusal does not say why, though, and
/// blaming every first one on the size logged "the encoder refused the size
/// this guest asked for" on a host whose encoder refused everything, and
/// dropped the guest's size over a failure it had no part in (ADR 0135). So
/// the size is withdrawn on trial, and the next frame decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GuestSize {
    /// In effect. A refusal of a frame fitted to it puts it on trial.
    Honoured,
    /// Withdrawn after one refusal, until the next frame, at the picture
    /// budget, says whether that refusal was the size's.
    OnTrial,
    /// The encoder refused it and took the budget: withdrawn for the rest of
    /// the session.
    Refused,
    /// The encoder refused the budget too, so the size was not the cause. In
    /// effect again, and not put on trial again until the encoder takes a
    /// frame.
    Cleared,
}

impl GuestSize {
    /// Whether the picture is fitted to the guest's size, when it named one.
    const fn honoured(self) -> bool {
        matches!(self, Self::Honoured | Self::Cleared)
    }

    /// The encoder refused a frame; `capped` is whether that frame had been
    /// fitted to the guest's size.
    const fn after_refusal(self, capped: bool) -> Self {
        match self {
            Self::Honoured if capped => Self::OnTrial,
            Self::OnTrial => Self::Cleared,
            other => other,
        }
    }

    /// The encoder took a frame.
    const fn after_frame(self) -> Self {
        match self {
            Self::OnTrial => Self::Refused,
            Self::Cleared => Self::Honoured,
            other => other,
        }
    }
}

/// Shortest gap between two log lines about refused frames (ADR 0135).
const ENCODE_REFUSAL_LOG_INTERVAL: Duration = Duration::from_secs(10);

/// Frames the encoder would not take, counted for the two things that depend
/// on them: the run that ends the loop at [`ENCODE_REFUSALS_BEFORE_FAULT`],
/// and the log, which gets one line per [`ENCODE_REFUSAL_LOG_INTERVAL`]
/// rather than one per frame. An encoder that refuses every other frame never
/// reaches the bound, and must not write fifteen lines a second either.
#[derive(Debug, Default)]
struct EncodeRefusals {
    /// Refusals since the encoder last took a frame.
    in_a_row: u32,
    /// Refusals no log line has counted yet.
    unlogged: u32,
    /// When the last line was written.
    logged_at: Option<Instant>,
}

impl EncodeRefusals {
    /// Counts one refusal, and answers how many are now in a row.
    fn refused(&mut self) -> u32 {
        self.in_a_row = self.in_a_row.saturating_add(1);
        self.unlogged = self.unlogged.saturating_add(1);
        self.in_a_row
    }

    /// The encoder took a frame. The run is over; what it left unlogged is
    /// counted by the next line.
    fn took(&mut self) {
        self.in_a_row = 0;
    }

    /// How many refusals the line about to be written stands for, or `None`
    /// while the last one is too recent for another.
    fn due(&mut self, now: Instant) -> Option<u32> {
        if self
            .logged_at
            .is_some_and(|at| now.duration_since(at) < ENCODE_REFUSAL_LOG_INTERVAL)
        {
            return None;
        }
        self.logged_at = Some(now);
        Some(std::mem::take(&mut self.unlogged))
    }
}

/// The host's cursor, kept at the captured surface's own scale, and the sizes
/// it was last sent scaled between (ADR 0138).
///
/// The guest draws the cursor at the scale of the picture it receives, so the
/// shape has to be re-sent when the picture changes size as well as when the
/// cursor changes: a preset picked mid-session halves the picture and leaves
/// an unchanged cursor as large as it was.
#[derive(Debug, Default)]
struct PictureCursor {
    /// The newest shape the capture backend reported.
    shape: Option<CursorShapeData>,
    /// `(captured, picture)` the current shape was last sent for; `None`
    /// while it has not been sent at all.
    sent_for: Option<((u32, u32), (u32, u32))>,
    /// `(captured, picture)` of the last frame that went out, for a shape
    /// that changes while the screen stands still.
    last_frame: Option<((u32, u32), (u32, u32))>,
}

impl PictureCursor {
    /// The capture backend reported a different cursor.
    fn changed(&mut self, shape: CursorShapeData) {
        self.shape = Some(shape);
        self.sent_for = None;
    }

    /// The cursor to send now that a `captured`-sized frame went out as a
    /// `picture`-sized one, or `None` when the guest already has it.
    fn due(&mut self, captured: (u32, u32), picture: (u32, u32)) -> Option<CursorShapeData> {
        self.last_frame = Some((captured, picture));
        let shape = self.shape.as_ref()?;
        if self.sent_for == Some((captured, picture)) {
            return None;
        }
        self.sent_for = Some((captured, picture));
        Some(cursor_for_picture(shape, captured, picture))
    }

    /// The cursor to send while no frame goes out, at the sizes of the last
    /// one that did, or `None` when the guest already has it — or has no
    /// picture yet, whose first frame will carry it.
    fn due_for_last_picture(&mut self) -> Option<CursorShapeData> {
        let (captured, picture) = self.last_frame?;
        self.due(captured, picture)
    }
}

/// Everything the guest's media loop needs to reach one host.
#[derive(Debug, Clone)]
pub struct MediaTarget {
    /// How to reach the host, decided when its invite was read and kept so
    /// this dial takes the same transport the control channel took
    /// (ADR 0080). It carries the address the ticket named, so the media dial
    /// does not depend on discovery having caught up.
    pub dialer: crate::network::HostDialer,
    /// The host being watched. Only ever used to name this loop's own reports
    /// back to the actor; nothing here can act on it.
    pub peer: NodeId,
    /// Where [`MediaReport`]s go. The media loop has no control channel of its
    /// own — the actor owns the only one — so everything this side has to
    /// *say* about reception travels through here (§2.3).
    pub reports: mpsc::Sender<(NodeId, MediaReport)>,
    /// Pseudonymized peer label, for logs (§15).
    pub tag: String,
    /// Decoder worker binary; `None` uses the one next to this executable.
    pub worker: Option<PathBuf>,
    /// Encoded frames for a view window that decodes them itself (ADR 0058).
    ///
    /// Shared with the IPC layer, which is also where the choice between this
    /// and the sandboxed worker is recorded — by the window, on its first
    /// call. Until a window has asked for either, this loop decodes nothing
    /// and only queues.
    pub bitstream: Arc<BitstreamFeed>,
    /// Where the live media connection lands once dialed (§4.1; ADR 0028):
    /// the guest's microphone reads it to open its tagged stream on the
    /// *same* connection, never a second one, and to follow the picture onto
    /// the next one a pass dials (ADR 0147).
    #[allow(
        dead_code,
        reason = "read by the actor through ViewState::media_connection; the cell is shared"
    )]
    pub connection_cell: Arc<std::sync::Mutex<Option<PeerConnection>>>,
    /// The host's sound as the window this loop feeds hears it: every audio
    /// stream a pass accepts reports here (ADR 0147).
    pub audio_in: Arc<AudioMeter>,
}

/// Guest side: dial media, decode, and keep the newest picture in `slot`.
///
/// Implements the connection-health policy: a failed media connection and a
/// crashed decoder both look identical to the user ("video stopped") and
/// neither is a revoke, so neither closes anything on the first failure.
/// One recovery pass, bounded by [`RECONNECT_WINDOW_SECS`], is attempted; a
/// stream that delivers a frame refreshes that budget, so it is a rolling
/// one-shot allowance rather than a lifetime total. Before the first frame
/// ever arrives, a failed pass keeps the slot `Waiting` instead of
/// `Reconnecting`: nothing was connected yet, so nothing was lost.
#[must_use]
pub fn spawn_media_receiver(
    target: MediaTarget,
    slot: Arc<watch::Sender<ViewSlot>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let window = Duration::from_secs(RECONNECT_WINDOW_SECS);
        let backoff = Duration::from_millis(MEDIA_REDIAL_BACKOFF_MS);
        // `None` means "healthy so far": the single recovery pass has not been
        // opened, or a delivered frame closed it again.
        let mut recovery_deadline: Option<Instant> = None;
        // Distinguishes "never got a picture yet" from "had one, then lost
        // it" so the very first dial attempt does not read as a lost
        // connection: a first-attempt hiccup (host still routing the media
        // ALPN, an extra NAT round trip) is completely normal and must stay
        // `Waiting`, not `Reconnecting`.
        let mut ever_live = false;

        loop {
            // Audio rides the very connection `stream_once` dials, so its
            // receiver is started in there, once per pass, and ends with it.
            let produced = stream_once(&target, &slot).await;
            // Whatever the window was holding references frames from a stream
            // that has ended. Saying so is what makes it throw its decoder
            // away and wait for an intra frame instead of painting garbage.
            target.bitstream.desync();
            if slot.is_closed() {
                // The actor tore this view down; nothing left to serve.
                return;
            }
            if produced {
                recovery_deadline = None;
                ever_live = true;
            }

            match recovery_deadline {
                None if ever_live => {
                    tracing::info!(peer = %target.tag, "media stopped: starting one recovery pass");
                    set_status(&slot, ViewStatus::Reconnecting);
                    recovery_deadline = Some(Instant::now() + window);
                }
                None => {
                    tracing::debug!(peer = %target.tag, "still waiting for the first frame");
                    recovery_deadline = Some(Instant::now() + window);
                }
                Some(deadline) if Instant::now() < deadline => {}
                Some(_) => {
                    tracing::warn!(
                        peer = %target.tag,
                        window_secs = RECONNECT_WINDOW_SECS,
                        "the media recovery pass elapsed without a frame"
                    );
                    set_status(&slot, ViewStatus::Failed);
                    return;
                }
            }
            tokio::time::sleep(backoff).await;
        }
    })
}

/// Replaces the slot's status, keeping whatever picture is already there so a
/// "reconnecting" state stays non-blocking over the last frame.
fn set_status(slot: &watch::Sender<ViewSlot>, status: ViewStatus) {
    slot.send_modify(|current| {
        if !current.status.is_terminal() {
            current.status = status;
        }
    });
}

/// Dials `rd/media/1` and accepts the stream the host opens on it.
///
/// `None` for either half failing, which is the ordinary outcome of a host
/// that is not ready yet rather than an error worth reporting: the caller's
/// recovery pass simply tries again.
async fn dial_media(
    target: &MediaTarget,
) -> Option<(
    PeerConnection,
    lumepeer_net::MediaFrameReader<iroh::endpoint::RecvStream>,
)> {
    let connection = match target.dialer.connect(lumepeer_net::ALPN_MEDIA).await {
        Ok(connection) => connection,
        Err(error) => {
            tracing::debug!(peer = %target.tag, %error, "media dial failed");
            return None;
        }
    };
    // Publish the live connection before anything else: the mic toggle must
    // see it the moment a picture can exist (§4.1; ADR 0028).
    *target
        .connection_cell
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(connection.clone());
    match accept_media_stream(&connection).await {
        Ok(reader) => Some((connection, reader)),
        Err(error) => {
            tracing::debug!(peer = %target.tag, %error, "host opened no media stream");
            None
        }
    }
}

/// One media attempt: dial, decode until something fails.
///
/// Returns whether at least one picture reached `slot`, which is what decides
/// if the recovery budget is refreshed.
#[allow(
    clippy::too_many_lines,
    reason = "one pass over one received frame — count, acknowledge, report,               parse, hand to whichever side decodes — in the order it must happen"
)]
async fn stream_once(target: &MediaTarget, slot: &watch::Sender<ViewSlot>) -> bool {
    let Some((connection, mut reader)) = dial_media(target).await else {
        return false;
    };
    target.bitstream.restart_stats();
    target.bitstream.note_path(connection.path_snapshot());
    let path_source = connection.clone();
    // How many frames have arrived on this pass, told to the host as they
    // arrive, so it sends no more than the link carries (ADR 0139).
    let (received_tx, received_rx) = watch::channel(0u64);
    let _acks = AbortOnDrop(spawn_frame_acks(
        connection.clone(),
        received_rx,
        target.tag.clone(),
    ));
    let _audio = spawn_audio_pass(
        connection,
        target.tag.clone(),
        Arc::new(lumepeer_media::playout::platform_player),
        Arc::clone(&target.audio_in),
    );

    // The sandboxed worker is spawned only once there is something to decode:
    // a session that never produces a frame must not leave a decoder process
    // behind (§8.1, §11.3).
    let mut decoder: Option<DecoderHandle> = None;
    let mut produced = false;
    // What this pass reports back to the host every `ABR_FEEDBACK_INTERVAL_MS`:
    // the only two things a receiver on a reliable ordered stream can honestly
    // measure (ADR 0037).
    let mut window = ReceptionWindow::new();
    // Guest-side half of the keyframe budget. The host enforces its own — the
    // one that actually protects it — but a guest that asks politely is a
    // guest whose requests are worth honouring.
    let mut last_keyframe_ask: Option<Instant> = None;

    loop {
        let payload = match reader.read_frame().await {
            Ok(payload) => payload,
            Err(error) => {
                tracing::debug!(peer = %target.tag, %error, "media stream ended");
                return produced;
            }
        };
        // Counted on arrival, before anything is made of it: the host counts
        // what it wrote, and a frame the decoder then refuses still arrived.
        received_tx.send_modify(|count| *count = count.saturating_add(1));
        window.received(payload.len());
        if let Some(report) = window.due() {
            send_report(target, report);
            target.bitstream.note_path(path_source.path_snapshot());
        }
        // A header with no picture is a frame the host's encoder skipped to
        // hold its bitrate, which hosts before ADR 0144 still sent: nothing
        // was lost and nothing is wrong.
        if payload.len() == MEDIA_PAYLOAD_HEADER_BYTES {
            continue;
        }
        let Some(encoded) = decode_media_payload(&payload) else {
            tracing::warn!(peer = %target.tag, "dropping a malformed media payload");
            window.lost();
            continue;
        };
        target.bitstream.note_arrival(&encoded, payload.len());

        // The window decodes for itself, or has not said yet. Either way the
        // bitstream goes straight through: no worker process, no RGBA, no
        // 8 MiB per picture across IPC (ADR 0058).
        if target.bitstream.path() != DecodePath::Worker {
            produced = offer_bitstream(target, slot, encoded, produced);
            // Dropping the slot receiver is how the actor tells this loop the
            // view is gone; on the worker path a failing `slot.send` says so,
            // and this branch never calls it.
            if slot.is_closed() {
                return produced;
            }
            continue;
        }

        let fresh_decoder = decoder.is_none();
        let handle = match decoder.take() {
            Some(handle) => handle,
            None => match spawn_decoder(target.worker.clone()).await {
                Ok(handle) => handle,
                Err(error) => {
                    tracing::warn!(peer = %target.tag, %error, "cannot start the decoder worker");
                    return produced;
                }
            },
        };
        // A decoder that has just started has nothing to decode against: the
        // stream is mid-flight, and what the host is sending right now almost
        // certainly references frames this process never saw (§11).
        if fresh_decoder {
            ask_for_a_keyframe(target, &mut last_keyframe_ask);
        }

        // `decode` blocks on the worker's pipes; it never runs on a tokio
        // worker thread. The handle travels with the closure and back.
        let finished = tokio::task::spawn_blocking(move || {
            let mut handle = handle;
            let result = handle.decode(&encoded);
            (handle, result)
        })
        .await;
        let (handle, result) = match finished {
            Ok(pair) => pair,
            Err(error) => {
                tracing::warn!(peer = %target.tag, %error, "the decode task ended unexpectedly");
                return produced;
            }
        };
        decoder = Some(handle);

        match result {
            Ok(Some(frame)) => {
                // Single slot, overwrite-on-push: the display only ever wants
                // the newest picture.
                if slot
                    .send(ViewSlot {
                        status: ViewStatus::Live,
                        frame: Some(frame),
                    })
                    .is_err()
                {
                    return produced;
                }
                produced = true;
            }
            // A bitstream that does not complete a picture yet is normal.
            Ok(None) => {}
            Err(error) => {
                // The frame is lost as far as this receiver is concerned, and
                // an intra frame is the only thing that can get it back in
                // step. Both facts go back to the host: one drives its
                // adaptation, the other its encoder.
                tracing::warn!(peer = %target.tag, %error, "decoder failed");
                window.lost();
                ask_for_a_keyframe(target, &mut last_keyframe_ask);
                return produced;
            }
        }
    }
}

/// Guest side: tells the host, as frames arrive, how many have (§11;
/// ADR 0139), on a stream of its own on the media connection.
///
/// Opened without asking whether the host understands it: a host that does
/// not skips the stream, and the first write that finds it gone ends this
/// task without touching the picture.
fn spawn_frame_acks(
    connection: PeerConnection,
    mut received: watch::Receiver<u64>,
    tag: String,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut writer =
            match lumepeer_net::open_tagged_media_stream(&connection, STREAM_ACKS).await {
                Ok(writer) => writer,
                Err(error) => {
                    tracing::debug!(peer = %tag, %error, "cannot open the frame acknowledgements");
                    return;
                }
            };
        // Only the newest count matters: one write that covers several
        // frames says the same as one per frame.
        while received.changed().await.is_ok() {
            let count = *received.borrow_and_update();
            if let Err(error) = writer.write_frame(&encode_frame_ack(count)).await {
                tracing::debug!(peer = %tag, %error, "the host takes no frame acknowledgements");
                return;
            }
        }
    })
}

/// Hands one encoded frame to the window that decodes for itself, and
/// returns whether this pass has now delivered a picture (ADR 0058).
///
/// "Delivered" is the recovery budget's word, not the renderer's: it decides
/// whether the media loop treats this pass as healthy. A window cannot start
/// decoding before an intra frame arrives, so the first keyframe is the
/// earliest honest moment to claim one — everything queued before it will be
/// skipped on the other side.
fn offer_bitstream(
    target: &MediaTarget,
    slot: &watch::Sender<ViewSlot>,
    encoded: EncodedFrame,
    produced: bool,
) -> bool {
    let live = produced || encoded.keyframe;
    target.bitstream.push(encoded);
    if live {
        // Nothing reaches `slot` on this path, and the overlay reads its
        // status from there.
        set_status(slot, ViewStatus::Live);
    }
    live
}

/// Sends one report to the actor, dropping it rather than stalling the decode
/// loop when the actor is busy.
///
/// A dropped report costs the host one interval of feedback, which the next
/// one replaces. A blocked decode loop costs the user their picture.
fn send_report(target: &MediaTarget, report: MediaReport) {
    if target.reports.try_send((target.peer, report)).is_err() {
        tracing::debug!(peer = %target.tag, "dropping a media report: the actor is backed up");
    }
}

/// Asks the host for an intra frame, at most once per
/// [`KEYFRAME_MIN_INTERVAL_MS`].
///
/// The host keeps a budget of its own, and that is the one that protects it
/// (§11). This one keeps a decoder that is failing on every frame from turning
/// its own trouble into a flood of requests.
fn ask_for_a_keyframe(target: &MediaTarget, last: &mut Option<Instant>) {
    let budget = Duration::from_millis(KEYFRAME_MIN_INTERVAL_MS);
    if last.is_some_and(|at| at.elapsed() < budget) {
        return;
    }
    *last = Some(Instant::now());
    send_report(target, MediaReport::KeyframeNeeded);
}

/// Guest side: what arrived over one [`ABR_FEEDBACK_INTERVAL_MS`] window.
///
/// Two numbers, and neither is guessed. `rd/media/1` is reliable and ordered,
/// so bytes are never dropped in transit — what a receiver *can* lose is a
/// frame it could not turn into a picture, and what it can *measure* is how
/// much arrived per second. Everything the host wants to know about this link,
/// it can only learn from these two (ADR 0037).
#[derive(Debug)]
struct ReceptionWindow {
    started: Instant,
    frames: u64,
    lost: u64,
    bytes: u64,
}

impl ReceptionWindow {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            frames: 0,
            lost: 0,
            bytes: 0,
        }
    }

    /// One media payload arrived.
    fn received(&mut self, bytes: usize) {
        self.frames = self.frames.saturating_add(1);
        self.bytes = self.bytes.saturating_add(bytes as u64);
    }

    /// One payload could not be turned into a picture.
    fn lost(&mut self) {
        self.lost = self.lost.saturating_add(1);
    }

    /// The report for this window, if it has run its course; starts the next
    /// window in the same call.
    fn due(&mut self) -> Option<MediaReport> {
        let elapsed = self.started.elapsed();
        if elapsed < Duration::from_millis(u64::from(ABR_FEEDBACK_INTERVAL_MS)) {
            return None;
        }
        let report = self.snapshot(elapsed);
        *self = Self::new();
        Some(report)
    }

    fn snapshot(&self, elapsed: Duration) -> MediaReport {
        // A window with no frames in it has no loss to report, which is
        // what `checked_div` returning `None` already says.
        let loss_permille = self
            .lost
            .saturating_mul(PERMILLE)
            .checked_div(self.frames)
            .map_or(0, |permille| u16::try_from(permille).unwrap_or(u16::MAX));
        let millis = u64::try_from(elapsed.as_millis())
            .unwrap_or(u64::MAX)
            .max(1);
        let goodput_kbps =
            u32::try_from(self.bytes.saturating_mul(BITS_PER_BYTE) / millis).unwrap_or(u32::MAX);
        MediaReport::Feedback {
            loss_permille,
            goodput_kbps,
        }
    }
}

/// Spawns the decoder worker off the async runtime.
async fn spawn_decoder(worker: Option<PathBuf>) -> Result<DecoderHandle, String> {
    tokio::task::spawn_blocking(move || match worker {
        Some(path) => DecoderHandle::spawn_with(&path),
        None => DecoderHandle::spawn(),
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
}

/// Host side: capture desktop audio, Opus-encode and write it onto one tagged
/// `rd/media/1` stream, until `stop` flips or the stream fails.
///
/// Started with every media session the host accepts — sound comes with the
/// picture, which the `view` grant already covers (§8.1; ADR 0137) — and by
/// the `audio_toggle` command. The guest learns audio exists by accepting a
/// stream that announces itself; that stream opens only once `video_stream`
/// says the picture's has, because a guest takes the first stream it accepts
/// as the picture. Capture runs behind the `audio-capture` feature; without a
/// backend the loop refuses loudly in the log and the session stays
/// video-only (§18). What it captured and sent is counted in `meter`
/// (ADR 0147).
pub fn spawn_audio_loop(
    connection: PeerConnection,
    stop: Arc<AtomicBool>,
    recorder: crate::view::SharedRecorder,
    tag: String,
    video_stream: impl Future<Output = bool> + Send + 'static,
    meter: Arc<AudioMeter>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let capturer = match lumepeer_media::capture_audio::platform_audio_capturer() {
            Ok(capturer) => capturer,
            Err(error) => {
                tracing::warn!(peer = %tag, %error, "no audio capture backend: staying video-only");
                meter.device_failed(&error.to_string());
                return;
            }
        };
        run_audio_loop(capturer, connection, stop, recorder, tag, video_stream, meter).await;
    })
}

/// The body of [`spawn_audio_loop`], over whichever capturer it was handed.
#[allow(
    clippy::too_many_lines,
    reason = "one pass per chunk — read, count, encode, write, record — in the order it must happen"
)]
async fn run_audio_loop(
    capturer: Box<dyn lumepeer_media::capture_audio::AudioCapturer>,
    connection: PeerConnection,
    stop: Arc<AtomicBool>,
    recorder: crate::view::SharedRecorder,
    tag: String,
    video_stream: impl Future<Output = bool>,
    meter: Arc<AudioMeter>,
) {
    let mut capturer = Some(capturer);
    let mut encoder = match lumepeer_media::audio::OpusEncoder::new() {
        Ok(encoder) => encoder,
        Err(error) => {
            tracing::warn!(peer = %tag, %error, "no Opus encoder: staying video-only");
            return;
        }
    };
    if !video_stream.await {
        tracing::debug!(peer = %tag, "the picture's stream never opened: no audio either");
        return;
    }
    let mut writer =
        match lumepeer_net::open_tagged_media_stream(&connection, lumepeer_net::STREAM_AUDIO).await
        {
            Ok(writer) => writer,
            Err(error) => {
                tracing::warn!(peer = %tag, %error, "cannot open the audio stream");
                return;
            }
        };
    meter.stream_opened();
    // `start` is fallible on the platform side; a refusal ends this loop
    // before any packet flows, leaving the session video-only (§18).
    let Some(started) = capturer.as_mut() else {
        return;
    };
    if let Err(error) = started.start() {
        tracing::warn!(peer = %tag, %error, "audio capture refused to start");
        meter.device_failed(&error.to_string());
        return;
    }
    meter.device_open();
    tracing::info!(peer = %tag, "audio loop started");

    loop {
        if stop.load(Ordering::Relaxed) {
            if let Some(mut c) = capturer.take() {
                c.stop();
            }
            tracing::info!(peer = %tag, "audio loop stopped");
            return;
        }
        // `next_chunk` blocks on the device; it must not sit on a tokio
        // worker thread. The trait object is `Send`, so it crosses into
        // `spawn_blocking` by value and comes back in the closure's return.
        // The `take`/restore dance keeps the capturer owned across the
        // blocking call; between reads it always sits back in the slot.
        let Some(mut borrowed) = capturer.take() else {
            return;
        };
        let read = match tokio::task::spawn_blocking(move || {
            let result = borrowed.next_chunk();
            (borrowed, result)
        })
        .await
        {
            Ok(read) => read,
            Err(error) => {
                tracing::warn!(peer = %tag, %error, "the audio read task ended unexpectedly");
                return;
            }
        };
        let (device, read_result) = read;
        capturer = Some(device);
        match read_result {
            Ok(samples_chunk) => {
                meter.observe(
                    &samples_chunk.samples,
                    usize::from(AUDIO_CHANNELS),
                    AUDIO_SAMPLE_RATE_HZ,
                );
                match encoder.encode(&samples_chunk.samples, samples_chunk.timestamp_us) {
                    Ok(packet) => {
                        if packet.data.len() > AUDIO_MAX_FRAME_BYTES {
                            tracing::warn!(peer = %tag, "dropping an oversized audio frame");
                            continue;
                        }
                        let payload = lumepeer_net::encode_audio_payload(&packet);
                        if let Err(error) = writer.write_frame(&payload).await {
                            tracing::info!(peer = %tag, %error, "audio stream ended");
                            if let Some(mut c) = capturer.take() {
                                c.stop();
                            }
                            return;
                        }
                        // Recording rides the successfully written packet
                        // (§17): the container stores the same Opus payload
                        // the guest received.
                        let slot_guard = recorder
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if let Some(recorder) = slot_guard.as_ref() {
                            recorder.write_audio(packet.timestamp_us, &packet.data);
                        }
                    }
                    Err(error) => {
                        // One bad chunk is a skip, not a teardown: audio
                        // degrades towards noise, never towards an error
                        // (§24.5).
                        tracing::debug!(peer = %tag, %error, "encoder refused a chunk");
                    }
                }
            }
            Err(error) => {
                tracing::info!(peer = %tag, %error, "audio capture ended");
                meter.device_failed(&error.to_string());
                if let Some(mut c) = capturer.take() {
                    c.stop();
                }
                return;
            }
        }
    }
}

/// Guest side: opens one playback backend for host audio. A factory rather
/// than a player, because a device that fails is replaced by a fresh one.
type OpenPlayer =
    Arc<dyn Fn() -> lumepeer_media::error::Result<Box<dyn AudioPlayer>> + Send + Sync>;

/// Chunks of host audio waiting for the speakers: 100 ms at
/// `AUDIO_FRAME_MS`. A burst beyond that is dropped rather than queued,
/// because audio already behind the picture is worse than a gap (ADR 0039).
const SPEAKER_QUEUE_CHUNKS: usize = 5;

/// How long a failed playback device is left alone before it is opened again
/// (ADR 0137).
const SPEAKER_RETRY: Duration = Duration::from_secs(2);

/// Guest side: the speakers host audio plays on (ADR 0137).
///
/// Their own thread rather than a tokio worker: every backend's `push`
/// blocks until the device has room, which on Windows is most of each 20 ms
/// chunk. A device that fails mid-session — headphones unplugged — is
/// reopened on the default one after [`SPEAKER_RETRY`] instead of leaving the
/// rest of the session silent.
struct Speakers {
    chunks: std::sync::mpsc::SyncSender<(Vec<i16>, u64)>,
    /// Where what reached the device, and what did not, is counted
    /// (ADR 0147).
    meter: Arc<AudioMeter>,
}

impl Speakers {
    /// Starts the playback thread; `None` when the OS refused a thread.
    fn open(open_player: OpenPlayer, tag: String, meter: Arc<AudioMeter>) -> Option<Self> {
        let counted = Arc::clone(&meter);
        let (chunks, queue) =
            std::sync::mpsc::sync_channel::<(Vec<i16>, u64)>(SPEAKER_QUEUE_CHUNKS);
        let spawned = std::thread::Builder::new()
            .name("audio-playout".to_owned())
            .spawn(move || {
                let mut player: Option<Box<dyn AudioPlayer>> = None;
                let mut retry_at = Instant::now();
                let mut warned = false;
                // Ends when the stream does: its reader drops the sender.
                while let Ok((samples, timestamp_us)) = queue.recv() {
                    if player.is_none() && Instant::now() >= retry_at {
                        match open_player().and_then(|mut opened| opened.start().map(|()| opened)) {
                            Ok(opened) => {
                                if warned {
                                    tracing::info!(peer = %tag, "a playback device opened: host audio plays again");
                                    warned = false;
                                }
                                counted.device_open();
                                player = Some(opened);
                            }
                            Err(error) => {
                                if !warned {
                                    tracing::warn!(peer = %tag, %error, "no playback device: host audio is silent");
                                    warned = true;
                                }
                                counted.device_failed(&error.to_string());
                                retry_at = Instant::now() + SPEAKER_RETRY;
                            }
                        }
                    }
                    if let Some(device) = player.as_mut() {
                        match device.push(&samples, timestamp_us) {
                            Ok(()) => counted.played(),
                            Err(error) => {
                                tracing::warn!(peer = %tag, %error, "host audio playback stopped; reopening");
                                counted.device_failed(&error.to_string());
                                device.stop();
                                player = None;
                                retry_at = Instant::now() + SPEAKER_RETRY;
                            }
                        }
                    }
                }
                if let Some(mut device) = player {
                    device.stop();
                }
            });
        match spawned {
            Ok(_thread) => Some(Self { chunks, meter }),
            Err(error) => {
                tracing::warn!(%error, "cannot start the audio playout thread");
                meter.device_failed(&error.to_string());
                None
            }
        }
    }

    /// Queues one decoded chunk, dropping it when the speakers are behind.
    fn play(&self, samples: Vec<i16>, timestamp_us: u64) {
        if let Err(std::sync::mpsc::TrySendError::Full(_)) =
            self.chunks.try_send((samples, timestamp_us))
        {
            tracing::debug!("dropping a chunk of host audio: the speakers are behind");
            self.meter.dropped();
        }
    }
}

/// Guest side: accepts the host's tagged audio stream, decodes Opus and plays
/// it on this machine's speakers.
///
/// Returns when the stream ends — the caller treats that the same way the
/// video path treats a lost stream.
async fn stream_audio_once(
    connection: &PeerConnection,
    tag: &str,
    open_player: &OpenPlayer,
    meter: &Arc<AudioMeter>,
) -> bool {
    let mut reader = match lumepeer_net::accept_audio_media_stream(connection).await {
        Ok(Some(reader)) => reader,
        Ok(None) => return false,
        Err(error) => {
            tracing::debug!(peer = %tag, %error, "no audio stream arrived");
            return false;
        }
    };
    // The Opus decoder runs in this process by design decision (questions.md
    // item 9): Opus packets are validated by libopus itself, which never
    // panics on hostile input and reports concealment instead. The *video*
    // bitstream keeps its out-of-process decoder; audio's worst case is noise.
    let mut decoder = match lumepeer_media::audio::OpusDecoder::new() {
        Ok(decoder) => decoder,
        Err(error) => {
            tracing::warn!(peer = %tag, %error, "no Opus decoder: audio stays off");
            return false;
        }
    };
    let Some(speakers) = Speakers::open(Arc::clone(open_player), tag.to_owned(), Arc::clone(meter))
    else {
        return false;
    };
    meter.stream_opened();
    tracing::info!(peer = %tag, "host audio stream accepted");
    loop {
        let payload = match reader.read_frame().await {
            Ok(payload) => payload,
            Err(error) => {
                tracing::debug!(peer = %tag, %error, "audio stream ended");
                return true;
            }
        };
        let Some(chunk) = lumepeer_net::decode_audio_payload(&payload) else {
            tracing::warn!(peer = %tag, "dropping a malformed audio payload");
            continue;
        };
        // An empty packet is a loss hint: libopus synthesizes concealment.
        let packet = if chunk.data.is_empty() {
            &[][..]
        } else {
            &chunk.data[..]
        };
        match decoder.decode(packet) {
            Ok(samples) => {
                meter.observe(&samples, usize::from(AUDIO_CHANNELS), AUDIO_SAMPLE_RATE_HZ);
                speakers.play(samples, chunk.timestamp_us);
            }
            Err(error) => {
                tracing::debug!(peer = %tag, %error, "decoder refused a packet");
            }
        }
    }
}

/// Ends the task it owns when the media pass that started it does.
///
/// A stream reader must never outlive the connection it reads from: a leaked
/// one keeps a dead pass's task alive and is started again by the next pass,
/// so passes accumulate readers instead of replacing them.
struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Guest side: accepts the host's tagged audio stream on the media connection
/// the picture already rides, for as long as that pass lasts, and plays it on
/// speakers `open_player` opens.
///
/// One per media pass, started by [`stream_once`] once the picture's stream
/// has been taken, and aborted with the pass. It parks by itself when a host
/// never opens an audio stream — a host without a capture backend, or an
/// older one that waits for its user to turn audio on (§11; ADR 0137).
fn spawn_audio_pass(
    connection: PeerConnection,
    tag: String,
    open_player: OpenPlayer,
    meter: Arc<AudioMeter>,
) -> AbortOnDrop {
    AbortOnDrop(tokio::spawn(async move {
        let backoff = Duration::from_millis(MEDIA_REDIAL_BACKOFF_MS.max(1_000));
        loop {
            let produced = stream_audio_once(&connection, &tag, &open_player, &meter).await;
            // A stream that carried audio and then ended may be followed by
            // another one — the host toggling audio off and on again.
            // One that never arrived means a video-only host: park, retry.
            if !produced {
                tokio::time::sleep(backoff).await;
            }
        }
    }))
}

/// How long the microphone waits after its stream ended before opening
/// another, so a host that refused the last one is not asked again at once.
const MIC_REOPEN_PAUSE: Duration = Duration::from_secs(1);

/// Guest side: capture the microphone, Opus-encode and write it onto a tagged
/// `M` media stream on whichever media connection the view's picture rides,
/// until the toggle turns it off (§11; ADR 0028, ADR 0147).
///
/// Started by the toolbar's mic button through the `mic_toggle` IPC command,
/// and owned by the view window rather than by one connection: when the
/// picture's pass ends and the next one dials — a lost link, a codec change,
/// a session coming back — the stream ends with the old connection and a new
/// one opens on the connection `connection` names next. It used to end with
/// its first connection while the button still said the microphone was on.
/// The tag, [`STREAM_MIC`], is what lets the host tell it from the streams it
/// opens itself. Without a backend, or when the OS refuses microphone access,
/// the loop refuses loudly in the log and in `meter` (§18).
#[must_use]
pub fn spawn_mic_loop(
    connection: Arc<Mutex<Option<PeerConnection>>>,
    tag: String,
    meter: Arc<AudioMeter>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let capturer = match lumepeer_media::capture_audio::platform_mic_capturer() {
            Ok(capturer) => capturer,
            Err(error) => {
                tracing::warn!(peer = %tag, %error, "no microphone backend: mic stays off");
                meter.device_failed(&error.to_string());
                return;
            }
        };
        run_mic_loop(capturer, connection, tag, meter).await;
    })
}

/// The body of [`spawn_mic_loop`], over whichever capturer it was handed.
async fn run_mic_loop(
    capturer: Box<dyn lumepeer_media::capture_audio::MicCapturer>,
    connection: Arc<Mutex<Option<PeerConnection>>>,
    tag: String,
    meter: Arc<AudioMeter>,
) {
    let mut capturer = Some(capturer);
    let Some(started) = capturer.as_mut() else {
        return;
    };
    if let Err(error) = started.start() {
        tracing::warn!(peer = %tag, %error, "microphone capture refused to start");
        meter.device_failed(&error.to_string());
        return;
    }
    meter.device_open();
    tracing::info!(peer = %tag, "mic loop started");

    // The open stream and an encoder of its own: the host decodes every
    // stream with a fresh decoder, and an encoder carried over from the last
    // one would start this one in the middle of its prediction.
    let mut writer = None;
    let mut reopen_at = Instant::now();
    loop {
        // `next_chunk` blocks on the device; it must not sit on a tokio
        // worker thread — the same take/restore dance the host audio loop
        // uses.
        let Some(mut borrowed) = capturer.take() else {
            return;
        };
        let read = match tokio::task::spawn_blocking(move || {
            let result = borrowed.next_chunk();
            (borrowed, result)
        })
        .await
        {
            Ok(read) => read,
            Err(error) => {
                tracing::warn!(peer = %tag, %error, "the mic read task ended unexpectedly");
                return;
            }
        };
        let (device, read_result) = read;
        capturer = Some(device);
        let chunk = match read_result {
            Ok(chunk) => chunk,
            Err(error) => {
                tracing::info!(peer = %tag, %error, "microphone capture ended");
                meter.device_failed(&error.to_string());
                if let Some(mut c) = capturer.take() {
                    c.stop();
                }
                return;
            }
        };
        if writer.is_none() && Instant::now() >= reopen_at {
            // The picture's connection as of now, and only a live one: a
            // pass that just ended leaves its closed connection in the cell
            // until the next dial replaces it.
            let live = connection
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
                .filter(|c| c.close_reason().is_none());
            if let Some(live) = live {
                let encoder = match lumepeer_media::audio::OpusEncoder::new() {
                    Ok(encoder) => encoder,
                    Err(error) => {
                        tracing::warn!(peer = %tag, %error, "no Opus encoder: mic stays off");
                        meter.device_failed(&error.to_string());
                        return;
                    }
                };
                match lumepeer_net::open_tagged_media_stream(&live, STREAM_MIC).await {
                    Ok(opened) => {
                        meter.stream_opened();
                        tracing::info!(peer = %tag, "mic stream opened on the picture's connection");
                        writer = Some((opened, encoder));
                    }
                    Err(error) => {
                        tracing::debug!(peer = %tag, %error, "cannot open the mic stream yet");
                        reopen_at = Instant::now() + MIC_REOPEN_PAUSE;
                    }
                }
            }
        }
        // With no stream open the chunk is read and let go: the device keeps
        // streaming, and nothing stale piles up for the next stream.
        let Some((open, encoder)) = writer.as_mut() else {
            continue;
        };
        match encoder.encode(&chunk.samples, chunk.timestamp_us) {
            Ok(packet) => {
                if packet.data.len() > AUDIO_MAX_FRAME_BYTES {
                    tracing::warn!(peer = %tag, "dropping an oversized mic frame");
                    continue;
                }
                let payload = lumepeer_net::encode_audio_payload(&packet);
                if let Err(error) = open.write_frame(&payload).await {
                    tracing::info!(peer = %tag, %error, "mic stream ended; it follows the picture's next connection");
                    writer = None;
                    reopen_at = Instant::now() + MIC_REOPEN_PAUSE;
                    continue;
                }
                meter.observe(&chunk.samples, usize::from(AUDIO_CHANNELS), AUDIO_SAMPLE_RATE_HZ);
            }
            Err(error) => {
                // One bad chunk is a skip, not a teardown: audio degrades
                // towards noise, never towards an error (§24.5).
                tracing::debug!(peer = %tag, %error, "encoder refused a mic chunk");
            }
        }
    }
}

/// Host side: every stream the guest opens on the media connection, by the
/// tag it announces itself with (§11): its microphone (ADR 0028) and its
/// acknowledgements of the picture (ADR 0139). One per media session, started
/// when the media connection is accepted; it parks while the guest opens
/// nothing and ends with the connection.
///
/// One acceptor for all of them, because there can be only one: a pass that
/// accepted streams looking for its own tag dropped every other kind it met,
/// which is what the microphone's pass alone used to do.
#[allow(
    clippy::must_use_candidate,
    reason = "the handle is a way to abort the task, not a result: it parks on               streams the guest may never open and is bounded by the media               session's own lifetime, so dropping it is the ordinary call"
)]
pub fn spawn_guest_streams(
    connection: PeerConnection,
    tag: String,
    acks: Arc<FrameAcks>,
    mic: Arc<AudioMeter>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let reader = match accept_media_stream(&connection).await {
                Ok(reader) => reader,
                Err(error) => {
                    tracing::debug!(peer = %tag, %error, "the media connection ended");
                    return;
                }
            };
            // On a task of its own: a stream that is slow to say what it is
            // must not keep the next one from being accepted.
            tokio::spawn(serve_guest_stream(
                reader,
                tag.clone(),
                Arc::clone(&acks),
                Arc::clone(&mic),
            ));
        }
    })
}

/// Reads the tag one guest stream opens with and serves it accordingly.
async fn serve_guest_stream(
    mut reader: lumepeer_net::MediaFrameReader<iroh::endpoint::RecvStream>,
    tag: String,
    acks: Arc<FrameAcks>,
    mic: Arc<AudioMeter>,
) {
    match reader.read_frame().await {
        Ok(kind) if kind == [STREAM_MIC] => play_guest_mic(reader, tag, &mic).await,
        Ok(kind) if kind == [STREAM_ACKS] => read_frame_acks(reader, &acks, &tag).await,
        Ok(_) => tracing::debug!(peer = %tag, "skipping an unannounced media stream"),
        Err(error) => {
            tracing::debug!(peer = %tag, %error, "a guest stream ended before saying what it carries");
        }
    }
}

/// Host side: hands every acknowledgement on one guest stream to the encode
/// loop, and tells it when they stop (ADR 0139).
async fn read_frame_acks(
    mut reader: lumepeer_net::MediaFrameReader<iroh::endpoint::RecvStream>,
    acks: &FrameAcks,
    tag: &str,
) {
    acks.opened();
    loop {
        match reader.read_frame().await {
            Ok(payload) => {
                let Some(count) = decode_frame_ack(&payload) else {
                    tracing::warn!(peer = %tag, "a malformed frame acknowledgement: ignoring the rest");
                    break;
                };
                acks.acked(count, Instant::now());
            }
            Err(error) => {
                tracing::debug!(peer = %tag, %error, "the frame acknowledgements ended");
                break;
            }
        }
    }
    acks.closed();
}

/// Host side: plays the guest's `M` mic stream on the speakers (§11;
/// ADR 0028) until it ends.
async fn play_guest_mic(
    mut reader: lumepeer_net::MediaFrameReader<iroh::endpoint::RecvStream>,
    tag: String,
    meter: &AudioMeter,
) {
    meter.stream_opened();
    // The Opus decoder runs in this process by the same decision as the
    // host→guest audio direction: libopus never panics on hostile input
    // and reports concealment instead.
    let mut decoder = match lumepeer_media::audio::OpusDecoder::new() {
        Ok(decoder) => decoder,
        Err(error) => {
            tracing::warn!(peer = %tag, %error, "no Opus decoder: guest mic stays off");
            return;
        }
    };
    // Playback is a platform backend like capture is: a target without
    // one refuses here and the mic simply stays off (§18).
    let mut player = match lumepeer_media::playout::platform_player() {
        Ok(player) => player,
        Err(error) => {
            tracing::warn!(peer = %tag, %error, "no playback backend: guest mic stays off");
            meter.device_failed(&error.to_string());
            return;
        }
    };
    if let Err(error) = player.start() {
        tracing::warn!(peer = %tag, %error, "no playback device: guest mic stays off");
        meter.device_failed(&error.to_string());
        return;
    }
    meter.device_open();
    loop {
        let payload = match reader.read_frame().await {
            Ok(payload) => payload,
            Err(error) => {
                tracing::debug!(peer = %tag, %error, "mic stream ended");
                return;
            }
        };
        let Some(chunk) = lumepeer_net::decode_audio_payload(&payload) else {
            tracing::warn!(peer = %tag, "dropping a malformed mic payload");
            continue;
        };
        // An empty packet is a loss hint: libopus synthesizes concealment.
        let packet = if chunk.data.is_empty() {
            &[][..]
        } else {
            &chunk.data[..]
        };
        match decoder.decode(packet) {
            Ok(samples) => {
                meter.observe(&samples, usize::from(AUDIO_CHANNELS), AUDIO_SAMPLE_RATE_HZ);
                if let Err(error) = player.push(&samples, chunk.timestamp_us) {
                    tracing::warn!(peer = %tag, %error, "mic playback stopped");
                    meter.device_failed(&error.to_string());
                    return;
                }
                meter.played();
            }
            Err(error) => {
                tracing::debug!(peer = %tag, %error, "decoder refused a mic packet");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failed assumption must fail the test")]

    use super::*;

    /// ADR 0141: a window that holds the frame rate reports its p95 and
    /// resets the count; one slow window alone is not enough to give up.
    #[test]
    fn one_slow_window_is_not_a_host_that_cannot_keep_up() {
        let mut watch = SoftwareAv1Watch::default();
        let fast = Duration::from_millis(15);
        let slow = Duration::from_millis(40);
        for _ in 1..SOFTWARE_AV1_WATCH_FRAMES {
            assert_eq!(watch.frame(slow), SoftwareAv1Verdict::Pending);
        }
        assert_eq!(watch.frame(slow), SoftwareAv1Verdict::KeepingUp(slow));
        for _ in 1..SOFTWARE_AV1_WATCH_FRAMES {
            watch.frame(fast);
        }
        assert_eq!(
            watch.frame(fast),
            SoftwareAv1Verdict::KeepingUp(fast),
            "a good window must reset the count"
        );
        for _ in 0..SOFTWARE_AV1_WATCH_FRAMES {
            assert_ne!(watch.frame(slow), SoftwareAv1Verdict::Behind(slow));
        }
    }

    /// ADR 0141: [`SOFTWARE_AV1_SLOW_WINDOWS`] windows in a row over the 30
    /// fps interval, by p95 — the rare long frame does not count.
    #[test]
    fn windows_in_a_row_over_the_interval_mean_the_host_is_behind() {
        let mut watch = SoftwareAv1Watch::default();
        let slow = Duration::from_millis(34);
        let mut verdict = SoftwareAv1Verdict::Pending;
        for _ in 0..SOFTWARE_AV1_WATCH_FRAMES * SOFTWARE_AV1_SLOW_WINDOWS as usize {
            verdict = watch.frame(slow);
        }
        assert_eq!(verdict, SoftwareAv1Verdict::Behind(slow));

        // Four frames in a hundred at a second each are under the p95.
        let mut watch = SoftwareAv1Watch::default();
        let mut verdict = SoftwareAv1Verdict::Pending;
        for window in 0..SOFTWARE_AV1_SLOW_WINDOWS as usize * 2 {
            for frame in 0..SOFTWARE_AV1_WATCH_FRAMES {
                let took = if frame % 25 == 0 {
                    Duration::from_secs(1)
                } else {
                    Duration::from_millis(20)
                };
                verdict = watch.frame(took);
            }
            assert_eq!(
                verdict,
                SoftwareAv1Verdict::KeepingUp(Duration::from_millis(20)),
                "window {window}"
            );
        }
    }

    #[test]
    fn picture_pixels_does_not_overflow() {
        assert_eq!(picture_pixels((1920, 1080)), SOFTWARE_AV1_MAX_PIXELS);
        assert!(picture_pixels((u32::MAX, u32::MAX)) > SOFTWARE_AV1_MAX_PIXELS);
    }

    /// ADR 0136: the encode loop's pacing reaches the rate it is asked for.
    ///
    /// Real time, on purpose — the defect was the platform timer, which a
    /// paused tokio clock cannot see. With `tokio::time::sleep` a 60 fps loop
    /// doing 2 ms of work a tick ran at about 32 on Windows; the bound leaves
    /// room for a loaded machine without letting that through.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_pacing_keeps_up_with_60_fps() {
        const TICKS: u32 = 60;
        let interval = frame_interval(60);
        let started = Instant::now();
        for _ in 0..TICKS {
            let tick_started = Instant::now();
            // Capture and encode, standing in.
            std::thread::sleep(Duration::from_millis(2));
            sleep_for_the_rest_of(interval, tick_started).await;
        }
        let fps = f64::from(TICKS) / started.elapsed().as_secs_f64();
        assert!(fps >= 48.0, "a 60 fps loop ran at {fps:.1} fps");
    }

    /// D7, docs/bugs/13-stream-resolution.md task 2: the ceiling starts
    /// unset, a new value replaces it, and the loop sees exactly what was
    /// set.
    #[test]
    fn manual_cap_starts_unset_and_is_read_back_after_being_set() {
        let control = EncodeControl::new(iroh::SecretKey::generate().public(), None);
        assert_eq!(control.manual_cap(), None);
        control.set_manual_cap(Some(75));
        assert_eq!(control.manual_cap(), Some(75));
        control.set_manual_cap(None);
        assert_eq!(control.manual_cap(), None);
    }

    /// The actor uses the return value of `set_manual_cap` to decide whether
    /// a keyframe is owed (task 2.4): repeating the value already in effect
    /// must not look like a change.
    #[test]
    fn set_manual_cap_reports_whether_it_actually_changed() {
        let control = EncodeControl::new(iroh::SecretKey::generate().public(), None);
        assert!(control.set_manual_cap(Some(50)), "unset to a value changed");
        assert!(
            !control.set_manual_cap(Some(50)),
            "repeating the same value must not read as a change"
        );
        assert!(
            control.set_manual_cap(Some(75)),
            "a different value changed"
        );
        assert!(control.set_manual_cap(None), "clearing the cap changed");
        assert!(
            !control.set_manual_cap(None),
            "clearing an already-clear cap did not"
        );
    }

    /// Audio must ride the media connection the picture already dialed
    /// (§4.1, §11), and this is what goes wrong when it does not: a second
    /// `rd/media/1` connection is indistinguishable, on the host, from the
    /// same guest redialing, so the host replaces the media session — killing
    /// the encode loop that was about to feed the picture. The guest then
    /// redials, dials audio again, and the session never delivers a frame.
    #[tokio::test(flavor = "multi_thread")]
    async fn one_media_pass_dials_one_connection() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let host = lumepeer_net::PeerEndpoint::bind_local(iroh::SecretKey::generate())
            .await
            .unwrap();
        let guest = lumepeer_net::PeerEndpoint::bind_local(iroh::SecretKey::generate())
            .await
            .unwrap();
        let addr = host.addr();

        let dials = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&dials);
        let accepting = tokio::spawn(async move {
            // Everything accepted is held: a dropped connection would end the
            // guest's pass and start a new one, which is not what is measured
            // here.
            let mut held = Vec::new();
            while let Some(Ok(connection)) = host.accept().await {
                counted.fetch_add(1, Ordering::Relaxed);
                let mut writer = open_media_stream(&connection).await.unwrap();
                // One frame too short to be a picture: it opens the stream on
                // the wire, so the guest's video path gets past `accept_uni`
                // and starts its audio pass, without pulling a decoder worker
                // into a unit test.
                writer.write_frame(&[0u8]).await.unwrap();
                held.push((connection, writer));
            }
        });

        let (slot_tx, _slot_rx) = watch::channel(ViewSlot::waiting());
        // The reports channel is held open for the length of the test: a
        // closed receiver would make every send fail, which is a different
        // path from the one under test.
        let (reports, _reports_rx) = mpsc::channel(4);
        let receiver = spawn_media_receiver(
            MediaTarget {
                dialer: crate::network::HostDialer::Iroh {
                    endpoint: guest.clone(),
                    addr,
                },
                peer: guest.node_id(),
                reports,
                tag: "test-peer".to_owned(),
                worker: None,
                bitstream: Arc::new(BitstreamFeed::default()),
                connection_cell: Arc::new(std::sync::Mutex::new(None)),
                audio_in: Arc::default(),
            },
            Arc::new(slot_tx),
        );

        // Comfortably longer than the audio dial's own head start: it used to
        // fire in the same breath as the video one.
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(
            dials.load(Ordering::Relaxed),
            1,
            "one media pass must occupy exactly one media connection"
        );

        receiver.abort();
        accepting.abort();
    }

    /// A host "playing" a 440 Hz tone, at roughly the pace a device would.
    #[derive(Debug)]
    struct ToneCapturer {
        sample: u32,
    }

    impl lumepeer_media::capture_audio::AudioCapturer for ToneCapturer {
        fn start(&mut self) -> lumepeer_media::error::Result<()> {
            Ok(())
        }

        fn next_chunk(
            &mut self,
        ) -> lumepeer_media::error::Result<lumepeer_media::capture_audio::PcmChunk> {
            std::thread::sleep(Duration::from_millis(5));
            let mut samples = Vec::new();
            for _ in 0..lumepeer_media::capture_audio::SAMPLES_PER_CHUNK {
                let t = f64::from(self.sample) / 48_000.0;
                #[allow(clippy::cast_possible_truncation, reason = "bounded to ±8000")]
                let value = ((t * 440.0 * std::f64::consts::TAU).sin() * 8_000.0) as i16;
                samples.extend([value, value]);
                self.sample += 1;
            }
            Ok(lumepeer_media::capture_audio::PcmChunk {
                samples,
                timestamp_us: 0,
            })
        }

        fn stop(&mut self) {}
    }

    /// Speakers that hand every chunk they are given to the test.
    #[derive(Debug)]
    struct RecordingSpeakers(std::sync::mpsc::Sender<Vec<i16>>);

    impl AudioPlayer for RecordingSpeakers {
        fn start(&mut self) -> lumepeer_media::error::Result<()> {
            Ok(())
        }

        fn push(
            &mut self,
            samples: &[i16],
            _timestamp_us: u64,
        ) -> lumepeer_media::error::Result<()> {
            let _ = self.0.send(samples.to_vec());
            Ok(())
        }

        fn stop(&mut self) {}
    }

    /// ADR 0137: what the host plays reaches the guest's speakers, and its
    /// stream opens only after the picture's. The picture's stream carries no
    /// tag, so a guest takes the first stream it accepts as the picture — an
    /// audio stream that got there first would be decoded as video.
    #[tokio::test(flavor = "multi_thread")]
    async fn host_audio_follows_the_picture_and_reaches_the_speakers() {
        let host = lumepeer_net::PeerEndpoint::bind_local(iroh::SecretKey::generate())
            .await
            .unwrap();
        let guest = lumepeer_net::PeerEndpoint::bind_local(iroh::SecretKey::generate())
            .await
            .unwrap();
        let addr = host.addr();
        let accepting = tokio::spawn(async move {
            let connection = host.accept().await.unwrap().unwrap();
            (host, connection)
        });
        let guest_side = guest.connect(addr, lumepeer_net::ALPN_MEDIA).await.unwrap();
        let (_host, host_side) = accepting.await.unwrap();

        let (video_open, mut video_seen) = watch::channel(false);
        let sent = Arc::new(AudioMeter::default());
        let audio = tokio::spawn(run_audio_loop(
            Box::new(ToneCapturer { sample: 0 }),
            host_side.clone(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(None)),
            "test-peer".to_owned(),
            async move { video_seen.wait_for(|open| *open).await.is_ok() },
            Arc::clone(&sent),
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(500), accept_media_stream(&guest_side))
                .await
                .is_err(),
            "audio opened a stream before the picture's"
        );

        let mut picture = open_media_stream(&host_side).await.unwrap();
        picture.write_frame(&[0, 42]).await.unwrap();
        video_open.send_replace(true);
        let mut first = accept_media_stream(&guest_side).await.unwrap();
        assert_eq!(first.read_frame().await.unwrap(), vec![0, 42]);

        let (heard, played) = std::sync::mpsc::channel();
        let received = Arc::new(AudioMeter::default());
        let _pass = spawn_audio_pass(
            guest_side,
            "test-peer".to_owned(),
            Arc::new(
                move || Ok(Box::new(RecordingSpeakers(heard.clone())) as Box<dyn AudioPlayer>),
            ),
            Arc::clone(&received),
        );
        let loudest = tokio::task::spawn_blocking(move || {
            let mut loudest = 0;
            while let Ok(chunk) = played.recv_timeout(Duration::from_secs(5)) {
                assert_eq!(
                    chunk.len(),
                    lumepeer_media::capture_audio::SAMPLES_PER_CHUNK * 2
                );
                loudest = chunk
                    .iter()
                    .map(|s| s.unsigned_abs())
                    .max()
                    .unwrap_or(0)
                    .max(loudest);
                if loudest > 4_000 {
                    break;
                }
            }
            loudest
        })
        .await
        .unwrap();
        assert!(
            loudest > 4_000,
            "the speakers heard only silence (peak {loudest})"
        );
        // ADR 0147: both ends counted what went through them, and the guest
        // can name the tone it heard — what the e2e matrix reads.
        let heard = received.snapshot();
        assert_eq!(heard.streams, 1);
        assert!(heard.loud > 0 && heard.played > 0, "{heard:?}");
        assert_eq!(heard.device, lumepeer_media::audio_meter::DeviceState::Open);
        let hz = heard.loud_hz.unwrap();
        assert!((418..=462).contains(&hz), "a 440 Hz tone was heard as {hz} Hz");
        let captured = sent.snapshot();
        assert_eq!(captured.streams, 1);
        assert!(captured.loud > 0, "{captured:?}");
        audio.abort();
    }

    /// A microphone "hearing" a 660 Hz tone, at roughly the pace a device
    /// would.
    #[derive(Debug)]
    struct ToneMic {
        sample: u32,
    }

    impl lumepeer_media::capture_audio::MicCapturer for ToneMic {
        fn start(&mut self) -> lumepeer_media::error::Result<()> {
            Ok(())
        }

        fn next_chunk(
            &mut self,
        ) -> lumepeer_media::error::Result<lumepeer_media::capture_audio::PcmChunk> {
            std::thread::sleep(Duration::from_millis(5));
            let mut samples = Vec::new();
            for _ in 0..lumepeer_media::capture_audio::SAMPLES_PER_CHUNK {
                let t = f64::from(self.sample) / 48_000.0;
                #[allow(clippy::cast_possible_truncation, reason = "bounded to ±8000")]
                let value = ((t * 660.0 * std::f64::consts::TAU).sin() * 8_000.0) as i16;
                samples.extend([value, value]);
                self.sample += 1;
            }
            Ok(lumepeer_media::capture_audio::PcmChunk {
                samples,
                timestamp_us: 0,
            })
        }

        fn stop(&mut self) {}
    }

    /// ADR 0147: the guest's microphone belongs to its window, not to the
    /// media connection it first rode. When the picture's pass ends and the
    /// next one dials — a lost link, a codec change, a session coming back —
    /// the microphone opens its stream again on the new connection. It used
    /// to end with the first one while the button still said "on".
    #[tokio::test(flavor = "multi_thread")]
    async fn the_guest_microphone_follows_the_picture_onto_its_next_connection() {
        let host = lumepeer_net::PeerEndpoint::bind_local(iroh::SecretKey::generate())
            .await
            .unwrap();
        let guest = lumepeer_net::PeerEndpoint::bind_local(iroh::SecretKey::generate())
            .await
            .unwrap();
        let addr = host.addr();
        let (accepted_tx, mut accepted) = mpsc::channel(2);
        let accepting = tokio::spawn(async move {
            while let Some(Ok(connection)) = host.accept().await {
                if accepted_tx.send(connection).await.is_err() {
                    return;
                }
            }
        });

        let cell = Arc::new(Mutex::new(None));
        let sent = Arc::new(AudioMeter::default());
        let mic = tokio::spawn(run_mic_loop(
            Box::new(ToneMic { sample: 0 }),
            Arc::clone(&cell),
            "test-peer".to_owned(),
            Arc::clone(&sent),
        ));

        // The picture's first pass.
        let first = guest
            .connect(addr.clone(), lumepeer_net::ALPN_MEDIA)
            .await
            .unwrap();
        *cell.lock().unwrap() = Some(first.clone());
        let host_first = accepted.recv().await.unwrap();
        let mut stream = lumepeer_net::accept_tagged_media_stream(&host_first, STREAM_MIC)
            .await
            .unwrap()
            .unwrap();
        stream.read_frame().await.unwrap();

        // It ends, the way a lost link or a redial leaves it, and the next
        // pass dials a connection of its own.
        first.close(
            lumepeer_net::connection::CLOSE_MALFORMED.into(),
            b"the pass ended",
        );
        let second = guest.connect(addr, lumepeer_net::ALPN_MEDIA).await.unwrap();
        *cell.lock().unwrap() = Some(second.clone());
        let host_second = accepted.recv().await.unwrap();
        let Ok(followed) = tokio::time::timeout(
            Duration::from_secs(5),
            lumepeer_net::accept_tagged_media_stream(&host_second, STREAM_MIC),
        )
        .await
        else {
            panic!("the microphone never followed the picture onto its next connection");
        };
        let mut again = followed.unwrap().unwrap();

        // And what it carries there is still the microphone. Measured past
        // the first loud chunks: a fresh Opus stream fades in over its
        // first frame, and that onset is not the tone.
        let mut decoder = lumepeer_media::audio::OpusDecoder::new().unwrap();
        let mut tone = None;
        let mut loud = 0;
        for _ in 0..50 {
            let payload = again.read_frame().await.unwrap();
            let chunk = lumepeer_net::decode_audio_payload(&payload).unwrap();
            let samples = decoder.decode(&chunk.data).unwrap();
            if samples.iter().any(|s| s.unsigned_abs() > 4_000) {
                loud += 1;
                if loud == 3 {
                    tone = lumepeer_media::audio_meter::tone_hz(&samples, 2, 48_000);
                    break;
                }
            }
        }
        let Some(hz) = tone else {
            panic!("the second stream carried only silence");
        };
        assert!((627..=693).contains(&hz), "a 660 Hz tone arrived as {hz} Hz");
        assert_eq!(sent.snapshot().streams, 2, "one stream per connection");

        mic.abort();
        accepting.abort();
    }

    #[test]
    fn a_media_payload_round_trips() {
        let frame = EncodedFrame {
            keyframe: true,
            timestamp_us: 0x0102_0304_0506_0708,
            data: vec![9, 8, 7],
        };
        let decoded = decode_media_payload(&encode_media_payload(&frame)).unwrap();
        assert!(decoded.keyframe);
        assert_eq!(decoded.timestamp_us, frame.timestamp_us);
        assert_eq!(decoded.data, frame.data);
    }

    #[test]
    fn a_truncated_media_payload_is_refused_rather_than_panicking() {
        assert!(decode_media_payload(&[]).is_none());
        assert!(decode_media_payload(&[0u8; MEDIA_PAYLOAD_HEADER_BYTES]).is_none());
    }

    #[test]
    fn the_frame_response_carries_the_header_even_with_no_frame() {
        let bytes = encode_view_response(&ViewSlot::waiting(), false, false);
        assert_eq!(bytes.len(), VIEW_RESPONSE_HEADER_BYTES);
        assert_eq!(bytes[0], ViewStatus::Waiting.code());
        assert_eq!(bytes[1], 0);
    }

    #[test]
    fn the_frame_response_carries_pixels_and_the_live_flags() {
        let slot = ViewSlot {
            status: ViewStatus::Live,
            frame: Some(DecodedFrame {
                width: 2,
                height: 1,
                timestamp_us: 7,
                data: vec![1, 2, 3, 4, 5, 6, 7, 8],
            }),
        };
        let bytes = encode_view_response(&slot, true, false);
        assert_eq!(bytes[0], ViewStatus::Live.code());
        assert_eq!(bytes[1], VIEW_FLAG_INPUT);
        assert_eq!(u32::from_le_bytes(bytes[2..6].try_into().unwrap()), 2);
        assert_eq!(u32::from_le_bytes(bytes[6..10].try_into().unwrap()), 1);
        assert_eq!(u64::from_le_bytes(bytes[10..18].try_into().unwrap()), 7);
        assert_eq!(
            &bytes[VIEW_RESPONSE_HEADER_BYTES..],
            &[1, 2, 3, 4, 5, 6, 7, 8]
        );

        // The two flags are independent: a view-only session being recorded
        // must be able to say so without claiming an input grant it lacks.
        let recorded = encode_view_response(&slot, false, true);
        assert_eq!(recorded[1], VIEW_FLAG_RECORDING);
        let both = encode_view_response(&slot, true, true);
        assert_eq!(both[1], VIEW_FLAG_INPUT | VIEW_FLAG_RECORDING);
    }

    #[test]
    fn a_view_window_label_is_derived_from_the_pseudonymized_peer_label() {
        assert_eq!(window_label("ab12cd34"), "view-ab12cd34");
    }

    fn slot_with_frame(timestamp_us: u64) -> ViewSlot {
        ViewSlot {
            status: ViewStatus::Live,
            frame: Some(DecodedFrame {
                width: 2,
                height: 1,
                timestamp_us,
                data: vec![1, 2, 3, 4, 5, 6, 7, 8],
            }),
        }
    }

    #[test]
    fn polling_with_the_current_frames_timestamp_omits_the_pixels() {
        let current = slot_with_frame(7);
        let polled = slot_for_poll(&current, 7);
        assert_eq!(polled.status, ViewStatus::Live);
        assert!(
            polled.frame.is_none(),
            "the caller already has this picture"
        );
    }

    #[test]
    fn polling_with_a_stale_or_missing_timestamp_still_carries_the_picture() {
        let current = slot_with_frame(7);
        assert_eq!(slot_for_poll(&current, 6).frame, current.frame);
        assert_eq!(slot_for_poll(&current, 0).frame, current.frame);
    }

    #[test]
    fn the_zero_sentinel_never_matches_even_a_zero_timestamped_frame() {
        // `since_us == 0` means "the caller has nothing yet", not "the
        // caller already has the frame timestamped 0" — a real capture
        // timestamp of exactly 0 is what a fresh session's first frame
        // would carry, and it must still be sent.
        let current = slot_with_frame(0);
        assert!(slot_for_poll(&current, 0).frame.is_some());
    }

    fn encoded(keyframe: bool, timestamp_us: u64, bytes: usize) -> EncodedFrame {
        EncodedFrame {
            keyframe,
            timestamp_us,
            data: vec![0xab; bytes],
        }
    }

    #[test]
    fn a_chunk_response_carries_every_frame_in_order() {
        let frames = [encoded(true, 1_000, 3), encoded(false, 2_000, 5)];
        let bytes = encode_chunk_response(ViewStatus::Live, true, false, &frames, false, 0);

        assert_eq!(bytes[0], ViewStatus::Live.code());
        assert_eq!(bytes[1], VIEW_FLAG_INPUT);
        assert_eq!(u16::from_le_bytes([bytes[2], bytes[3]]), 2);

        let mut at = CHUNK_RESPONSE_HEADER_BYTES;
        for expected in &frames {
            assert_eq!(bytes[at] != 0, expected.keyframe);
            let timestamp = u64::from_le_bytes(bytes[at + 1..at + 9].try_into().unwrap());
            assert_eq!(timestamp, expected.timestamp_us);
            let length = u32::from_le_bytes(bytes[at + 9..at + 13].try_into().unwrap()) as usize;
            assert_eq!(length, expected.data.len());
            at += CHUNK_FRAME_HEADER_BYTES + length;
        }
        assert_eq!(at, bytes.len(), "no bytes left over and none missing");
    }

    /// The header has to ride an empty answer too: a still screen is exactly
    /// when a lowered grant or a started recording has to reach the window,
    /// and there is no frame to carry it.
    #[test]
    fn an_empty_chunk_response_still_carries_the_status_and_flags() {
        let bytes = encode_chunk_response(ViewStatus::SecureDesktop, false, true, &[], true, 0);
        assert_eq!(bytes.len(), CHUNK_RESPONSE_HEADER_BYTES);
        assert_eq!(bytes[0], ViewStatus::SecureDesktop.code());
        assert_eq!(bytes[1], VIEW_FLAG_RECORDING | VIEW_FLAG_DESYNC);
    }

    /// ADR 0067: the negotiated codec rides the last header byte of every
    /// answer, including an empty one, so the window sees a mid-session
    /// change (or the lack of one) on every poll rather than fetching it once.
    #[test]
    fn a_chunk_response_carries_the_negotiated_codec_byte() {
        let bytes = encode_chunk_response(ViewStatus::Live, true, false, &[], false, 3);
        assert_eq!(bytes.len(), CHUNK_RESPONSE_HEADER_BYTES);
        assert_eq!(bytes[8], 3);
    }

    #[tokio::test]
    async fn the_bitstream_feed_hands_back_every_frame_it_was_given() {
        let feed = BitstreamFeed::default();
        feed.push(encoded(true, 1, 4));
        feed.push(encoded(false, 2, 4));

        let (frames, desync) = feed.take(Duration::from_millis(50)).await;
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].timestamp_us, 1);
        assert_eq!(frames[1].timestamp_us, 2);
        assert!(!desync);
    }

    /// The whole reason this is a queue and not the single slot the RGBA path
    /// uses: an inter frame is meaningless without the frames it references,
    /// so keeping only the newest would corrupt everything after it.
    #[tokio::test]
    async fn a_frame_pushed_while_the_window_is_away_is_not_replaced_by_the_next() {
        let feed = BitstreamFeed::default();
        for i in 0..8 {
            feed.push(encoded(i == 0, i, 4));
        }
        let (frames, _) = feed.take(Duration::from_millis(50)).await;
        assert_eq!(frames.len(), 8);
    }

    #[tokio::test]
    async fn an_empty_feed_answers_with_nothing_rather_than_waiting_forever() {
        let feed = BitstreamFeed::default();
        let at = Instant::now();
        let (frames, desync) = feed.take(Duration::from_millis(30)).await;
        assert!(frames.is_empty());
        assert!(!desync);
        assert!(at.elapsed() >= Duration::from_millis(25));
    }

    #[tokio::test]
    async fn a_frame_arriving_during_the_wait_ends_it_immediately() {
        let feed = Arc::new(BitstreamFeed::default());
        let pusher = Arc::clone(&feed);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            pusher.push(encoded(true, 7, 4));
        });
        let at = Instant::now();
        let (frames, _) = feed.take(Duration::from_secs(5)).await;
        assert_eq!(frames.len(), 1);
        assert!(
            at.elapsed() < Duration::from_secs(1),
            "the call must return with the frame, not on the timeout"
        );
    }

    #[tokio::test]
    async fn a_queue_nobody_drains_is_bounded_and_says_so() {
        let feed = BitstreamFeed::default();
        for i in 0..(BITSTREAM_QUEUE_FRAMES + 2) {
            feed.push(encoded(false, i as u64, 16));
        }
        let (frames, desync) = feed.take(Duration::from_millis(50)).await;
        assert!(frames.len() <= BITSTREAM_QUEUE_FRAMES);
        assert!(
            desync,
            "a window that lost frames has to be told, or it paints garbage"
        );
    }

    #[tokio::test]
    async fn a_broken_stream_is_reported_once_and_then_forgotten() {
        let feed = BitstreamFeed::default();
        feed.desync();
        let (_, desync) = feed.take(Duration::from_millis(50)).await;
        assert!(desync);
        let (_, again) = feed.take(Duration::from_millis(10)).await;
        assert!(
            !again,
            "the window has already reset; saying so twice would reset it again"
        );
    }

    #[test]
    fn nobody_decodes_until_a_window_says_which_way() {
        // The sandboxed worker process must not be started for a session
        // whose window turns out to decode for itself (§8.1, ADR 0058).
        let feed = BitstreamFeed::default();
        assert_eq!(feed.path(), DecodePath::Undecided);
        feed.choose(DecodePath::Native);
        assert_eq!(feed.path(), DecodePath::Native);
        // A window whose own decoder gave up falls back by simply going back
        // to the frame poll, so the newest caller wins.
        feed.choose(DecodePath::Worker);
        assert_eq!(feed.path(), DecodePath::Worker);
    }

    /// Backdates the window so `due` answers without the test waiting out
    /// `ABR_FEEDBACK_INTERVAL_MS` of wall clock.
    fn window_elapsed(backlog: &mut Backlog) {
        backlog.started -= Duration::from_millis(u64::from(ABR_FEEDBACK_INTERVAL_MS));
    }

    #[test]
    fn a_link_that_took_every_frame_reports_no_congestion() {
        let mut backlog = Backlog::default();
        for _ in 0..30 {
            backlog.offered();
        }
        window_elapsed(&mut backlog);
        assert!(backlog.due().unwrap_or(f32::NAN).abs() < f32::EPSILON);
    }

    #[test]
    fn a_window_with_nothing_in_it_reports_no_congestion() {
        // No frames offered and none skipped is an idle screen, not a
        // congested link. Reporting anything else here is exactly how the
        // measurement this replaced walked an untouched desktop down to its
        // quality floor.
        let mut backlog = Backlog::default();
        window_elapsed(&mut backlog);
        assert!(backlog.due().unwrap_or(f32::NAN).abs() < f32::EPSILON);
    }

    /// A single skipped frame must not be a measurement: the controller
    /// halves the bitrate above `HEAVY_LOSS`, so a per-frame reading would
    /// turn one scheduling hiccup into half the picture quality — which is
    /// exactly what the timing-based signal this replaced did.
    #[test]
    fn nothing_is_reported_before_the_window_has_run() {
        let mut backlog = Backlog::default();
        backlog.skipped();
        assert!(backlog.due().is_none());
    }

    #[test]
    fn skipped_frames_are_reported_as_their_share_of_the_window() {
        let mut backlog = Backlog::default();
        for _ in 0..3 {
            backlog.offered();
        }
        backlog.skipped();
        window_elapsed(&mut backlog);
        assert!((backlog.due().unwrap_or(f32::NAN) - 0.25).abs() < 0.001);
    }

    #[test]
    fn a_link_that_took_nothing_saturates_at_full_loss() {
        let mut backlog = Backlog::default();
        for _ in 0..10 {
            backlog.skipped();
        }
        window_elapsed(&mut backlog);
        let loss = backlog.due().unwrap_or(f32::NAN);
        assert!(
            (loss - 1.0).abs() < f32::EPSILON,
            "loss must stay within AbrController's 0.0..=1.0 contract"
        );
    }

    #[test]
    fn reading_the_backlog_starts_the_next_window() {
        let mut backlog = Backlog::default();
        backlog.skipped();
        window_elapsed(&mut backlog);
        assert!((backlog.due().unwrap_or(f32::NAN) - 1.0).abs() < f32::EPSILON);
        // The next window is compared against the frames it reports on, not
        // against every frame of the session.
        backlog.offered();
        window_elapsed(&mut backlog);
        assert!(backlog.due().unwrap_or(f32::NAN).abs() < f32::EPSILON);
    }

    /// While the guest is reporting, the local window is not a second opinion
    /// — it is stale evidence that must not be carried into the next silence.
    #[test]
    fn a_guests_own_report_clears_the_local_window() {
        let mut backlog = Backlog::default();
        for _ in 0..10 {
            backlog.skipped();
        }
        backlog.reset();
        window_elapsed(&mut backlog);
        assert!(backlog.due().unwrap_or(f32::NAN).abs() < f32::EPSILON);
    }

    #[test]
    fn the_local_measurement_cannot_trip_the_goodput_branch() {
        // `goodput_kbps` and `sent_kbps` are the same local number on
        // purpose: nothing here crossed the network, so the controller's
        // "less arrived than was offered" check must have nothing to say.
        let feedback = backlog_feedback(0.0, 2_500);
        assert_eq!(feedback.goodput_kbps, feedback.sent_kbps);
        assert_eq!(feedback.rtt_ms, 0);
    }

    /// §18, docs/adr/0024: the two "no picture" states are their own terminal
    /// statuses, so the window can say the connection is fine and the host
    /// cannot send a picture — instead of `Failed`, which says the opposite.
    #[test]
    fn a_host_fault_maps_to_its_own_terminal_status() {
        assert_eq!(
            ViewStatus::from(MediaUnavailableReason::NoCaptureBackend),
            ViewStatus::NoCapture
        );
        assert_eq!(
            ViewStatus::from(MediaUnavailableReason::NoEncoder),
            ViewStatus::NoEncoder
        );
        assert_eq!(ViewStatus::NoCapture.code(), 4);
        assert_eq!(ViewStatus::NoEncoder.code(), 5);
        assert!(ViewStatus::NoCapture.is_terminal());
        assert!(ViewStatus::NoEncoder.is_terminal());
        assert!(ViewStatus::Failed.is_terminal());
        assert!(!ViewStatus::Waiting.is_terminal());
        assert!(!ViewStatus::Reconnecting.is_terminal());
        assert!(!ViewStatus::Live.is_terminal());
    }

    /// docs/bugs/11-uac-degradation.md: unlike the two faults above, the
    /// secure desktop is not terminal — the session and the encode loop
    /// behind it are both still alive.
    #[test]
    fn secure_desktop_maps_to_a_non_terminal_status() {
        assert_eq!(
            ViewStatus::from(MediaUnavailableReason::SecureDesktopActive),
            ViewStatus::SecureDesktop
        );
        assert_eq!(ViewStatus::SecureDesktop.code(), 6);
        assert!(!ViewStatus::SecureDesktop.is_terminal());
    }

    /// ADR 0049's central requirement, exercised at the one point in this
    /// file that actually checks it: without the grant, `secure_desktop_
    /// frame` never even reaches the service — proven here by the throttle
    /// clock never moving, since only a real attempt advances it — and with
    /// the grant revoked again, the very next call is refused just as
    /// promptly, with no separate teardown step needed.
    #[tokio::test(flavor = "multi_thread")]
    async fn secure_desktop_frame_is_gated_by_the_grant_before_anything_else() {
        let peer = iroh::SecretKey::generate().public();
        let control = EncodeControl::new(peer, None);
        let started = Instant::now();

        let mut next_attempt_at = Instant::now();
        assert!(
            secure_desktop_frame(&control, &mut next_attempt_at, started)
                .await
                .is_none()
        );
        let untouched = next_attempt_at;
        tokio::time::sleep(Duration::from_millis(5)).await;

        control.set_secure_desktop_allowed(true);
        let _ = secure_desktop_frame(&control, &mut next_attempt_at, started).await;
        assert!(
            next_attempt_at > untouched,
            "holding the grant must actually attempt a capture, which is what advances the throttle"
        );

        // Revoked again: even though the throttle above is not yet due, the
        // grant check runs first and refuses on its own — a revoke must not
        // have to wait out whatever throttle window happened to be open.
        control.set_secure_desktop_allowed(false);
        assert!(
            secure_desktop_frame(&control, &mut next_attempt_at, started)
                .await
                .is_none()
        );
    }

    /// Where a guest's click is routed does not depend on whether that guest
    /// is allowed to *see* the secure desktop. Both flags exist because they
    /// answer different questions, and conflating them sent every click of a
    /// session without the viewing grant to an injector whose `SendInput`
    /// could only answer `ERROR_ACCESS_DENIED`
    /// (`docs/bugs/15-secure-desktop-capture.md`).
    #[test]
    fn input_routing_follows_the_capturer_and_the_indicator_follows_the_grant() {
        let peer = iroh::SecretKey::generate().public();
        let control = EncodeControl::new(peer, None);

        // What the encode loop does on a `SecureDesktopActive` tick, for a
        // session that may not see the secure desktop.
        control.set_secure_desktop_blocked(true);
        control.set_secure_desktop_active(control.secure_desktop_allowed());
        assert!(
            control.secure_desktop_blocked(),
            "input must be routed to the helper whenever the desktop is out of reach"
        );
        assert!(
            !control.secure_desktop_active(),
            "a session without the viewing grant is not being shown anything"
        );

        // The same tick, once the host grants the viewing permission.
        control.set_secure_desktop_allowed(true);
        control.set_secure_desktop_blocked(true);
        control.set_secure_desktop_active(control.secure_desktop_allowed());
        assert!(control.secure_desktop_blocked());
        assert!(control.secure_desktop_active());

        // And a tick where ordinary capture works again clears both.
        control.set_secure_desktop_blocked(false);
        control.set_secure_desktop_active(false);
        assert!(!control.secure_desktop_blocked());
        assert!(!control.secure_desktop_active());
    }

    /// The actor writes the host's reason into the slot and then aborts the
    /// media task; an abort only lands at the next await, so the task's
    /// wind-down must not be able to paint over that reason.
    #[test]
    fn a_terminal_status_survives_the_media_task_winding_down() {
        let (slot, _rx) = watch::channel(ViewSlot::waiting());
        slot.send_modify(|current| current.status = ViewStatus::NoEncoder);

        set_status(&slot, ViewStatus::Reconnecting);
        assert_eq!(slot.borrow().status, ViewStatus::NoEncoder);
        set_status(&slot, ViewStatus::Failed);
        assert_eq!(slot.borrow().status, ViewStatus::NoEncoder);
    }

    /// A host reports what it knows: the capture backend at startup, the
    /// encoder only once a session has actually asked for one.
    #[test]
    fn media_health_reports_the_fault_that_comes_first() {
        let healthy = MediaHealth::healthy();
        assert!(healthy.can_capture());
        assert!(healthy.can_encode());
        assert_eq!(healthy.fault(), None);

        healthy.record(MediaUnavailableReason::NoEncoder);
        assert!(healthy.can_capture());
        assert!(!healthy.can_encode());
        assert_eq!(healthy.fault(), Some(MediaUnavailableReason::NoEncoder));

        let blind = MediaHealth::without_capture();
        assert!(!blind.can_capture());
        assert_eq!(
            blind.fault(),
            Some(MediaUnavailableReason::NoCaptureBackend)
        );
        // With no backend the encoder is never reached, so a stray encoder
        // fault must not become the reason the guest is given.
        blind.record(MediaUnavailableReason::NoEncoder);
        assert_eq!(
            blind.fault(),
            Some(MediaUnavailableReason::NoCaptureBackend)
        );
    }

    #[test]
    fn status_and_flags_stay_live_on_every_poll_even_when_pixels_are_skipped() {
        let mut current = slot_with_frame(7);
        current.status = ViewStatus::Reconnecting;
        let polled = slot_for_poll(&current, 7);
        assert_eq!(polled.status, ViewStatus::Reconnecting);
        let bytes = encode_view_response(&polled, true, true);
        assert_eq!(bytes[0], ViewStatus::Reconnecting.code());
        assert_eq!(bytes[1], VIEW_FLAG_INPUT | VIEW_FLAG_RECORDING);
        assert_eq!(
            bytes.len(),
            VIEW_RESPONSE_HEADER_BYTES,
            "no stale pixels ride along"
        );
    }

    /// An encoder that exists and refuses every frame, which is what an
    /// `openh264` fallback handed zero-copy GPU frames did on 2026-09-30
    /// (ADR 0135).
    struct RefusingEncoder;

    impl VideoEncoder for RefusingEncoder {
        fn encode(&mut self, _frame: &Frame) -> lumepeer_media::Result<EncodedFrame> {
            Err(MediaError::Encode("frame buffer is short".to_owned()))
        }

        fn set_bitrate(&mut self, _bitrate_kbps: u32) -> lumepeer_media::Result<()> {
            Ok(())
        }

        fn request_keyframe(&mut self) -> lumepeer_media::Result<()> {
            Ok(())
        }

        fn kind(&self) -> lumepeer_media::encode::EncoderKind {
            lumepeer_media::encode::EncoderKind::SoftwareOpenH264
        }
    }

    /// A capturer that hands over a picture every time it is asked, so every
    /// tick of the loop reaches the encoder.
    #[derive(Debug)]
    struct ChangingCapturer;

    impl lumepeer_media::capture::ScreenCapturer for ChangingCapturer {
        fn start(
            &mut self,
            _target: lumepeer_media::capture::CaptureTarget,
        ) -> lumepeer_media::Result<()> {
            Ok(())
        }

        fn next_frame(&mut self) -> lumepeer_media::Result<Option<Frame>> {
            Ok(Some(Frame::cpu(
                64,
                64,
                PixelFormat::Bgra8,
                0,
                vec![0; 64 * 64 * 4],
            )))
        }

        fn stop(&mut self) {}

        fn input_capability(&self) -> lumepeer_media::capture::InputCapability {
            lumepeer_media::capture::InputCapability::None
        }
    }

    /// ADR 0135: a refused frame used to be skipped and the next one tried,
    /// for as long as the session lasted, and the guest was never told. The
    /// loop now gives up after a bounded run, says why, and ends.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_encoder_that_refuses_every_frame_ends_the_loop_and_says_so() {
        let host = lumepeer_net::PeerEndpoint::bind_local(iroh::SecretKey::generate())
            .await
            .unwrap();
        let guest = lumepeer_net::PeerEndpoint::bind_local(iroh::SecretKey::generate())
            .await
            .unwrap();
        let (dialed, accepted) = tokio::join!(
            guest.connect(host.addr(), lumepeer_net::ALPN_MEDIA),
            host.accept()
        );
        // Held so the media connection stays up for the whole run: a closed
        // stream would end the loop down a different path.
        let _guest_side = dialed.unwrap();
        let connection = accepted.unwrap().unwrap();

        let peer = guest.node_id();
        let capture: SharedCapture = Arc::new(Mutex::new(CaptureController::new(
            Box::new(ChangingCapturer),
            lumepeer_media::capture::CaptureTarget::PrimaryDisplay,
        )));
        lock_capture(&capture).add_viewer(peer).unwrap();
        let (faults, mut faults_rx) = mpsc::channel(4);
        let task = spawn_encode_loop_with(
            connection,
            capture,
            Arc::new(Mutex::new(None)),
            "test-peer".to_owned(),
            VideoCodec::H264,
            peer,
            faults,
            EncodeControl::new(peer, None),
            |_| Ok(Box::new(RefusingEncoder)),
        );

        // Two seconds of frames at the default rate; the rest is headroom
        // for a loaded machine. A timeout here is the old behaviour: the loop
        // skipping refused frames for as long as the session lasts.
        let fault = tokio::time::timeout(Duration::from_secs(30), faults_rx.recv())
            .await
            .unwrap();
        assert_eq!(fault, Some((peer, MediaUnavailableReason::EncoderFailed)));
        // And having said so, the loop is over rather than still encoding.
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
    }

    /// ADR 0135: a refusal is blamed on the guest's size only when the next
    /// frame, at the picture budget, goes through.
    #[test]
    fn a_refusal_the_budget_shares_is_not_the_guest_sizes() {
        let on_trial = GuestSize::Honoured.after_refusal(true);
        assert_eq!(on_trial, GuestSize::OnTrial);
        assert!(!on_trial.honoured(), "the next frame tries the budget");
        assert_eq!(on_trial.after_frame(), GuestSize::Refused);

        let cleared = on_trial.after_refusal(false);
        assert_eq!(cleared, GuestSize::Cleared);
        assert!(cleared.honoured(), "the size was not the cause");
        assert_eq!(
            cleared.after_refusal(true),
            GuestSize::Cleared,
            "not put on trial again within the same run"
        );
        assert_eq!(cleared.after_frame(), GuestSize::Honoured);

        assert_eq!(
            GuestSize::Honoured.after_refusal(false),
            GuestSize::Honoured,
            "a guest that named no size has nothing to withdraw"
        );
    }

    /// ADR 0138: the cursor goes out at the scale of the picture it is drawn
    /// over, once per shape and again whenever the picture changes size.
    #[test]
    fn the_cursor_is_sent_at_the_picture_scale_and_again_when_that_changes() {
        let mut cursor = PictureCursor::default();
        assert!(
            cursor.due((3840, 2160), (1920, 1080)).is_none(),
            "no shape yet, nothing to send"
        );

        cursor.changed(CursorShapeData {
            width: 64,
            height: 64,
            hotspot_x: 20,
            hotspot_y: 30,
            rgba: vec![0xFF; 64 * 64 * 4],
        });
        // A new shape is sent, at the picture's scale.
        let sent = cursor.due((3840, 2160), (1920, 1080)).unwrap();
        assert_eq!((sent.width, sent.height), (32, 32));
        assert_eq!((sent.hotspot_x, sent.hotspot_y), (10, 15));
        assert!(
            cursor.due((3840, 2160), (1920, 1080)).is_none(),
            "the guest already has it at this scale"
        );

        // The guest switched to `quality`: same cursor, a full-size picture,
        // and the cursor goes out again.
        let resent = cursor.due((3840, 2160), (3840, 2160)).unwrap();
        assert_eq!((resent.width, resent.height), (64, 64));
        assert_eq!((resent.hotspot_x, resent.hotspot_y), (20, 30));
    }

    /// ADR 0150: an arrow turning into an I-beam over a still screen goes
    /// out without a frame, at the size of the last picture sent.
    #[test]
    fn a_cursor_that_changes_over_a_still_screen_is_sent_at_the_last_pictures_scale() {
        let shape = |width: u16| CursorShapeData {
            width,
            height: 64,
            hotspot_x: 0,
            hotspot_y: 0,
            rgba: vec![0xFF; usize::from(width) * 64 * 4],
        };
        let mut cursor = PictureCursor::default();
        cursor.changed(shape(64));
        assert!(
            cursor.due_for_last_picture().is_none(),
            "no picture yet: the first frame carries the cursor"
        );
        assert!(cursor.due((3840, 2160), (1920, 1080)).is_some());
        assert!(
            cursor.due_for_last_picture().is_none(),
            "the guest already has this one"
        );

        cursor.changed(shape(32));
        let sent = cursor.due_for_last_picture().unwrap();
        assert_eq!((sent.width, sent.height), (16, 32));
        assert!(cursor.due_for_last_picture().is_none());
    }

    /// ADR 0135: one log line per interval, however fast frames are refused,
    /// and each line counts the refusals it stands for.
    #[test]
    fn refused_frames_are_logged_once_per_interval_with_a_count() {
        let mut refusals = EncodeRefusals::default();
        let start = Instant::now();
        assert_eq!(refusals.refused(), 1);
        assert_eq!(
            refusals.due(start),
            Some(1),
            "the first refusal is logged at once"
        );
        for _ in 0..29 {
            refusals.refused();
            assert_eq!(refusals.due(start + Duration::from_millis(500)), None);
        }
        refusals.took();
        assert_eq!(
            refusals.refused(),
            1,
            "a frame the encoder took ends the run"
        );
        assert_eq!(
            refusals.due(start + ENCODE_REFUSAL_LOG_INTERVAL),
            Some(30),
            "the next line counts every refusal since the last one"
        );
    }

    /// ADR 0135: the host does have an encoder, so a failed one says nothing
    /// about the next session.
    #[test]
    fn a_failed_encoder_is_not_recorded_as_a_missing_one() {
        let health = MediaHealth::healthy();
        health.record(MediaUnavailableReason::EncoderFailed);
        assert!(health.can_encode());
        assert_eq!(health.fault(), None);
    }

    fn ms(millis: u64) -> Duration {
        Duration::from_millis(millis)
    }

    /// ADR 0139: a guest that sends no acknowledgements — every guest before
    /// this one — is never held back, and nothing is kept for it.
    #[test]
    fn frame_acks_hold_nothing_until_the_guest_acknowledges() {
        let acks = FrameAcks::default();
        let start = Instant::now();
        for _ in 0..100 {
            acks.sent(start);
        }
        assert!(acks.admits(start + ms(10_000), ms(16)));
        assert!(
            acks.lock().in_flight.is_empty(),
            "nothing kept for an old guest"
        );
    }

    /// ADR 0139: once the link's best case is known, a frame is held back
    /// exactly while the oldest unacknowledged one has been on its way longer
    /// than that and the slack.
    #[test]
    fn frame_acks_hold_a_frame_while_the_link_runs_behind_its_best_case() {
        let acks = FrameAcks::default();
        acks.opened();
        let start = Instant::now();
        acks.sent(start);
        assert!(
            acks.admits(start + ms(500), ms(16)),
            "nothing is held before anything has been measured"
        );
        // The first frame took 40 ms: that is the link's best case so far.
        acks.acked(1, start + ms(40));
        assert_eq!(acks.lock().best(start + ms(40)), Some(ms(40)));
        assert!(acks.admits(start + ms(41), ms(16)), "nothing is in flight");

        acks.sent(start + ms(50));
        assert!(acks.admits(start + ms(50 + 40 + 16), ms(16)));
        assert!(
            !acks.admits(start + ms(50 + 40 + 17), ms(16)),
            "a frame on its way longer than the best case and the slack is queued"
        );
        acks.acked(2, start + ms(150));
        assert!(
            acks.admits(start + ms(150), ms(16)),
            "acknowledged, the link is free"
        );
        assert_eq!(
            acks.lock().best(start + ms(150)),
            Some(ms(40)),
            "a slower acknowledgement does not raise the best case"
        );
    }

    /// ADR 0139: an acknowledgement that covers several frames times only the
    /// newest of them, and one that repeats an older count changes nothing.
    #[test]
    fn frame_acks_time_only_the_newest_frame_an_acknowledgement_covers() {
        let acks = FrameAcks::default();
        acks.opened();
        let start = Instant::now();
        acks.sent(start);
        acks.sent(start + ms(30));
        acks.sent(start + ms(60));
        acks.acked(3, start + ms(80));
        assert_eq!(acks.lock().best(start + ms(80)), Some(ms(20)));
        assert!(acks.lock().in_flight.is_empty());

        acks.sent(start + ms(100));
        acks.acked(2, start + ms(500));
        assert_eq!(
            acks.lock().in_flight.len(),
            1,
            "a stale count acknowledges nothing"
        );
        assert_eq!(acks.lock().best(start + ms(500)), Some(ms(20)));
    }

    /// ADR 0139: the best case is forgotten after its window, so a session
    /// moved onto a slower path is not held to the old one's round trip.
    #[test]
    fn frame_acks_forget_a_best_case_older_than_its_window() {
        let acks = FrameAcks::default();
        acks.opened();
        let start = Instant::now();
        acks.sent(start);
        acks.acked(1, start + ms(10));
        let later = start + ms(10 + MEDIA_ACK_BEST_WINDOW_MS + 1);
        acks.sent(later);
        acks.acked(2, later + ms(90));
        assert_eq!(acks.lock().best(later + ms(90)), Some(ms(90)));
    }

    /// ADR 0139: when the acknowledgements stop, so does the holding back.
    #[test]
    fn frame_acks_hold_nothing_once_their_stream_ends() {
        let acks = FrameAcks::default();
        acks.opened();
        let start = Instant::now();
        acks.sent(start);
        acks.acked(1, start + ms(5));
        acks.sent(start + ms(10));
        assert!(!acks.admits(start + ms(500), ms(16)));
        acks.closed();
        assert!(acks.admits(start + ms(500), ms(16)));
        acks.sent(start + ms(600));
        assert!(acks.lock().in_flight.is_empty());
    }

    /// ADR 0139: the slack is two frame intervals, held between its bounds.
    #[test]
    fn the_queue_slack_is_two_frames_within_its_bounds() {
        assert_eq!(
            queue_slack(frame_interval(144)),
            ms(MEDIA_QUEUE_SLACK_MIN_MS)
        );
        assert_eq!(queue_slack(frame_interval(60)), frame_interval(60) * 2);
        assert_eq!(
            queue_slack(frame_interval(30)),
            ms(MEDIA_QUEUE_SLACK_MAX_MS)
        );
    }

    /// ADR 0144: a link whose acknowledgements arrive steadily keeps the
    /// slack of ADR 0139; one whose round trip swings is given twice its
    /// swing, up to the bound.
    #[test]
    fn a_jittery_link_widens_the_slack_up_to_its_bound() {
        let interval = frame_interval(30);
        let steady = FrameAcks::default();
        steady.opened();
        let start = Instant::now();
        for frame in 0..40u64 {
            let sent = start + ms(frame * 33);
            steady.sent(sent);
            steady.acked(frame + 1, sent + ms(110));
        }
        assert_eq!(steady.slack(interval), queue_slack(interval));

        // 215 to 311 ms, as the 2026-10-07 path measured.
        let jittery = FrameAcks::default();
        jittery.opened();
        for frame in 0..40u64 {
            let sent = start + ms(frame * 33);
            jittery.sent(sent);
            let delay = if frame % 2 == 0 { 215 } else { 311 };
            jittery.acked(frame + 1, sent + ms(delay));
        }
        let slack = jittery.slack(interval);
        assert!(
            slack > ms(MEDIA_QUEUE_SLACK_MAX_MS) && slack <= ms(MEDIA_QUEUE_JITTER_SLACK_MAX_MS),
            "a link swinging by 96 ms got a slack of {slack:?}"
        );

        let wild = FrameAcks::default();
        wild.opened();
        for frame in 0..40u64 {
            let sent = start + ms(frame * 33);
            wild.sent(sent);
            let delay = if frame % 2 == 0 { 100 } else { 1_100 };
            wild.acked(frame + 1, sent + ms(delay));
        }
        assert_eq!(wild.slack(interval), ms(MEDIA_QUEUE_JITTER_SLACK_MAX_MS));
    }

    /// A capturer whose every frame is stamped with when it was taken, against
    /// `epoch`, so the far end can tell how old a picture is when it lands.
    #[derive(Debug)]
    struct StampingCapturer {
        epoch: Instant,
    }

    impl lumepeer_media::capture::ScreenCapturer for StampingCapturer {
        fn start(
            &mut self,
            _target: lumepeer_media::capture::CaptureTarget,
        ) -> lumepeer_media::Result<()> {
            Ok(())
        }

        fn next_frame(&mut self) -> lumepeer_media::Result<Option<Frame>> {
            let stamp = u64::try_from(self.epoch.elapsed().as_micros()).unwrap();
            Ok(Some(Frame::cpu(
                64,
                64,
                PixelFormat::Bgra8,
                stamp,
                vec![0; 64 * 64 * 4],
            )))
        }

        fn stop(&mut self) {}

        fn input_capability(&self) -> lumepeer_media::capture::InputCapability {
            lumepeer_media::capture::InputCapability::None
        }
    }

    /// An encoder that turns every picture into a frame of `bytes` bytes and
    /// keeps its timestamp, like a real one does.
    struct StampEncoder {
        bytes: usize,
    }

    impl VideoEncoder for StampEncoder {
        fn encode(&mut self, frame: &Frame) -> lumepeer_media::Result<EncodedFrame> {
            Ok(EncodedFrame {
                keyframe: false,
                timestamp_us: frame.timestamp_us,
                data: vec![7; self.bytes],
            })
        }

        fn set_bitrate(&mut self, _bitrate_kbps: u32) -> lumepeer_media::Result<()> {
            Ok(())
        }

        fn request_keyframe(&mut self) -> lumepeer_media::Result<()> {
            Ok(())
        }

        fn kind(&self) -> lumepeer_media::encode::EncoderKind {
            lumepeer_media::encode::EncoderKind::SoftwareOpenH264
        }
    }

    /// How old the pictures a slow guest is shown get, with and without it
    /// acknowledging them: the encode loop at its own pace, the guest taking
    /// one frame every 50 ms — a link slower than the encoder, in miniature.
    ///
    /// Returns the age of the last picture read, after `reads` of them.
    async fn age_at_a_slow_guest(acknowledging: bool, reads: u32) -> Duration {
        let host = lumepeer_net::PeerEndpoint::bind_local(iroh::SecretKey::generate())
            .await
            .unwrap();
        let guest = lumepeer_net::PeerEndpoint::bind_local(iroh::SecretKey::generate())
            .await
            .unwrap();
        let (dialed, accepted) = tokio::join!(
            guest.connect(host.addr(), lumepeer_net::ALPN_MEDIA),
            host.accept()
        );
        let guest_side = dialed.unwrap();
        let host_side = accepted.unwrap().unwrap();

        let epoch = Instant::now();
        let peer = guest.node_id();
        let capture: SharedCapture = Arc::new(Mutex::new(CaptureController::new(
            Box::new(StampingCapturer { epoch }),
            lumepeer_media::capture::CaptureTarget::PrimaryDisplay,
        )));
        lock_capture(&capture).add_viewer(peer).unwrap();
        let control = EncodeControl::new(peer, None);
        let streams = spawn_guest_streams(
            host_side.clone(),
            "test-peer".to_owned(),
            control.acks(),
            Arc::default(),
        );
        let (faults, _faults_rx) = mpsc::channel(4);
        let encode = spawn_encode_loop_with(
            host_side,
            capture,
            Arc::new(Mutex::new(None)),
            "test-peer".to_owned(),
            VideoCodec::H264,
            peer,
            faults,
            control,
            |_| Ok(Box::new(StampEncoder { bytes: 4_000 })),
        );

        let mut reader = accept_media_stream(&guest_side).await.unwrap();
        let (received_tx, received_rx) = watch::channel(0u64);
        let _acks = acknowledging.then(|| {
            AbortOnDrop(spawn_frame_acks(
                guest_side.clone(),
                received_rx,
                "test-peer".to_owned(),
            ))
        });
        let mut age = Duration::ZERO;
        for read in 1..=reads {
            let payload = reader.read_frame().await.unwrap();
            received_tx.send_replace(u64::from(read));
            let frame = decode_media_payload(&payload).unwrap();
            age = epoch
                .elapsed()
                .saturating_sub(Duration::from_micros(frame.timestamp_us));
            tokio::time::sleep(ms(50)).await;
        }
        encode.abort();
        streams.abort();
        age
    }

    /// ADR 0139: without acknowledgements the host writes ahead of a guest
    /// that cannot keep up, and every picture it is shown is older than the
    /// last; with them the host waits, and every picture is fresh.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_acknowledging_guest_is_shown_fresh_pictures_on_a_slow_link() {
        let queued = age_at_a_slow_guest(false, 40).await;
        let fresh = age_at_a_slow_guest(true, 40).await;
        eprintln!(
            "a slow guest's last picture: {queued:?} old without acknowledgements, {fresh:?} with"
        );
        assert!(
            queued > ms(800),
            "the test's link is not slow enough to queue anything: {queued:?}"
        );
        assert!(
            fresh < ms(400),
            "an acknowledging guest was shown a picture {fresh:?} old (without: {queued:?})"
        );
    }

    /// An encoder that takes `took` over every picture, and hands back an
    /// empty frame for every `skip_every`-th — what `openh264` does when its
    /// rate control skips one.
    struct SlowEncoder {
        took: Duration,
        skip_every: u32,
        count: u32,
    }

    impl VideoEncoder for SlowEncoder {
        fn encode(&mut self, frame: &Frame) -> lumepeer_media::Result<EncodedFrame> {
            std::thread::sleep(self.took);
            self.count += 1;
            let skipped = self.skip_every > 0 && self.count.is_multiple_of(self.skip_every);
            Ok(EncodedFrame {
                keyframe: self.count == 1,
                timestamp_us: frame.timestamp_us,
                data: if skipped { Vec::new() } else { vec![7; 2_000] },
            })
        }

        fn set_bitrate(&mut self, _bitrate_kbps: u32) -> lumepeer_media::Result<()> {
            Ok(())
        }

        fn request_keyframe(&mut self) -> lumepeer_media::Result<()> {
            Ok(())
        }

        fn kind(&self) -> lumepeer_media::encode::EncoderKind {
            lumepeer_media::encode::EncoderKind::SoftwareOpenH264
        }
    }

    /// Serves a host encode loop with `encoder` to a guest that reads
    /// `reads` payloads as fast as they come, and returns them with how long
    /// that took.
    async fn read_from_a_host(encoder: SlowEncoder, reads: usize) -> (Vec<Vec<u8>>, Duration) {
        let host = lumepeer_net::PeerEndpoint::bind_local(iroh::SecretKey::generate())
            .await
            .unwrap();
        let guest = lumepeer_net::PeerEndpoint::bind_local(iroh::SecretKey::generate())
            .await
            .unwrap();
        let (dialed, accepted) = tokio::join!(
            guest.connect(host.addr(), lumepeer_net::ALPN_MEDIA),
            host.accept()
        );
        let guest_side = dialed.unwrap();
        let host_side = accepted.unwrap().unwrap();

        let peer = guest.node_id();
        let capture: SharedCapture = Arc::new(Mutex::new(CaptureController::new(
            Box::new(ChangingCapturer),
            lumepeer_media::capture::CaptureTarget::PrimaryDisplay,
        )));
        lock_capture(&capture).add_viewer(peer).unwrap();
        let (faults, _faults_rx) = mpsc::channel(4);
        let encoder = std::sync::Mutex::new(Some(encoder));
        let task = spawn_encode_loop_with(
            host_side,
            capture,
            Arc::new(Mutex::new(None)),
            "test-peer".to_owned(),
            VideoCodec::H264,
            peer,
            faults,
            EncodeControl::new(peer, None),
            move |_| {
                let encoder = encoder.lock().unwrap().take().unwrap();
                Ok(Box::new(encoder) as Box<dyn VideoEncoder>)
            },
        );

        let mut reader = accept_media_stream(&guest_side).await.unwrap();
        // The first payload waits on the connection and the first capture;
        // the clock starts once frames are flowing.
        let mut payloads = vec![reader.read_frame().await.unwrap()];
        let started = Instant::now();
        while payloads.len() <= reads {
            payloads.push(reader.read_frame().await.unwrap());
        }
        let took = started.elapsed();
        task.abort();
        (payloads, took)
    }

    /// ADR 0144: an encoder slower than the frame interval used to be
    /// followed by a skipped interval after every frame, because the writer
    /// it had just handed the frame to had not run yet. Now the loop waits
    /// for it, and the frame rate is the encoder's own.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_encoder_slower_than_the_interval_sets_the_frame_rate_alone() {
        let took = ms(30);
        let (_, elapsed) = read_from_a_host(
            SlowEncoder {
                took,
                skip_every: 0,
                count: 0,
            },
            30,
        )
        .await;
        let per_frame = elapsed / 30;
        eprintln!("an encoder taking {took:?} a frame delivered one every {per_frame:?}");
        // The default 60 fps interval is 16.7 ms: a skipped interval after
        // every frame made it 47 ms.
        assert!(
            per_frame < ms(40),
            "a frame every {per_frame:?} from an encoder that takes {took:?}"
        );
    }

    /// ADR 0144: a preset named before the stream opens is the rate the
    /// encoder is built for, so the stream opens with one keyframe rather
    /// than one for the display's rate and another for the preset's.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_preset_named_before_the_stream_builds_the_encoder_once() {
        let host = lumepeer_net::PeerEndpoint::bind_local(iroh::SecretKey::generate())
            .await
            .unwrap();
        let guest = lumepeer_net::PeerEndpoint::bind_local(iroh::SecretKey::generate())
            .await
            .unwrap();
        let (dialed, accepted) = tokio::join!(
            guest.connect(host.addr(), lumepeer_net::ALPN_MEDIA),
            host.accept()
        );
        let guest_side = dialed.unwrap();
        let host_side = accepted.unwrap().unwrap();
        let peer = guest.node_id();
        let capture: SharedCapture = Arc::new(Mutex::new(CaptureController::new(
            Box::new(ChangingCapturer),
            lumepeer_media::capture::CaptureTarget::PrimaryDisplay,
        )));
        lock_capture(&capture).add_viewer(peer).unwrap();
        let control = EncodeControl::new(peer, None);
        control.set_manual_cap(Some(100));
        control.set_fps_cap(Some(30));
        let builds = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&builds);
        let (faults, _faults_rx) = mpsc::channel(4);
        let task = spawn_encode_loop_with(
            host_side,
            capture,
            Arc::new(Mutex::new(None)),
            "test-peer".to_owned(),
            VideoCodec::H264,
            peer,
            faults,
            control,
            move |config| {
                counted.fetch_add(1, Ordering::Relaxed);
                assert_eq!(
                    config.fps, 30,
                    "built for the display's rate, not the preset's"
                );
                Ok(Box::new(StampEncoder { bytes: 500 }) as Box<dyn VideoEncoder>)
            },
        );
        let mut reader = accept_media_stream(&guest_side).await.unwrap();
        for _ in 0..10 {
            reader.read_frame().await.unwrap();
        }
        task.abort();
        assert_eq!(builds.load(Ordering::Relaxed), 1);
    }

    /// ADR 0144: a frame the encoder skipped is not sent at all.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_frame_the_encoder_skipped_is_not_sent() {
        let (payloads, _) = read_from_a_host(
            SlowEncoder {
                took: Duration::ZERO,
                skip_every: 2,
                count: 0,
            },
            10,
        )
        .await;
        for payload in &payloads {
            assert!(
                decode_media_payload(payload).is_some(),
                "a payload of {} bytes reached the guest",
                payload.len()
            );
        }
    }
}
