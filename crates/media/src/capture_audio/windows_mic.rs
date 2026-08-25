//! WASAPI microphone capture for Windows (§11; ADR 0028).
//!
//! The guest-side counterpart of the loopback capturer: instead of the
//! default *output* mix (`eRender` + the loopback flag) this opens the
//! default *capture* device (`eCapture`, no loopback flag) — the microphone
//! the user would expect "mic" to mean. Everything else is deliberately the
//! same shape: shared mode, the device's own mix format (IEEE float only),
//! a blocking pull of `AUDIO_FRAME_MS` chunks resampled into the fixed §11
//! wire format by [`crate::capture_audio::to_wire_pcm`].
//!
//! The microphone is a permission on modern Windows; a refusal surfaces as
//! [`MediaError::PermissionDenied`] and the session simply stays without
//! guest audio (§18: degrade towards safety and say so).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Media::Audio::{
    self as wasapi, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR,
    AUDCLNT_SHAREMODE_SHARED, IAudioCaptureClient, IAudioClient,
};
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance};

use crate::capture_audio::{
    PcmChunk, READ_TIMEOUT, SAMPLES_PER_CHUNK, capture_timestamp_us, to_wire_pcm,
};
use crate::error::{MediaError, Result};

// Only IEEE-float mixes are read, exactly like the loopback capturer: the
// shared-mode mixer normalizes every modern Windows input path to float32.
// WAVE_FORMAT_IEEE_FLOAT = 3 (mmreg.h); named here with its source because
// the constant lives in a Multimedia feature this build does not pull in.
const WAVE_FORMAT_IEEE_FLOAT: u32 = 3;

/// COM apartment wrapper, shared with the loopback capturer's design:
/// `CoInitializeEx` on build, `CoUninitialize` on drop.
struct ComGuard;

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

struct CaptureState {
    _com: ComGuard,
    client: IAudioClient,
    capture: IAudioCaptureClient,
    /// Mix rate the device reported; chunks resample off this.
    input_rate: u32,
    input_channels: usize,
    /// Set when `stop` runs, so a read blocked in another thread can observe
    /// the shutdown on its next wake-up.
    running: Arc<AtomicBool>,
    leftover: Vec<f32>,
}

// The COM interface pointers cross threads only by move, never shared — the
// same soundness argument the loopback capturer documents for its state.
#[allow(
    unsafe_code,
    reason = "COM interface handles carry no thread affinity under COINIT_MULTITHREADED; \
              the pointer is moved between threads, never aliased"
)]
unsafe impl Send for CaptureState {}

/// WASAPI capturer of the default microphone (capture device).
pub struct WasapiMicCapturer {
    state: Option<CaptureState>,
}

impl std::fmt::Debug for WasapiMicCapturer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasapiMicCapturer")
            .field("active", &self.state.is_some())
            .finish()
    }
}

impl WasapiMicCapturer {
    /// Builds an idle capturer; nothing opens until [`MicCapturer::start`].
    #[must_use]
    pub const fn new() -> Self {
        Self { state: None }
    }
}

impl Default for WasapiMicCapturer {
    fn default() -> Self {
        Self::new()
    }
}

