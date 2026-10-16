//! Whether this build can encode AV1 in software (§11; ADR 0141, ADR 0162).
//!
//! libaom's realtime AV1 is the one software video encoder: since ADR 0162
//! removed the `openh264` H.264 fallback, it is what a host with no hardware
//! encoder for a session encodes with, so there is nothing left to measure it
//! against and nothing to fall back to when it is slow. Each host used to time
//! it against `openh264` once per process and give it up for the rest of the
//! process when a session could not keep up (ADR 0142-0145); now a slow host
//! shrinks the picture instead (ADR 0144's encoder-speed controller), and
//! which session gets it is `choose_media_codec`'s rule alone.

/// Whether software AV1 is compiled into this build at all: `encode-aom`, on
/// a target `lumepeer-aom-sys` builds libaom for.
#[must_use]
pub const fn built() -> bool {
    cfg!(all(
        feature = "encode-aom",
        any(target_arch = "x86_64", target_arch = "aarch64"),
        any(
            target_os = "windows",
            target_os = "linux",
            target_os = "macos"
        )
    ))
}
