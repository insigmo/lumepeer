//! `lumepeer-media` — capture, encode, decode, jitter buffer and adaptive
//! bitrate (design doc §4).
//!
//! Privileged component, but the decoder is not part of it: decoding happens in
//! a separate sandboxed process (§11.3). Capture starts only once a viewer
//! holds a `view` grant and stops with the last viewer (§8.1, §11).

// `deny` rather than `forbid`: the shared memory ring buffer that §11.3
// mandates for decoder IPC cannot be expressed in safe Rust, and neither can
// driving `CreateProcessW`'s AppContainer attributes for the Windows decoder
// sandbox. The exception is confined to `decode::shm` and, on Windows,
// `decode::windows_sandbox`; every block in either carries a SAFETY note and
// is covered by tests, as §21 requires. Nothing else in the crate may opt in.
#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod abr;
pub mod capture;
pub mod decode;
pub mod encode;
pub mod error;
pub mod jitter;

pub use error::{MediaError, Result};
