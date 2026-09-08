//! `CoreAudio` playback for macOS (§11; ADR 0028; docs/gap-tasks/
//! 02-macos-audio-playout.md), the third [`AudioPlayer`] backend alongside
//! WASAPI (Windows) and PipeWire (Linux) in `crate::playout`.
//!
//! `AudioQueue`, not `AudioUnit`. Both open the default output device; this
//! backend uses `AudioQueue` because its buffer-pool-plus-completion-
//! callback shape is already exactly what [`AudioPlayer::push`] needs: hand
//! over one converted chunk, block until a buffer is free, enqueue it,
//! repeat — no render thread of our own to feed in real time. `AudioUnit`'s
//! render callback instead runs on a `CoreAudio`-owned real-time thread that
//! must *pull* samples from us under strict real-time constraints (no
//! blocking, no allocation), which would mean building a lock-free ring
//! buffer underneath it; `AudioQueue` already does that bookkeeping
//! internally and only ever calls back to say "this buffer is free again",
//! which is safe to do from any thread. (`capture::macos`'s
//! `MacosAudioCapturer`, docs/gap-tasks/01, does not weigh in on this
//! choice — it captures through `ScreenCaptureKit`, not `CoreAudio`.)
//!
//! Two crates, not one. `objc2-core-audio` is the `CoreAudio` HAL
//! (`AudioObjectGetPropertyData` and friends), used only to read the default
//! output device's real nominal sample rate before opening it — mirroring
//! `WasapiPlayout::start`'s `GetMixFormat` call and
//! `capture_audio::linux_pipewire`'s negotiated-format discipline: never
//! assume a rate, ask the device and convert onto it with
//! [`crate::playout::to_device_pcm`], the one converter this crate has.
//! `objc2-audio-toolbox` is the `AudioToolbox` framework itself
//! (`AudioQueue*`). Both are pinned at 0.3.2, the version the rest of the
//! objc2 framework family in this crate already resolves to; see the
//! `audio-playout-coreaudio` feature comment in `Cargo.toml` for why neither
//! addition is a new objc2 major version.

#![allow(
    unsafe_code,
    reason = "every CoreAudio/AudioQueue entry point here is a raw C FFI call with no safe binding; every block carries a SAFETY note, per §21"
)]

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::time::Duration;

use objc2_audio_toolbox::{
    AudioQueueAllocateBuffer, AudioQueueBufferRef, AudioQueueDispose, AudioQueueEnqueueBuffer,
    AudioQueueNewOutput, AudioQueueRef, AudioQueueStart, AudioQueueStop,
};
use objc2_core_audio::{
    AudioObjectGetPropertyData, AudioObjectID, AudioObjectPropertyAddress,
    kAudioDevicePropertyNominalSampleRate, kAudioHardwarePropertyDefaultOutputDevice,
    kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject,
    kAudioObjectUnknown,
};
use objc2_core_audio_types::{
    AudioStreamBasicDescription, kAudioFormatFlagIsFloat, kAudioFormatFlagIsPacked,
    kAudioFormatLinearPCM,
};

use crate::capture_audio::SAMPLES_PER_CHUNK;
use crate::error::{MediaError, Result};
use crate::playout::{AudioPlayer, to_device_pcm};
use lumepeer_core::constants::{AUDIO_CHANNELS, AUDIO_SAMPLE_RATE_HZ};

/// How many queue-owned buffers [`CoreAudioPlayout::start`] allocates. Each
/// buffer carries one converted wire chunk (`AUDIO_FRAME_MS` = 20ms), so 4
/// buffers is 80ms of depth: enough to ride out a scheduling hiccup on the
/// completion callback without piling up latency, the same order of
/// magnitude as `WasapiPlayout`'s 200ms render buffer and
/// `linux_pipewire::CHANNEL_DEPTH`'s 160ms.
const AUDIO_QUEUE_BUFFER_COUNT: usize = 4;

/// How long one blocking [`CoreAudioPlayout::push`] may wait for a buffer
/// the completion callback has not yet returned before the queue is
/// considered stuck. Mirrors `WasapiPlayout`'s `PLAYBACK_TIMEOUT`.
const PLAYBACK_TIMEOUT: Duration = Duration::from_secs(2);

/// Bytes per sample of the f32 PCM [`to_device_pcm`] produces.
const BYTES_PER_SAMPLE: u32 = 4;

/// A queue buffer on loan from `CoreAudio`, wrapped so it can cross the
/// completion callback's channel. Sound because ownership of one buffer
/// pointer is exclusive at every instant: either [`CoreAudioPlayout::push`]
/// holds it (about to fill and enqueue) or the queue holds it (playing);
/// [`output_callback`] only ever names the exact buffer `CoreAudio` just
/// finished with and hands it back once, so no two owners ever touch it at
/// the same time.
struct FreeBuffer(AudioQueueBufferRef);

