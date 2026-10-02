//! The encoders under test, behind one trait.

use lumepeer_media::capture::{Frame, PixelFormat};
use lumepeer_media::encode::{EncoderConfig, VideoCodec, VideoEncoder};
use openh264::formats::{YUVBuffer, YUVSource};

/// What one encoder is handed per frame.
pub enum Input<'a> {
    /// The captured BGRA picture itself, for encoders that convert it the way
    /// the product does (openh264 through lumepeer's own wrapper).
    Bgra(&'a mut Vec<u8>),
    /// The picture after the shared BGRA->I420 conversion.
    I420(&'a YUVBuffer),
}

/// One encoded picture: which input frame it belongs to, and its bytes.
pub struct Packet {
    pub frame: u64,
    pub data: Vec<u8>,
    pub key: bool,
}

/// What one `encode` call returned: usually the packet of the frame just
/// sent, but an encoder with lookahead returns an earlier frame's (or none).
pub type Output = Vec<Packet>;

pub trait Bench {
    /// Whether this encoder takes the harness's I420 rather than BGRA.
    fn wants_i420(&self) -> bool;
    fn codec(&self) -> Codec;
    fn encode(&mut self, input: Input<'_>, pts: i64) -> Result<Output, String>;
    /// Packets still inside the encoder once the input has ended.
    fn flush(&mut self) -> Result<Output, String> {
        Ok(Vec::new())
    }
    fn describe(&self) -> String;
}

fn one(pts: i64, data: Vec<u8>, key: bool) -> Output {
    vec![Packet { frame: pts as u64, data, key }]
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    Av1,
}

pub struct Params {
    pub width: usize,
    pub height: usize,
    pub fps: u32,
    pub kbps: u32,
    pub speed: i32,
    pub screen: bool,
    pub threads: i32,
    pub tiles: i32,
    pub min_q: i32,
    pub max_q: i32,
    pub extra: String,
}

pub fn build(name: &str, p: &Params) -> Result<Box<dyn Bench>, String> {
    match name {
        "openh264" => OpenH264::new(p).map(|e| Box::new(e) as Box<dyn Bench>),
        "openh264-noskip" => OpenH264Variant::new(p, false).map(|e| Box::new(e) as Box<dyn Bench>),
        "openh264-cbr" => OpenH264Variant::new(p, true).map(|e| Box::new(e) as Box<dyn Bench>),
        #[cfg(feature = "mf")]
        "mf-h264" => Mf::new(p, VideoCodec::H264).map(|e| Box::new(e) as Box<dyn Bench>),
        #[cfg(feature = "mf")]
        "mf-av1" => Mf::new(p, VideoCodec::Av1).map(|e| Box::new(e) as Box<dyn Bench>),
        #[cfg(feature = "aom")]
        "aom" => aom::Aom::new(p).map(|e| Box::new(e) as Box<dyn Bench>),
        #[cfg(feature = "svt")]
        "svt" => svt::Svt::new(p).map(|e| Box::new(e) as Box<dyn Bench>),
        #[cfg(feature = "rav1e")]
        "rav1e" => r1e::Rav1e::new(p).map(|e| Box::new(e) as Box<dyn Bench>),
        other => Err(format!("unknown or not built-in encoder: {other}")),
    }
}

fn lp_config(p: &Params, codec: VideoCodec) -> EncoderConfig {
    EncoderConfig {
        fps: u8::try_from(p.fps).unwrap_or(u8::MAX),
        bitrate_kbps: p.kbps,
        codec,
    }
}

/// lumepeer's own openh264 fallback, product code path: BGRA in, its own
/// conversion and its own settings (`encode::software::OpenH264Encoder`).
struct OpenH264 {
    inner: lumepeer_media::encode::software::OpenH264Encoder,
    w: u32,
    h: u32,
}

impl OpenH264 {
    fn new(p: &Params) -> Result<Self, String> {
        let inner = lumepeer_media::encode::software::OpenH264Encoder::new(lp_config(p, VideoCodec::H264))
            .map_err(|e| e.to_string())?;
        Ok(Self { inner, w: p.width as u32, h: p.height as u32 })
    }
}

impl Bench for OpenH264 {
    fn wants_i420(&self) -> bool {
        false
    }
    fn codec(&self) -> Codec {
        Codec::H264
    }
    fn encode(&mut self, input: Input<'_>, pts: i64) -> Result<Output, String> {
        let Input::Bgra(buf) = input else { return Err("openh264 takes BGRA".into()) };
        let frame = Frame::cpu(self.w, self.h, PixelFormat::Bgra8, pts as u64, std::mem::take(buf));
        let result = self.inner.encode(&frame);
        *buf = frame.data;
        let out = result.map_err(|e| e.to_string())?;
        Ok(one(pts, out.data, out.keyframe))
    }
    fn describe(&self) -> String {
        format!("lumepeer OpenH264Encoder (openh264 crate 0.9.8, nasm at build: {})", env!("CB_OPENH264_ASM"))
    }
}

/// lumepeer's openh264 settings with the rate control changed: what the
/// fallback could do without changing codec. Built here rather than through
/// lumepeer's wrapper because that wrapper has no knob for these two; every
/// other setting copies `encode::software::OpenH264Encoder::build`.
///
/// `noskip`: frame skipping off (the crate default, which lumepeer keeps, is
/// on). `cbr`: also `RateControlMode::Bitrate` instead of the crate default
/// `Quality`.
struct OpenH264Variant {
    inner: openh264::encoder::Encoder,
    cbr: bool,
}

impl OpenH264Variant {
    fn new(p: &Params, cbr: bool) -> Result<Self, String> {
        use openh264::encoder::{BitRate, Complexity, EncoderConfig as H264Config, FrameRate, RateControlMode, UsageType};
        let threads = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get).clamp(1, 4) as u16;
        let mut config = H264Config::new()
            .bitrate(BitRate::from_bps(p.kbps * 1000))
            .max_frame_rate(FrameRate::from_hz(p.fps as f32))
            .usage_type(UsageType::ScreenContentRealTime)
            .complexity(Complexity::Low)
            .num_threads(threads)
            .skip_frames(false);
        if cbr {
            config = config.rate_control_mode(RateControlMode::Bitrate);
        }
        let inner = openh264::encoder::Encoder::with_api_config(openh264::OpenH264API::from_source(), config)
            .map_err(|e| e.to_string())?;
        Ok(Self { inner, cbr })
    }
}

impl Bench for OpenH264Variant {
    fn wants_i420(&self) -> bool {
        true
    }
    fn codec(&self) -> Codec {
        Codec::H264
    }
    fn encode(&mut self, input: Input<'_>, pts: i64) -> Result<Output, String> {
        let Input::I420(yuv) = input else { return Err("openh264 variant takes I420".into()) };
        let out = self.inner.encode(yuv).map_err(|e| e.to_string())?;
        let key = matches!(out.frame_type(), openh264::encoder::FrameType::IDR | openh264::encoder::FrameType::I);
        Ok(one(pts, out.to_vec(), key))
    }
    fn describe(&self) -> String {
        format!(
            "openh264 0.9.8, lumepeer settings but skip_frames(false){} (nasm at build: {})",
            if self.cbr { " + RateControlMode::Bitrate" } else { "" },
            env!("CB_OPENH264_ASM")
        )
    }
}

/// lumepeer's Media Foundation hardware encoder, fed NV12 built from the
/// shared I420 so it sees exactly the pixels every other encoder does.
#[cfg(feature = "mf")]
struct Mf {
    inner: Box<dyn VideoEncoder>,
    codec: VideoCodec,
    w: u32,
    h: u32,
    nv12: Vec<u8>,
}

#[cfg(feature = "mf")]
impl Mf {
    fn new(p: &Params, codec: VideoCodec) -> Result<Self, String> {
        let config = lp_config(p, codec);
        let inner = lumepeer_media::encode::windows::MediaFoundationEncoder::new(config)
            .map(|e| Box::new(e) as Box<dyn VideoEncoder>)
            .map_err(|e| e.to_string())?;
        Ok(Self { inner, codec, w: p.width as u32, h: p.height as u32, nv12: Vec::new() })
    }
}

#[cfg(feature = "mf")]
impl Bench for Mf {
    fn wants_i420(&self) -> bool {
        true
    }
    fn codec(&self) -> Codec {
        if self.codec == VideoCodec::Av1 { Codec::Av1 } else { Codec::H264 }
    }
    fn encode(&mut self, input: Input<'_>, pts: i64) -> Result<Output, String> {
        let Input::I420(yuv) = input else { return Err("mf takes I420".into()) };
        let (w, h) = (self.w as usize, self.h as usize);
        let (ys, us, vs) = yuv.strides();
        let mut nv12 = std::mem::take(&mut self.nv12);
        nv12.clear();
        nv12.reserve(w * h * 3 / 2);
        for row in 0..h {
            nv12.extend_from_slice(&yuv.y()[row * ys..row * ys + w]);
        }
        for row in 0..h / 2 {
            let (u, v) = (&yuv.u()[row * us..row * us + w / 2], &yuv.v()[row * vs..row * vs + w / 2]);
            for x in 0..w / 2 {
                nv12.push(u[x]);
                nv12.push(v[x]);
            }
        }
        let frame = Frame::cpu(self.w, self.h, PixelFormat::Nv12, pts as u64, nv12);
        let result = self.inner.encode(&frame);
        self.nv12 = frame.data;
        let out = result.map_err(|e| e.to_string())?;
        Ok(one(pts, out.data, out.keyframe))
    }
    fn describe(&self) -> String {
        format!("lumepeer MediaFoundationEncoder {:?}", self.codec)
    }
}

fn planes(yuv: &YUVBuffer) -> (&[u8], &[u8], &[u8], usize, usize) {
    let (ys, us, _vs) = yuv.strides();
    (yuv.y(), yuv.u(), yuv.v(), ys, us)
}

#[cfg(feature = "aom")]
mod aom {
    use super::{one, planes, Bench, Codec, Input, Output, Params};
    use std::ffi::{c_char, c_int, CStr};

    #[repr(C)]
    struct Shim {
        _private: [u8; 0],
    }
    unsafe extern "C" {
        fn aom_shim_open(
            w: c_int, h: c_int, fps: c_int, kbps: c_int, speed: c_int, screen: c_int, threads: c_int,
            tile_cols_log2: c_int, min_q: c_int, max_q: c_int, err: *mut c_char, errlen: c_int,
        ) -> *mut Shim;
        fn aom_shim_encode(
            s: *mut Shim, y: *const u8, u: *const u8, v: *const u8, ys: c_int, uvs: c_int, pts: i64,
            force_kf: c_int, out: *mut *const u8, out_len: *mut usize, is_key: *mut c_int,
        ) -> c_int;
        fn aom_shim_close(s: *mut Shim);
        fn aom_shim_version() -> *const c_char;
    }

    pub struct Aom {
        shim: *mut Shim,
        desc: String,
    }

    impl Aom {
        pub fn new(p: &Params) -> Result<Self, String> {
            let threads = if p.threads > 0 { p.threads } else { 4 };
            let tiles = if p.tiles >= 0 { p.tiles } else { 1 };
            let mut err = [0 as c_char; 256];
            // SAFETY: plain values in, a NUL-terminated buffer of the size we pass.
            let shim = unsafe {
                aom_shim_open(
                    p.width as c_int, p.height as c_int, p.fps as c_int, p.kbps as c_int, p.speed,
                    c_int::from(p.screen), threads, tiles, p.min_q, p.max_q, err.as_mut_ptr(), 256,
                )
            };
            if shim.is_null() {
                // SAFETY: the shim always NUL-terminates `err` (snprintf).
                return Err(unsafe { CStr::from_ptr(err.as_ptr()) }.to_string_lossy().into_owned());
            }
            // SAFETY: a static string owned by libaom.
            let version = unsafe { CStr::from_ptr(aom_shim_version()) }.to_string_lossy().into_owned();
            Ok(Self {
                shim,
                desc: format!(
                    "libaom {version} realtime cpu-used={} threads={threads} tile-cols-log2={tiles} screen={} q={}..{}",
                    p.speed, p.screen, p.min_q, p.max_q
                ),
            })
        }
    }

    impl Bench for Aom {
        fn wants_i420(&self) -> bool {
            true
        }
        fn codec(&self) -> Codec {
            Codec::Av1
        }
        fn encode(&mut self, input: Input<'_>, pts: i64) -> Result<Output, String> {
            let Input::I420(yuv) = input else { return Err("aom takes I420".into()) };
            let (y, u, v, ys, uvs) = planes(yuv);
            let (mut out, mut len, mut key) = (std::ptr::null(), 0usize, 0 as c_int);
            // SAFETY: the planes outlive the call; the shim writes the three out-pointers.
            let rc = unsafe {
                aom_shim_encode(
                    self.shim, y.as_ptr(), u.as_ptr(), v.as_ptr(), ys as c_int, uvs as c_int, pts, 0,
                    &mut out, &mut len, &mut key,
                )
            };
            if rc != 0 {
                return Err("aom encode failed".into());
            }
            // SAFETY: `out` points at `len` bytes owned by the shim until the next call.
            let data = if len == 0 { Vec::new() } else { unsafe { std::slice::from_raw_parts(out, len) }.to_vec() };
            Ok(one(pts, data, key != 0))
        }
        fn describe(&self) -> String {
            self.desc.clone()
        }
    }

    impl Drop for Aom {
        fn drop(&mut self) {
            // SAFETY: opened by aom_shim_open, closed once.
            unsafe { aom_shim_close(self.shim) };
        }
    }
}

#[cfg(feature = "svt")]
mod svt {
    use super::{one, planes, Bench, Codec, Input, Output, Params};
    use std::ffi::{c_char, c_int, CStr, CString};

    #[repr(C)]
    struct Shim {
        _private: [u8; 0],
    }
    unsafe extern "C" {
        fn svt_shim_open(
            w: c_int, h: c_int, fps: c_int, kbps: c_int, preset: c_int, screen: c_int, lp: c_int,
            min_q: c_int, max_q: c_int, extra: *const c_char, err: *mut c_char, errlen: c_int,
        ) -> *mut Shim;
        fn svt_shim_encode(
            s: *mut Shim, y: *const u8, u: *const u8, v: *const u8, ys: c_int, uvs: c_int, pts: i64,
            force_kf: c_int, out: *mut *const u8, out_len: *mut usize, is_key: *mut c_int,
        ) -> c_int;
        fn svt_shim_close(s: *mut Shim);
        fn svt_shim_version() -> *const c_char;
    }

    pub struct Svt {
        shim: *mut Shim,
        desc: String,
    }

    impl Svt {
        pub fn new(p: &Params) -> Result<Self, String> {
            let lp = p.threads.max(0);
            let extra = CString::new(p.extra.clone()).map_err(|e| e.to_string())?;
            let mut err = [0 as c_char; 256];
            // SAFETY: as for aom.
            let shim = unsafe {
                svt_shim_open(
                    p.width as c_int, p.height as c_int, p.fps as c_int, p.kbps as c_int, p.speed,
                    c_int::from(p.screen), lp, p.min_q, p.max_q, extra.as_ptr(), err.as_mut_ptr(), 256,
                )
            };
            if shim.is_null() {
                // SAFETY: the shim always NUL-terminates `err`.
                return Err(unsafe { CStr::from_ptr(err.as_ptr()) }.to_string_lossy().into_owned());
            }
            // SAFETY: a static string owned by SVT-AV1.
            let version = unsafe { CStr::from_ptr(svt_shim_version()) }.to_string_lossy().into_owned();
            Ok(Self {
                shim,
                desc: format!(
                    "SVT-AV1 {version} rtc preset={} lp={lp} screen={} q={}..{} extra='{}'",
                    p.speed, p.screen, p.min_q, p.max_q, p.extra
                ),
            })
        }
    }

    impl Bench for Svt {
        fn wants_i420(&self) -> bool {
            true
        }
        fn codec(&self) -> Codec {
            Codec::Av1
        }
        fn encode(&mut self, input: Input<'_>, pts: i64) -> Result<Output, String> {
            let Input::I420(yuv) = input else { return Err("svt takes I420".into()) };
            let (y, u, v, ys, uvs) = planes(yuv);
            let (mut out, mut len, mut key) = (std::ptr::null(), 0usize, 0 as c_int);
            // SAFETY: as for aom.
            let rc = unsafe {
                svt_shim_encode(
                    self.shim, y.as_ptr(), u.as_ptr(), v.as_ptr(), ys as c_int, uvs as c_int, pts, 0,
                    &mut out, &mut len, &mut key,
                )
            };
            if rc != 0 {
                return Err("svt encode failed".into());
            }
            // SAFETY: as for aom.
            let data = if len == 0 { Vec::new() } else { unsafe { std::slice::from_raw_parts(out, len) }.to_vec() };
            Ok(one(pts, data, key != 0))
        }
        fn describe(&self) -> String {
            self.desc.clone()
        }
    }

    impl Drop for Svt {
        fn drop(&mut self) {
            // SAFETY: opened by svt_shim_open, closed once.
            unsafe { svt_shim_close(self.shim) };
        }
    }
}

#[cfg(feature = "rav1e")]
mod r1e {
    use super::{Bench, Codec, Input, Output, Packet, Params};
    use openh264::formats::YUVSource;
    use rav1e::prelude::*;

    pub struct Rav1e {
        ctx: Context<u8>,
        desc: String,
    }

    impl Rav1e {
        /// Every packet rav1e has ready now. With its minimum lookahead the
        /// packet of frame N may only come out after frame N+1 went in; the
        /// packet carries its own frame number, so the record stays honest.
        fn drain(&mut self) -> Result<Output, String> {
            let mut out = Vec::new();
            loop {
                match self.ctx.receive_packet() {
                    Ok(pkt) => out.push(Packet {
                        frame: pkt.input_frameno,
                        key: pkt.frame_type == FrameType::KEY,
                        data: pkt.data,
                    }),
                    Err(EncoderStatus::Encoded) => continue,
                    Err(EncoderStatus::NeedMoreData | EncoderStatus::LimitReached) => return Ok(out),
                    Err(e) => return Err(format!("{e:?}")),
                }
            }
        }
    }

    impl Rav1e {
        pub fn new(p: &Params) -> Result<Self, String> {
            let speed = p.speed.clamp(0, 10) as u8;
            let mut speed_settings = SpeedSettings::from_preset(speed);
            speed_settings.rdo_lookahead_frames = 1;
            let enc = EncoderConfig {
                width: p.width,
                height: p.height,
                time_base: Rational::new(1, u64::from(p.fps)),
                bit_depth: 8,
                chroma_sampling: ChromaSampling::Cs420,
                low_latency: true,
                bitrate: (p.kbps * 1000) as i32,
                min_key_frame_interval: 0,
                max_key_frame_interval: 100_000,
                tiles: if p.tiles > 0 { p.tiles as usize } else { 0 },
                speed_settings,
                ..EncoderConfig::with_speed_preset(speed)
            };
            let threads = if p.threads > 0 { p.threads as usize } else { 4 };
            let ctx = Config::new()
                .with_encoder_config(enc)
                .with_threads(threads)
                .new_context()
                .map_err(|e| e.to_string())?;
            Ok(Self {
                ctx,
                desc: format!("rav1e 0.8.1 speed={speed} low_latency rdo_lookahead=1 threads={threads}"),
            })
        }
    }

    impl Bench for Rav1e {
        fn wants_i420(&self) -> bool {
            true
        }
        fn codec(&self) -> Codec {
            Codec::Av1
        }
        fn encode(&mut self, input: Input<'_>, _pts: i64) -> Result<Output, String> {
            let Input::I420(yuv) = input else { return Err("rav1e takes I420".into()) };
            let (ys, us, vs) = yuv.strides();
            let mut frame = self.ctx.new_frame();
            frame.planes[0].copy_from_raw_u8(yuv.y(), ys, 1);
            frame.planes[1].copy_from_raw_u8(yuv.u(), us, 1);
            frame.planes[2].copy_from_raw_u8(yuv.v(), vs, 1);
            self.ctx.send_frame(frame).map_err(|e| format!("{e:?}"))?;
            self.drain()
        }
        fn flush(&mut self) -> Result<Output, String> {
            self.ctx.flush();
            let mut all = Vec::new();
            loop {
                let got = self.drain()?;
                if got.is_empty() {
                    return Ok(all);
                }
                all.extend(got);
            }
        }
        fn describe(&self) -> String {
            self.desc.clone()
        }
    }
}
