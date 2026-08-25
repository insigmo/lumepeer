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
    AUDIO_MAX_FRAME_BYTES, ENCODE_DEFAULT_FPS, MAX_MEDIA_FRAME_BYTES, MEDIA_REDIAL_BACKOFF_MS,
    RECONNECT_WINDOW_SECS,
};
use lumepeer_core::protocol::MediaUnavailableReason;
use lumepeer_media::abr::{AbrController, ReceiverFeedback};
use lumepeer_media::capture::{CaptureController, InputInjector};
use lumepeer_media::decode::{DecodedFrame, DecoderHandle};
use lumepeer_media::encode::{EncodedFrame, EncoderConfig, select_encoder};
use lumepeer_media::scale::fit_within_budget;
use lumepeer_net::{PeerEndpoint, STREAM_MIC, accept_media_stream, open_media_stream};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

/// Milliseconds in a second, for turning `ENCODE_DEFAULT_FPS` into a delay.
const MILLIS_PER_SEC: u64 = 1_000;

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
    pub fn record(&self, reason: MediaUnavailableReason) {
        match reason {
            MediaUnavailableReason::NoCaptureBackend => {
                self.capture_missing.store(true, Ordering::Relaxed);
            }
            MediaUnavailableReason::NoEncoder => {
                self.encoder_missing.store(true, Ordering::Relaxed);
            }
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
}

impl From<MediaUnavailableReason> for ViewStatus {
    fn from(reason: MediaUnavailableReason) -> Self {
        match reason {
            MediaUnavailableReason::NoCaptureBackend => Self::NoCapture,
            MediaUnavailableReason::NoEncoder => Self::NoEncoder,
        }
    }
}

impl ViewStatus {
    /// Wire value carried in the first byte of the IPC frame response.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Waiting => 0,
            Self::Live => 1,
            Self::Reconnecting => 2,
            Self::Failed => 3,
            Self::NoCapture => 4,
            Self::NoEncoder => 5,
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

/// Serializes a slot for `view_next_frame`'s binary IPC response.
///
/// Layout, little endian: `status | input | width | height | timestamp_us |
/// RGBA8 pixels`. Binary rather than JSON because a 1080p picture is ~8 MB and
/// base64-ing it per frame would dominate the frame budget of §15.
///
/// `input` rides along on every frame instead of being fetched once at window
/// load: the grant is live, so a later `session_grant` that lowers the role has
/// to be able to take the guest's input listeners away again (§8.1).
#[must_use]
pub fn encode_view_response(slot: &ViewSlot, input: bool) -> Vec<u8> {
    let frame = slot.frame.as_ref();
    let pixels = frame.map_or(&[][..], |f| f.data.as_slice());
    let mut out = Vec::with_capacity(VIEW_RESPONSE_HEADER_BYTES + pixels.len());
    out.push(slot.status.code());
    out.push(u8::from(input));
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
pub fn spawn_encode_loop(
    connection: Connection,
    capture: SharedCapture,
    recorder: SharedRecorder,
    tag: String,
    peer: NodeId,
    faults: mpsc::Sender<MediaFault>,
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
        let interval = Duration::from_millis(MILLIS_PER_SEC / u64::from(ENCODE_DEFAULT_FPS.max(1)));
        // `rd/media/1` is a reliable, ordered QUIC stream: nothing on it is
        // ever silently lost the way `ReceiverFeedback.loss` is named for.
        // The one congestion signal available without a guest-side wire
        // message is how long each write actually takes relative to the
        // frame budget — see docs/adr/0015-host-local-abr.md.
        let mut abr = AbrController::new();
        // Session recording (§17): the actor swaps a recorder into the shared
        // `recorder` slot; each written frame is offered to whatever is in
        // there now, so a mid-session start/stop needs no pipeline restart.
        let mut recorder_now: Option<Arc<crate::recorder::SessionRecorder>>;

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
                Ok(Ok(Some(frame))) => frame,
                // The screen has not changed (§11.1): nothing to send.
                Ok(Ok(None)) => {
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

            // A screen larger than the pipeline's picture budget is reduced
            // here, before anything downstream has to carry it: the encoder,
            // the wire, the sandboxed decoder's shared-memory slot and the
            // guest's canvas all size themselves off this frame (ADR 0018).
            let frame = fit_within_budget(frame);

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
            // Recording rides the successfully written frame (§17): the
            // container stores the same bitstream the guest received.
            if let Some(recorder) = recorder_now.as_ref() {
                recorder.write_video(bitstream.timestamp_us, &bitstream.data);
            }
            if let Some(target_kbps) = abr.on_feedback(write_congestion_feedback(
                write_started.elapsed(),
                interval,
                bitstream.data.len(),
            )) && let Err(error) = encoder.set_bitrate(target_kbps)
            {
                tracing::warn!(peer = %tag, %error, target_kbps, "encoder refused a bitrate change");
            }
            sleep_for_the_rest_of(interval, tick_started).await;
        }
    })
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
/// equivalent here and `goodput_kbps` is not read by
/// `AbrController::on_feedback`'s current decision, but both are filled in
/// honestly rather than left at a placeholder.
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

    loop {
        let payload = match reader.read_frame().await {
            Ok(payload) => payload,
            Err(error) => {
                tracing::debug!(peer = %target.tag, %error, "media stream ended");
                return produced;
            }
        };
        let Some(encoded) = decode_media_payload(&payload) else {
            tracing::warn!(peer = %target.tag, "dropping a malformed media payload");
            continue;
        };

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
                tracing::warn!(peer = %target.tag, %error, "decoder failed");
                return produced;
            }
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
        let receiver = spawn_media_receiver(
            MediaTarget {
                endpoint: guest.clone(),
                addr,
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
        let bytes = encode_view_response(&ViewSlot::waiting(), false);
        assert_eq!(bytes.len(), VIEW_RESPONSE_HEADER_BYTES);
        assert_eq!(bytes[0], ViewStatus::Waiting.code());
        assert_eq!(bytes[1], 0);
    }

    #[test]
    fn the_frame_response_carries_pixels_and_the_live_input_grant() {
        let slot = ViewSlot {
            status: ViewStatus::Live,
            frame: Some(DecodedFrame {
                width: 2,
                height: 1,
                timestamp_us: 7,
                data: vec![1, 2, 3, 4, 5, 6, 7, 8],
            }),
        };
        let bytes = encode_view_response(&slot, true);
        assert_eq!(bytes[0], ViewStatus::Live.code());
        assert_eq!(bytes[1], 1);
        assert_eq!(u32::from_le_bytes(bytes[2..6].try_into().unwrap()), 2);
        assert_eq!(u32::from_le_bytes(bytes[6..10].try_into().unwrap()), 1);
        assert_eq!(u64::from_le_bytes(bytes[10..18].try_into().unwrap()), 7);
        assert_eq!(
            &bytes[VIEW_RESPONSE_HEADER_BYTES..],
            &[1, 2, 3, 4, 5, 6, 7, 8]
        );
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
    fn status_and_input_stay_live_on_every_poll_even_when_pixels_are_skipped() {
        let mut current = slot_with_frame(7);
        current.status = ViewStatus::Reconnecting;
        let polled = slot_for_poll(&current, 7);
        assert_eq!(polled.status, ViewStatus::Reconnecting);
        let bytes = encode_view_response(&polled, true);
        assert_eq!(bytes[0], ViewStatus::Reconnecting.code());
        assert_eq!(bytes[1], 1);
        assert_eq!(
            bytes.len(),
            VIEW_RESPONSE_HEADER_BYTES,
            "no stale pixels ride along"
        );
    }
}
