//! Encoder selection: hardware first, software fallback second (design doc
//! §11, §18).
//!
//! H.264 is the desktop baseline a hardware encoder gives, and AV1 is what a
//! session prefers whenever both sides have hardware for it (ADR 0069, ADR
//! 0072). The one software encoder is libaom's AV1, for a host with no
//! hardware encoder for the session (ADR 0141, ADR 0162): the `openh264`
//! H.264 fallback is gone, so there is no H.264 without hardware. Opus is the
//! only audio codec.

use lumepeer_core::constants::{ENCODE_DEFAULT_BITRATE_KBPS, ENCODE_DEFAULT_FPS};

use crate::capture::Frame;
use crate::error::{MediaError, Result};

/// Video codec on the wire (§11).
///
/// The wire byte each of these travels as is
/// `lumepeer_core::protocol::MediaCodec` (ADR 0067); this enum is the
/// encoder's own name for the same closed set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    /// The desktop baseline, from a hardware encoder only (ADR 0162).
    H264,
    /// Preferred over the baseline with mutual hardware support (ADR 0069,
    /// ADR 0072), and the one codec a host encodes in software: libaom, where
    /// it has no hardware encoder for the session (ADR 0141, ADR 0162).
    Av1,
}

/// Where the encoding happens (§18).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderKind {
    /// Platform hardware encoder.
    Hardware,
    /// libaom's realtime AV1 in software, the one software encoder (ADR 0141,
    /// ADR 0162).
    SoftwareAv1,
}

/// Encoder settings; defaults come from §14, not from magic numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderConfig {
    /// Target frame rate.
    pub fps: u8,
    /// Target bitrate.
    pub bitrate_kbps: u32,
    /// Codec to use.
    pub codec: VideoCodec,
}

impl Default for EncoderConfig {
    fn default() -> Self {
        Self {
            fps: ENCODE_DEFAULT_FPS,
            bitrate_kbps: ENCODE_DEFAULT_BITRATE_KBPS,
            codec: VideoCodec::H264,
        }
    }
}

/// One encoded video frame.
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    /// Whether this frame can be decoded without previous frames.
    pub keyframe: bool,
    /// Monotonic timestamp in microseconds, copied from the capture frame.
    pub timestamp_us: u64,
    /// Bitstream bytes.
    pub data: Vec<u8>,
}

/// Video encoder.
pub trait VideoEncoder: Send {
    /// Encodes one captured frame.
    ///
    /// # Errors
    /// [`MediaError::Encode`] if the backend rejects the frame.
    fn encode(&mut self, frame: &Frame) -> Result<EncodedFrame>;

    /// Applies a new bitrate target, rate-limited by the caller to
    /// `ABR_ADJUST_MAX_RATE_PER_SEC` (§11).
    ///
    /// # Errors
    /// [`MediaError::Encode`] if the backend refuses the change.
    fn set_bitrate(&mut self, bitrate_kbps: u32) -> Result<()>;

    /// Makes the next encoded frame an intra frame, so a decoder that joined
    /// mid-stream or lost more than it could conceal has something to start
    /// from (§11's `KeyframeRequest`).
    ///
    /// A request, not a guarantee about any particular frame: the backend
    /// emits one at the next opportunity. The caller is responsible for the
    /// `KEYFRAME_MIN_INTERVAL_MS` budget — a keyframe is the most expensive
    /// frame in the stream, so nothing downstream may be allowed to ask for
    /// one per frame.
    ///
    /// # Errors
    /// [`MediaError::Encode`] if the backend has no way to force one.
    fn request_keyframe(&mut self) -> Result<()>;

    /// Which backend is in use, for telemetry and the UI.
    fn kind(&self) -> EncoderKind;
}

