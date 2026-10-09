//! WASAPI loopback capture for Windows (§11; questions.md item 8; ADR 0023).
//!
//! The loopback flag on `IAudioClient` hands back whatever the default output
//! device is mixing — the same bytes the speakers would play, which is exactly
//! what "share this desktop's sound" means. The device is opened at its own
//! mix rate and the frames are converted to the fixed §11 wire format by
//! [`crate::capture_audio::to_wire_pcm`], so no negotiation ever travels the
//! wire (§9.1: parameters are constants).
//!
//! The blocking pull model matches every other capturer in this crate:
//! `start`, then `next_chunk` once per 20 ms of audio, from a worker thread
//! (`spawn_blocking` on the caller's side). A silent desktop produces real
//! silence rather than no chunks, so the Opus encoder's concealment never has
//! to paper over a stalled clock.
//!
//! "The default output device" is whichever one Windows names *now*, not the
//! one it named when the session started (ADR 0147). A loopback client stays
//! on the endpoint it was opened on: when Windows moves the default — a
//! monitor's HDMI audio waking up, headphones plugged in — every program
//! follows it and the old endpoint goes quiet, and a capture left on it sends
//! the guest silence for the rest of the session. So the capturer asks once a
//! second which endpoint is the default and moves when it changed, and an
//! endpoint that disappears is reopened on whatever is the default then
//! instead of ending the session's sound.
//!
//! This module is the one new place outside `decode`/`encode::windows`/
//! `capture::windows` that touches COM; every call below is an `unsafe fn` of
//! the `windows` crate, so the module opts back into `unsafe_code` under the
//! same justification standard as ADR 0012 (WASAPI is raw FFI with no safe
//! binding) and every block carries a SAFETY note.

use std::time::{Duration, Instant};

use windows::Win32::Media::Audio::{
    self as wasapi, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR,
    AUDCLNT_SHAREMODE_SHARED, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator,
};
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance};

use crate::capture_audio::{
    PcmChunk, READ_TIMEOUT, capture_timestamp_us, frames_per_chunk, to_wire_pcm,
};
use crate::error::{MediaError, Result};

// Only IEEE-float mixes are read: the shared-mode mixer normalizes every
// modern Windows output path to float32. Anything else would be
// misinterpreted byte-for-byte, so it is refused loudly (§18).
// WAVE_FORMAT_IEEE_FLOAT = 3 (mmreg.h); the constant lives in the Multimedia
// feature this build does not pull in, so name the value once, here, with its
// source.
const WAVE_FORMAT_IEEE_FLOAT: u32 = 3;

/// How often the capturer asks Windows which output is the default.
const DEFAULT_CHECK: Duration = Duration::from_secs(1);

/// How long a capturer with no endpoint waits before trying to open one
/// again. It answers with silence meanwhile.
const REOPEN_INTERVAL: Duration = Duration::from_secs(2);

/// How soon an endpoint that just went away is replaced: long enough that
/// one which fails straight after opening cannot spin the thread.
const REOPEN_AFTER_LOSS: Duration = Duration::from_millis(200);

/// COM apartment wrapper: `CoInitializeEx` on build, `CoUninitialize` on drop,
/// so an abandoned capturer cannot leak its apartment into the calling thread.
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

/// The endpoint id of the current default output, or `None` when there is
/// none (or Windows would not say).
fn default_output_id(enumerator: &IMMDeviceEnumerator) -> Option<String> {
    // SAFETY: plain queries on a live enumerator.
    #[allow(
        unsafe_code,
        reason = "IMMDeviceEnumerator is raw WASAPI with no safe binding"
    )]
    let device =
        unsafe { enumerator.GetDefaultAudioEndpoint(wasapi::eRender, wasapi::eConsole) }.ok()?;
    endpoint_id(&device)
}

/// An endpoint's id, copied out of its COM allocation.
fn endpoint_id(device: &wasapi::IMMDevice) -> Option<String> {
    // SAFETY: `GetId` hands back a string the caller owns; it is copied and
    // freed here, once, and never read after.
    #[allow(
        unsafe_code,
        reason = "IMMDevice::GetId is raw WASAPI; its string is CoTaskMem-allocated"
    )]
    unsafe {
        let id = device.GetId().ok()?;
        let text = id.to_string().ok();
        windows::Win32::System::Com::CoTaskMemFree(Some(id.0.cast_const().cast()));
        text
    }
}

