//! `CoreAudio` microphone capture for macOS (§11; ADR 0028, ADR 0156).
//!
//! The guest-side sibling of `windows_mic` and `linux_pipewire`'s
//! microphone, and the playout's `AudioQueue` run the other way:
//! [`crate::playout::CoreAudioPlayout`] hands buffers to the queue to play,
//! this one gets them back full. An *input* queue on the default input
//! device fills one buffer at a time; the completion callback copies it out
//! and gives the buffer straight back to the queue, and
//! [`MicCapturer::next_chunk`] cuts what arrived into `AUDIO_FRAME_MS`
//! chunks of the §11 wire format.
//!
//! The queue is asked for mono f32 at the device's own nominal rate — the
//! "ask, don't assume" rule the playout follows — and
//! [`crate::capture_audio::to_wire_pcm`] does the rest. A microphone is one
//! channel of content; a stereo or multichannel input is folded to it by
//! the queue's own converter.
//!
//! The microphone is a privacy permission. The bundle has to declare
//! `NSMicrophoneUsageDescription` (apps/desktop/src-tauri/Info.plist) —
//! without it macOS ends the process the moment it touches the input — and
//! the hardened-runtime release build carries the
//! `com.apple.security.device.audio-input` entitlement
//! (apps/desktop/src-tauri/Entitlements.plist). The first start asks the
//! person; what a refusal delivers is silence, which the session's
//! `AudioMeter` shows as chunks with nothing non-zero in them.
//!
//! Lives beside the playout under `audio-playout-coreaudio`: the same
//! binding set, the way `audio-capture-pipewire` carries both directions on
//! Linux.

#![allow(
    unsafe_code,
    reason = "every AudioQueue entry point here is a raw C FFI call with no safe binding; every block carries a SAFETY note, per §21"
)]

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};

use objc2_audio_toolbox::{
    AudioQueueAllocateBuffer, AudioQueueBufferRef, AudioQueueDispose, AudioQueueEnqueueBuffer,
    AudioQueueNewInput, AudioQueueRef, AudioQueueStart, AudioQueueStop,
};
use objc2_core_audio::kAudioHardwarePropertyDefaultInputDevice;
use objc2_core_audio_types::{
    AudioStreamBasicDescription, AudioStreamPacketDescription, AudioTimeStamp,
    kAudioFormatFlagIsFloat, kAudioFormatFlagIsPacked, kAudioFormatLinearPCM,
};

use crate::capture_audio::{
    MicCapturer, PcmChunk, READ_TIMEOUT, capture_timestamp_us, frames_per_chunk, to_wire_pcm,
};
use crate::error::{MediaError, Result};
use crate::playout::macos_coreaudio::default_device_sample_rate;

/// Buffers the input queue cycles through, each one `AUDIO_FRAME_MS` long:
/// 80 ms in flight, the playout's depth.
const BUFFER_COUNT: usize = 4;

/// Filled buffers waiting for [`MicCapturer::next_chunk`]: 320 ms. A reader
/// further behind than that loses the oldest audio rather than the queue
/// stalling — a microphone that lags the conversation is worse than a gap.
const CHANNEL_DEPTH: usize = 16;

/// Bytes of one sample of the mono f32 the queue is asked for.
const BYTES_PER_SAMPLE: u32 = 4;

/// Shared with the C callback through `AudioQueueNewInput`'s `inUserData`;
/// reclaimed in [`CoreAudioMicCapturer::stop`] once the queue that could
/// call back into it is disposed.
struct CallbackState {
    samples_tx: SyncSender<Vec<f32>>,
}

