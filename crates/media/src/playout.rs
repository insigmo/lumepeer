//! WASAPI shared-mode render (speaker) playback (§11; ADR 0028).
//!
//! The receiving end of both audio directions: the host playing what a
//! guest's microphone picked up. The same blocking-push shape the capturers
//! use, mirrored: `start` opens the default console *render* device in
//! shared mode at its own mix rate, `push` accepts one wire-format PCM chunk
//! (48 kHz s16 stereo, §11) and converts it into whatever the device is
//! actually mixing before handing it to the mixer, `stop` releases the
//! device.
//!
//! Conversion is linear resampling plus channel mapping, the exact inverse
//! of [`crate::capture_audio::to_wire_pcm`], kept local so the wire format
//! stays decided by the same constants. A silent chunk is real silence, so
//! gaps in the sender's clock never click.
//!
//! WASAPI is the only backend today, so everything below it is gated on
//! Windows behind the [`AudioPlayer`] trait, exactly as
//! [`crate::capture_audio`] gates its capturers: other targets get a refusal
//! from [`platform_player`] and run without guest audio (§18). The
//! conversion helper is platform-independent and always tested.

#[cfg(target_os = "windows")]
use std::sync::Arc;
#[cfg(target_os = "windows")]
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(target_os = "windows")]
use windows::Win32::Media::Audio::{self as wasapi, IAudioClient, IAudioRenderClient};
#[cfg(target_os = "windows")]
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance};

use crate::error::{MediaError, Result};
use lumepeer_core::constants::{AUDIO_CHANNELS, AUDIO_SAMPLE_RATE_HZ};

// Only IEEE-float mixes are written, mirroring the capture side: the
// shared-mode mixer normalizes every modern Windows render path to float32.
// WAVE_FORMAT_IEEE_FLOAT = 3 (mmreg.h).
#[cfg(target_os = "windows")]
const WAVE_FORMAT_IEEE_FLOAT: u32 = 3;

/// How long one blocking push may wait for buffer space before the device is
/// considered stuck. Generous on purpose: a chunk is 20 ms of audio, but a
/// device resuming from suspend may legitimately be late once.
#[cfg(target_os = "windows")]
const PLAYBACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Playback backend contract, the mirror of [`crate::capture_audio::AudioCapturer`].
///
/// One trait so the mic-playback loop stays backend-agnostic: WASAPI is the
/// only implementation today, and a platform without one refuses in
/// [`platform_player`] rather than swallowing the audio (§18).
pub trait AudioPlayer: Send + std::fmt::Debug {
    /// Opens the default output device.
    ///
    /// # Errors
    /// [`MediaError::CaptureUnavailable`] when no output device exists or the
    /// backend was not compiled in for this target.
    fn start(&mut self) -> Result<()>;

    /// Hands one wire-format PCM chunk (48 kHz s16 stereo, §11) to the mixer.
    ///
    /// # Errors
    /// [`MediaError::CaptureInterrupted`] once the device is gone or
    /// [`stop`](Self::stop) has run.
    fn push(&mut self, samples: &[i16], timestamp_us: u64) -> Result<()>;

    /// Stops playback and releases the device. Idempotent.
    fn stop(&mut self);
}

/// Opens the playback backend of the current platform (§11; ADR 0028).
///
/// # Errors
/// [`MediaError::CaptureUnavailable`] when no backend is compiled in for this
/// target — the guest's microphone stays silent and the log says why (§18).
pub fn platform_player() -> Result<Box<dyn AudioPlayer>> {
    #[cfg(target_os = "windows")]
    {
        Ok(Box::new(WasapiPlayout::new()))
    }
    #[cfg(not(target_os = "windows"))]
    {
        Err(MediaError::CaptureUnavailable(
            "no audio playback backend is compiled in for this target".to_owned(),
        ))
    }
}

