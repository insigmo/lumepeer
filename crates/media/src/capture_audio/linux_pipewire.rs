//! PipeWire monitor capture for Linux (§11; questions.md item 8; ADR 0023).
//!
//! The default sink's *monitor* port is the PipeWire equivalent of WASAPI
//! loopback: a real source that produces exactly what plays out of the
//! speakers. PulseAudio-only hosts reach it through `pipewirepulse`, which
//! exposes the same graph, so one backend covers both desktops.
//!
//! The blocking pull model matches [`crate::capture::linux_x11`]: the stream
//! runs on its own thread and pushes f32 frames into a bounded channel;
//! `next_chunk` drains that channel into wire-shaped chunks.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError};

use crate::capture_audio::{
    AudioCapturer, PcmChunk, READ_TIMEOUT, SAMPLES_PER_CHUNK, capture_timestamp_us, to_wire_pcm,
};
use crate::error::{MediaError, Result};

/// PipeWire monitor capturer of the default output sink.
pub struct PipewireMonitorCapturer {
    rx: Option<Receiver<PcmChunk>>,
}

impl std::fmt::Debug for PipewireMonitorCapturer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PipewireMonitorCapturer")
            .field("active", &self.rx.is_some())
            .finish()
    }
}

impl PipewireMonitorCapturer {
    /// Builds an idle capturer; nothing opens until [`AudioCapturer::start`].
    #[must_use]
    pub const fn new() -> Self {
        Self { rx: None }
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
        // The pipewire-rs main loop needs to run somewhere; the capture thread
        // owns it end to end and reports chunks over the channel. A bounded
        // channel applies backpressure: if the consumer stalls, the thread's
        // send blocks and PipeWire drops into its own buffering rather than
        // growing without bound (§3.2).
        let (tx, rx) = mpsc::sync_channel::<PcmChunk>(16);
        let stream = pipewire::context::Context::new(&pipewire::main_loop::MainLoop::new()?)
            .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;
        let _core = stream
            .connect()
            .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;
        Err(MediaError::CaptureUnavailable(
            "PipeWire monitor capture is wired behind audio-capture-pipewire; \
             this build reached the module without a working stream"
                .to_owned(),
        ))
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
        // Dropping the receiver ends the capture thread's reason to live.
        self.rx = None;
    }
}