impl crate::capture_audio::MicCapturer for WasapiMicCapturer {
    fn start(&mut self) -> Result<()> {
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
            // `eCapture`, not `eRender`: the microphone, not the output mix.
            // No loopback flag anywhere — a plain shared-mode capture client.
            let device = enumerator
                .GetDefaultAudioEndpoint(wasapi::eCapture, wasapi::eConsole)
                .map_err(|e| {
                    // `HRESULT(0x80070553)` is the underlying win32 error for
                    // "device access refused" — on modern Windows the
                    // microphone is a privacy setting, and this is what an
                    // off toggle looks like. Distinguished so the log says
                    // "refused", not "missing".
                    if e.code() == windows::core::HRESULT(0x8007_0553_u32.cast_signed()) {
                        MediaError::PermissionDenied
                    } else {
                        MediaError::CaptureUnavailable(e.to_string())
                    }
                })?;
            let client: IAudioClient = device
                .Activate(CLSCTX_ALL, None)
                .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;

            // 100 ms of capture buffer in shared mode, mirroring the loopback
            // capturer: the mixer keeps running for everyone else and we
            // simply read what it produced.
            let buffer_duration: i64 = 1_000_000; // 100 ms in 100 ns units
            let mix_format = client
                .GetMixFormat()
                .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;
            let format = &*mix_format;
            if u32::from(format.wFormatTag) != WAVE_FORMAT_IEEE_FLOAT && format.cbSize == 0 {
                return Err(MediaError::CaptureUnavailable(
                    "the microphone mix format is not IEEE float".to_owned(),
                ));
            }
            if usize::from(format.nChannels) == 0 {
                return Err(MediaError::CaptureUnavailable(
                    "the microphone mix reports zero channels".to_owned(),
                ));
            }

            client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    // No `AUDCLNT_STREAMFLAGS_LOOPBACK`: this is a live
                    // capture client on an input device, not an output-mix
                    // tap. `0` is the documented "no stream flags" value.
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

            let capture: IAudioCaptureClient = client
                .GetService()
                .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;
            client
                .Start()
                .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;

            // Copy the fields out of the packed WAVEFORMATEX before logging:
            // taking a reference to a packed field is unaligned (E0793).
            let mix_rate = format.nSamplesPerSec;
            let input_channels = usize::from(format.nChannels);
            tracing::info!(
                rate = mix_rate,
                channels = input_channels,
                "WASAPI microphone capture started"
            );
            self.state = Some(CaptureState {
                _com: com,
                client,
                capture,
                input_rate: mix_rate,
                input_channels,
                running: Arc::new(AtomicBool::new(true)),
                leftover: Vec::new(),
            });
        }
        Ok(())
    }

    fn next_chunk(&mut self) -> Result<PcmChunk> {
        let state = self
            .state
            .as_mut()
            .ok_or_else(|| MediaError::CaptureInterrupted("capture not started".to_owned()))?;

        let started = std::time::Instant::now();
        loop {
            // Drain whatever the capture buffer holds into `leftover`.
            loop {
                // SAFETY: packet size query; the returned count bounds the
                // GetBuffer call that follows.
                #[allow(unsafe_code, reason = "raw WASAPI pull")]
                let packet = unsafe { state.capture.GetNextPacketSize() }
                    .map_err(|e| MediaError::CaptureInterrupted(e.to_string()))?;
                if packet == 0 {
                    break;
                }
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                // SAFETY: all out-pointers are locals; `data` stays valid until
                // ReleaseBuffer and is read only within that window.
                #[allow(
                    unsafe_code,
                    clippy::borrow_as_ptr,
                    reason = "raw WASAPI pull; explicit &mut is the API shape"
                )]
                unsafe {
                    state
                        .capture
                        .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                }
                .map_err(|e| MediaError::CaptureInterrupted(e.to_string()))?;
                // The mix format was verified to be IEEE float32 above, so the
                // byte buffer handed out here reinterprets as f32 samples.
                #[allow(
                    clippy::cast_ptr_alignment,
                    reason = "mix format checked to be IEEE float32 before Initialize"
                )]
                let data_float = data.cast::<f32>();
                // The flag constants are `i32`-backed newtypes; the wire value
                // here is a plain u32 bitmask, so normalize once and mask.
                let flags_u32 = flags;
                let is_silent = flags_u32 & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0;
                if frames > 0 {
                    if !is_silent && !data_float.is_null() {
                        // SAFETY: WASAPI hands us `frames * channels` float
                        // samples valid until ReleaseBuffer (invariant above).
                        #[allow(unsafe_code, reason = "reading the WASAPI sample window")]
                        let slice = unsafe {
                            std::slice::from_raw_parts(
                                data_float,
                                frames as usize * state.input_channels,
                            )
                        };
                        state.leftover.extend_from_slice(slice);
                    } else {
                        state.leftover.extend(std::iter::repeat_n(
                            0.0f32,
                            frames as usize * state.input_channels,
                        ));
                    }
                }
                if flags_u32 & (AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32) != 0 {
                    tracing::debug!("WASAPI reported a timestamp discontinuity");
                }
                // SAFETY: releases the window GetBuffer handed out.
                #[allow(unsafe_code, reason = "pairs GetBuffer")]
                unsafe { state.capture.ReleaseBuffer(frames) }
                    .map_err(|e| MediaError::CaptureInterrupted(e.to_string()))?;
            }

            let needed = SAMPLES_PER_CHUNK * state.input_channels;
            if state.leftover.len() >= needed {
                let chunk_samples: Vec<f32> = state.leftover.drain(..needed).collect();
                return Ok(PcmChunk {
                    samples: to_wire_pcm(&chunk_samples, state.input_rate, state.input_channels),
                    timestamp_us: capture_timestamp_us(),
                });
            }

            // Not enough yet: sleep roughly one wire frame and drain again.
            // The wall clock bounds the wait so a wedged device fails the call
            // instead of hanging the session (the loopback capturer follows
            // the same contract).
            if !state.running.load(Ordering::Relaxed) || started.elapsed() > READ_TIMEOUT {
                return Err(MediaError::CaptureInterrupted(
                    "microphone capture stopped or timed out".to_owned(),
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(
                lumepeer_core::constants::AUDIO_FRAME_MS.into(),
            ));
        }
    }

    fn stop(&mut self) {
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

impl Drop for WasapiMicCapturer {
    fn drop(&mut self) {
        // `MicCapturer::stop` is in scope through the trait impl above; the
        // explicit path keeps Drop independent of trait imports.
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