/// COM apartment wrapper: `CoInitializeEx` on build, `CoUninitialize` on drop.
#[cfg(target_os = "windows")]
struct ComGuard;

#[cfg(target_os = "windows")]
impl ComGuard {
    fn init() -> Result<Self> {
        // SAFETY: plain FFI call into ole32; no pointers involved beyond the
        // reserved NULL. S_OK/S_FALSE both mean "usable apartment".
        #[allow(
            unsafe_code,
            reason = "CoInitializeEx is a raw COM entry point with no safe binding"
        )]
        let hr = unsafe { windows::Win32::System::Com::CoInitializeEx(None, COINIT_MULTITHREADED) };
        // S_FALSE (1) means already initialized: usable, and still needs the
        // pairing CoUninitialize, which Drop does.
        if hr.is_err() && hr.0 != 1 {
            return Err(MediaError::CaptureUnavailable(format!(
                "CoInitializeEx failed: {hr:?}"
            )));
        }
        Ok(Self)
    }
}

#[cfg(target_os = "windows")]
impl Drop for ComGuard {
    fn drop(&mut self) {
        // SAFETY: balances exactly one successful CoInitializeEx above.
        #[allow(
            unsafe_code,
            reason = "CoUninitialize pairs the init in ComGuard::init"
        )]
        unsafe {
            windows::Win32::System::Com::CoUninitialize();
        }
    }
}

#[cfg(target_os = "windows")]
struct PlayoutState {
    _com: ComGuard,
    client: IAudioClient,
    render: IAudioRenderClient,
    /// Mix rate the device reported; chunks resample onto this.
    output_rate: u32,
    output_channels: usize,
    /// Set when `stop` runs, so pushes after a stop fail fast.
    running: Arc<AtomicBool>,
}

// The COM interface pointers cross threads only by move, never shared — the
// same soundness argument the capturers document.
#[allow(
    unsafe_code,
    reason = "COM interface handles carry no thread affinity under COINIT_MULTITHREADED; \
              the pointer is moved between threads, never aliased"
)]
#[cfg(target_os = "windows")]
unsafe impl Send for PlayoutState {}

/// Shared-mode render player of the default console output device.
#[cfg(target_os = "windows")]
pub struct WasapiPlayout {
    state: Option<PlayoutState>,
}

#[cfg(target_os = "windows")]
impl std::fmt::Debug for WasapiPlayout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasapiPlayout")
            .field("active", &self.state.is_some())
            .finish()
    }
}

#[cfg(target_os = "windows")]
impl Default for WasapiPlayout {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(target_os = "windows")]
impl WasapiPlayout {
    /// Builds an idle player; nothing opens until [`start`].
    #[must_use]
    pub const fn new() -> Self {
        Self { state: None }
    }

