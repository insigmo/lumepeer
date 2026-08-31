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
//! - The **host** loop pulls frames out of the shared [`CaptureController`],
//!   encodes them and writes them onto that peer's `rd/media/1` stream. The
//!   controller is the gate: with no viewer it refuses to produce a frame, so
//!   the loop ends by itself when the last viewer leaves (§8.1, §11).
//! - The **guest** loop dials `rd/media/1`, decodes in the sandboxed worker
//!   process of §11.3 and keeps only the newest picture in a single-slot
//!   `watch` channel. Reordering is the jitter buffer's problem upstream of
//!   decode; the display side only ever wants the latest frame.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use iroh::EndpointAddr;
use iroh::endpoint::Connection;
use lumepeer_core::NodeId;
use lumepeer_core::constants::{
    ABR_FEEDBACK_INTERVAL_MS, ABR_FEEDBACK_STALE_AFTER_MS, AUDIO_MAX_FRAME_BYTES,
    KEYFRAME_MIN_INTERVAL_MS, MAX_MEDIA_FRAME_BYTES, MEDIA_REDIAL_BACKOFF_MS,
    RECONNECT_WINDOW_SECS,
};
use lumepeer_core::protocol::{CursorShapeData, MediaUnavailableReason};
use lumepeer_media::abr::{AbrController, QualityTarget, ReceiverFeedback, effective_scale};
use lumepeer_media::capture::{CaptureController, InputInjector};
use lumepeer_media::decode::{DecodedFrame, DecoderHandle};
use lumepeer_media::encode::{EncodedFrame, EncoderConfig, select_encoder};
use lumepeer_media::error::MediaError;
use lumepeer_media::scale::{fit_within_budget, scale_to_percent};
use lumepeer_net::{PeerEndpoint, STREAM_MIC, accept_media_stream, open_media_stream};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

/// Milliseconds in a second, for turning a frame rate into a delay.
const MILLIS_PER_SEC: u64 = 1_000;

/// Permille, for turning a share of frames into the integer the wire carries.
const PERMILLE: u64 = 1_000;

/// Bits in a byte, for turning received bytes into kilobits per second.
const BITS_PER_BYTE: u64 = 8;

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
    /// The guest's manual scale ceiling, if it asked for one (§11; D7,
    /// docs/bugs/13-stream-resolution.md task 2). Combined with the ABR
    /// target by `lumepeer_media::abr::effective_scale`, never applied on
    /// its own — a ceiling this loop reads is not a target ABR stops
    /// adapting around.
    manual_cap: Arc<Mutex<Option<u32>>>,
}