/// Runs on the queue's own thread (`inCallbackRunLoop = NULL`) each time
/// `CoreAudio` has filled a buffer.
extern "C-unwind" fn input_callback(
    user_data: *mut c_void,
    queue: AudioQueueRef,
    buffer: AudioQueueBufferRef,
    _start_time: NonNull<AudioTimeStamp>,
    _packets: u32,
    _descriptions: *const AudioStreamPacketDescription,
) {
    if user_data.is_null() || buffer.is_null() {
        return;
    }
    // SAFETY: `user_data` is the `Box<CallbackState>` `start` leaked, and
    // `stop` reclaims it only after `AudioQueueDispose` has returned, after
    // which no callback runs. `buffer` is one of this queue's own, filled
    // with `mAudioDataByteSize` bytes of the mono f32 `start` asked for, and
    // is ours until it is enqueued again below.
    let (state, samples) = unsafe {
        let state = &*user_data.cast::<CallbackState>();
        let len =
            usize::try_from((*buffer).mAudioDataByteSize).unwrap_or(0) / std::mem::size_of::<f32>();
        let data = (*buffer).mAudioData.as_ptr().cast::<f32>();
        (state, std::slice::from_raw_parts(data, len).to_vec())
    };
    if !samples.is_empty() {
        // Full means the reader is behind: this buffer's audio is the one
        // let go (see `CHANNEL_DEPTH`).
        let _ = state.samples_tx.try_send(samples);
    }
    // SAFETY: the buffer goes back to the queue it came from. While `stop`
    // is tearing the queue down this is refused, which is what should
    // happen to it.
    let _ = unsafe { AudioQueueEnqueueBuffer(queue, buffer, 0, std::ptr::null()) };
}

struct MicState {
    queue: AudioQueueRef,
    /// The device rate the queue delivers at, queried once in `start`.
    rate: u32,
    samples_rx: Receiver<Vec<f32>>,
    /// Samples received but not yet a whole chunk.
    pending: Vec<f32>,
    /// Reclaimed in `stop`, after `AudioQueueDispose`.
    callback_state: *mut c_void,
}

// SAFETY: `queue` and `callback_state` are handles no second owner aliases;
// this state is only ever driven through `&mut CoreAudioMicCapturer`, the
// justification `CoreAudioPlayout`'s state gives for the same handles.
unsafe impl Send for MicState {}

/// `AudioQueue` capture of the default input device (§11; ADR 0156).
pub struct CoreAudioMicCapturer {
    state: Option<MicState>,
}

impl std::fmt::Debug for CoreAudioMicCapturer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoreAudioMicCapturer")
            .field("active", &self.state.is_some())
            .finish()
    }
}

impl Default for CoreAudioMicCapturer {
    fn default() -> Self {
        Self::new()
    }
}

impl CoreAudioMicCapturer {
    /// Builds an idle capturer; nothing opens until [`MicCapturer::start`].
    #[must_use]
    pub const fn new() -> Self {
        Self { state: None }
    }
}

/// Disposes a queue `start` could not finish setting up, and the callback
/// state only it referenced.
///
/// # Safety
/// `queue` must be the queue created with `callback_state`, and no other
/// reference to either may outlive this call.
unsafe fn abandon(queue: AudioQueueRef, callback_state: *mut c_void) {
    // SAFETY: per this function's contract; `AudioQueueDispose` also frees
    // every buffer allocated on the queue.
    unsafe {
        let _ = AudioQueueDispose(queue, true);
        drop(Box::from_raw(callback_state.cast::<CallbackState>()));
    }
}

