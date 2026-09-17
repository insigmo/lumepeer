//! PipeWire frame consumption for the Wayland portal capture path (§11).
//!
//! The negotiated `Session` (`linux_wayland::portal::PortalHandle`) grants a
//! PipeWire node id, not pixels: turning that node id into `Frame`s needs its
//! own PipeWire `MainLoop`, run on a dedicated thread because the loop blocks
//! for the life of the capture (`MainLoop::run` does not return until
//! something calls `quit()`).
//!
//! With `encode-vaapi-zero-copy` built in, the stream first offers to take the
//! compositor's buffers as DMA-BUF (ADR 0088), the way PipeWire's own
//! DMA-BUF sharing procedure lays out for a consumer: one `EnumFormat` with a
//! modifier list the producer fixates, one without for shared memory. A
//! compositor that takes the first hands over its rendered buffer itself, and
//! the frame carries it to the VA-API encoder without a byte of it passing
//! through main memory. A compositor that does not — or a build without the
//! feature — goes exactly the way this module always went.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, SyncSender};
use std::thread::JoinHandle;

use crate::capture::linux_wayland::portal::StreamSize;
use crate::capture::{Frame, PixelFormat};
use crate::error::{MediaError, Result};

#[cfg(feature = "encode-vaapi-zero-copy")]
pub use dmabuf::DmaBuf;
#[cfg(feature = "encode-vaapi-zero-copy")]
pub(crate) use dmabuf::{DRM_FORMAT_ARGB8888, DRM_FORMAT_XRGB8888};

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

    Some(Frame::cpu(
        width,
        height,
        PixelFormat::Bgra8,
        timestamp_us(started_at),
        packed,
    ))
}

/// Microseconds since `started_at`, saturating rather than wrapping.
fn timestamp_us(started_at: std::time::Instant) -> u64 {
    u64::try_from(started_at.elapsed().as_micros()).unwrap_or(u64::MAX)
}

