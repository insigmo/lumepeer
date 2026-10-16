//! Raw bindings to the libaom shim (`shim/aom_shim.c`; ADR 0141, ADR 0162).
//!
//! Declarations only. Every call is `unsafe`, and the two callers,
//! `lumepeer_media::encode::aom` and `lumepeer_media::decode::aom`, carry the
//! `SAFETY:` reasoning for each of them. The shim, not libaom itself, is what
//! is bound: it owns the `aom_codec_enc_cfg_t`, `aom_codec_dec_cfg_t` and
//! `aom_image_t` layouts, so no libaom struct has to be mirrored here and kept
//! in step with the vendored release by hand.
//!
//! The declarations resolve only with the `vendored` feature, which builds and
//! links libaom; without it nothing may call them.

use std::ffi::{c_char, c_int};

/// One encoder: libaom's context, its configuration and the output buffer.
/// Opaque; only ever handled by pointer.
#[repr(C)]
#[derive(Debug)]
pub struct AomShim {
    _private: [u8; 0],
}

/// One decoder: libaom's context and its last error (ADR 0162). Opaque; only
/// ever handled by pointer.
#[repr(C)]
#[derive(Debug)]
pub struct AomShimDec {
    _private: [u8; 0],
}

#[allow(
    unsafe_code,
    reason = "a foreign-function declaration is the whole of this crate; the calls, and their SAFETY notes, are in lumepeer-media"
)]
unsafe extern "C" {
    /// Opens a realtime AV1 encoder with the WebRTC RTC configuration.
    ///
    /// Returns null on failure, with a NUL-terminated reason in `err`
    /// (`errlen` bytes long).
    pub fn aom_shim_open(
        width: c_int,
        height: c_int,
        fps: c_int,
        kbps: c_int,
        speed: c_int,
        screen: c_int,
        threads: c_int,
        tile_cols_log2: c_int,
        min_q: c_int,
        max_q: c_int,
        err: *mut c_char,
        errlen: c_int,
    ) -> *mut AomShim;

    /// Encodes one I420 picture of the size the encoder was opened at.
    ///
    /// On success (0) `*out` points at `*out_len` bytes owned by the shim,
    /// valid until the next call on the same encoder. On failure (-1) the
    /// reason is in [`aom_shim_error`].
    pub fn aom_shim_encode(
        shim: *mut AomShim,
        y: *const u8,
        u: *const u8,
        v: *const u8,
        y_stride: c_int,
        uv_stride: c_int,
        pts: i64,
        force_keyframe: c_int,
        out: *mut *const u8,
        out_len: *mut usize,
        is_key: *mut c_int,
    ) -> c_int;

    /// Moves the bitrate target through `aom_codec_enc_config_set`, without a
    /// new encoder. 0 on success, -1 with the reason in [`aom_shim_error`].
    pub fn aom_shim_set_bitrate(shim: *mut AomShim, kbps: c_int) -> c_int;

    /// The bitrate target the encoder currently holds, kbit/s.
    pub fn aom_shim_bitrate(shim: *const AomShim) -> c_int;

    /// The controls this build refused, comma-separated; empty when none.
    /// Owned by the encoder.
    pub fn aom_shim_refused(shim: *const AomShim) -> *const c_char;

    /// The last error. Owned by the encoder.
    pub fn aom_shim_error(shim: *const AomShim) -> *const c_char;

    /// Destroys an encoder opened by [`aom_shim_open`].
    pub fn aom_shim_close(shim: *mut AomShim);

    /// libaom's version string. Static.
    pub fn aom_shim_version() -> *const c_char;

    /// Opens an AV1 decoder on `threads` threads (ADR 0162).
    ///
    /// Returns null on failure, with a NUL-terminated reason in `err`
    /// (`errlen` bytes long).
    pub fn aom_shim_dec_open(threads: c_int, err: *mut c_char, errlen: c_int) -> *mut AomShimDec;

    /// Decodes one temporal unit of `len` bytes at `data`.
    ///
    /// On success (0) `*got` says whether a picture came out; if one did, the
    /// planes of that 8-bit 4:2:0 picture are owned by the decoder and valid
    /// until the next call on it. On failure (-1) the reason is in
    /// [`aom_shim_dec_error`].
    pub fn aom_shim_dec_decode(
        dec: *mut AomShimDec,
        data: *const u8,
        len: usize,
        got: *mut c_int,
        y: *mut *const u8,
        u: *mut *const u8,
        v: *mut *const u8,
        y_stride: *mut c_int,
        uv_stride: *mut c_int,
        width: *mut c_int,
        height: *mut c_int,
    ) -> c_int;

    /// The decoder's last error. Owned by the decoder.
    pub fn aom_shim_dec_error(dec: *const AomShimDec) -> *const c_char;

    /// Destroys a decoder opened by [`aom_shim_dec_open`].
    pub fn aom_shim_dec_close(dec: *mut AomShimDec);
}