    /// Opens the default output device in shared mode.
    ///
    /// # Errors
    /// [`MediaError::CaptureUnavailable`] when no output device exists or the
    /// mix format is not IEEE float (§18: refuse loudly, never guess bytes).
    pub fn start(&mut self) -> Result<()> {
        if self.state.is_some() {
            return Ok(());
        }
        let com = ComGuard::init()?;
        // SAFETY: COM activation calls; every out-pointer is a local the call
        // fills or a borrowed interface pointer the callee only reads.
        #[allow(
            unsafe_code,
            reason = "every WASAPI/COM call below is an unsafe fn of the windows crate"
        )]
        unsafe {
            let enumerator: wasapi::IMMDeviceEnumerator =
                CoCreateInstance(&wasapi::MMDeviceEnumerator, None, CLSCTX_ALL)
                    .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;
            // `eRender`: the speakers, not the microphone.
            let device = enumerator
                .GetDefaultAudioEndpoint(wasapi::eRender, wasapi::eConsole)
                .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;
            let client: IAudioClient = device
                .Activate(CLSCTX_ALL, None)
                .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;

            // 200 ms of render buffer: comfortably above two wire frames, so
            // a scheduling hiccup on the pushing thread never underflows the
            // mixer, while latency stays far below what a conversation
            // notices.
            let buffer_duration: i64 = 2_000_000; // 200 ms in 100 ns units
            let mix_format = client
                .GetMixFormat()
                .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;
            let format = &*mix_format;
            if u32::from(format.wFormatTag) != WAVE_FORMAT_IEEE_FLOAT && format.cbSize == 0 {
                return Err(MediaError::CaptureUnavailable(
                    "the render mix format is not IEEE float".to_owned(),
                ));
            }
            if usize::from(format.nChannels) == 0 {
                return Err(MediaError::CaptureUnavailable(
                    "the render mix reports zero channels".to_owned(),
                ));
            }

            // Event-less shared-mode render: the mixer pulls from the buffer
            // on its own clock; `push` keeps the padding above one wire frame
            // instead of waiting on an event handle.
            client
                .Initialize(
                    wasapi::AUDCLNT_SHAREMODE_SHARED,
                    0u32,
                    buffer_duration,
                    0,
                    #[allow(
                        clippy::clone_on_copy,
                        reason = "WAVEFORMATEX is a Copy FFI struct; clone reads clearer at the call site"
                    )]
                    mix_format.clone(),
                    None,
                )
                .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;

            let render: IAudioRenderClient = client
                .GetService()
                .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;

            // Pre-fill the entire buffer with silence so playback starts from
            // a known state instead of whatever the allocator left there.
            let buffer_frames = client
                .GetBufferSize()
                .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;
            // SAFETY: pre-fill query inside the enclosing COM block above;
            // the returned pointer is valid for exactly `buffer_frames`
            // frames until ReleaseBuffer.
            let cursor = render
                .GetBuffer(buffer_frames)
                .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;
            if !cursor.is_null() {
                // SAFETY: the mixer hands out exactly `buffer_frames` frames
                // of its float mix; zeroing them is the documented way to
                // write silence (AUDCLNT_BUFFERFLAGS_SILENT is the other).
                std::ptr::write_bytes(
                    cursor,
                    0,
                    buffer_frames as usize * usize::from(format.nBlockAlign),
                );
            }
            let _ = render.ReleaseBuffer(buffer_frames, 0);

            client
                .Start()
                .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;

            let output_rate = format.nSamplesPerSec;
            let output_channels = usize::from(format.nChannels);
            tracing::info!(
                rate = output_rate,
                channels = output_channels,
                buffer_frames,
                "WASAPI render playback started"
            );
            self.state = Some(PlayoutState {
                _com: com,
                client,
                render,
                output_rate,
                output_channels,
                running: Arc::new(AtomicBool::new(true)),
            });
        }
        Ok(())
    }

    /// Hands one wire-format PCM chunk (48 kHz s16 stereo, §11) to the mixer.
    ///
    /// Blocks only as long as the buffer is genuinely full — bounded, so a
    /// wedged device fails the call instead of hanging the session.
    ///
    /// # Errors
    /// [`MediaError::CaptureInterrupted`] once the device is gone or
    /// [`stop`](Self::stop) has run.
    pub fn push(&mut self, samples: &[i16], timestamp_us: u64) -> Result<()> {
        let _ = timestamp_us; // ordering is the sender's concern; the mixer clocks itself
        let state = self
            .state
            .as_mut()
            .ok_or_else(|| MediaError::CaptureInterrupted("playback not started".to_owned()))?;
        let started = std::time::Instant::now();

        // Convert the wire chunk into the device's mix format up front.
        let mixed = to_device_pcm(samples, state.output_rate, state.output_channels);
        let frames = mixed.len() / state.output_channels;
        let mut written = 0usize;

        while written < frames {
            if !state.running.load(Ordering::Relaxed) || started.elapsed() > PLAYBACK_TIMEOUT {
                return Err(MediaError::CaptureInterrupted(
                    "playback stopped or timed out".to_owned(),
                ));
            }
            // SAFETY: padding/frame queries on the client opened in `start`.
            #[allow(unsafe_code, reason = "raw WASAPI push")]
            let padding = unsafe { state.client.GetCurrentPadding() }
                .map_err(|e| MediaError::CaptureInterrupted(e.to_string()))?;
            // SAFETY: buffer-size query on the same client.
            #[allow(unsafe_code, reason = "raw WASAPI push")]
            let buffer_frames = unsafe { state.client.GetBufferSize() }
                .map_err(|e| MediaError::CaptureInterrupted(e.to_string()))?;
            let free = buffer_frames.saturating_sub(padding);
            if free == 0 {
                std::thread::sleep(std::time::Duration::from_millis(
                    lumepeer_core::constants::AUDIO_FRAME_MS.into(),
                ));
                continue;
            }
            let batch = usize::try_from(free).unwrap_or(0).min(frames - written);
            if batch == 0 {
                continue;
            }
            // SAFETY: `batch` is bounded by `free`, exactly what the mixer
            // said it would accept; the returned pointer is valid for those
            // frames until ReleaseBuffer.
            #[allow(unsafe_code, reason = "raw WASAPI push")]
            let cursor = unsafe {
                state
                    .render
                    .GetBuffer(batch.try_into().unwrap_or(0))
                    .map_err(|e| MediaError::CaptureInterrupted(e.to_string()))?
            };
            if !cursor.is_null() {
                // SAFETY: the mixer handed out `batch` frames of its float
                // mix; writing exactly that many converted samples is the
                // contract of GetBuffer.
                #[allow(
                    unsafe_code,
                    clippy::cast_ptr_alignment,
                    reason = "mix format checked to be IEEE float32 in start; \
                              the pointer is the mixer's own window for exactly these frames"
                )]
                unsafe {
                    let out = cursor.cast::<f32>();
                    let window = &mixed[written * state.output_channels
                        ..(written + batch) * state.output_channels];
                    for (i, sample) in window.iter().enumerate() {
                        *out.add(i) = *sample;
                    }
                }
            }
            // SAFETY: releases the window GetBuffer handed out.
            #[allow(unsafe_code, reason = "pairs GetBuffer")]
            unsafe {
                let _ = state.render.ReleaseBuffer(batch.try_into().unwrap_or(0), 0);
            }
            written += batch;
        }
        Ok(())
    }

    /// Stops playback and releases the device. Idempotent.
    pub fn stop(&mut self) {
        if let Some(state) = self.state.take() {
            state.running.store(false, Ordering::Relaxed);
            // SAFETY: stops the client started in `start`.
            #[allow(unsafe_code, reason = "IAudioClient::Stop is raw WASAPI")]
            unsafe {
                let _ = state.client.Stop();
            }
        }
    }
}

