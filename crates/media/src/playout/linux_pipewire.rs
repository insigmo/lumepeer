//! PipeWire playback for Linux (§11; ADR 0023, ADR 0028).
//!
//! The mirror of [`crate::capture_audio::linux_pipewire`], pointed the other
//! way: a `Direction::Output` stream the session manager connects to the
//! default sink, fed one wire-format chunk at a time.
//!
//! The conversion is [`crate::playout::to_device_pcm`], the same function the
//! WASAPI backend uses — the wire format is decided by `AUDIO_SAMPLE_RATE_HZ`
//! / `AUDIO_CHANNELS` / `AUDIO_FRAME_MS` and nothing here negotiates per
//! session. The stream *asks* for exactly the wire format, so on a normal
//! graph that conversion is a plain s16→f32 widening and PipeWire's own
//! converter handles the sink; when the graph answers with something else,
//! `param_changed` publishes what it actually chose and `push` resamples onto
//! that instead of playing back at the wrong speed.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::thread::JoinHandle;

use crate::error::{MediaError, Result};
use crate::playout::{AudioPlayer, to_device_pcm};
use lumepeer_core::constants::{AUDIO_CHANNELS, AUDIO_SAMPLE_RATE_HZ};

/// Chunks buffered towards the sink before `push` gives up on a chunk: 8 of
/// them is 160 ms at `AUDIO_FRAME_MS`. Deliberately shallower than the
/// capture side's queue — this one is latency the listener hears, and audio
/// already 160 ms behind is better dropped than played late.
const CHANNEL_DEPTH: usize = 8;

/// Sent to shut the PipeWire thread down.
struct Shutdown;

/// The format the graph actually negotiated, published from the PipeWire
/// thread's `param_changed` for `push` to convert onto.
///
/// The same shape as `linux_wayland::portal::StreamSize`, and for the same
/// reason: one value written on the loop thread, read on the caller's, with
/// nothing to order it against.
#[derive(Debug)]
struct DeviceFormat {
    rate: AtomicU32,
    channels: AtomicU32,
}

impl DeviceFormat {
    /// Starts at the wire format, which is what the stream asks for and what
    /// a graph that has not answered yet will most likely give.
    fn new() -> Self {
        Self {
            rate: AtomicU32::new(AUDIO_SAMPLE_RATE_HZ),
            channels: AtomicU32::new(u32::from(AUDIO_CHANNELS)),
        }
    }

    fn set(&self, rate: u32, channels: u32) {
        if rate > 0 {
            self.rate.store(rate, Ordering::Relaxed);
        }
        if channels > 0 {
            self.channels.store(channels, Ordering::Relaxed);
        }
    }

    fn get(&self) -> (u32, usize) {
        (
            self.rate.load(Ordering::Relaxed),
            usize::try_from(self.channels.load(Ordering::Relaxed))
                .unwrap_or(usize::from(AUDIO_CHANNELS)),
        )
    }
}

/// State the `process` callback drains from.
struct StreamUserData {
    /// Device-format samples waiting to be handed to the sink.
    pending: Vec<f32>,
    incoming: Receiver<Vec<f32>>,
    format: Arc<DeviceFormat>,
}

/// PipeWire playback of the default sink.
pub struct PipewirePlayout {
    tx: Option<SyncSender<Vec<f32>>>,
    handle: Option<JoinHandle<()>>,
    shutdown: Option<pipewire::channel::Sender<Shutdown>>,
    format: Arc<DeviceFormat>,
}

impl std::fmt::Debug for PipewirePlayout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PipewirePlayout")
            .field("active", &self.tx.is_some())
            .field("format", &self.format)
            // The thread handle and the PipeWire channel have no useful
            // debug form and are summarized by `active` above.
            .finish_non_exhaustive()
    }
}