// SAFETY: see the doc comment on `FreeBuffer` above — exclusive handoff
// through the channel, never aliased.
unsafe impl Send for FreeBuffer {}

/// Shared with the C completion callback through a raw `*mut c_void`
/// (`AudioQueueNewOutput`'s `inUserData`); reclaimed in
/// [`CoreAudioPlayout::stop`] after the queue that could call back into it
/// has been disposed.
struct CallbackState {
    free_tx: SyncSender<FreeBuffer>,
}

/// Runs on one of the audio queue's own internal threads (`inCallbackRunLoop
/// = NULL` in [`CoreAudioPlayout::start`]) whenever `CoreAudio` is done
/// playing a buffer and hands it back for reuse.
extern "C-unwind" fn output_callback(
    user_data: *mut c_void,
    _queue: AudioQueueRef,
    buffer: AudioQueueBufferRef,
) {
    if user_data.is_null() {
        return;
    }
    // SAFETY: `user_data` was set in `start` to a `Box<CallbackState>`
    // leaked via `Box::into_raw`, and `stop` only reclaims that box after
    // `AudioQueueDispose` returns — which CoreAudio's own documentation
    // guarantees means no further callback runs afterwards.
    let state = unsafe { &*user_data.cast::<CallbackState>() };
    let _ = state.free_tx.send(FreeBuffer(buffer));
}

struct CoreAudioState {
    queue: AudioQueueRef,
    /// The rate `push` resamples every chunk onto, queried once in `start`.
    device_rate: u32,
    /// Bytes one allocated buffer holds; `push` clamps to this rather than
    /// writing out of bounds if a chunk somehow converts to more than
    /// `start` sized buffers for.
    buffer_capacity: u32,
    free_rx: Receiver<FreeBuffer>,
    /// Reclaimed in `stop`, after `AudioQueueDispose`.
    callback_state: *mut c_void,
}

// SAFETY: `queue` and `callback_state` are raw CoreAudio/heap handles that
// never alias a second owner — this type is only ever driven by whichever
// thread holds `&mut CoreAudioPlayout`, the same justification
// `PlayoutState` (the WASAPI backend, this same crate) gives for its own COM
// interface pointers.
unsafe impl Send for CoreAudioState {}

/// `AudioQueue` playback of the default output device (§11; ADR 0028;
/// docs/gap-tasks/02-macos-audio-playout.md).
pub struct CoreAudioPlayout {
    state: Option<CoreAudioState>,
}

impl std::fmt::Debug for CoreAudioPlayout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoreAudioPlayout")
            .field("active", &self.state.is_some())
            .finish()
    }
}

impl Default for CoreAudioPlayout {
    fn default() -> Self {
        Self::new()
    }
}

impl CoreAudioPlayout {
    /// Builds an idle player; nothing opens until [`start`](Self::start).
    #[must_use]
    pub const fn new() -> Self {
        Self { state: None }
    }

