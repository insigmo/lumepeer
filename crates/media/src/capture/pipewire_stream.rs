//! PipeWire frame consumption for the Wayland portal capture path (§11).
//!
//! The negotiated `Session` (`linux_wayland::portal::PortalHandle`) grants a
//! PipeWire node id, not pixels: turning that node id into `Frame`s needs its
//! own PipeWire `MainLoop`, run on a dedicated thread because the loop blocks
//! for the life of the capture (`MainLoop::run` does not return until
//! something calls `quit()`).

use std::sync::Arc;
use std::sync::mpsc::{Receiver, SyncSender};
use std::thread::JoinHandle;

use crate::capture::linux_wayland::portal::StreamSize;
use crate::capture::{Frame, PixelFormat};
use crate::error::{MediaError, Result};

/// Packs a raw row-strided buffer into a tightly-packed `BGRx` `Frame`,
/// deduplicating against `last_hash` the same way `linux_x11.rs` does.
///
/// Returns `None` when the frame is identical to the last one handed out, or
/// when the buffer doesn't yet carry a full frame (a short read during format
/// renegotiation) — neither is an error.
fn pack_frame(
    width: u32,
    height: u32,
    stride: usize,
    bytes: &[u8],
    started_at: std::time::Instant,
    last_hash: &mut Option<[u8; 32]>,
) -> Option<Frame> {
    if width == 0 || height == 0 {
        return None;
    }
    let row_bytes = (width as usize) * 4;
    let effective_stride = stride.max(row_bytes);
    let needed = effective_stride * (height as usize - 1) + row_bytes;
    if bytes.len() < needed {
        return None;
    }

    let mut packed = Vec::with_capacity(row_bytes * height as usize);
    for row in 0..height as usize {
        let start = row * effective_stride;
        packed.extend_from_slice(&bytes[start..start + row_bytes]);
    }

    let hash = *blake3::hash(&packed).as_bytes();
    if *last_hash == Some(hash) {
        return None;
    }
    *last_hash = Some(hash);

    Some(Frame {
        width,
        height,
        format: PixelFormat::Bgra8,
        timestamp_us: u64::try_from(started_at.elapsed().as_micros()).unwrap_or(u64::MAX),
        data: packed,
    })
}

struct StreamUserData {
    width: u32,
    height: u32,
    sender: SyncSender<Frame>,
    started_at: std::time::Instant,
    last_hash: Option<[u8; 32]>,
    stream_size: Arc<StreamSize>,
}

/// Sent to shut the PipeWire thread down; see `pipewire::channel`, which
/// exists exactly for signaling a loop running on another thread.
struct Shutdown;

/// Owns a PipeWire `MainLoop` on a dedicated thread, feeding decoded frames
/// through a bounded channel. Dropping this joins the thread.
pub(crate) struct PipeWireFrameThread {
    handle: Option<JoinHandle<()>>,
    shutdown: pipewire::channel::Sender<Shutdown>,
    frames: Receiver<Frame>,
}

// `pipewire::channel::Sender`/`Receiver` do not implement `Debug`, so this is
// written by hand instead of derived; every field that owns platform state is
// summarized rather than printed, matching `ScreenCapturer: Send + Debug`'s
// intent without depending on debug support the dependency doesn't provide.
impl std::fmt::Debug for PipeWireFrameThread {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PipeWireFrameThread")
            .field("running", &self.handle.is_some())
            .finish_non_exhaustive()
    }
}

