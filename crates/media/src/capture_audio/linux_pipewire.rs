//! PipeWire monitor capture for Linux (§11; questions.md item 8; ADR 0023).
//!
//! The default sink's *monitor* port is the PipeWire equivalent of WASAPI
//! loopback: a real source that produces exactly what plays out of the
//! speakers. PulseAudio-only hosts reach it through `pipewire-pulse`, which
//! exposes the same graph, so one backend covers both desktops.
//!
//! The blocking pull model matches [`crate::capture::linux_x11`]: the stream
//! runs on its own thread and pushes wire-shaped chunks into a bounded
//! channel; `next_chunk` drains that channel.
//!
//! Structured the same way as [`crate::capture::pipewire_stream`], and for
//! the same reason: `MainLoop::run` blocks for the life of the stream, so it
//! cannot share a thread with anything that has to answer in the meantime.
//! What differs is only the direction of the flow control — the video path
//! drops a frame the consumer did not keep up with, because a stale frame is
//! worthless, while this one blocks, because dropping a chunk of audio is an
//! audible click and 20 ms of backpressure is not.

use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender};
use std::thread::JoinHandle;

use crate::capture_audio::{
    AudioCapturer, PcmChunk, READ_TIMEOUT, SAMPLES_PER_CHUNK, capture_timestamp_us, to_wire_pcm,
};
use crate::error::{MediaError, Result};
use lumepeer_core::constants::AUDIO_CHANNELS;

/// Chunks the channel holds before the PipeWire thread blocks: 16 of them is
/// 320 ms at `AUDIO_FRAME_MS`, enough to ride out a scheduling hiccup in the
/// encoder without letting a stalled consumer grow a queue that would arrive
/// as latency rather than audio (§3.2).
const CHANNEL_DEPTH: usize = 16;

/// What the stream asks PipeWire for. Not a negotiation: §11 fixes the wire
/// format, so this asks for exactly it and lets PipeWire's own converter do
/// the resampling that every graph already has in it. `to_wire_pcm` still
/// runs on the result — PipeWire may hand back a different rate if the
/// requested one cannot be met, and the wire format is not a request.
const STREAM_RATE_HZ: u32 = lumepeer_core::constants::AUDIO_SAMPLE_RATE_HZ;

/// Sent to shut the PipeWire thread down; see `pipewire::channel`, which
/// exists exactly for signaling a loop running on another thread.
struct Shutdown;

/// Per-stream state the `process` callback accumulates into.
struct StreamUserData {
    sender: SyncSender<PcmChunk>,
    /// Rate PipeWire actually negotiated, which drives `to_wire_pcm`.
    rate: u32,
    /// Channel count PipeWire actually negotiated.
    channels: usize,
    /// Samples read but not yet long enough to make a chunk. PipeWire's
    /// buffer size is its own business and has nothing to do with
    /// `AUDIO_FRAME_MS`, so chunking happens here rather than being assumed.
    pending: Vec<f32>,
}

/// PipeWire monitor capturer of the default output sink.
pub struct PipewireMonitorCapturer {
    rx: Option<Receiver<PcmChunk>>,
    handle: Option<JoinHandle<()>>,
    shutdown: Option<pipewire::channel::Sender<Shutdown>>,
}

// `pipewire::channel::Sender` does not implement `Debug`, so this is written
// by hand rather than derived, matching `PipeWireFrameThread`.
impl std::fmt::Debug for PipewireMonitorCapturer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PipewireMonitorCapturer")
            .field("active", &self.rx.is_some())
            .finish_non_exhaustive()
    }
}

impl PipewireMonitorCapturer {
    /// Builds an idle capturer; nothing opens until [`AudioCapturer::start`].
    #[must_use]
    pub const fn new() -> Self {
        Self {
            rx: None,
            handle: None,
            shutdown: None,
        }
    }

    /// Runs the PipeWire loop for the life of the capture. Returns when
    /// `Shutdown` arrives or the graph goes away.
    fn run(
        tx: &SyncSender<PcmChunk>,
        shutdown_rx: pipewire::channel::Receiver<Shutdown>,
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
            // Two handles for the same reason as the video path: `attach`
            // borrows the loop for the listener's life, and the closure needs
            // an owned handle to quit with.
            let quit = mainloop.clone();
            shutdown_rx.attach(mainloop.loop_(), move |Shutdown| quit.quit())
        };

        // `STREAM_CAPTURE_SINK` is the whole trick: it tells PipeWire this
        // capture stream wants a *sink's monitor*, not a microphone. Without
        // it the session manager connects a Capture stream to the default
        // source and the guest hears the host's microphone where the desktop
        // audio should be.
        let props = pipewire::properties::properties! {
            *pipewire::keys::MEDIA_TYPE => "Audio",
            *pipewire::keys::MEDIA_CATEGORY => "Capture",
            *pipewire::keys::MEDIA_ROLE => "Music",
            *pipewire::keys::STREAM_CAPTURE_SINK => "true",
        };
        let stream = pipewire::stream::Stream::new(&core, "lumepeer-desktop-audio", props)
            .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;