/// Whether a platform hardware encoder can be used right now for the codec
/// `config` names.
///
/// Each backend answers for its own codecs, and only after a real activation.
/// Nothing is refused here up front: a central "H.264 only" gate is what
/// would have to be remembered and edited every time a backend learns a new
/// codec, and the mismatch §11's mutual-hardware-support rule actually
/// guards against — handing back an H.264 rehearsal as an answer about
/// another codec — is prevented by each backend checking `config.codec`
/// itself before it claims anything (ADR 0069).
///
/// On Windows, with the `encode-mf` feature built in, [`windows`] genuinely
/// enumerates and activates a hardware-accelerated encoder MFT via Media
/// Foundation (`MFTEnumEx` filtered to `MFT_ENUM_FLAG_HARDWARE`) for the
/// output subtype the codec names, and only reports
/// [`EncoderKind::Hardware`] if one actually activates and accepts NV12
/// input — never a hopeful guess (ADR 0011). For [`VideoCodec::Av1`] it goes
/// one step further and pushes a frame through the transform it just
/// activated, because "it enumerated" and "it produces a picture" are
/// different questions for a codec whose encoder is far younger than the
/// baseline's (ADR 0069).
///
/// On Linux, with `encode-vaapi` built in, [`linux_vaapi`] does the same
/// thing through VA-API for H.264 and then goes the step further
/// `macos_videotoolbox` goes: it opens a DRM display, creates an H.264 config
/// on whichever encode entrypoint the driver offers — the low-power one is
/// the only one on current Intel — and pushes a frame through it, reporting
/// [`EncoderKind::Hardware`] only when an IDR picture comes back (ADR 0040,
/// ADR 0088). A context that opened turned out to say nothing about a stream
/// that decodes. It answers `None` for
/// [`VideoCodec::Av1`], and not because nobody looked: VA-API's AV1 encode
/// entrypoint cannot be driven through this workspace's libva bindings at
/// all, which ADR 0069 records rather than leaving as an unexplained gap.
///
/// On macOS, with `encode-videotoolbox` built in,
/// [`macos_videotoolbox`] goes one step further than the H.264 paths of
/// either of those: it opens a real `VTCompressionSession` *and pushes a
/// frame through it*, reporting [`EncoderKind::Hardware`] only once a picture
/// has actually come back out (ADR 0066). Every Mac of the last decade has an
/// H.264 encoder, so "is one installed" is a question whose answer is almost
/// always yes and almost never the reason a session shows nothing.
/// `VideoCodec::Av1` is `None` there, and not for want of looking: no Mac
/// has an AV1 hardware encoder at all — Apple silicon decodes AV1 from M3
/// onwards and encodes none of it.
///
/// `MediaCodec` remains phase 4 work (§19), so this is `None` on Android.
#[must_use]
pub fn probe_hardware(config: EncoderConfig) -> Option<EncoderKind> {
    #[cfg(all(target_os = "windows", feature = "encode-mf"))]
    {
        if windows::hardware_available(config) {
            return Some(EncoderKind::Hardware);
        }
    }
    #[cfg(all(
        target_os = "linux",
        not(target_os = "android"),
        feature = "encode-vaapi"
    ))]
    {
        if linux_vaapi::hardware_available(config) {
            return Some(EncoderKind::Hardware);
        }
    }
    #[cfg(all(target_os = "macos", feature = "encode-videotoolbox"))]
    {
        if macos_videotoolbox::hardware_available(config) {
            return Some(EncoderKind::Hardware);
        }
    }
    // Says which codec found nothing, which is the whole answer to "why did
    // this session negotiate H.264" on a machine whose hardware does not
    // encode the codec the guest asked for (§18). It also keeps `config` used
    // in a build with no encoder backend compiled in at all, which every
    // branch above is behind a `cfg` for.
    tracing::debug!(
        codec = ?config.codec,
        "no hardware encoder backend on this machine reports support"
    );
    None
}