impl PipeWireFrameThread {
    /// Spawns the thread and connects to `node_id`.
    ///
    /// # Errors
    /// [`MediaError::CaptureUnavailable`] if the thread itself cannot be
    /// spawned. Errors from inside the thread (PipeWire connection failure,
    /// format negotiation failure) are not reported synchronously — the
    /// thread exits and `try_recv_frame` then always returns `None`, which
    /// callers already treat as "no new frame right now", not a failure.
    ///
    /// `stream_size` is the same `Arc` `PortalHandle::stream_size_handle`
    /// returns — the thread publishes the negotiated width/height into it
    /// from `param_changed` so `WaylandPortalInjector` can scale pointer
    /// coordinates correctly.
    pub(crate) fn spawn(node_id: u32, stream_size: Arc<StreamSize>) -> Result<Self> {
        let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel::<Frame>(1);
        let (shutdown_tx, shutdown_rx) = pipewire::channel::channel::<Shutdown>();

        let handle = std::thread::Builder::new()
            .name("lumepeer-pipewire-capture".to_owned())
            .spawn(move || {
                if let Err(err) = Self::run(node_id, &frame_tx, shutdown_rx, &stream_size) {
                    tracing::warn!("pipewire capture thread exited: {err}");
                }
            })
            .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;

        Ok(Self {
            handle: Some(handle),
            shutdown: shutdown_tx,
            frames: frame_rx,
        })
    }

    fn run(
        node_id: u32,
        frame_tx: &SyncSender<Frame>,
        shutdown_rx: pipewire::channel::Receiver<Shutdown>,
        stream_size: &Arc<StreamSize>,
    ) -> Result<()> {
        use pipewire::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
        use pipewire::spa::param::video::{VideoFormat, VideoInfoRaw};
        use pipewire::spa::pod::serialize::PodSerializer;
        use pipewire::spa::pod::{Pod, Value};
        use pipewire::spa::utils::{Direction, SpaTypes};
        use pipewire::stream::StreamFlags;

        pipewire::init();
        // pipewire 0.8.0 API: MainLoop is already Rc-backed and Clone on its
        // own — no separate `*Rc` type. `Context::new` takes only the loop,
        // no properties argument. `connect()` (not `connect_rc()`) returns
        // `Core` directly. See crates/media/Cargo.toml's `capture-portal`
        // feature comment for why 0.8.0 specifically.
        let mainloop = pipewire::main_loop::MainLoop::new(None)
            .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;
        let context = pipewire::context::Context::new(&mainloop)
            .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;
        let core = context
            .connect(None)
            .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;

        // Cross-thread shutdown: dropping `PipeWireFrameThread` sends
        // `Shutdown`, which this attaches to the loop as an IO source.
        let _shutdown_listener = {
            // Two handles, deliberately: `attach` borrows the `LoopRef` for as
            // long as the returned listener lives, so the loop it is taken
            // from has to be the outer `mainloop` that outlives this block —
            // and the closure needs an owned handle of its own to call
            // `quit()` on. One clone cannot be both.
            let quit = mainloop.clone();
            shutdown_rx.attach(mainloop.loop_(), move |Shutdown| quit.quit())
        };

        let props = pipewire::properties::properties! {
            *pipewire::keys::MEDIA_TYPE => "Video",
            *pipewire::keys::MEDIA_CATEGORY => "Capture",
            *pipewire::keys::MEDIA_ROLE => "Screen",
        };
        let stream = pipewire::stream::Stream::new(&core, "lumepeer-capture", props)
            .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;

        let data = StreamUserData {
            width: 0,
            height: 0,
            sender: frame_tx.clone(),
            started_at: std::time::Instant::now(),
            last_hash: None,
            stream_size: Arc::clone(stream_size),
        };

        let _listener = stream
            .add_local_listener_with_user_data(data)
            .param_changed(|_, user_data, id, param| {
                let Some(param) = param else { return };
                if id != pipewire::spa::param::ParamType::Format.as_raw() {
                    return;
                }
                let mut info = VideoInfoRaw::new();
                if info.parse(param).is_err() {
                    return;
                }
                let size = info.size();
                user_data.width = size.width;
                user_data.height = size.height;
                // Published for WaylandPortalInjector, which reads this
                // through the same PortalHandle to scale pointer coordinates
                // into the stream's own logical space.
                user_data.stream_size.set(size.width, size.height);
            })
            .process(|stream, user_data| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let datas = buffer.datas_mut();
                let Some(data) = datas.first_mut() else {
                    return;
                };
                // `max(0)` already excluded the negative half, so the
                // cast has no sign to lose; clippy cannot see that through
                // the method call.
                let stride = usize::try_from(data.chunk().stride().max(0)).unwrap_or(0);
                let Some(bytes) = data.data() else { return };

                if let Some(frame) = pack_frame(
                    user_data.width,
                    user_data.height,
                    stride,
                    bytes,
                    user_data.started_at,
                    &mut user_data.last_hash,
                ) {
                    // A full channel means the consumer hasn't caught up:
                    // drop this frame rather than block the PipeWire thread.
                    // An error here also covers a disconnected receiver
                    // (WaylandPortalCapturer gone); nothing more to do until
                    // `stop()` tears this down.
                    let _ = user_data.sender.try_send(frame);
                }
            })
            .register()
            .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;