        let data = StreamUserData {
            sender: tx.clone(),
            rate: STREAM_RATE_HZ,
            channels: usize::from(AUDIO_CHANNELS),
            pending: Vec::new(),
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
                // What was actually negotiated, which is what the converter
                // has to be told. Assuming the requested rate here is how a
                // capture ends up playing back at the wrong speed.
                if info.rate() > 0 {
                    user_data.rate = info.rate();
                }
                if info.channels() > 0 {
                    user_data.channels =
                        usize::try_from(info.channels()).unwrap_or(usize::from(AUDIO_CHANNELS));
                }
                user_data.pending.clear();
            })
            .process(|stream, user_data| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let Some(data) = buffer.datas_mut().first_mut() else {
                    return;
                };
                // Read before `data()`: the chunk descriptor borrows the
                // same `data` the mapped bytes do.
                let offset = usize::try_from(data.chunk().offset()).unwrap_or(0);
                let size = usize::try_from(data.chunk().size()).unwrap_or(0);
                let Some(bytes) = data.data() else { return };
                let Some(payload) = bytes.get(offset..offset.saturating_add(size)) else {
                    return;
                };

                // F32LE is what was asked for, so four bytes per sample.
                for sample in payload.chunks_exact(4) {
                    // `chunks_exact(4)` guarantees the length; the array is
                    // built rather than transmuted so no alignment is assumed
                    // about a buffer PipeWire owns.
                    let raw = [sample[0], sample[1], sample[2], sample[3]];
                    user_data.pending.push(f32::from_le_bytes(raw));
                }

                emit_chunks(user_data);
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
                Direction::Input,
                None,
                StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
                &mut params,
            )
            .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;

        mainloop.run();
        Ok(())
    }
}