/// Selects an encoder: hardware if available, otherwise libaom for AV1
/// (§18; ADR 0162).
///
/// # Errors
/// [`MediaError::EncoderUnavailable`] when neither path is usable; the UI must
/// explain why instead of silently degrading.
pub fn select_encoder(config: EncoderConfig) -> Result<Box<dyn VideoEncoder>> {
    // Probing now does real, non-free work (COM enumeration on Windows), so
    // it is computed once rather than once per branch below.
    let hardware = probe_hardware(config);

    if config.codec == VideoCodec::Av1 && hardware != Some(EncoderKind::Hardware) {
        // The one software encoder (ADR 0141, ADR 0162). When a session gets
        // it is `choose_media_codec`'s rule; what is checked here is only
        // what is fixed for the life of the process, so the answer cannot
        // change between the codec being announced on the wire and the
        // encoder being built for it.
        return select_software_encoder(config);
    }

    if let Some(EncoderKind::Hardware) = hardware {
        #[cfg(all(target_os = "windows", feature = "encode-mf"))]
        {
            tracing::info!(codec = ?config.codec, "hardware encoder available, using Media Foundation (§18)");
            return windows::MediaFoundationEncoder::new(config)
                .map(|e| Box::new(e) as Box<dyn VideoEncoder>);
        }
        #[cfg(all(
            target_os = "linux",
            not(target_os = "android"),
            feature = "encode-vaapi"
        ))]
        {
            tracing::info!("hardware H.264 encoder available, using VA-API (§18)");
            // Deliberately not `?`: a probe that succeeded and a constructor
            // that then failed is a driver that changed its mind between two
            // calls a microsecond apart — rare, and the error below says so
            // (§18, ADR 0040).
            match linux_vaapi::VaapiEncoder::new(config) {
                Ok(encoder) => return Ok(Box::new(encoder) as Box<dyn VideoEncoder>),
                Err(error) => {
                    tracing::info!(%error, "the VA-API encoder probed available but would not build");
                }
            }
        }
        #[cfg(all(target_os = "macos", feature = "encode-videotoolbox"))]
        {
            tracing::info!("hardware H.264 encoder available, using VideoToolbox (§18)");
            // Deliberately not `?`, for the same reason the VA-API arm above
            // is not: the probe has already opened a session and encoded a
            // frame through it, so a constructor that then fails is the
            // framework changing its mind between two calls a microsecond
            // apart (§18, ADR 0066).
            match macos_videotoolbox::VideoToolboxEncoder::new(config) {
                Ok(encoder) => return Ok(Box::new(encoder) as Box<dyn VideoEncoder>),
                Err(error) => {
                    tracing::info!(%error, "the VideoToolbox encoder probed available but would not build");
                }
            }
        }
        // A platform whose `probe_hardware` above can return `Hardware` but
        // that has no constructor wired up here would report hardware it is
        // not using. This arm exists so that mistake is an error instead.
        #[cfg(not(any(
            all(target_os = "windows", feature = "encode-mf"),
            all(
                target_os = "linux",
                not(target_os = "android"),
                feature = "encode-vaapi"
            ),
            all(target_os = "macos", feature = "encode-videotoolbox"),
        )))]
        {
            return Err(MediaError::EncoderUnavailable(
                "hardware encoder probing reported a backend that is not implemented yet"
                    .to_owned(),
            ));
        }
    }

    // No H.264 without hardware (ADR 0162): the probe found none, or the one
    // it found would not build. Encoding the AV1 libaom makes into a session
    // that announced H.264 would be a black window with no error anywhere,
    // so this fails instead, and the caller says why.
    Err(MediaError::EncoderUnavailable(format!(
        "no hardware encoder for {:?} on this host, and {:?} has no software encoder (ADR 0162)",
        config.codec, config.codec
    )))
}

/// Builds the software encoder for `config.codec` even where a hardware one
/// exists: libaom, for AV1, the only codec with one (ADR 0162). For a guest
/// that picked it by hand (ADR 0143), and for [`select_encoder`] on a host
/// with no hardware AV1 encoder.
///
/// # Errors
/// [`MediaError::EncoderUnavailable`] for H.264, which has no software
/// encoder any more, and for AV1 in a build without libaom.
pub fn select_software_encoder(config: EncoderConfig) -> Result<Box<dyn VideoEncoder>> {
    if config.codec == VideoCodec::Av1 && software_av1::built() {
        #[cfg(all(
            feature = "encode-aom",
            any(target_arch = "x86_64", target_arch = "aarch64"),
            any(target_os = "windows", target_os = "linux", target_os = "macos")
        ))]
        {
            tracing::info!("encoding AV1 in software, with libaom (ADR 0141, ADR 0162)");
            return aom::AomEncoder::new(config).map(|e| Box::new(e) as Box<dyn VideoEncoder>);
        }
    }
    Err(MediaError::EncoderUnavailable(format!(
        "this build has no software encoder for {:?} (ADR 0162)",
        config.codec
    )))
}

/// BGRA/NV12 conversion shared by every encoder: the hardware backends take
/// NV12, and libaom the I420 it splits into (ADR 0162).
#[cfg(any(
    all(target_os = "windows", feature = "encode-mf"),
    all(
        target_os = "linux",
        not(target_os = "android"),
        feature = "encode-vaapi"
    ),
    all(target_os = "macos", feature = "encode-videotoolbox"),
    feature = "encode-aom",
))]
mod nv12;

/// Telling a decoder an H.264 stream never reorders, where its encoder did
/// not (ADR 0154). Pure byte work, for every platform's encoders.
pub mod h264_sps;

/// macOS `VideoToolbox` hardware H.264 encoder (§5.1, §11, §18/§19 phase 4;
/// ADR 0066).
///
/// Compiled off macOS under `cfg(test)` as well as on it: the AVCC-to-Annex-B
/// rewrite this backend owes the guest is pure byte manipulation, and running
/// its tests on whatever machine happens to build the crate is worth more than
/// keeping the module strictly Mac-only.
#[cfg(all(feature = "encode-videotoolbox", any(target_os = "macos", test)))]
pub mod macos_videotoolbox;

/// VA-API hardware H.264 encoder for Linux (§5.1, §11, §18/§19 phase 4;
/// ADR 0040).
#[cfg(all(
    target_os = "linux",
    not(target_os = "android"),
    feature = "encode-vaapi"
))]
pub mod linux_vaapi;