    /// Opens the default output device's `AudioQueue` at its own nominal
    /// sample rate.
    ///
    /// # Errors
    /// [`MediaError::CaptureUnavailable`] when no output device exists or
    /// any `CoreAudio` call fails.
    pub fn start(&mut self) -> Result<()> {
        if self.state.is_some() {
            return Ok(());
        }

        let device_rate = default_output_sample_rate()?;
        let bytes_per_frame = BYTES_PER_SAMPLE * u32::from(AUDIO_CHANNELS);
        let format = AudioStreamBasicDescription {
            mSampleRate: f64::from(device_rate),
            mFormatID: kAudioFormatLinearPCM,
            mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
            mBytesPerPacket: bytes_per_frame,
            mFramesPerPacket: 1,
            mBytesPerFrame: bytes_per_frame,
            mChannelsPerFrame: u32::from(AUDIO_CHANNELS),
            mBitsPerChannel: 32,
            mReserved: 0,
        };

        let (free_tx, free_rx) = mpsc::sync_channel::<FreeBuffer>(AUDIO_QUEUE_BUFFER_COUNT);
        let callback_state = Box::into_raw(Box::new(CallbackState {
            free_tx: free_tx.clone(),
        }))
        .cast::<c_void>();

        let mut queue: AudioQueueRef = std::ptr::null_mut();
        // SAFETY: `format` is a local the call only reads; `callback_state`
        // is a live leaked box `output_callback` only ever reads through a
        // shared reference; `queue` is a local out-pointer.
        let status = unsafe {
            AudioQueueNewOutput(
                NonNull::from(&format),
                Some(output_callback),
                callback_state,
                None,
                None,
                0,
                NonNull::from(&mut queue),
            )
        };
        if status != 0 {
            // SAFETY: the queue was never created, so nothing else can
            // reference the box just leaked above; reclaim it here.
            unsafe {
                drop(Box::from_raw(callback_state.cast::<CallbackState>()));
            }
            return Err(MediaError::CaptureUnavailable(format!(
                "AudioQueueNewOutput failed: OSStatus {status}"
            )));
        }

        // Worst case one wire chunk (`SAMPLES_PER_CHUNK` frames at the wire
        // rate) grows to once resampled onto `device_rate`, plus a one-frame
        // margin for `to_device_pcm`'s own floor() rounding (see its doc
        // comment).
        let wire_frames = u32::try_from(SAMPLES_PER_CHUNK).unwrap_or(u32::MAX);
        let max_frames_exact = (f64::from(wire_frames) * f64::from(device_rate)
            / f64::from(AUDIO_SAMPLE_RATE_HZ))
        .ceil();
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a 20ms chunk resampled onto any real device rate stays far below u32::MAX, and ceil() of a non-negative product is never negative"
        )]
        let max_frames = max_frames_exact as u32 + 1;
        let buffer_capacity = max_frames.saturating_mul(bytes_per_frame);

        for _ in 0..AUDIO_QUEUE_BUFFER_COUNT {
            let mut buffer: AudioQueueBufferRef = std::ptr::null_mut();
            // SAFETY: `queue` was just created above and is still valid;
            // `buffer` is a local out-pointer.
            let status = unsafe {
                AudioQueueAllocateBuffer(queue, buffer_capacity, NonNull::from(&mut buffer))
            };
            if status != 0 {
                // SAFETY: tears down whatever was created above; no `push`
                // has run yet, so no buffer is outstanding anywhere else.
                // `AudioQueueDispose` also frees every buffer already
                // allocated on this queue, per its own documentation.
                unsafe {
                    let _ = AudioQueueDispose(queue, true);
                    drop(Box::from_raw(callback_state.cast::<CallbackState>()));
                }
                return Err(MediaError::CaptureUnavailable(format!(
                    "AudioQueueAllocateBuffer failed: OSStatus {status}"
                )));
            }
            // The free-list channel has exactly `AUDIO_QUEUE_BUFFER_COUNT`
            // capacity and nothing has received from it yet, so this send
            // never blocks.
            let _ = free_tx.send(FreeBuffer(buffer));
        }

        // SAFETY: `queue` is valid; a null start time starts as soon as
        // possible.
        let status = unsafe { AudioQueueStart(queue, std::ptr::null()) };
        if status != 0 {
            // SAFETY: as in the allocate-buffer failure path above.
            unsafe {
                let _ = AudioQueueDispose(queue, true);
                drop(Box::from_raw(callback_state.cast::<CallbackState>()));
            }
            return Err(MediaError::CaptureUnavailable(format!(
                "AudioQueueStart failed: OSStatus {status}"
            )));
        }

        tracing::info!(
            rate = device_rate,
            buffer_capacity,
            buffers = AUDIO_QUEUE_BUFFER_COUNT,
            "CoreAudio AudioQueue playback started"
        );
        self.state = Some(CoreAudioState {
            queue,
            device_rate,
            buffer_capacity,
            free_rx,
            callback_state,
        });
        Ok(())
    }

    /// Hands one wire-format PCM chunk (48 kHz s16 stereo, §11) to the
    /// queue, blocking until a buffer is free or [`PLAYBACK_TIMEOUT`]
    /// expires.
    ///
    /// # Errors
    /// [`MediaError::CaptureInterrupted`] once the device is gone,
    /// [`stop`](Self::stop) has run, or no buffer freed up in time.
    pub fn push(&mut self, samples: &[i16], _timestamp_us: u64) -> Result<()> {
        let state = self
            .state
            .as_ref()
            .ok_or_else(|| MediaError::CaptureInterrupted("playback not started".to_owned()))?;

        let converted = to_device_pcm(samples, state.device_rate, usize::from(AUDIO_CHANNELS));
        if converted.is_empty() {
            return Ok(());
        }

        let FreeBuffer(buffer) =
            state
                .free_rx
                .recv_timeout(PLAYBACK_TIMEOUT)
                .map_err(|e| match e {
                    RecvTimeoutError::Timeout => MediaError::CaptureInterrupted(
                        "no CoreAudio buffer freed up within the playback timeout".to_owned(),
                    ),
                    RecvTimeoutError::Disconnected => MediaError::CaptureInterrupted(
                        "the CoreAudio playback queue is gone".to_owned(),
                    ),
                })?;

        let byte_len = converted.len() * std::mem::size_of::<f32>();
        let capacity = usize::try_from(state.buffer_capacity).unwrap_or(0);
        // A chunk larger than what `start` sized buffers for is a caller
        // surprise, not a crash: write only what fits and say so, rather
        // than reading or writing past the buffer.
        let write_len = byte_len.min(capacity);
        if write_len < byte_len {
            tracing::warn!(
                byte_len,
                capacity,
                "CoreAudio buffer too small for this chunk, truncating"
            );
        }
        let write_samples = write_len / std::mem::size_of::<f32>();

        // SAFETY: `buffer` came from this queue's own pool, allocated in
        // `start` with `buffer_capacity` bytes of storage; `write_len` is
        // clamped to that capacity above, so the write below never leaves
        // the buffer's own memory.
        unsafe {
            let dest = (*buffer).mAudioData.as_ptr().cast::<f32>();
            std::ptr::copy_nonoverlapping(converted.as_ptr(), dest, write_samples);
            (*buffer).mAudioDataByteSize =
                u32::try_from(write_len).unwrap_or(state.buffer_capacity);
        }

        // SAFETY: `state.queue` is the queue `buffer` was allocated from and
        // is still running; `buffer` is not enqueued anywhere else right
        // now — it just came off the free channel.
        let status = unsafe { AudioQueueEnqueueBuffer(state.queue, buffer, 0, std::ptr::null()) };
        if status != 0 {
            return Err(MediaError::CaptureInterrupted(format!(
                "AudioQueueEnqueueBuffer failed: OSStatus {status}"
            )));
        }
        Ok(())
    }

    /// Stops playback and releases the device. Idempotent.
    pub fn stop(&mut self) {
        let Some(state) = self.state.take() else {
            return;
        };
        // SAFETY: `state.queue` was created in `start` and is still valid.
        // Stopping immediately (synchronously) guarantees the device is
        // fully released before this call returns, so a second session can
        // reacquire it right away.
        unsafe {
            let _ = AudioQueueStop(state.queue, true);
            let _ = AudioQueueDispose(state.queue, true);
        }
        // SAFETY: `AudioQueueDispose` above (`inImmediate = true`)
        // guarantees no further callback runs after it returns, so nothing
        // else can still be reading through `callback_state`.
        unsafe {
            drop(Box::from_raw(state.callback_state.cast::<CallbackState>()));
        }
    }
}

