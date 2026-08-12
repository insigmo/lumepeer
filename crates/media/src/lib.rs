//! `lumepeer-media` — capture, encode, decode, jitter buffer and adaptive
//! bitrate (design doc §4).
//!
//! Privileged component, but the decoder is not part of it: decoding happens in
//! a separate sandboxed process (§11.3). Capture starts only once a viewer
//! holds a `view` grant and stops with the last viewer (§8.1, §11).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod abr;
pub mod capture;
pub mod decode;
pub mod encode;
pub mod error;
pub mod jitter;

pub use error::{MediaError, Result};