/// One open loopback client on one endpoint.
struct CaptureState {
    enumerator: IMMDeviceEnumerator,
    client: IAudioClient,
    capture: IAudioCaptureClient,
    /// The endpoint this client captures, to tell when the default moved.
    device_id: Option<String>,
    /// When [`default_output_id`] was last asked.
    checked: Instant,
    /// Mix rate the device reported; chunks resample off this.
    input_rate: u32,
    input_channels: usize,
    leftover: Vec<f32>,
    /// Last, so it is dropped after every interface above is released.
    _com: ComGuard,
}

// The COM interface pointers cross threads only by move, never shared: all
// calls happen on whichever thread currently owns the capturer, and WASAPI in
// the multithreaded COM apartment imposes no thread affinity. That makes the
// hand-off into `spawn_blocking` sound without runtime cost.
#[allow(
    unsafe_code,
    reason = "COM interface handles carry no thread affinity under COINIT_MULTITHREADED; \
              the pointer is moved between threads, never aliased"
)]
unsafe impl Send for CaptureState {}

impl CaptureState {
    /// Opens a loopback client on the current default output and starts it.
    fn open_default() -> Result<Self> {
        let com = ComGuard::init()?;
        // SAFETY: COM activation calls; every out-pointer is a local the call
        // fills or a borrowed interface pointer the callee only reads.
        #[allow(
            unsafe_code,
            reason = "every WASAPI/COM call below is an unsafe fn of the windows crate"
        )]
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&wasapi::MMDeviceEnumerator, None, CLSCTX_ALL)
                    .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;
            let device = enumerator
                .GetDefaultAudioEndpoint(wasapi::eRender, wasapi::eConsole)
                .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;
            let device_id = endpoint_id(&device);
            let client: IAudioClient = device
                .Activate(CLSCTX_ALL, None)
                .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;

            // 100 ms of loopback buffer in shared mode: the mixer keeps running
            // for everyone else and we simply read what it produced.
            let buffer_duration: i64 = 1_000_000; // 100 ms in 100 ns units
            let mix_format = client
                .GetMixFormat()
                .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;
            let format = &*mix_format;
            if u32::from(format.wFormatTag) != WAVE_FORMAT_IEEE_FLOAT && format.cbSize == 0 {
                return Err(MediaError::CaptureUnavailable(
                    "the audio mix format is not IEEE float".to_owned(),
                ));
            }
            if usize::from(format.nChannels) == 0 {
                return Err(MediaError::CaptureUnavailable(
                    "the audio mix reports zero channels".to_owned(),
                ));
            }

            client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    wasapi::AUDCLNT_STREAMFLAGS_LOOPBACK,
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
                "WASAPI loopback capture started"
            );
            Ok(Self {
                enumerator,
                client,
                capture,
                device_id,
                checked: Instant::now(),
                input_rate: mix_rate,
                input_channels,
                leftover: Vec::new(),
                _com: com,
            })
        }
    }

    /// Moves whatever the loopback buffer holds into `leftover`.
    ///
    /// # Errors
    /// The endpoint is gone or the audio service refused: this client is
    /// finished and the caller opens a new one.
    fn drain(&mut self) -> Result<()> {
        loop {
            // SAFETY: packet size query; the returned count bounds the
            // GetBuffer call that follows.
            #[allow(unsafe_code, reason = "raw WASAPI pull")]
            let packet = unsafe { self.capture.GetNextPacketSize() }
                .map_err(|e| MediaError::CaptureInterrupted(e.to_string()))?;
            if packet == 0 {
                return Ok(());
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
                self.capture
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
                            frames as usize * self.input_channels,
                        )
                    };
                    self.leftover.extend_from_slice(slice);
                } else {
                    self.leftover.extend(std::iter::repeat_n(
                        0.0f32,
                        frames as usize * self.input_channels,
                    ));
                }
            }
            if flags_u32 & (AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32) != 0 {
                tracing::debug!("WASAPI reported a timestamp discontinuity");
            }
            // SAFETY: releases the window GetBuffer handed out.
            #[allow(unsafe_code, reason = "pairs GetBuffer")]
            unsafe { self.capture.ReleaseBuffer(frames) }
                .map_err(|e| MediaError::CaptureInterrupted(e.to_string()))?;
        }
    }

    /// One whole chunk out of `leftover`, once it holds 20 ms of this
    /// device's own frames — 882 of them at 44.1 kHz, not 960 (ADR 0147).
    fn take_chunk(&mut self) -> Option<PcmChunk> {
        let needed = frames_per_chunk(self.input_rate) * self.input_channels;
        if self.leftover.len() < needed {
            return None;
        }
        let chunk_samples: Vec<f32> = self.leftover.drain(..needed).collect();
        Some(PcmChunk {
            samples: to_wire_pcm(&chunk_samples, self.input_rate, self.input_channels),
            timestamp_us: capture_timestamp_us(),
        })
    }

    /// Whether Windows has named another output the default since this
    /// client opened. Asked at most once per [`DEFAULT_CHECK`].
    fn default_moved(&mut self) -> bool {
        if self.checked.elapsed() < DEFAULT_CHECK {
            return false;
        }
        self.checked = Instant::now();
        // No default at all is not a reason to leave the one still open.
        default_output_id(&self.enumerator).is_some_and(|now| self.device_id.as_ref() != Some(&now))
    }
}