struct StreamUserData {
    width: u32,
    height: u32,
    sender: SyncSender<Frame>,
    started_at: std::time::Instant,
    last_hash: Option<[u8; 32]>,
    stream_size: Arc<StreamSize>,
    /// Whether this stream offered DMA-BUF, what it negotiated, and the
    /// buffers it may have to give back (ADR 0088).
    #[cfg(feature = "encode-vaapi-zero-copy")]
    zero_copy: dmabuf::StreamState,
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
        Self::spawn_offering(node_id, stream_size, true)
    }

    /// [`Self::spawn`], saying whether to offer DMA-BUF at all.
    ///
    /// `false` is the shared-memory stream every build without
    /// `encode-vaapi-zero-copy` negotiates, which is what a test needs in
    /// order to show that path still carries the picture on the same
    /// compositor (ADR 0088). Inert without the feature.
    pub(crate) fn spawn_offering(
        node_id: u32,
        stream_size: Arc<StreamSize>,
        offer_dmabuf: bool,
    ) -> Result<Self> {
        let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel::<Frame>(1);
        let (shutdown_tx, shutdown_rx) = pipewire::channel::channel::<Shutdown>();

        let handle = std::thread::Builder::new()
            .name("lumepeer-pipewire-capture".to_owned())
            .spawn(move || {
                if let Err(err) =
                    Self::run(node_id, &frame_tx, shutdown_rx, &stream_size, offer_dmabuf)
                {
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

    #[allow(
        clippy::too_many_lines,
        reason = "the stream's whole negotiation, in the order PipeWire runs it; the DMA-BUF half adds two callbacks and a second format to the one sequence"
    )]
    fn run(
        node_id: u32,
        frame_tx: &SyncSender<Frame>,
        shutdown_rx: pipewire::channel::Receiver<Shutdown>,
        stream_size: &Arc<StreamSize>,
        offer_dmabuf: bool,
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

        #[cfg(feature = "encode-vaapi-zero-copy")]
        let (zero_copy, requeue_rx) = dmabuf::StreamState::new(offer_dmabuf);
        #[cfg(not(feature = "encode-vaapi-zero-copy"))]
        let _ = offer_dmabuf;

        // Frames this stream handed out as DMA-BUF come back through here
        // when the last holder drops them, and only this thread may give a
        // buffer back to the stream. Attached after the stream exists, so it
        // is torn down before the stream is.
        #[cfg(feature = "encode-vaapi-zero-copy")]
        let _requeue_listener = zero_copy.attach_requeue(requeue_rx, &mainloop, &stream);

        let data = StreamUserData {
            width: 0,
            height: 0,
            sender: frame_tx.clone(),
            started_at: std::time::Instant::now(),
            last_hash: None,
            stream_size: Arc::clone(stream_size),
            #[cfg(feature = "encode-vaapi-zero-copy")]
            zero_copy,
        };

        let listener = stream
            .add_local_listener_with_user_data(data)
            .param_changed(|stream, user_data, id, param| {
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

                // PipeWire's DMA-BUF procedure: a format that carries a
                // modifier was negotiated as DMA-BUF, and the buffers are
                // asked for as that; one without is shared memory.
                #[cfg(feature = "encode-vaapi-zero-copy")]
                user_data
                    .zero_copy
                    .format_changed(stream, param, info.format(), info.modifier());
                #[cfg(not(feature = "encode-vaapi-zero-copy"))]
                let _ = stream;
            });

        #[cfg(feature = "encode-vaapi-zero-copy")]
        let listener = listener
            .add_buffer(|_stream, user_data, buffer| user_data.zero_copy.buffer_added(buffer))
            .remove_buffer(|_stream, user_data, buffer| {
                user_data.zero_copy.buffer_removed(buffer);
            })
            .process(dmabuf::process);

        #[cfg(not(feature = "encode-vaapi-zero-copy"))]
        let listener = listener.process(|stream, user_data| {
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
            send_packed(user_data, stride, bytes);
        });

        let _listener = listener
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

        // The DMA-BUF format goes first: it is the one PipeWire tries before
        // falling back to the next (ADR 0088).
        #[cfg(feature = "encode-vaapi-zero-copy")]
        let dmabuf_values = if offer_dmabuf {
            Some(dmabuf::enum_format()?)
        } else {
            None
        };
        #[cfg(feature = "encode-vaapi-zero-copy")]
        let dmabuf_pod = dmabuf_values
            .as_deref()
            .map(|bytes| {
                Pod::from_bytes(bytes).ok_or_else(|| {
                    MediaError::CaptureUnavailable(
                        "could not build the DMA-BUF format pod".to_owned(),
                    )
                })
            })
            .transpose()?;
        #[cfg(feature = "encode-vaapi-zero-copy")]
        let mut params: Vec<&Pod> = dmabuf_pod.into_iter().chain([format_pod]).collect();
        #[cfg(not(feature = "encode-vaapi-zero-copy"))]
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

/// Packs a shared-memory buffer into a frame and offers it to the consumer.
fn send_packed(user_data: &mut StreamUserData, stride: usize, bytes: &[u8]) {
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
}

impl Drop for PipeWireFrameThread {
    fn drop(&mut self) {
        let _ = self.shutdown.send(Shutdown);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// The DMA-BUF half of the stream (ADR 0088).
///
/// The fifth place in the crate that needs `unsafe`. PipeWire's safe `Buffer`
/// gives a buffer back to the stream the moment it goes out of scope, which is
/// exactly wrong for a buffer the compositor must not draw into again until
/// the encoder has read it — so this half dequeues and queues raw buffers, and
/// maps and synchronizes a DMA-BUF for the rare frame that has to be read back.
/// Each `unsafe` block carries a `SAFETY:` note, as §21 requires.
#[cfg(feature = "encode-vaapi-zero-copy")]
#[allow(
    unsafe_code,
    reason = "raw PipeWire buffers held past the process callback, a DMA-BUF fd borrowed from one, and mmap plus DMA_BUF_IOCTL_SYNC to read one back. See ADR 0088."
)]
mod dmabuf {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd, RawFd};
    use std::rc::Rc;
    use std::sync::{Arc, OnceLock};

    use pipewire::spa::buffer::{Data, DataType};
    use pipewire::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    use pipewire::spa::param::video::VideoFormat;
    use pipewire::spa::pod::serialize::PodSerializer;
    use pipewire::spa::pod::{ChoiceValue, Object, Pod, Property, PropertyFlags, Value};
    use pipewire::spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Id, SpaTypes};

    use super::{StreamUserData, timestamp_us};
    use crate::capture::{Frame, PixelFormat};
    use crate::error::{MediaError, Result};

    /// A DRM format code, as `drm_fourcc.h` builds one from four characters.
    const fn fourcc(code: [u8; 4]) -> u32 {
        u32::from_le_bytes(code)
    }

    /// `DRM_FORMAT_XRGB8888`: what `SPA_VIDEO_FORMAT_BGRx` is on a
    /// little-endian machine — B, G, R, X in memory order.
    pub(crate) const DRM_FORMAT_XRGB8888: u32 = fourcc(*b"XR24");
    /// `DRM_FORMAT_ARGB8888`: `SPA_VIDEO_FORMAT_BGRA`, likewise.
    pub(crate) const DRM_FORMAT_ARGB8888: u32 = fourcc(*b"AR24");
    /// `DRM_FORMAT_MOD_LINEAR`: rows of pixels one after another, which is the
    /// only layout main memory can read and the only one this stream asks for.
    pub(crate) const DRM_FORMAT_MOD_LINEAR: u64 = 0;

    /// `DMA_BUF_IOCTL_SYNC`, `_IOW('b', 0, struct dma_buf_sync)` in
    /// `<linux/dma-buf.h>`. `libc` does not carry it.
    const DMA_BUF_IOCTL_SYNC: libc::c_ulong = 0x4008_6200;
    /// `DMA_BUF_SYNC_READ`.
    const DMA_BUF_SYNC_READ: u64 = 1;
    /// `DMA_BUF_SYNC_START`.
    const DMA_BUF_SYNC_START: u64 = 0;
    /// `DMA_BUF_SYNC_END`.
    const DMA_BUF_SYNC_END: u64 = 4;
    /// How many times an interrupted `DMA_BUF_IOCTL_SYNC` is retried. The
    /// kernel asks callers to retry on `EINTR` and `EAGAIN`; a few times is
    /// that, and a bound is what keeps a wedged exporter from being a loop.
    const SYNC_ATTEMPTS: u32 = 8;

    /// The DRM format of a SPA video format this stream takes as DMA-BUF, or
    /// `None` for one it does not.
    pub(crate) fn drm_format(format: VideoFormat) -> Option<u32> {
        if format == VideoFormat::BGRx {
            Some(DRM_FORMAT_XRGB8888)
        } else if format == VideoFormat::BGRA {
            Some(DRM_FORMAT_ARGB8888)
        } else {
            None
        }
    }

    /// The `EnumFormat` offering DMA-BUF: `BGRx` or `BGRA`, linear layout,
    /// fixated by the producer (PipeWire's DMA-BUF sharing procedure).
    pub(crate) fn enum_format() -> Result<Vec<u8>> {
        #[allow(
            clippy::cast_possible_wrap,
            reason = "DRM_FORMAT_MOD_LINEAR is zero; SPA carries modifiers as signed 64-bit values"
        )]
        let linear = DRM_FORMAT_MOD_LINEAR as i64;
        let object = Object {
            type_: SpaTypes::ObjectParamFormat.as_raw(),
            id: pipewire::spa::param::ParamType::EnumFormat.as_raw(),
            properties: vec![
                Property::new(
                    FormatProperties::MediaType.as_raw(),
                    Value::Id(Id(MediaType::Video.as_raw())),
                ),
                Property::new(
                    FormatProperties::MediaSubtype.as_raw(),
                    Value::Id(Id(MediaSubtype::Raw.as_raw())),
                ),
                Property::new(
                    FormatProperties::VideoFormat.as_raw(),
                    Value::Choice(ChoiceValue::Id(Choice(
                        ChoiceFlags::empty(),
                        ChoiceEnum::Enum {
                            default: Id(VideoFormat::BGRx.as_raw()),
                            alternatives: vec![
                                Id(VideoFormat::BGRx.as_raw()),
                                Id(VideoFormat::BGRA.as_raw()),
                            ],
                        },
                    ))),
                ),
                Property {
                    key: FormatProperties::VideoModifier.as_raw(),
                    flags: PropertyFlags::MANDATORY
                        | PropertyFlags::from_bits_retain(
                            pipewire::spa::sys::SPA_POD_PROP_FLAG_DONT_FIXATE,
                        ),
                    value: Value::Choice(ChoiceValue::Long(Choice(
                        ChoiceFlags::empty(),
                        ChoiceEnum::Enum {
                            default: linear,
                            alternatives: vec![linear],
                        },
                    ))),
                },
            ],
        };
        serialize(&Value::Object(object))
    }

    /// The `Buffers` parameter asking for `data_types` (a mask of
    /// `1 << SPA_DATA_*`).
    fn buffers_param(data_types: u32) -> Result<Vec<u8>> {
        let mask = i32::try_from(data_types).unwrap_or(i32::MAX);
        let object = Object {
            type_: SpaTypes::ObjectParamBuffers.as_raw(),
            id: pipewire::spa::param::ParamType::Buffers.as_raw(),
            properties: vec![Property::new(
                pipewire::spa::sys::SPA_PARAM_BUFFERS_dataType,
                Value::Choice(ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Flags {
                        default: mask,
                        flags: Vec::new(),
                    },
                ))),
            )],
        };
        serialize(&Value::Object(object))
    }

    fn serialize(value: &Value) -> Result<Vec<u8>> {
        Ok(
            PodSerializer::serialize(std::io::Cursor::new(Vec::new()), value)
                .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?
                .0
                .into_inner(),
        )
    }

    /// Whether a negotiated `Format` carries a modifier, which is what makes
    /// it a DMA-BUF format.
    ///
    /// Read off the pod itself rather than off `SPA_VIDEO_FLAG_MODIFIER`: that
    /// flag is set by `spa_format_video_raw_parse` only in PipeWire headers
    /// from 0.3.65, and ADR 0017's build floor is older than that.
    fn has_modifier(format: &Pod) -> bool {
        match pipewire::spa::pod::deserialize::PodDeserializer::deserialize_any_from(
            format.as_bytes(),
        ) {
            Ok((_, Value::Object(object))) => object
                .properties
                .iter()
                .any(|property| property.key == FormatProperties::VideoModifier.as_raw()),
            _ => false,
        }
    }

    /// One buffer of the stream, told apart from a later one at the same
    /// address by the generation it was added in.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct BufferToken {
        address: usize,
        generation: u64,
    }

    /// What a frame sends back when its last holder lets go of it.
    pub(crate) struct Requeue(BufferToken);

    /// The stream's buffers that exist right now, by address (ADR 0088).
    ///
    /// A buffer can be removed while a frame still holds it — a renegotiation
    /// replaces the pool — and its address can then be reused by a new one.
    /// Giving back the old token would queue a buffer this side never took, so
    /// a token is honoured only while its generation is still the live one.
    #[derive(Debug, Default)]
    pub(crate) struct LiveBuffers {
        by_address: HashMap<usize, u64>,
        next_generation: u64,
    }

    impl LiveBuffers {
        pub(crate) fn added(&mut self, address: usize) {
            self.next_generation = self.next_generation.wrapping_add(1);
            self.by_address.insert(address, self.next_generation);
        }

        pub(crate) fn removed(&mut self, address: usize) {
            self.by_address.remove(&address);
        }

        pub(crate) fn token(&self, address: usize) -> Option<BufferToken> {
            self.by_address
                .get(&address)
                .map(|&generation| BufferToken {
                    address,
                    generation,
                })
        }

        pub(crate) fn still_live(&self, token: BufferToken) -> bool {
            self.by_address.get(&token.address) == Some(&token.generation)
        }
    }

    /// Everything the stream's callbacks need about DMA-BUF.
    pub(crate) struct StreamState {
        offered: bool,
        /// The DRM format and modifier of the negotiated stream, when it is
        /// DMA-BUF.
        negotiated: Option<(u32, u64)>,
        live: Rc<RefCell<LiveBuffers>>,
        requeue: pipewire::channel::Sender<Requeue>,
    }

    impl StreamState {
        pub(crate) fn new(offered: bool) -> (Self, pipewire::channel::Receiver<Requeue>) {
            let (requeue, requeue_rx) = pipewire::channel::channel::<Requeue>();
            (
                Self {
                    offered,
                    negotiated: None,
                    live: Rc::default(),
                    requeue,
                },
                requeue_rx,
            )
        }

        /// Gives a buffer back to `stream` when a frame lets go of it.
        pub(crate) fn attach_requeue<'l>(
            &self,
            receiver: pipewire::channel::Receiver<Requeue>,
            mainloop: &'l pipewire::main_loop::MainLoop,
            stream: &pipewire::stream::Stream,
        ) -> pipewire::channel::AttachedReceiver<'l, Requeue> {
            let live = Rc::clone(&self.live);
            let stream_ptr = stream.as_raw_ptr();
            receiver.attach(mainloop.loop_(), move |Requeue(token)| {
                if !live.borrow().still_live(token) {
                    return;
                }
                // SAFETY: the address was dequeued from this stream by
                // `process` below and is still one of its buffers by the
                // generation check just made; nothing else queues it, since
                // one `Release` exists per dequeue. This closure runs on the
                // stream's own loop thread, the one PipeWire requires, and
                // the attached receiver it belongs to is dropped before the
                // stream is, so `stream_ptr` is live whenever it runs.
                unsafe {
                    pipewire::sys::pw_stream_queue_buffer(
                        stream_ptr,
                        token.address as *mut pipewire::sys::pw_buffer,
                    );
                }
            })
        }

        pub(crate) fn buffer_added(&self, buffer: *mut pipewire::sys::pw_buffer) {
            self.live.borrow_mut().added(buffer as usize);
        }

        pub(crate) fn buffer_removed(&self, buffer: *mut pipewire::sys::pw_buffer) {
            self.live.borrow_mut().removed(buffer as usize);
        }

        /// Answers a negotiated format with the buffers it needs.
        pub(crate) fn format_changed(
            &mut self,
            stream: &pipewire::stream::StreamRef,
            format: &Pod,
            video_format: VideoFormat,
            modifier: u64,
        ) {
            let dmabuf = self.offered && has_modifier(format);
            self.negotiated = if dmabuf {
                drm_format(video_format).map(|drm_format| (drm_format, modifier))
            } else {
                None
            };
            let data_types = if self.negotiated.is_some() {
                1 << pipewire::spa::sys::SPA_DATA_DmaBuf
            } else {
                (1 << pipewire::spa::sys::SPA_DATA_MemFd)
                    | (1 << pipewire::spa::sys::SPA_DATA_MemPtr)
            };
            tracing::info!(
                dmabuf = self.negotiated.is_some(),
                modifier,
                "the portal stream negotiated its buffers (ADR 0088)"
            );
            let Ok(bytes) = buffers_param(data_types) else {
                return;
            };
            let Some(pod) = Pod::from_bytes(&bytes) else {
                return;
            };
            if let Err(error) = stream.update_params(&mut [pod]) {
                tracing::warn!(%error, "the stream refused its buffer parameters");
            }
        }
    }

    /// The `process` callback: every buffer is dequeued raw, a DMA-BUF leaves
    /// with the frame, shared memory is packed and given straight back.
    pub(crate) fn process(stream: &pipewire::stream::StreamRef, user_data: &mut StreamUserData) {
        // SAFETY: called on the stream's loop thread from its own `process`
        // event, which is where PipeWire allows dequeuing. A null return is
        // "no buffer" and is checked before anything reads through it.
        let raw = unsafe { stream.dequeue_raw_buffer() };
        if raw.is_null() {
            return;
        }
        // SAFETY: `raw` is a buffer this stream just handed out, so its
        // `spa_buffer` and that buffer's `datas` array are valid for as long
        // as it stays dequeued; `Data` is `repr(transparent)` over
        // `spa_data`, which is how pipewire-rs's own `Buffer::datas_mut`
        // reads the same array.
        let datas: &mut [Data] = unsafe {
            let spa_buffer = (*raw).buffer;
            if spa_buffer.is_null() || (*spa_buffer).datas.is_null() {
                &mut []
            } else {
                std::slice::from_raw_parts_mut(
                    (*spa_buffer).datas.cast::<Data>(),
                    (*spa_buffer).n_datas as usize,
                )
            }
        };
        let give_back = || {
            // SAFETY: `raw` was dequeued from this stream above and has not
            // been queued since; this is its one return.
            unsafe { stream.queue_raw_buffer(raw) };
        };
        let Some(data) = datas.first_mut() else {
            give_back();
            return;
        };

        if data.type_() == DataType::DmaBuf {
            match (
                user_data.zero_copy.negotiated,
                dmabuf_frame(user_data, data, raw),
            ) {
                (Some(_), Some(frame)) => {
                    // A full channel drops the frame, and dropping it is what
                    // gives the buffer back.
                    let _ = user_data.sender.try_send(frame);
                }
                _ => give_back(),
            }
            return;
        }

        let stride = usize::try_from(data.chunk().stride().max(0)).unwrap_or(0);
        if let Some(bytes) = data.data() {
            super::send_packed(user_data, stride, bytes);
        }
        give_back();
    }

    /// A frame carrying the DMA-BUF `data` describes, holding `raw` until its
    /// last holder lets go.
    fn dmabuf_frame(
        user_data: &StreamUserData,
        data: &Data,
        raw: *mut pipewire::sys::pw_buffer,
    ) -> Option<Frame> {
        let (drm_format, modifier) = user_data.zero_copy.negotiated?;
        let (width, height) = (user_data.width, user_data.height);
        let chunk = data.chunk();
        // Not `chunk.size()`: for a DMA-BUF it is held to nothing (see
        // `buffer_size`). A corrupted chunk is a copy that failed on the
        // compositor's side, which `xdg-desktop-portal-wlr` does send.
        if width == 0
            || height == 0
            || chunk
                .flags()
                .contains(pipewire::spa::buffer::ChunkFlags::CORRUPTED)
        {
            return None;
        }
        let token = user_data.zero_copy.live.borrow().token(raw as usize)?;
        let raw_fd = RawFd::try_from(data.as_raw().fd).ok()?;
        // SAFETY: a DMA-BUF `spa_data` carries a file descriptor the stream
        // owns and keeps open while the buffer is dequeued, which it is for
        // the whole of this call; it is only borrowed long enough to be
        // duplicated into a descriptor this frame owns outright.
        let fd = unsafe { BorrowedFd::borrow_raw(raw_fd) }
            .try_clone_to_owned()
            .ok()?;
        let size = buffer_size(&fd).unwrap_or(data.as_raw().maxsize);
        let dmabuf = DmaBuf {
            fd,
            drm_format,
            modifier,
            offset: chunk.offset(),
            stride: u32::try_from(chunk.stride().max(0)).unwrap_or(0),
            width,
            height,
            size,
            readback: OnceLock::new(),
            _release: Some(Release {
                token,
                sender: user_data.zero_copy.requeue.clone(),
            }),
        };
        let mut frame = Frame::cpu(
            width,
            height,
            PixelFormat::Bgra8,
            timestamp_us(user_data.started_at),
            Vec::new(),
        );
        frame.dmabuf = Some(Arc::new(dmabuf));
        Some(frame)
    }

    /// The size of the DMA-BUF behind `fd`, from the kernel.
    ///
    /// Not the `spa_data`'s own `maxsize` and not its chunk's `size`: for a
    /// DMA-BUF neither is held to anything, and `xdg-desktop-portal-wlr` was
    /// seen sending `0` and `9` for a 1280x720 picture (ADR 0088). Seeking to
    /// the end of a DMA-BUF is how the kernel reports its size, and moves no
    /// position that anything reads from: the buffer is mapped, not read.
    fn buffer_size(fd: &OwnedFd) -> Option<u32> {
        use std::io::Seek as _;

        let mut file = std::fs::File::from(fd.try_clone().ok()?);
        u32::try_from(file.seek(std::io::SeekFrom::End(0)).ok()?).ok()
    }

    /// Sends a buffer's token back to its stream's loop on drop.
    struct Release {
        token: BufferToken,
        sender: pipewire::channel::Sender<Requeue>,
    }

    impl Drop for Release {
        fn drop(&mut self) {
            // A stream that has already stopped has nothing to give it back
            // to; the frame's own descriptor keeps the memory alive until it
            // goes.
            let _ = self.sender.send(Requeue(self.token));
        }
    }

    /// A captured frame's pixels as a DMA-BUF the compositor rendered
    /// (ADR 0088).
    ///
    /// Holds its own duplicate of the buffer's file descriptor, so the memory
    /// outlives the stream if it has to, and — when it came from a stream —
    /// the right to give that buffer back, which it exercises when dropped.
    /// Until then the compositor draws into other buffers of its pool, and
    /// the pixels an encoder reads are the ones this frame was captured with.
    pub struct DmaBuf {
        fd: OwnedFd,
        drm_format: u32,
        modifier: u64,
        offset: u32,
        stride: u32,
        width: u32,
        height: u32,
        size: u32,
        readback: OnceLock<std::result::Result<Vec<u8>, String>>,
        _release: Option<Release>,
    }

    impl std::fmt::Debug for DmaBuf {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("DmaBuf")
                .field("width", &self.width)
                .field("height", &self.height)
                .field("drm_format", &self.drm_format)
                .field("modifier", &self.modifier)
                .finish_non_exhaustive()
        }
    }

    impl DmaBuf {
        /// A DMA-BUF that belongs to no stream, for a test that exports one
        /// itself and for nothing else.
        #[cfg(test)]
        #[allow(
            clippy::too_many_arguments,
            reason = "the eight numbers that describe one DMA-BUF plane"
        )]
        pub(crate) fn detached(
            fd: OwnedFd,
            drm_format: u32,
            modifier: u64,
            offset: u32,
            stride: u32,
            width: u32,
            height: u32,
            size: u32,
        ) -> Self {
            Self {
                fd,
                drm_format,
                modifier,
                offset,
                stride,
                width,
                height,
                size,
                readback: OnceLock::new(),
                _release: None,
            }
        }

        /// The buffer's file descriptor, for importing it.
        #[must_use]
        pub fn fd(&self) -> BorrowedFd<'_> {
            use std::os::fd::AsFd as _;
            self.fd.as_fd()
        }

        /// DRM format code of the one plane (`XR24` or `AR24`).
        #[must_use]
        pub const fn drm_format(&self) -> u32 {
            self.drm_format
        }

        /// DRM format modifier: always linear here.
        #[must_use]
        pub const fn modifier(&self) -> u64 {
            self.modifier
        }

        /// Offset of the first pixel within the buffer, in bytes.
        #[must_use]
        pub const fn offset(&self) -> u32 {
            self.offset
        }

        /// Bytes per row.
        #[must_use]
        pub const fn stride(&self) -> u32 {
            self.stride
        }

        /// Width in pixels.
        #[must_use]
        pub const fn width(&self) -> u32 {
            self.width
        }

        /// Height in pixels.
        #[must_use]
        pub const fn height(&self) -> u32 {
            self.height
        }

        /// Size of the whole buffer object, in bytes.
        #[must_use]
        pub const fn size(&self) -> u32 {
            self.size
        }

        /// The kernel's name for the driver that allocated this buffer
        /// (`i915`, `amdgpu`, …), from `/proc/self/fdinfo`, or `None` when the
        /// kernel does not say.
        ///
        /// What the encoder compares with its own device: a buffer from another
        /// GPU would import, if at all, as a copy through main memory, which is
        /// the one thing this path exists to avoid (ADR 0088).
        #[must_use]
        pub fn exporter(&self) -> Option<String> {
            self.fdinfo_field("exp_name")
        }

        /// A number naming the buffer itself rather than this descriptor for
        /// it: every duplicate of one DMA-BUF has the same inode.
        #[must_use]
        pub fn identity(&self) -> Option<u64> {
            self.fdinfo_field("ino")?.parse().ok()
        }

        fn fdinfo_field(&self, name: &str) -> Option<String> {
            let info =
                std::fs::read_to_string(format!("/proc/self/fdinfo/{}", self.fd.as_raw_fd()))
                    .ok()?;
            info.lines().find_map(|line| {
                let (key, value) = line.split_once(':')?;
                (key.trim() == name).then(|| value.trim().to_owned())
            })
        }

        /// The pixels as tightly packed BGRA, read back once and remembered.
        ///
        /// # Errors
        /// [`MediaError::CaptureInterrupted`] when the buffer cannot be mapped
        /// or synchronized, or is smaller than the picture it describes.
        pub fn pixels(&self) -> Result<&[u8]> {
            self.readback
                .get_or_init(|| self.read_back())
                .as_deref()
                .map_err(|reason| MediaError::CaptureInterrupted(reason.clone()))
        }

        fn read_back(&self) -> std::result::Result<Vec<u8>, String> {
            if self.modifier != DRM_FORMAT_MOD_LINEAR {
                return Err(
                    "a DMA-BUF with a tiled or compressed layout cannot be read row by row"
                        .to_owned(),
                );
            }
            let row = self.width as usize * 4;
            let stride = (self.stride as usize).max(row);
            let height = self.height as usize;
            let needed = (self.offset as usize)
                .checked_add(
                    stride
                        .checked_mul(height.saturating_sub(1))
                        .ok_or("overflow")?,
                )
                .and_then(|end| end.checked_add(row))
                .ok_or("the picture's extent overflows")?;
            if row == 0 || height == 0 || needed > self.size as usize {
                return Err("the DMA-BUF is smaller than the picture it describes".to_owned());
            }
            // SAFETY: a read-only mapping of `size` bytes of a DMA-BUF whose
            // descriptor this struct owns. The mapping is dropped before this
            // function returns, and every byte read from it is bracketed by
            // `DMA_BUF_IOCTL_SYNC`, which is the kernel's contract for CPU
            // access to a buffer a GPU may still be writing.
            let map = unsafe {
                memmap2::MmapOptions::new()
                    .len(self.size as usize)
                    .map(&self.fd)
            }
            .map_err(|error| format!("mmap of the DMA-BUF: {error}"))?;
            self.sync(DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ)?;
            let mut packed = Vec::with_capacity(row * height);
            for index in 0..height {
                let start = self.offset as usize + index * stride;
                packed.extend_from_slice(&map[start..start + row]);
            }
            self.sync(DMA_BUF_SYNC_END | DMA_BUF_SYNC_READ)?;
            Ok(packed)
        }

        fn sync(&self, flags: u64) -> std::result::Result<(), String> {
            for _ in 0..SYNC_ATTEMPTS {
                // SAFETY: `DMA_BUF_IOCTL_SYNC` takes a pointer to one `u64`
                // of flags (`struct dma_buf_sync`), which lives on this stack
                // frame for the duration of the call, on a descriptor this
                // struct owns.
                let result = unsafe {
                    libc::ioctl(self.fd.as_raw_fd(), DMA_BUF_IOCTL_SYNC, &raw const flags)
                };
                if result == 0 {
                    return Ok(());
                }
                let error = std::io::Error::last_os_error();
                if !matches!(error.raw_os_error(), Some(libc::EINTR | libc::EAGAIN)) {
                    return Err(format!("DMA_BUF_IOCTL_SYNC: {error}"));
                }
            }
            Err("DMA_BUF_IOCTL_SYNC kept being interrupted".to_owned())
        }
    }

    #[cfg(test)]
    mod tests {
        #![allow(clippy::unwrap_used, reason = "a failed assumption must fail the test")]

        use super::*;
        use crate::capture::pipewire_stream::{PipeWireFrameThread, StreamSize};
        use std::sync::Arc;

        #[test]
        fn drm_codes_are_the_little_endian_fourccs_of_the_spa_formats() {
            assert_eq!(DRM_FORMAT_XRGB8888, 0x3432_5258);
            assert_eq!(DRM_FORMAT_ARGB8888, 0x3432_5241);
            assert_eq!(drm_format(VideoFormat::BGRx), Some(DRM_FORMAT_XRGB8888));
            assert_eq!(drm_format(VideoFormat::BGRA), Some(DRM_FORMAT_ARGB8888));
            assert_eq!(drm_format(VideoFormat::RGBx), None);
        }

        /// ADR 0088: a token from before a buffer was removed must not queue
        /// whatever buffer later took its address.
        #[test]
        fn a_token_is_honoured_only_for_the_buffer_it_was_taken_from() {
            let mut live = LiveBuffers::default();
            live.added(0x1000);
            let first = live.token(0x1000).unwrap_or_else(|| panic!("just added"));
            assert!(live.still_live(first));

            live.removed(0x1000);
            assert!(!live.still_live(first), "a removed buffer went back");

            live.added(0x1000);
            let second = live.token(0x1000).unwrap_or_else(|| panic!("just added"));
            assert!(live.still_live(second));
            assert!(
                !live.still_live(first),
                "an old token queued the buffer that reused its address"
            );
            assert_eq!(live.token(0x2000), None);
        }

        #[test]
        fn the_offered_formats_serialize() {
            assert!(enum_format().is_ok_and(|bytes| Pod::from_bytes(&bytes).is_some()));
            assert!(buffers_param(1 << 3).is_ok_and(|bytes| Pod::from_bytes(&bytes).is_some()));
        }

        #[test]
        fn a_format_with_a_modifier_is_told_from_one_without() {
            let with = enum_format().unwrap_or_default();
            let Some(with) = Pod::from_bytes(&with) else {
                panic!("the DMA-BUF format has to parse");
            };
            assert!(has_modifier(with));
            let without = serialize(&Value::Object(Object {
                type_: SpaTypes::ObjectParamFormat.as_raw(),
                id: pipewire::spa::param::ParamType::EnumFormat.as_raw(),
                properties: vec![Property::new(
                    FormatProperties::MediaType.as_raw(),
                    Value::Id(Id(MediaType::Video.as_raw())),
                )],
            }))
            .unwrap_or_default();
            let Some(without) = Pod::from_bytes(&without) else {
                panic!("the plain format has to parse");
            };
            assert!(!has_modifier(without));
        }

        /// A ScreenCast-only portal session's node id, and the session that
        /// keeps it alive: no dialog, where a compositor's portal is set to
        /// share without asking.
        fn screencast_node(
            runtime: &tokio::runtime::Runtime,
        ) -> (
            ashpd::desktop::screencast::Screencast,
            ashpd::desktop::Session<ashpd::desktop::screencast::Screencast>,
            u32,
        ) {
            use ashpd::desktop::PersistMode;
            use ashpd::desktop::screencast::{
                CursorMode, Screencast, SelectSourcesOptions, SourceType, StartCastOptions,
            };

            runtime.block_on(async {
                let screencast = Screencast::new().await.unwrap();
                #[allow(
                    clippy::default_trait_access,
                    reason = "ashpd keeps CreateSessionOptions private"
                )]
                let session = screencast.create_session(Default::default()).await.unwrap();
                screencast
                    .select_sources(
                        &session,
                        SelectSourcesOptions::default()
                            .set_cursor_mode(CursorMode::Embedded)
                            .set_sources(ashpd::enumflags2::BitFlags::from(SourceType::Monitor))
                            .set_multiple(false)
                            .set_persist_mode(PersistMode::DoNot),
                    )
                    .await
                    .unwrap();
                let streams = screencast
                    .start(&session, None, StartCastOptions::default())
                    .await
                    .unwrap()
                    .response()
                    .unwrap();
                let node = streams.streams().first().unwrap().pipe_wire_node_id();
                (screencast, session, node)
            })
        }

        /// The first frame a stream delivers, within a few seconds.
        fn first_frame(thread: &PipeWireFrameThread) -> Frame {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                if let Some(frame) = thread.try_recv_frame() {
                    return frame;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "the stream delivered no frame"
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }

        /// ADR 0088, against a real compositor: offered DMA-BUF, a portal
        /// stream hands over the GPU's buffer, which the VA-API encoder takes
        /// without a readback; not offered it, nothing arrives as a DMA-BUF.
        /// Opt-in, because it needs a running
        /// compositor whose portal shares a monitor without a dialog
        /// (`xdg-desktop-portal-wlr` with `chooser_type=none` does).
        #[test]
        fn a_portal_stream_delivers_dmabuf_only_when_it_is_offered() {
            use crate::encode::VideoEncoder as _;
            use crate::encode::linux_vaapi::VaapiEncoder;

            if std::env::var("LUMEPEER_TEST_PORTAL_SCREENCAST").as_deref() != Ok("1") {
                eprintln!(
                    "skipping: set LUMEPEER_TEST_PORTAL_SCREENCAST=1 with a compositor to run this"
                );
                return;
            }
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            let (_proxy, session, node) = screencast_node(&runtime);
            let offered =
                PipeWireFrameThread::spawn_offering(node, Arc::new(StreamSize::default()), true)
                    .unwrap();
            let frame = first_frame(&offered);
            let Some(dmabuf) = frame.dmabuf.clone() else {
                panic!("offered DMA-BUF, the stream still sent shared memory");
            };
            assert!(frame.data.is_empty(), "a DMA-BUF frame was read back");
            assert!(dmabuf.exporter().is_some(), "the kernel names no exporter");

            let mut encoder = VaapiEncoder::new(crate::encode::EncoderConfig::default()).unwrap();
            assert!(encoder.encode(&frame).unwrap().keyframe);
            assert!(
                frame.data.is_empty(),
                "the encoder read the frame back instead of importing it"
            );
            let gpu_pixels = (dmabuf.modifier() == DRM_FORMAT_MOD_LINEAR)
                .then(|| dmabuf.pixels().unwrap().to_vec());
            drop((frame, dmabuf, offered));
            runtime.block_on(session.close()).unwrap();

            let (_proxy, session, node) = screencast_node(&runtime);
            let plain =
                PipeWireFrameThread::spawn_offering(node, Arc::new(StreamSize::default()), false)
                    .unwrap();
            // Not offered DMA-BUF, nothing may arrive as one. Whether shared
            // memory arrives at all is the compositor's choice: the one this
            // was written against (`xdg-desktop-portal-wlr` on a headless
            // sway) offers its shared memory only as RGBx, which the fixed
            // BGRx request of §11 does not take (ADR 0088).
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while std::time::Instant::now() < deadline {
                if let Some(frame) = plain.try_recv_frame() {
                    assert!(
                        frame.dmabuf.is_none(),
                        "not offered DMA-BUF, it came anyway"
                    );
                    assert_eq!(
                        frame.data.len(),
                        frame.width as usize * frame.height as usize * 4
                    );
                    if let Some(gpu_pixels) = &gpu_pixels {
                        assert_eq!(gpu_pixels.len(), frame.data.len());
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            drop(plain);
            runtime.block_on(session.close()).unwrap();
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