/// Serializes the §11 wire format as a SPA `EnumFormat` pod.
///
/// Asking for exactly the wire format lets PipeWire's own converter — which
/// every graph already has in it — do the resampling, so the common case
/// reaches [`to_wire_pcm`] as a no-op. It is a request, not a guarantee:
/// `param_changed` reads back what was actually negotiated.
fn wire_format_pod() -> Result<Vec<u8>> {
    use pipewire::spa::param::audio::{AudioFormat, AudioInfoRaw};
    use pipewire::spa::pod::Value;
    use pipewire::spa::pod::serialize::PodSerializer;
    use pipewire::spa::utils::SpaTypes;

    let mut audio_info = AudioInfoRaw::new();
    audio_info.set_format(AudioFormat::F32LE);
    audio_info.set_rate(STREAM_RATE_HZ);
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

/// Drains whole `AUDIO_FRAME_MS` chunks out of `pending` and sends them.
///
/// Split out of the `process` closure so the chunking — the part with the
/// arithmetic in it — is testable without a PipeWire graph.
fn emit_chunks(user_data: &mut StreamUserData) {
    let channels = user_data.channels.max(1);
    // How many input frames make one wire chunk at the negotiated rate.
    // A rate is bounded by what a sound card reports, so the quotient is a
    // few thousand — `try_from` rather than a cast only because a 32-bit
    // target makes the narrowing fallible on paper.
    let frames_per_chunk = usize::try_from(
        (SAMPLES_PER_CHUNK as u64 * u64::from(user_data.rate)
            / u64::from(lumepeer_core::constants::AUDIO_SAMPLE_RATE_HZ))
        .max(1),
    )
    .unwrap_or(SAMPLES_PER_CHUNK);
    let samples_per_chunk = frames_per_chunk.saturating_mul(channels);
    if samples_per_chunk == 0 {
        return;
    }

    while user_data.pending.len() >= samples_per_chunk {
        let rest = user_data.pending.split_off(samples_per_chunk);
        let taken = std::mem::replace(&mut user_data.pending, rest);
        let chunk = PcmChunk {
            samples: to_wire_pcm(&taken, user_data.rate, channels),
            timestamp_us: capture_timestamp_us(),
        };
        // Blocking, unlike the video path: a dropped chunk of audio is an
        // audible click, and the bounded channel is what applies the
        // backpressure. A send that fails means the capturer is gone, and
        // there is nothing left to produce for.
        if user_data.sender.send(chunk).is_err() {
            user_data.pending.clear();
            return;
        }
    }
}

impl Default for PipewireMonitorCapturer {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioCapturer for PipewireMonitorCapturer {
    fn start(&mut self) -> Result<()> {
        if self.rx.is_some() {
            return Ok(());
        }
        let (tx, rx) = std::sync::mpsc::sync_channel::<PcmChunk>(CHANNEL_DEPTH);
        let (shutdown_tx, shutdown_rx) = pipewire::channel::channel::<Shutdown>();

        let handle = std::thread::Builder::new()
            .name("lumepeer-pipewire-audio".to_owned())
            .spawn(move || {
                if let Err(err) = Self::run(&tx, shutdown_rx) {
                    tracing::warn!("pipewire audio capture thread exited: {err}");
                }
            })
            .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;

        self.rx = Some(rx);
        self.handle = Some(handle);
        self.shutdown = Some(shutdown_tx);
        Ok(())
    }

    fn next_chunk(&mut self) -> Result<PcmChunk> {
        let rx = self
            .rx
            .as_ref()
            .ok_or_else(|| MediaError::CaptureInterrupted("capture not started".to_owned()))?;
        match rx.recv_timeout(READ_TIMEOUT) {
            Ok(chunk) => Ok(chunk),
            Err(RecvTimeoutError::Timeout) => Err(MediaError::CaptureInterrupted(
                "no audio arrived within the read timeout".to_owned(),
            )),
            Err(RecvTimeoutError::Disconnected) => Err(MediaError::CaptureInterrupted(
                "the capture thread is gone".to_owned(),
            )),
        }
    }

    fn stop(&mut self) {
        // Order matters: the loop is told to quit first, then the receiver is
        // dropped, then the thread is joined. Dropping the receiver alone
        // would only unblock a *sending* thread, and this one spends its life
        // inside `MainLoop::run`.
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(Shutdown);
        }
        self.rx = None;
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for PipewireMonitorCapturer {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failed assumption must fail the test")]

    use super::*;

    fn user_data(rate: u32, channels: usize) -> (StreamUserData, Receiver<PcmChunk>) {
        let (tx, rx) = std::sync::mpsc::sync_channel(CHANNEL_DEPTH);
        (
            StreamUserData {
                sender: tx,
                rate,
                channels,
                pending: Vec::new(),
            },
            rx,
        )
    }

    /// PipeWire's buffer size has nothing to do with `AUDIO_FRAME_MS`, so the
    /// chunker has to hold a partial chunk rather than emit a short one — a
    /// short chunk is what the Opus encoder rejects.
    #[test]
    fn a_partial_buffer_is_held_back_rather_than_sent_short() {
        let (mut data, rx) = user_data(STREAM_RATE_HZ, 2);
        data.pending = vec![0.0; SAMPLES_PER_CHUNK]; // half a stereo chunk
        emit_chunks(&mut data);
        assert!(rx.try_recv().is_err(), "half a chunk must not be sent");
        assert_eq!(data.pending.len(), SAMPLES_PER_CHUNK);
    }

    #[test]
    fn whole_chunks_are_emitted_and_the_remainder_kept() {
        let (mut data, rx) = user_data(STREAM_RATE_HZ, 2);
        let per_chunk = SAMPLES_PER_CHUNK * 2;
        data.pending = vec![0.25; per_chunk * 2 + 7];
        emit_chunks(&mut data);

        for _ in 0..2 {
            let Ok(chunk) = rx.try_recv() else {
                panic!("two whole chunks were available");
            };
            assert_eq!(
                chunk.samples.len(),
                SAMPLES_PER_CHUNK * usize::from(AUDIO_CHANNELS),
                "every chunk handed on is exactly one Opus frame"
            );
        }
        assert!(rx.try_recv().is_err());
        assert_eq!(data.pending.len(), 7, "the remainder waits for more input");
    }

    /// A graph that could not give us 48 kHz hands back its own rate, and the
    /// chunker has to count input frames at *that* rate — otherwise every
    /// chunk carries the wrong duration and playback drifts.
    #[test]
    fn a_negotiated_rate_other_than_the_wire_rate_still_yields_wire_chunks() {
        let (mut data, rx) = user_data(44_100, 2);
        // One 20 ms chunk at 44.1 kHz is 882 frames, not 960.
        let frame_ms = usize::try_from(lumepeer_core::constants::AUDIO_FRAME_MS).unwrap();
        let frames = 44_100 * frame_ms / 1000;
        data.pending = vec![0.5; frames * 2];
        emit_chunks(&mut data);

        let Ok(chunk) = rx.try_recv() else {
            panic!("one chunk's worth of 44.1 kHz input");
        };
        assert_eq!(
            chunk.samples.len(),
            SAMPLES_PER_CHUNK * usize::from(AUDIO_CHANNELS),
            "the wire format is fixed regardless of what the graph negotiated"
        );
    }

    #[test]
    fn a_mono_graph_is_chunked_on_its_own_channel_count() {
        let (mut data, rx) = user_data(STREAM_RATE_HZ, 1);
        data.pending = vec![0.1; SAMPLES_PER_CHUNK];
        emit_chunks(&mut data);

        let Ok(chunk) = rx.try_recv() else {
            panic!("mono input is still a whole chunk");
        };
        assert_eq!(
            chunk.samples.len(),
            SAMPLES_PER_CHUNK * usize::from(AUDIO_CHANNELS)
        );
        assert!(data.pending.is_empty());
    }
}