impl Drop for CaptureState {
    fn drop(&mut self) {
        // SAFETY: stops the client started in `open_default`.
        #[allow(unsafe_code, reason = "IAudioClient::Stop is raw WASAPI")]
        unsafe {
            let _ = self.client.Stop();
        }
    }
}

/// WASAPI loopback capturer of the default console output device.
pub struct WasapiLoopbackCapturer {
    state: Option<CaptureState>,
    /// Between `start` and `stop`: a capturer whose endpoint went away keeps
    /// trying to open the default one rather than ending the session's sound.
    running: bool,
    /// When to try opening an endpoint again after a failed attempt.
    retry_at: Option<Instant>,
}

impl std::fmt::Debug for WasapiLoopbackCapturer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasapiLoopbackCapturer")
            .field("active", &self.state.is_some())
            .field("running", &self.running)
            .finish_non_exhaustive()
    }
}

impl WasapiLoopbackCapturer {
    /// Builds an idle capturer; nothing opens until [`AudioCapturer::start`].
    ///
    /// [`AudioCapturer::start`]: crate::capture_audio::AudioCapturer::start
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: None,
            running: false,
            retry_at: None,
        }
    }

    /// Opens the default endpoint when there is no client and it is time to
    /// try; a failure is retried after [`REOPEN_INTERVAL`].
    fn reopen_if_due(&mut self) {
        if self.state.is_some() || self.retry_at.is_some_and(|at| Instant::now() < at) {
            return;
        }
        match CaptureState::open_default() {
            Ok(state) => {
                self.state = Some(state);
                self.retry_at = None;
            }
            Err(error) => {
                tracing::debug!(%error, "no output to capture yet; trying again");
                self.retry_at = Some(Instant::now() + REOPEN_INTERVAL);
            }
        }
    }
}

impl Default for WasapiLoopbackCapturer {
    fn default() -> Self {
        Self::new()
    }
}

impl crate::capture_audio::AudioCapturer for WasapiLoopbackCapturer {
    fn start(&mut self) -> Result<()> {
        if self.running {
            return Ok(());
        }
        // The first open is the one that may refuse: a host with no output
        // at all stays video-only, and says so (§18).
        self.state = Some(CaptureState::open_default()?);
        self.running = true;
        Ok(())
    }

    fn next_chunk(&mut self) -> Result<PcmChunk> {
        if !self.running {
            return Err(MediaError::CaptureInterrupted(
                "capture not started".to_owned(),
            ));
        }
        let started = Instant::now();
        loop {
            self.reopen_if_due();
            if let Some(state) = self.state.as_mut() {
                if let Err(error) = state.drain() {
                    // The endpoint went away (unplugged, disabled, its format
                    // changed) or the audio service restarted. Not the end of
                    // the session's sound: whatever is the default now is
                    // opened in its place (ADR 0147).
                    tracing::info!(%error, "the captured output went away; capturing the default one");
                    self.state = None;
                    // Not at once: an endpoint that opens and fails again
                    // straight away would otherwise spin this thread.
                    self.retry_at = Some(Instant::now() + REOPEN_AFTER_LOSS);
                    continue;
                }
                if let Some(chunk) = state.take_chunk() {
                    return Ok(chunk);
                }
                if state.default_moved() {
                    tracing::info!("the default output changed; capturing the new one");
                    self.state = None;
                    continue;
                }
            }
            // Loopback hands back no packets at all while nothing plays, so a
            // quiet wait is a silent host, not a dead device (ADR 0137). One
            // chunk of silence keeps the caller's loop turning without a
            // timeout ending the stream.
            if started.elapsed() > READ_TIMEOUT {
                return Ok(PcmChunk::silence(capture_timestamp_us()));
            }
            std::thread::sleep(Duration::from_millis(
                lumepeer_core::constants::AUDIO_FRAME_MS.into(),
            ));
        }
    }

    fn stop(&mut self) {
        self.running = false;
        self.state = None;
    }
}