        // Request a fixed BGRx format — no negotiation of alternates (§11,
        // per the design's non-goals). Built via the `object!`/`property!`
        // macros the same way pipewire-rs's own `examples/streams.rs` builds
        // an EnumFormat pod: safer than hand-rolling `libspa-sys` constant
        // names, which this crate cannot verify by compiling on this
        // (Windows) machine.
        let format_obj = pipewire::spa::pod::object!(
            SpaTypes::ObjectParamFormat,
            pipewire::spa::param::ParamType::EnumFormat,
            pipewire::spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
            pipewire::spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
            pipewire::spa::pod::property!(FormatProperties::VideoFormat, Id, VideoFormat::BGRx),
        );
        let values: Vec<u8> =
            PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(format_obj))
                .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?
                .0
                .into_inner();
        let format_pod = Pod::from_bytes(&values).ok_or_else(|| {
            MediaError::CaptureUnavailable("could not build format pod".to_owned())
        })?;
        let mut params = [format_pod];

        stream
            .connect(
                Direction::Input,
                Some(node_id),
                StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
                &mut params,
            )
            .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;

        mainloop.run();
        Ok(())
    }

    /// Drains the next available frame, or `None` if nothing new has
    /// arrived, matching `ScreenCapturer::next_frame`'s "no change" contract.
    pub(crate) fn try_recv_frame(&self) -> Option<Frame> {
        // Both error arms — nothing queued, and the producer thread gone —
        // are "no new frame right now" to the caller, which is exactly what
        // `ok()` collapses them to.
        self.frames.try_recv().ok()
    }
}

impl Drop for PipeWireFrameThread {
    fn drop(&mut self) {
        let _ = self.shutdown.send(Shutdown);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "a failed assumption must fail the test")]

    use super::*;

    #[test]
    fn packs_a_strided_buffer_into_a_tight_frame() {
        // 2x2 BGRx, stride padded to 12 bytes/row (row_bytes is 8).
        let mut buf = vec![0xAAu8; 12 * 2];
        buf[0..8].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        buf[12..20].copy_from_slice(&[9, 10, 11, 12, 13, 14, 15, 16]);

        let mut last_hash = None;
        let frame = pack_frame(2, 2, 12, &buf, std::time::Instant::now(), &mut last_hash)
            .expect("first frame must not be deduplicated");

        assert_eq!(frame.width, 2);
        assert_eq!(frame.height, 2);
        assert_eq!(frame.format, PixelFormat::Bgra8);
        assert_eq!(
            frame.data,
            vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]
        );
    }

    #[test]
    fn identical_bytes_deduplicate_to_none() {
        let buf = vec![7u8; 8 * 3];
        let mut last_hash = None;
        assert!(pack_frame(2, 3, 8, &buf, std::time::Instant::now(), &mut last_hash).is_some());
        assert!(
            pack_frame(2, 3, 8, &buf, std::time::Instant::now(), &mut last_hash).is_none(),
            "identical bytes must dedup to None"
        );
    }

    #[test]
    fn a_short_buffer_yields_no_frame_instead_of_panicking() {
        let buf = vec![0u8; 4];
        let mut last_hash = None;
        assert!(pack_frame(4, 4, 16, &buf, std::time::Instant::now(), &mut last_hash).is_none());
    }
}