impl AudioPlayer for CoreAudioPlayout {
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

impl Drop for CoreAudioPlayout {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Reads the default output device's real nominal sample rate from the
/// `CoreAudio` HAL — the same "ask, don't assume" discipline
/// `WasapiPlayout::start`'s `GetMixFormat` call and
/// `capture_audio::linux_pipewire`'s negotiated-format handling both follow,
/// so the queue this module opens never relies on `CoreAudio`'s own,
/// undocumented internal resampler.
fn default_output_sample_rate() -> Result<u32> {
    let mut device_id: AudioObjectID = kAudioObjectUnknown;
    let mut size = u32::try_from(std::mem::size_of::<AudioObjectID>()).unwrap_or(0);
    let mut address = AudioObjectPropertyAddress {
        mSelector: kAudioHardwarePropertyDefaultOutputDevice,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    // SAFETY: `address`/`size`/`device_id` are locals the call reads or
    // fills; `kAudioObjectSystemObject` names a well-known, always-valid
    // object ID, not a resource that needs releasing.
    let status = unsafe {
        AudioObjectGetPropertyData(
            AudioObjectID::try_from(kAudioObjectSystemObject).unwrap_or(0),
            NonNull::from(&mut address),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut device_id).cast(),
        )
    };
    if status != 0 {
        return Err(MediaError::CaptureUnavailable(format!(
            "reading the default output device failed: OSStatus {status}"
        )));
    }
    if device_id == kAudioObjectUnknown {
        return Err(MediaError::CaptureUnavailable(
            "no default output device is configured".to_owned(),
        ));
    }

    let mut rate: f64 = 0.0;
    let mut size = u32::try_from(std::mem::size_of::<f64>()).unwrap_or(0);
    let mut address = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyNominalSampleRate,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    // SAFETY: as above; `device_id` came from the successful query just
    // above.
    let status = unsafe {
        AudioObjectGetPropertyData(
            device_id,
            NonNull::from(&mut address),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut rate).cast(),
        )
    };
    if status != 0 || rate <= 0.0 {
        return Err(MediaError::CaptureUnavailable(format!(
            "reading the default output device's nominal sample rate failed: OSStatus {status}"
        )));
    }
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a real device's nominal sample rate is far below u32::MAX and never negative"
    )]
    Ok(rate.round() as u32)
}