impl MicCapturer for CoreAudioMicCapturer {
    fn start(&mut self) -> Result<()> {
        if self.state.is_some() {
            return Ok(());
        }
        let rate = default_device_sample_rate(kAudioHardwarePropertyDefaultInputDevice, "input")?;
        let format = AudioStreamBasicDescription {
            mSampleRate: f64::from(rate),
            mFormatID: kAudioFormatLinearPCM,
            mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
            mBytesPerPacket: BYTES_PER_SAMPLE,
            mFramesPerPacket: 1,
            mBytesPerFrame: BYTES_PER_SAMPLE,
            mChannelsPerFrame: 1,
            mBitsPerChannel: 32,
            mReserved: 0,
        };

        let (samples_tx, samples_rx) = mpsc::sync_channel::<Vec<f32>>(CHANNEL_DEPTH);
        let callback_state = Box::into_raw(Box::new(CallbackState { samples_tx })).cast::<c_void>();

        let mut queue: AudioQueueRef = std::ptr::null_mut();
        // SAFETY: `format` is a local the call only reads; `callback_state`
        // is a live leaked box the callback only reads through a shared
        // reference; `queue` is a local out-pointer.
        let status = unsafe {
            AudioQueueNewInput(
                NonNull::from(&format),
                Some(input_callback),
                callback_state,
                None,
                None,
                0,
                NonNull::from(&mut queue),
            )
        };
        if status != 0 {
            // SAFETY: no queue was created, so nothing else can reference
            // the box just leaked.
            unsafe {
                drop(Box::from_raw(callback_state.cast::<CallbackState>()));
            }
            return Err(MediaError::CaptureUnavailable(format!(
                "AudioQueueNewInput failed: OSStatus {status}"
            )));
        }

        // One chunk of the device's own rate per buffer, so each callback
        // carries one `AUDIO_FRAME_MS` of audio.
        let buffer_bytes = u32::try_from(frames_per_chunk(rate))
            .unwrap_or(u32::MAX)
            .saturating_mul(BYTES_PER_SAMPLE);
        for _ in 0..BUFFER_COUNT {
            let mut buffer: AudioQueueBufferRef = std::ptr::null_mut();
            // SAFETY: `queue` was just created; `buffer` is a local
            // out-pointer, and an allocated buffer is handed straight to the
            // queue it belongs to.
            let status = unsafe {
                let status =
                    AudioQueueAllocateBuffer(queue, buffer_bytes, NonNull::from(&mut buffer));
                if status == 0 {
                    AudioQueueEnqueueBuffer(queue, buffer, 0, std::ptr::null())
                } else {
                    status
                }
            };
            if status != 0 {
                // SAFETY: the queue has not started, so no callback holds
                // the state.
                unsafe { abandon(queue, callback_state) };
                return Err(MediaError::CaptureUnavailable(format!(
                    "preparing the microphone's AudioQueue buffers failed: OSStatus {status}"
                )));
            }
        }

        // SAFETY: `queue` is valid; a null start time starts at once.
        let status = unsafe { AudioQueueStart(queue, std::ptr::null()) };
        if status != 0 {
            // SAFETY: as above — a queue that did not start calls nothing.
            unsafe { abandon(queue, callback_state) };
            return Err(MediaError::CaptureUnavailable(format!(
                "AudioQueueStart (microphone) failed: OSStatus {status}"
            )));
        }

        tracing::info!(rate, "CoreAudio microphone capture started");
        self.state = Some(MicState {
            queue,
            rate,
            samples_rx,
            pending: Vec::new(),
            callback_state,
        });
        Ok(())
    }

    fn next_chunk(&mut self) -> Result<PcmChunk> {
        let state = self
            .state
            .as_mut()
            .ok_or_else(|| MediaError::CaptureInterrupted("capture not started".to_owned()))?;
        // Mono, so a chunk's frames are its samples.
        let needed = frames_per_chunk(state.rate);
        while state.pending.len() < needed {
            match state.samples_rx.recv_timeout(READ_TIMEOUT) {
                Ok(samples) => state.pending.extend_from_slice(&samples),
                // A microphone always streams, silence included: nothing
                // for this long is a stuck device (`READ_TIMEOUT`).
                Err(RecvTimeoutError::Timeout) => {
                    return Err(MediaError::CaptureInterrupted(
                        "the microphone delivered nothing within the read timeout".to_owned(),
                    ));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(MediaError::CaptureInterrupted(
                        "the CoreAudio input queue is gone".to_owned(),
                    ));
                }
            }
        }
        let rest = state.pending.split_off(needed);
        let taken = std::mem::replace(&mut state.pending, rest);
        Ok(PcmChunk {
            samples: to_wire_pcm(&taken, state.rate, 1),
            timestamp_us: capture_timestamp_us(),
        })
    }

    fn stop(&mut self) {
        let Some(state) = self.state.take() else {
            return;
        };
        // SAFETY: `state.queue` was created in `start` and is still valid;
        // stopping synchronously releases the microphone before this
        // returns, and `AudioQueueDispose` (`inImmediate = true`) means no
        // callback runs after it, so the state box is reclaimed last.
        unsafe {
            let _ = AudioQueueStop(state.queue, true);
            abandon(state.queue, state.callback_state);
        }
    }
}

impl Drop for CoreAudioMicCapturer {
    fn drop(&mut self) {
        self.stop();
    }
}