/// Windows Media Foundation hardware video encoder — H.264 and AV1 (§5.1,
/// §11, §18/§19 phase 4; ADR 0011, ADR 0069).
///
/// The second and only other place in the crate that needs `unsafe`, besides
/// `decode::shm` (ADR 0005): every `IMFTransform`/`IMFActivate`/`IMFSample`
/// call in the `windows` crate's Media Foundation bindings is `unsafe fn`
/// because it crosses into COM. Each `unsafe` block in this module carries a
/// `SAFETY:` note, as §21 requires.
#[cfg(all(target_os = "windows", feature = "encode-mf"))]
#[allow(
    unsafe_code,
    reason = "Media Foundation is COM; every IMFTransform/IMFSample call in the `windows` crate is `unsafe fn`. See ADR 0011."
)]
pub mod windows;

/// Whether this build can encode AV1 in software (§11; ADR 0141, ADR 0162).
pub mod software_av1;

/// libaom's realtime AV1 encoder, in software (§11; ADR 0141, ADR 0162).
///
/// The third module of the encoder that needs `unsafe`: every libaom call
/// crosses into C through `lumepeer-aom-sys`'s shim. Each `unsafe` block
/// carries a `SAFETY:` note, as §21 requires.
#[cfg(all(
    feature = "encode-aom",
    any(target_arch = "x86_64", target_arch = "aarch64"),
    any(target_os = "windows", target_os = "linux", target_os = "macos")
))]
#[allow(
    unsafe_code,
    reason = "libaom is C; every call through lumepeer-aom-sys is `unsafe`. See ADR 0141."
)]
pub mod aom;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_come_from_the_constants_of_14() {
        let config = EncoderConfig::default();
        assert_eq!(config.fps, ENCODE_DEFAULT_FPS);
        assert_eq!(config.bitrate_kbps, ENCODE_DEFAULT_BITRATE_KBPS);
        assert_eq!(config.codec, VideoCodec::H264);
    }

    /// §11's mutual-hardware-support rule and its one exception, checked
    /// mechanically rather than by inspection so it runs the same on every
    /// machine: AV1 is served by genuine hardware when the probe says there
    /// is some; otherwise by libaom exactly when this build has it (ADR 0141,
    /// ADR 0162); otherwise refused.
    #[test]
    fn av1_is_served_by_hardware_then_by_libaom() {
        let config = EncoderConfig {
            codec: VideoCodec::Av1,
            ..EncoderConfig::default()
        };
        let hardware = probe_hardware(config) == Some(EncoderKind::Hardware);
        let software = software_av1::built();
        match select_encoder(config) {
            Ok(encoder) if hardware => assert_eq!(encoder.kind(), EncoderKind::Hardware),
            Ok(encoder) => {
                assert!(software, "AV1 was served with no hardware and no libaom");
                assert_eq!(encoder.kind(), EncoderKind::SoftwareAv1);
            }
            Err(MediaError::EncoderUnavailable(_)) => {
                assert!(
                    !hardware,
                    "AV1 was refused even though the probe reported hardware"
                );
                assert!(!software, "AV1 was refused even though libaom is built in");
            }
            Err(other) => panic!("unexpected error selecting an AV1 encoder: {other}"),
        }
    }

    /// No H.264 without hardware (ADR 0162): a host whose probe finds none is
    /// refused, never handed a software encoder of another codec.
    #[test]
    fn h264_is_served_by_hardware_or_not_at_all() {
        let config = EncoderConfig::default();
        let hardware = probe_hardware(config) == Some(EncoderKind::Hardware);
        match select_encoder(config) {
            Ok(encoder) => {
                assert!(hardware, "H.264 was served with no hardware encoder");
                assert_eq!(encoder.kind(), EncoderKind::Hardware);
            }
            Err(MediaError::EncoderUnavailable(_)) => {}
            Err(other) => panic!("unexpected error selecting an H.264 encoder: {other}"),
        }
        assert!(matches!(
            select_software_encoder(config),
            Err(MediaError::EncoderUnavailable(_))
        ));
    }

    /// The software AV1 branch builds an encoder that actually produces a
    /// picture, not one that only constructs (ADR 0141).
    #[test]
    fn a_software_av1_encoder_from_select_encoder_encodes() {
        let config = EncoderConfig {
            codec: VideoCodec::Av1,
            ..EncoderConfig::default()
        };
        let Ok(mut encoder) = select_encoder(config) else {
            return;
        };
        if encoder.kind() != EncoderKind::SoftwareAv1 {
            return;
        }
        let frame = Frame::cpu(
            128,
            72,
            crate::capture::PixelFormat::Bgra8,
            0,
            vec![0x80; 128 * 72 * 4],
        );
        let first = match encoder.encode(&frame) {
            Ok(first) => first,
            Err(error) => panic!("libaom refused a frame: {error}"),
        };
        assert!(first.keyframe && !first.data.is_empty());
    }
}