impl PipewirePlayout {
    /// Builds an idle player; nothing opens until [`AudioPlayer::start`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            tx: None,
            handle: None,
            shutdown: None,
            format: Arc::new(DeviceFormat::new()),
        }
    }

    fn run(
        incoming: Receiver<Vec<f32>>,
        shutdown_rx: pipewire::channel::Receiver<Shutdown>,
        format: &Arc<DeviceFormat>,
    ) -> Result<()> {
        use pipewire::spa::param::audio::AudioInfoRaw;
        use pipewire::spa::pod::Pod;
        use pipewire::spa::utils::Direction;
        use pipewire::stream::StreamFlags;

        pipewire::init();
        let mainloop = pipewire::main_loop::MainLoop::new(None)
            .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;
        let context = pipewire::context::Context::new(&mainloop)
            .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;
        let core = context
            .connect(None)
            .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;

        let _shutdown_listener = {
            let quit = mainloop.clone();
            shutdown_rx.attach(mainloop.loop_(), move |Shutdown| quit.quit())
        };

        let props = pipewire::properties::properties! {
            *pipewire::keys::MEDIA_TYPE => "Audio",
            *pipewire::keys::MEDIA_CATEGORY => "Playback",
            *pipewire::keys::MEDIA_ROLE => "Communication",
        };
        let stream = pipewire::stream::Stream::new(&core, "lumepeer-remote-audio", props)
            .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;

        let data = StreamUserData {
            pending: Vec::new(),
            incoming,
            format: Arc::clone(format),
        };

        let _listener = stream
            .add_local_listener_with_user_data(data)
            .param_changed(|_, user_data, id, param| {
                let Some(param) = param else { return };
                if id != pipewire::spa::param::ParamType::Format.as_raw() {
                    return;
                }
                let mut info = AudioInfoRaw::new();
                if info.parse(param).is_err() {
                    return;
                }
                user_data.format.set(info.rate(), info.channels());
                // Anything already converted was converted for the old
                // format, so it would play at the wrong speed.
                user_data.pending.clear();
            })
            .process(|stream, user_data| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                fill_buffer(user_data, &mut buffer);
            })
            .register()
            .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;

        let values = wire_format_pod()?;
        let format_pod = Pod::from_bytes(&values).ok_or_else(|| {
            MediaError::CaptureUnavailable("could not build the audio format pod".to_owned())
        })?;
        let mut params = [format_pod];

        stream
            .connect(
                Direction::Output,
                None,
                StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
                &mut params,
            )
            .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;

        mainloop.run();
        Ok(())
    }
}

/// Serializes the §11 wire format as a SPA `EnumFormat` pod, the playback
/// twin of the capture side's.
fn wire_format_pod() -> Result<Vec<u8>> {
    use pipewire::spa::param::audio::{AudioFormat, AudioInfoRaw};
    use pipewire::spa::pod::Value;
    use pipewire::spa::pod::serialize::PodSerializer;
    use pipewire::spa::utils::SpaTypes;

    let mut audio_info = AudioInfoRaw::new();
    audio_info.set_format(AudioFormat::F32LE);
    audio_info.set_rate(AUDIO_SAMPLE_RATE_HZ);
    audio_info.set_channels(u32::from(AUDIO_CHANNELS));
    Ok(PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &Value::Object(pipewire::spa::pod::Object {
            type_: SpaTypes::ObjectParamFormat.as_raw(),
            id: pipewire::spa::param::ParamType::EnumFormat.as_raw(),
            properties: audio_info.into(),
        }),
    )
    .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?
    .0
    .into_inner())
}

/// Bytes one f32 sample occupies in the negotiated `F32LE` buffers.
const SAMPLE_BYTES: usize = 4;

/// Writes as much of `pending` into `buffer` as the sink asked for, topping
/// `pending` up from the channel first and padding with silence if there is
/// not enough.
///
/// Silence rather than a short write on purpose: PipeWire reads the size this
/// sets, and a gap in the sender's clock is real silence to the listener. A
/// short write there is a click.
fn fill_buffer(user_data: &mut StreamUserData, buffer: &mut pipewire::buffer::Buffer<'_>) {
    // Everything the sender has queued, converted already.
    while let Ok(chunk) = user_data.incoming.try_recv() {
        user_data.pending.extend_from_slice(&chunk);
    }

    let Some(data) = buffer.datas_mut().first_mut() else {
        return;
    };
    let Some(bytes) = data.data() else { return };
    let capacity_samples = bytes.len() / SAMPLE_BYTES;
    if capacity_samples == 0 {
        return;
    }

    let taken = user_data.pending.len().min(capacity_samples);
    for (index, sample) in user_data.pending.drain(..taken).enumerate() {
        let start = index * SAMPLE_BYTES;
        bytes[start..start + SAMPLE_BYTES].copy_from_slice(&sample.to_le_bytes());
    }
    // The rest of what the sink asked for is silence, not stale audio.
    for byte in bytes
        .iter_mut()
        .take(capacity_samples * SAMPLE_BYTES)
        .skip(taken * SAMPLE_BYTES)
    {
        *byte = 0;
    }

    let (_, channels) = user_data.format.get();
    // libspa exposes the chunk descriptor as `*_mut` accessors rather than
    // setters; these three are the whole contract with the sink — where the
    // audio starts, how wide one frame is, and how much of the buffer is
    // real.
    let chunk = data.chunk_mut();
    *chunk.offset_mut() = 0;
    *chunk.stride_mut() =
        i32::try_from(SAMPLE_BYTES.saturating_mul(channels.max(1))).unwrap_or(i32::MAX);
    *chunk.size_mut() = u32::try_from(capacity_samples.saturating_mul(SAMPLE_BYTES)).unwrap_or(0);
}