impl EncodeControl {
    /// A control surface for `peer`, with the cursor channel on only when the
    /// guest said it will draw the cursor itself (§11; `FEATURE_CURSOR_SHAPE`).
    #[must_use]
    pub fn new(peer: NodeId, cursors: Option<mpsc::Sender<(NodeId, CursorShapeData)>>) -> Self {
        Self {
            peer,
            cursors,
            keyframe: Arc::new(AtomicBool::new(false)),
            feedback: Arc::new(Mutex::new(None)),
            target: Arc::new(Mutex::new(QualityTarget::default())),
            manual_cap: Arc::new(Mutex::new(None)),
        }
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

    /// The guest's manual scale ceiling right now, if any (§11; D7).
    #[must_use]
    pub fn manual_cap(&self) -> Option<u32> {
        *self
            .manual_cap
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Sets the guest's manual scale ceiling, replacing whatever was there.
    ///
    /// Returns whether this actually changed the ceiling, which is what the
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
    /// (`docs/bugs/11-uac-degradation.md`).
    pub fn record(&self, reason: MediaUnavailableReason) {
        match reason {
            MediaUnavailableReason::NoCaptureBackend => {
                self.capture_missing.store(true, Ordering::Relaxed);
            }
            MediaUnavailableReason::NoEncoder => {
                self.encoder_missing.store(true, Ordering::Relaxed);
            }
            MediaUnavailableReason::SecureDesktopActive => {}
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
fn encode_media_payload(frame: &EncodedFrame) -> Vec<u8> {
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
}

impl From<MediaUnavailableReason> for ViewStatus {
    fn from(reason: MediaUnavailableReason) -> Self {
        match reason {
            MediaUnavailableReason::NoCaptureBackend => Self::NoCapture,
            MediaUnavailableReason::NoEncoder => Self::NoEncoder,
            MediaUnavailableReason::SecureDesktopActive => Self::SecureDesktop,
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
        }
    }

    /// Whether the pipeline behind this status has stopped for good, so the
    /// guest has nothing left to wait for.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Failed | Self::NoCapture | Self::NoEncoder)
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

/// How the actor opens and closes the guest's remote-view window.
///
/// A trait rather than a bare `tauri::AppHandle` so the actor's own tests can
/// drive the full grant/revoke cycle without a Tauri runtime; the production
/// implementation ([`TauriViewWindows`]) is the one built from the `AppHandle`
/// that `spawn_actor` receives.
pub trait ViewWindows: std::fmt::Debug + Send + Sync {
    /// Opens the view window `label` onto `peer_label`.
    fn open(&self, label: &str, peer_label: &str, input: bool);
    /// Closes the view window `label`, if it is open.
    fn close(&self, label: &str);
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
    fn open(&self, label: &str, _peer_label: &str, _input: bool) {
        tracing::debug!(window = %label, "no webview attached: not opening a view window");
    }

    fn close(&self, _label: &str) {}
}

/// Default width of a freshly opened view window.
const VIEW_WINDOW_WIDTH: f64 = 1280.0;
/// Default height of a freshly opened view window.
const VIEW_WINDOW_HEIGHT: f64 = 720.0;

/// [`ViewWindows`] backed by the real Tauri application.
#[derive(Debug)]
pub struct TauriViewWindows {
    app: tauri::AppHandle,
}

impl TauriViewWindows {
    /// Wraps the handle `spawn_actor` was given.
    #[must_use]
    pub const fn new(app: tauri::AppHandle) -> Self {
        Self { app }
    }
}

impl ViewWindows for TauriViewWindows {
    fn open(&self, label: &str, peer_label: &str, input: bool) {
        // Only the pseudonymized label ever reaches a URL (§15), and it is hex
        // from `peer_tag`, so there is nothing to escape.
        let url = format!("view.html?peer={peer_label}&input={}", u8::from(input));
        let label = label.to_owned();
        let app = self.app.clone();
        // Window creation must happen on the platform's main thread; the actor
        // runs on a tokio worker.
        let queued = self.app.run_on_main_thread(move || {
            let built = tauri::WebviewWindowBuilder::new(
                &app,
                label.clone(),
                tauri::WebviewUrl::App(url.into()),
            )
            .title("Lumepeer — remote screen")
            .inner_size(VIEW_WINDOW_WIDTH, VIEW_WINDOW_HEIGHT)
            .resizable(true)
            .build();
            match built {
                Ok(_) => tracing::info!(window = %label, input, "view window opened"),
                Err(error) => {
                    tracing::warn!(window = %label, %error, "cannot open the view window");
                }
            }
        });
        if let Err(error) = queued {
            tracing::warn!(%error, "cannot reach the main thread to open a view window");
        }
    }

    fn close(&self, label: &str) {
        let label = label.to_owned();
        let app = self.app.clone();
        let queued = self.app.run_on_main_thread(move || {
            use tauri::Manager as _;
            if let Some(window) = app.get_webview_window(&label)
                && let Err(error) = window.close()
            {
                tracing::warn!(window = %label, %error, "cannot close the view window");
            }
        });
        if let Err(error) = queued {
            tracing::warn!(%error, "cannot reach the main thread to close a view window");
        }
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
#[allow(
    clippy::too_many_lines,
    reason = "one uninterrupted pass over one frame — capture, scale, encode, \
              write, record, adapt — and every split would put a step of it \
              behind a call that hides the order the steps must happen in"
)]
pub fn spawn_encode_loop(
    connection: Connection,
    capture: SharedCapture,
    recorder: SharedRecorder,
    tag: String,
    peer: NodeId,
    faults: mpsc::Sender<MediaFault>,
    control: EncodeControl,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut writer = match open_media_stream(&connection).await {
            Ok(writer) => writer,
            Err(error) => {
                tracing::warn!(peer = %tag, %error, "cannot open the media stream");
                return;
            }
        };
        let mut encoder = match select_encoder(EncoderConfig::default()) {
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
        // Three knobs now, not one: the controller walks bitrate, then frame
        // rate, then picture scale (ADR 0037). The loop's own pacing is where
        // the frame rate lives, so `interval` is derived from the target
        // rather than fixed for the session.
        let mut abr = AbrController::new();
        let mut target = abr.target();
        control.publish(target);
        let mut interval = frame_interval(target.fps);
        // When the guest last told this side what it actually received. A
        // guest that reports is the authority on its own link; without one —
        // an older peer, or one that has gone quiet — the only congestion
        // signal left is how long each write takes relative to the frame
        // budget, which is what ADR 0015 settled for.
        let mut last_report: Option<Instant> = None;
        let feedback_stale = Duration::from_millis(ABR_FEEDBACK_STALE_AFTER_MS);
        // What this side actually put on the wire, over the window the guest
        // measures its own arrivals across. Without it the controller has only
        // the bitrate *ceiling* to compare arrivals against, and a desktop
        // nobody is touching encodes to a fraction of that — which read as
        // congestion on a link with no loss at all, and walked the whole
        // degradation ladder down on an idle LAN (docs/bugs/07-video-quality.md).
        let mut sent = SendRate::default();
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

        loop {
            let tick_started = Instant::now();
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
            let captured =
                tokio::task::spawn_blocking(move || lock_capture(&shared).next_frame()).await;
            let frame = match captured {
                Ok(Ok(Some(frame))) => {
                    secure_desktop_notified = false;
                    frame
                }
                // The screen has not changed (§11.1): nothing to send. Also
                // the "just reopened, no pixels yet" answer `WindowsCapturer`
                // gives right after a secure-desktop recovery, so this is
                // where that recovery is noticed too.
                Ok(Ok(None)) => {
                    secure_desktop_notified = false;
                    sleep_for_the_rest_of(interval, tick_started).await;
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
                Ok(Err(MediaError::SecureDesktopActive(reason))) => {
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
                Ok(Err(error)) => {
                    tracing::info!(peer = %tag, %error, "capture ended: stopping the encode loop");
                    return;
                }
                Err(error) => {
                    tracing::warn!(peer = %tag, %error, "the capture task ended unexpectedly");
                    return;
                }
            };

            // Three reductions, in this order and for different reasons. The
            // manual ceiling and the adaptive target are combined first,
            // because they are not allowed to fight over the same variable
            // (D7, docs/bugs/13-stream-resolution.md): a ceiling below the
            // ABR target wins, and ABR stays free to sit below a higher one
            // when the link cannot carry it. The budget reduction is the hard
            // ceiling of §15 that no choice may exceed, so it goes last and
            // has the final say (ADR 0018).
            let scale_percent = effective_scale(control.manual_cap(), target.scale_percent);
            let frame = fit_within_budget(scale_to_percent(frame, scale_percent));

            // The cursor rides its own channel when the guest asked for one,
            // and is read only then: a shape the loop would never send is a
            // platform call made for nothing. `cursor_shape` answers `None`
            // for a cursor that has not changed, so this is one comparison on
            // a steady screen (§11).
            if control.cursor_channel()
                && let Some(shape) = lock_capture(&capture).cursor_shape()
            {
                control.send_cursor(shape);
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

            let bitstream = match encoder.encode(&frame) {
                Ok(bitstream) => bitstream,
                Err(error) => {
                    tracing::warn!(peer = %tag, %error, "encoder refused a frame");
                    sleep_for_the_rest_of(interval, tick_started).await;
                    continue;
                }
            };
            if bitstream.data.len() > MAX_MEDIA_FRAME_BYTES {
                tracing::warn!(
                    peer = %tag,
                    bytes = bitstream.data.len(),
                    "dropping a frame larger than the media frame bound"
                );
                continue;
            }
            let write_started = Instant::now();
            if let Err(error) = writer.write_frame(&encode_media_payload(&bitstream)).await {
                tracing::info!(peer = %tag, %error, "media stream ended");
                return;
            }
            sent.wrote(bitstream.data.len());
            // Recording rides the successfully written frame (§17): the
            // container stores the same bitstream the guest received.
            if let Some(recorder) = recorder_now.as_ref() {
                recorder.write_video(bitstream.timestamp_us, &bitstream.data);
            }
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
                    Some(feedback)
                }
                None if last_report.is_none_or(|at| at.elapsed() > feedback_stale) => {
                    Some(write_congestion_feedback(
                        write_started.elapsed(),
                        interval,
                        bitstream.data.len(),
                    ))
                }
                None => None,
            };
            if let Some(feedback) = measured
                && let Some(next) = abr.on_feedback(feedback)
            {
                if next.bitrate_kbps != target.bitrate_kbps
                    && let Err(error) = encoder.set_bitrate(next.bitrate_kbps)
                {
                    tracing::warn!(
                        peer = %tag,
                        %error,
                        target_kbps = next.bitrate_kbps,
                        "encoder refused a bitrate change"
                    );
                }
                if next.fps != target.fps {
                    interval = frame_interval(next.fps);
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
            sleep_for_the_rest_of(interval, tick_started).await;
        }
    })
}

/// The pacing delay of one frame at `fps`.
///
/// `max(1)` is not defensive noise: the frame rate is clamped by
/// `ABR_MIN_FPS` upstream, and dividing by a zero that cannot occur would
/// still be a panic in a loop that must not have one.
fn frame_interval(fps: u8) -> Duration {
    Duration::from_millis(MILLIS_PER_SEC / u64::from(fps.max(1)))
}

/// Sleeps only what's left of `interval` after `tick_started`, instead of
/// unconditionally sleeping the whole interval on top of however long the
/// tick's own work took — the latter compounds every tick that capture,
/// encode or write is not instant, and real throughput falls under
/// `ENCODE_DEFAULT_FPS` under any load at all.
async fn sleep_for_the_rest_of(interval: Duration, tick_started: Instant) {
    if let Some(remaining) = interval.checked_sub(tick_started.elapsed()) {
        tokio::time::sleep(remaining).await;
    }
}

/// Turns how long one write actually took into the [`ReceiverFeedback`]
/// shape [`AbrController`] expects, standing in for real loss on a stream
/// that cannot lose bytes (see docs/adr/0015-host-local-abr.md).
///
/// A write finishing inside the frame budget reports no congestion; one
/// running twice the budget or beyond reports total loss, saturating the
/// controller's multiplicative-decrease branch. `rtt_ms` has no local
/// equivalent here, and the goodput half is deliberately self-cancelling:
/// both `goodput_kbps` and `sent_kbps` are the same local write rate, so the
/// controller's arrival check cannot fire on a measurement that never crossed
/// the network. Congestion on this path is `loss`, and only `loss`.
fn write_congestion_feedback(
    write_elapsed: Duration,
    interval: Duration,
    frame_bytes: usize,
) -> ReceiverFeedback {
    let over_budget = write_elapsed.as_secs_f32() / interval.as_secs_f32().max(f32::EPSILON) - 1.0;
    let bits = (frame_bytes as u128).saturating_mul(8);
    let millis = write_elapsed.as_millis().max(1);
    let goodput_kbps = u32::try_from(bits / millis).unwrap_or(u32::MAX);
    ReceiverFeedback {
        loss: over_budget.clamp(0.0, 1.0),
        rtt_ms: 0,
        goodput_kbps,
        sent_kbps: goodput_kbps,
    }
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

/// Everything the guest's media loop needs to reach one host.
#[derive(Debug, Clone)]
pub struct MediaTarget {
    /// This node's endpoint, reused for the media dial.
    pub endpoint: PeerEndpoint,
    /// Host address, remembered from the invite ticket so the media dial does
    /// not depend on discovery having caught up.
    pub addr: EndpointAddr,
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
    /// Where the live media connection lands once dialed (§4.1; ADR 0028):
    /// the mic toggle reads it to open its tagged stream on the *same*
    /// connection, never a second one.
    #[allow(
        dead_code,
        reason = "read by the actor through ViewState::media_connection; the cell is shared"
    )]
    pub connection_cell: Arc<std::sync::Mutex<Option<Connection>>>,
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
pub fn spawn_media_receiver(
    target: MediaTarget,
    slot: Arc<watch::Sender<ViewSlot>>,
) -> JoinHandle<()> {
    // Guest-side audio (§11, questions.md item 9): one receiver per media
    // loop. It owns the Opus decoder and publishes decoded PCM on its own
    // channel; the playback device drains it in the view window process.
    // `pcm_rx` is the handle the view window's playback device reads from;
    // it is returned below so it outlives this function.
    let (pcm_tx, pcm_rx) = watch::channel(None);
    std::mem::forget(pcm_rx); // placeholder until the playback sink lands
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
            let produced = stream_once(&target, &slot, &pcm_tx).await;
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

/// One media attempt: dial, decode until something fails.
///
/// Returns whether at least one picture reached `slot`, which is what decides
/// if the recovery budget is refreshed.
async fn stream_once(
    target: &MediaTarget,
    slot: &watch::Sender<ViewSlot>,
    pcm: &watch::Sender<Option<Vec<i16>>>,
) -> bool {
    let connection = match target
        .endpoint
        .connect(target.addr.clone(), lumepeer_net::ALPN_MEDIA)
        .await
    {
        Ok(connection) => connection,
        Err(error) => {
            tracing::debug!(peer = %target.tag, %error, "media dial failed");
            return false;
        }
    };
    // Publish the live connection before anything else: the mic toggle must
    // see it the moment a picture can exist (§4.1; ADR 0028).
    *target
        .connection_cell
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(connection.clone());
    let mut reader = match accept_media_stream(&connection).await {
        Ok(reader) => reader,
        Err(error) => {
            tracing::debug!(peer = %target.tag, %error, "host opened no media stream");
            return false;
        }
    };
    let _audio = spawn_audio_pass(connection.clone(), target.tag.clone(), pcm.clone());

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
        window.received(payload.len());
        if let Some(report) = window.due() {
            send_report(target, report);
        }
        let Some(encoded) = decode_media_payload(&payload) else {
            tracing::warn!(peer = %target.tag, "dropping a malformed media payload");
            window.lost();
            continue;
        };

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
/// Started only when the host user turns audio on for a granted session (the
/// `AudioStart` control message of §11 is what the actor sends); the guest
/// learns audio exists by accepting a stream that announces itself, so a
/// video-only host never opens one. Capture runs behind the `audio-capture`
/// feature; without a backend the loop refuses loudly in the log and the
/// session stays video-only (§18).
pub fn spawn_audio_loop(
    connection: Connection,
    stop: Arc<AtomicBool>,
    recorder: crate::view::SharedRecorder,
    tag: String,
) -> JoinHandle<()> {
    use lumepeer_media::audio::OpusEncoder;
    use lumepeer_media::capture_audio::{AudioCapturer, platform_audio_capturer};

    tokio::spawn(async move {
        let mut capturer: Option<Box<dyn AudioCapturer>> = match platform_audio_capturer() {
            Ok(capturer) => Some(capturer),
            Err(error) => {
                tracing::warn!(peer = %tag, %error, "no audio capture backend: staying video-only");
                return;
            }
        };
        let mut encoder = match OpusEncoder::new() {
            Ok(encoder) => encoder,
            Err(error) => {
                tracing::warn!(peer = %tag, %error, "no Opus encoder: staying video-only");
                return;
            }
        };
        let mut writer =
            match lumepeer_net::open_tagged_media_stream(&connection, lumepeer_net::STREAM_AUDIO)
                .await
            {
                Ok(writer) => writer,
                Err(error) => {
                    tracing::warn!(peer = %tag, %error, "cannot open the audio stream");
                    return;
                }
            };
        // `start` is fallible on the platform side; a refusal ends this loop
        // before any packet flows, leaving the session video-only (§18).
        let Some(started) = capturer.as_mut() else {
            return;
        };
        if let Err(error) = started.start() {
            tracing::warn!(peer = %tag, %error, "audio capture refused to start");
            return;
        }
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
                    if let Some(mut c) = capturer.take() {
                        c.stop();
                    }
                    return;
                }
            }
        }
    })
}

/// Guest side: accepts the host's tagged audio stream, decodes Opus in the
/// sandboxed worker process (§11.3, questions.md item 9) and hands PCM to
/// `sink` for playback.
///
/// The sandbox stays the only place an untrusted bitstream is processed; the
/// main process only ever sees decoded PCM. Returns when the stream ends —
/// the caller treats that the same way the video path treats a lost stream.
async fn stream_audio_once(
    connection: &Connection,
    tag: &str,
    sink: &watch::Sender<Option<Vec<i16>>>,
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
                if sink.send(Some(samples)).is_err() {
                    return true;
                }
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
/// the picture already rides, for as long as that pass lasts. `pcm` carries
/// the newest decoded chunk; the playback device drains it.
///
/// One per media pass, started by [`stream_once`] once the picture's stream
/// has been taken, and aborted with the pass. It parks by itself when a host
/// never opens an audio stream — audio is opt-in on the host and may be
/// turned on mid-session, which opens a fresh stream for exactly this
/// purpose (§11).
fn spawn_audio_pass(
    connection: Connection,
    tag: String,
    pcm: watch::Sender<Option<Vec<i16>>>,
) -> AbortOnDrop {
    AbortOnDrop(tokio::spawn(async move {
        let backoff = Duration::from_millis(MEDIA_REDIAL_BACKOFF_MS.max(1_000));
        loop {
            let produced = stream_audio_once(&connection, &tag, &pcm).await;
            if pcm.is_closed() {
                return;
            }
            // A stream that carried audio and then ended may be followed by
            // another one — the host user toggling audio off and on again.
            // One that never arrived means a video-only host: park, retry.
            if !produced {
                tokio::time::sleep(backoff).await;
            }
        }
    }))
}

/// Guest side: capture the microphone, Opus-encode and write it onto one
/// tagged `M` media stream, until `stop` flips or the stream fails
/// (§11; ADR 0028).
///
/// Started by the toolbar's mic button through the `mic_toggle` IPC command;
/// rides the media connection the picture already dialed, exactly like the
/// host→guest audio stream but in the other direction, announcing itself
/// with [`STREAM_MIC`] so the host can tell it from the streams it opens
/// itself. Capture runs behind the `audio-capture` feature; without a
/// backend, or when the OS refuses microphone access, the loop refuses
/// loudly in the log and the toolbar button reports the refusal (§18).
pub fn spawn_mic_loop(connection: Connection, tag: String) -> JoinHandle<()> {
    use lumepeer_media::audio::OpusEncoder;
    use lumepeer_media::capture_audio::{MicCapturer, platform_mic_capturer};

    tokio::spawn(async move {
        let mut capturer: Option<Box<dyn MicCapturer>> = match platform_mic_capturer() {
            Ok(capturer) => Some(capturer),
            Err(error) => {
                tracing::warn!(peer = %tag, %error, "no microphone backend: mic stays off");
                return;
            }
        };
        let mut encoder = match OpusEncoder::new() {
            Ok(encoder) => encoder,
            Err(error) => {
                tracing::warn!(peer = %tag, %error, "no Opus encoder: mic stays off");
                return;
            }
        };
        let mut writer = match lumepeer_net::open_tagged_media_stream(&connection, STREAM_MIC).await
        {
            Ok(writer) => writer,
            Err(error) => {
                tracing::warn!(peer = %tag, %error, "cannot open the mic stream");
                return;
            }
        };
        let Some(started) = capturer.as_mut() else {
            return;
        };
        if let Err(error) = started.start() {
            tracing::warn!(peer = %tag, %error, "microphone capture refused to start");
            return;
        }
        tracing::info!(peer = %tag, "mic loop started");

        loop {
            // `next_chunk` blocks on the device; it must not sit on a tokio
            // worker thread — the same take/restore dance the host audio
            // loop uses.
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
            match read_result {
                Ok(samples_chunk) => {
                    match encoder.encode(&samples_chunk.samples, samples_chunk.timestamp_us) {
                        Ok(packet) => {
                            if packet.data.len() > AUDIO_MAX_FRAME_BYTES {
                                tracing::warn!(peer = %tag, "dropping an oversized mic frame");
                                continue;
                            }
                            let payload = lumepeer_net::encode_audio_payload(&packet);
                            if let Err(error) = writer.write_frame(&payload).await {
                                tracing::info!(peer = %tag, %error, "mic stream ended");
                                if let Some(mut c) = capturer.take() {
                                    c.stop();
                                }
                                return;
                            }
                        }
                        Err(error) => {
                            // One bad chunk is a skip, not a teardown: audio
                            // degrades towards noise, never towards an error
                            // (§24.5).
                            tracing::debug!(peer = %tag, %error, "encoder refused a mic chunk");
                        }
                    }
                }
                Err(error) => {
                    tracing::info!(peer = %tag, %error, "microphone capture ended");
                    if let Some(mut c) = capturer.take() {
                        c.stop();
                    }
                    return;
                }
            }
        }
    })
}

/// Host side: accept the guest's tagged `M` mic stream on the media
/// connection and play it on the speakers (§11; ADR 0028), for as long as
/// that pass lasts. One per media session, started when the media connection
/// is accepted; the loop inside parks while no mic stream exists and ends
/// with the session.
pub fn spawn_guest_mic_pass(connection: Connection, tag: String) -> JoinHandle<()> {
    tokio::spawn(async move {
        // The mic stream is opt-in on the guest: most sessions never carry
        // one, and the accept call parks until it shows up or the connection
        // ends — no polling, no spin.
        let mut reader = match lumepeer_net::accept_tagged_media_stream(&connection, STREAM_MIC)
            .await
        {
            Ok(Some(reader)) => reader,
            Ok(None) => {
                tracing::debug!(peer = %tag, "connection ended before a mic stream arrived");
                return;
            }
            Err(error) => {
                tracing::debug!(peer = %tag, %error, "media connection ended before a mic stream");
                return;
            }
        };
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
                return;
            }
        };
        if let Err(error) = player.start() {
            tracing::warn!(peer = %tag, %error, "no playback device: guest mic stays off");
            return;
        }
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
                    if let Err(error) = player.push(&samples, chunk.timestamp_us) {
                        tracing::warn!(peer = %tag, %error, "mic playback stopped");
                        return;
                    }
                }
                Err(error) => {
                    tracing::debug!(peer = %tag, %error, "decoder refused a mic packet");
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failed assumption must fail the test")]

    use super::*;

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

        let host = PeerEndpoint::bind_local(iroh::SecretKey::generate())
            .await
            .unwrap();
        let guest = PeerEndpoint::bind_local(iroh::SecretKey::generate())
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
                endpoint: guest.clone(),
                addr,
                peer: guest.node_id(),
                reports,
                tag: "test-peer".to_owned(),
                worker: None,
                connection_cell: Arc::new(std::sync::Mutex::new(None)),
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

    #[test]
    fn a_write_inside_budget_reports_no_congestion() {
        let interval = Duration::from_millis(33);
        let feedback = write_congestion_feedback(Duration::from_millis(10), interval, 1_000);
        assert!(feedback.loss.abs() < f32::EPSILON);
    }

    #[test]
    fn a_write_exactly_at_budget_reports_no_congestion() {
        let interval = Duration::from_millis(33);
        let feedback = write_congestion_feedback(interval, interval, 1_000);
        assert!(feedback.loss.abs() < f32::EPSILON);
    }

    #[test]
    fn a_write_double_the_budget_saturates_at_full_loss() {
        let interval = Duration::from_millis(33);
        let feedback = write_congestion_feedback(interval * 2, interval, 1_000);
        assert!((feedback.loss - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn a_write_double_the_budget_or_worse_never_exceeds_full_loss() {
        let interval = Duration::from_millis(33);
        let feedback = write_congestion_feedback(interval * 10, interval, 1_000);
        assert!(
            (feedback.loss - 1.0).abs() < f32::EPSILON,
            "loss must stay within AbrController's 0.0..=1.0 contract"
        );
    }

    #[test]
    fn a_write_halfway_over_budget_reports_half_loss() {
        let interval = Duration::from_millis(100);
        let feedback = write_congestion_feedback(Duration::from_millis(150), interval, 1_000);
        assert!((feedback.loss - 0.5).abs() < 0.01);
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
}