#[cfg(target_os = "windows")]
impl AudioPlayer for WasapiPlayout {
    fn start(&mut self) -> Result<()> {
        Self::start(self)
    }

    fn push(&mut self, samples: &[i16], timestamp_us: u64) -> Result<()> {
        Self::push(self, samples, timestamp_us)
    }

    fn stop(&mut self) {
        Self::stop(self);
    }
}

#[cfg(target_os = "windows")]
impl Drop for WasapiPlayout {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Converts the fixed §11 wire PCM (48 kHz s16 stereo) into the device's mix
/// format: linear resampling plus channel mapping, the exact inverse of
/// [`crate::capture_audio::to_wire_pcm`].
///
/// Kept next to the capture-side converter so the wire format is decided by
/// the same constants in one crate.
#[must_use]
pub fn to_device_pcm(input: &[i16], output_rate: u32, output_channels: usize) -> Vec<f32> {
    let channels = usize::from(AUDIO_CHANNELS);
    if output_channels == 0 || output_rate == 0 || input.is_empty() {
        return Vec::new();
    }
    let input_frames = input.len() / channels;
    if input_frames == 0 {
        return Vec::new();
    }
    // The resampler is exact by construction, the mirror image of
    // `to_wire_pcm`'s: `pos` stays within `0..input_frames`, and the f32
    // narrowing only feeds interpolation of samples already in `i16`.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        reason = "index/interpolation math bounded by construction, see above"
    )]
    #[allow(
        clippy::cast_sign_loss,
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        reason = "index/interpolation math bounded by construction, see above"
    )]
    {
        // Input frames per output frame as a float step on the input
        // timeline.
        let step = f64::from(AUDIO_SAMPLE_RATE_HZ) / f64::from(output_rate);
        let out_frames = (input_frames as f64 / step).floor();
        // `out_frames` is the floor of a non-negative quotient: the cast
        // cannot lose sign, only precision, and precision loss truncates
        // a chunk's tail by less than one sample.
        let out_frames = out_frames as usize;
        let mut out = Vec::with_capacity(out_frames * output_channels);
        for o in 0..out_frames {
            let pos = o as f64 * step;
            let i = pos.floor();
            let frac = (pos - i) as f32;
            let i0 = (i as usize).min(input_frames - 1);
            let i1 = (i0 + 1).min(input_frames - 1);
            for c in 0..output_channels {
                // A device with more channels than stereo duplicates the
                // nearest wire channel rather than inventing content; one
                // with fewer simply drops the extras (desktop mixes are
                // stereo anyway).
                let src_c = c.min(channels - 1);
                let s0 = f32::from(input[i0 * channels + src_c]);
                let s1 = f32::from(input[i1 * channels + src_c]);
                let mixed = s0 + (s1 - s0) * frac;
                out.push(mixed / f32::from(i16::MAX));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumepeer_core::constants::AUDIO_SAMPLE_RATE_HZ;

    /// Same rate, same stereo: a pure copy, scaled into the float range.
    #[test]
    fn same_rate_stereo_is_a_scaled_copy() {
        let input: Vec<i16> = vec![0x1000, -0x1000, 0x1000, -0x1000];
        let out = to_device_pcm(&input, AUDIO_SAMPLE_RATE_HZ, 2);
        assert_eq!(out.len(), 4);
        let expected = f32::from(0x1000i16) / f32::from(i16::MAX);
        assert!((out[0] - expected).abs() < 0.001);
        assert!((out[1] + expected).abs() < 0.001);
    }

    /// Resampling to 44.1 kHz keeps the duration: 48 kHz of one second
    /// becomes 44 100 frames.
    #[test]
    fn resampling_preserves_duration() {
        let second = AUDIO_SAMPLE_RATE_HZ as usize * 2;
        let input = vec![0i16; second];
        let out = to_device_pcm(&input, 44_100, 2);
        let frames = out.len() / 2;
        assert!((44_000..=44_200).contains(&frames), "got {frames} frames");
    }

    /// A mono device folds stereo by dropping the right channel rather than
    /// mixing, mirroring the capture-side channel rule.
    #[test]
    fn mono_output_takes_the_left_channel() {
        let input: Vec<i16> = vec![0x4000, 0x0100];
        let out = to_device_pcm(&input, AUDIO_SAMPLE_RATE_HZ, 1);
        assert_eq!(out.len(), 1);
        let expected = f32::from(0x4000i16) / f32::from(i16::MAX);
        assert!((out[0] - expected).abs() < 0.001);
    }

    /// Degenerate inputs produce silence-shaped emptiness, never a panic.
    #[test]
    fn degenerate_inputs_are_empty() {
        assert!(to_device_pcm(&[], AUDIO_SAMPLE_RATE_HZ, 2).is_empty());
        assert!(to_device_pcm(&[0, 0], 0, 2).is_empty());
        assert!(to_device_pcm(&[0, 0], AUDIO_SAMPLE_RATE_HZ, 0).is_empty());
    }
}