impl Default for PipewirePlayout {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioPlayer for PipewirePlayout {
    fn start(&mut self) -> Result<()> {
        if self.tx.is_some() {
            return Ok(());
        }
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(CHANNEL_DEPTH);
        let (shutdown_tx, shutdown_rx) = pipewire::channel::channel::<Shutdown>();
        let format = Arc::clone(&self.format);

        let handle = std::thread::Builder::new()
            .name("lumepeer-pipewire-playout".to_owned())
            .spawn(move || {
                if let Err(err) = Self::run(rx, shutdown_rx, &format) {
                    tracing::warn!("pipewire playout thread exited: {err}");
                }
            })
            .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;

        self.tx = Some(tx);
        self.handle = Some(handle);
        self.shutdown = Some(shutdown_tx);
        Ok(())
    }

    fn push(&mut self, samples: &[i16], _timestamp_us: u64) -> Result<()> {
        let tx = self
            .tx
            .as_ref()
            .ok_or_else(|| MediaError::CaptureInterrupted("playback not started".to_owned()))?;
        let (rate, channels) = self.format.get();
        let converted = to_device_pcm(samples, rate, channels);
        if converted.is_empty() {
            return Ok(());
        }
        match tx.try_send(converted) {
            Ok(()) => Ok(()),
            // A full queue is 160 ms already waiting to be heard. Dropping
            // the newest chunk keeps the delay from growing without bound,
            // and is not a session error (§18).
            Err(TrySendError::Full(_)) => {
                tracing::debug!("playout queue is full: dropping a chunk rather than adding delay");
                Ok(())
            }
            Err(TrySendError::Disconnected(_)) => Err(MediaError::CaptureInterrupted(
                "the playout thread is gone".to_owned(),
            )),
        }
    }

    fn stop(&mut self) {
        // Same order as the capture side: quit the loop, drop the sender,
        // join. The thread lives inside `MainLoop::run` and nothing else
        // wakes it.
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(Shutdown);
        }
        self.tx = None;
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for PipewirePlayout {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failed assumption must fail the test")]

    use super::*;

    /// The negotiated format is what `push` converts onto, so a graph that
    /// answered with something other than the wire rate must be visible to
    /// the converter — otherwise playback runs fast or slow.
    #[test]
    fn the_negotiated_format_is_what_push_converts_onto() {
        let format = DeviceFormat::new();
        assert_eq!(format.get(), (AUDIO_SAMPLE_RATE_HZ, 2));

        format.set(44_100, 1);
        assert_eq!(format.get(), (44_100, 1));

        // One 20 ms wire chunk resampled onto 44.1 kHz mono is 882 samples
        // in exact arithmetic. `to_device_pcm` floors a float quotient, so it
        // can land one sample short — the same tail truncation the WASAPI
        // path has always had, and inaudible at 1/44100 s. What this test
        // pins is that the *negotiated* rate drove the conversion at all: at
        // the wire rate the answer would be 960.
        let wire = vec![0i16; crate::capture_audio::SAMPLES_PER_CHUNK * 2];
        let (rate, channels) = format.get();
        let produced = to_device_pcm(&wire, rate, channels).len();
        assert!(
            produced.abs_diff(882) <= 1,
            "expected ~882 samples at 44.1 kHz mono, got {produced}"
        );
    }

    /// A zero rate or channel count is a graph that answered with nothing
    /// usable; keeping the previous value beats dividing by it.
    #[test]
    fn a_nonsense_format_leaves_the_previous_one_standing() {
        let format = DeviceFormat::new();
        format.set(0, 0);
        assert_eq!(format.get(), (AUDIO_SAMPLE_RATE_HZ, 2));
    }
}
