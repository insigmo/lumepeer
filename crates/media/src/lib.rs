//! `lumepeer-media` — capture, encode, decode, jitter buffer and adaptive
//! bitrate (design doc §4).
//!
//! Privileged component, but the decoder is not part of it: decoding happens in
//! a separate sandboxed process (§11.3). Capture starts only once a viewer
//! holds a `view` grant and stops with the last viewer (§8.1, §11).

// `deny` rather than `forbid`: the shared memory ring buffer that §11.3
// mandates for decoder IPC cannot be expressed in safe Rust, and neither can
// driving `CreateProcessW`'s AppContainer attributes for the Windows decoder
// sandbox, the Windows Media Foundation COM calls the hardware H.264 encoder
// needs (every `IMFTransform`/`IMFSample`/... call in the `windows` crate's MF
// bindings is `unsafe fn`), or the macOS `ScreenCaptureKit` capture path
// (every entry point in the `objc2` framework bindings is `unsafe fn` because
// it crosses into Objective-C, and the `SCStreamOutput` delegate has to be a
// real Objective-C class). Exactly four modules opt back in, each with a
// `reason`: `decode::shm` (ADR 0005), `decode::windows_sandbox` (ADR 0007),
// `encode::windows` (ADR 0011) and `capture::macos` (ADR 0013). Every block in
// all four carries a SAFETY note and is covered by tests, as §21 requires.
// Nothing else in the crate may opt in.
#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod abr;
pub mod capture;
pub mod decode;
pub mod encode;
pub mod error;
pub mod jitter;

pub use error::{MediaError, Result};
