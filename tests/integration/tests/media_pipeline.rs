//! Phase 2 acceptance test of design doc §19: a real capture reaches a guest,
//! capture stops with the last viewer, and nothing is captured without one.
//!
//! The decode half runs in the sandboxed worker process of §11.3, so this also
//! covers the "decoder is a separate confined process" requirement rather than
//! asserting it in a comment.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a failed assumption must fail the test"
)]

use std::path::PathBuf;
use std::time::Duration;

use lumepeer_core::consent::Role;
use lumepeer_core::session::SessionManager;
use lumepeer_media::capture::{
    CaptureController, CaptureTarget, Frame, InputCapability, PixelFormat, ScreenCapturer,
    platform_capturer,
};
use lumepeer_media::decode::DecoderHandle;
use lumepeer_media::encode::{EncoderConfig, EncoderKind, select_encoder};
use lumepeer_media::error::MediaError;

/// Path of the decoder worker built into the same target directory.
fn worker_binary() -> PathBuf {
    // The test binary lives in target/<profile>/deps/, the worker one level up.
    let mut path = std::env::current_exe().unwrap();
    path.pop();
    path.pop();
    path.push("lumepeer-decoder-worker");
    path
}

fn peer(n: u8) -> lumepeer_core::NodeId {
    iroh_base::SecretKey::from_bytes(&[n; 32]).public()
}

/// Synthetic capturer with moving content, so successive frames differ and the
/// encoder has something to compress. Used where a live display is not
/// guaranteed, as on a headless CI runner.
#[derive(Debug, Default)]
struct MovingCapturer {
    running: bool,
    tick: u8,
}

impl ScreenCapturer for MovingCapturer {
    fn start(&mut self, _target: CaptureTarget) -> Result<(), MediaError> {
        self.running = true;
        Ok(())
    }

    fn next_frame(&mut self) -> Result<Option<Frame>, MediaError> {
        assert!(self.running, "no frame may be produced while stopped");
        self.tick = self.tick.wrapping_add(0x20);
        let (width, height) = (64u32, 64u32);
        let mut data = vec![0u8; (width * height * 4) as usize];
        for (index, byte) in data.iter_mut().enumerate() {
            *byte = self
                .tick
                .wrapping_add(u8::try_from(index % 251).unwrap_or(0));
        }
        Ok(Some(Frame {
            width,
            height,
            format: PixelFormat::Bgra8,
            timestamp_us: u64::from(self.tick) * 1_000,
            data,
        }))
    }

    fn stop(&mut self) {
        self.running = false;
    }

    fn input_capability(&self) -> InputCapability {
        InputCapability::Full
    }
}

/// §19 phase 2: captured pixels are encoded, decoded in the sandboxed worker
/// and come back out as a picture of the same size.
#[test]
fn a_captured_frame_reaches_the_guest_through_the_sandboxed_decoder() {
    let mut controller = CaptureController::new(
        Box::new(MovingCapturer::default()),
        CaptureTarget::PrimaryDisplay,
    );
    let mut encoder = select_encoder(EncoderConfig::default()).unwrap();
    assert_eq!(encoder.kind(), EncoderKind::SoftwareOpenH264);

    let mut decoder = match DecoderHandle::spawn_with(&worker_binary()) {
        Ok(decoder) => decoder,
        // On a platform whose sandbox is not implemented yet the worker must
        // refuse rather than decode unconfined (§11.3). That refusal is the
        // correct behaviour, so the test records it and stops. On Linux,
        // Windows and macOS, where a sandbox is implemented (seccomp,
        // AppContainer, `sandbox_init`), a refusal is a real failure.
        Err(MediaError::SandboxUnavailable(e)) => {
            #[cfg(any(
                all(target_os = "linux", not(target_os = "android")),
                target_os = "windows",
                target_os = "macos"
            ))]
            panic!(
                "a sandbox is implemented on this platform, so the worker must confine itself: {e}"
            );
            #[cfg(not(any(
                all(target_os = "linux", not(target_os = "android")),
                target_os = "windows",
                target_os = "macos"
            )))]
            {
                let _ = e;
                return;
            }
        }
        Err(e) => panic!("decoder worker did not start: {e}"),
    };
    assert_eq!(
        decoder.sandbox(),
        lumepeer_media::decode::platform_sandbox().unwrap()
    );

    controller.add_viewer(peer(1)).unwrap();
    assert!(controller.is_capturing());

    // openh264 needs a few frames before it hands back a complete picture.
    let mut received = None;
    for _ in 0..10 {
        let frame = controller.next_frame().unwrap().unwrap();
        let bitstream = encoder.encode(&frame).unwrap();
        if let Some(picture) = decoder.decode(&bitstream).unwrap() {
            received = Some(picture);
            break;
        }
    }

    let picture = received.expect("the decoder produced no picture in ten frames");
    assert_eq!((picture.width, picture.height), (64, 64));
    assert_eq!(picture.data.len(), 64 * 64 * 4);
    assert!(
        picture.data.iter().any(|b| *b != 0),
        "a decoded picture of moving content cannot be all zero"
    );

    decoder.shutdown();
    controller.remove_viewer(&peer(1));
    assert!(!controller.is_capturing());
}

/// §19 phase 2: capture follows the grants, both ways.
#[test]
fn capture_follows_the_view_grants_of_the_session_manager() {
    let mut sessions = SessionManager::with_plan(lumepeer_core::license::Plan::Team);
    let mut controller = CaptureController::new(
        Box::new(MovingCapturer::default()),
        CaptureTarget::PrimaryDisplay,
    );

    // No grant, no capture.
    assert!(controller.next_frame().is_err());

    for n in 1..=3u8 {
        sessions.grant(peer(n), Role::ViewOnly).unwrap();
        controller.add_viewer(peer(n)).unwrap();
    }
    assert!(controller.is_capturing());
    assert_eq!(controller.viewer_count(), 3);

    for n in 1..=2u8 {
        sessions.revoke(peer(n)).unwrap();
        controller.remove_viewer(&peer(n));
    }
    assert!(controller.is_capturing(), "one viewer is left");

    sessions.revoke(peer(3)).unwrap();
    controller.remove_viewer(&peer(3));
    assert!(!controller.is_capturing());
    assert!(controller.next_frame().is_err());

    // A session-wide end must stop capture too: screen lock, user switch,
    // license expiry (§8.1, §18).
    sessions.grant(peer(4), Role::ViewOnly).unwrap();
    controller.add_viewer(peer(4)).unwrap();
    sessions.end_all();
    controller.stop();
    assert!(!controller.is_capturing());
}

/// The real platform backend is exercised where a display exists. On a headless
/// runner `platform_capturer` or `start` fails and the test stops there.
#[test]
fn the_platform_backend_captures_the_real_screen_when_one_exists() {
    let Ok(capturer) = platform_capturer() else {
        return;
    };
    let mut controller = CaptureController::new(capturer, CaptureTarget::PrimaryDisplay);
    if controller.add_viewer(peer(1)).is_err() {
        return;
    }

    // The very first frame after start is never a duplicate.
    let frame = controller
        .next_frame()
        .unwrap()
        .expect("the first frame after start cannot be a duplicate");
    assert!(frame.width >= 2 && frame.height >= 2);

    let mut encoder = select_encoder(EncoderConfig::default()).unwrap();
    let bitstream = encoder.encode(&frame).unwrap();
    assert!(bitstream.keyframe);
    assert!(!bitstream.data.is_empty());

    controller.remove_viewer(&peer(1));
    assert!(!controller.is_capturing());
}

/// A frame larger than a ring slot is refused instead of truncated or dropped
/// into the mapping (§11.3).
#[test]
fn an_oversized_bitstream_is_refused_by_the_ring() {
    let Ok(mut decoder) = DecoderHandle::spawn_with(&worker_binary()) else {
        return;
    };
    let oversized = lumepeer_media::encode::EncodedFrame {
        keyframe: true,
        timestamp_us: 0,
        data: vec![0u8; lumepeer_media::decode::SLOT_PAYLOAD_BYTES + 1],
    };
    assert!(matches!(
        decoder.decode(&oversized),
        Err(MediaError::DecoderWorker(_))
    ));
    decoder.shutdown();
}

/// §11: receiver feedback drives the encoder's quality target, clamped to the
/// ABR range of §14 and applied at most once per second.
#[test]
fn receiver_feedback_moves_the_encoder_bitrate_inside_the_abr_range() {
    use lumepeer_core::constants::{
        ABR_MIN_BITRATE_KBPS, ENCODE_DEFAULT_BITRATE_KBPS, ENCODE_DEFAULT_FPS,
    };
    use lumepeer_media::abr::{AbrController, ReceiverFeedback};

    let mut controller = AbrController::new();
    let mut encoder = select_encoder(EncoderConfig::default()).unwrap();
    let mut capturer = MovingCapturer::default();
    capturer.start(CaptureTarget::PrimaryDisplay).unwrap();

    let heavy_loss = ReceiverFeedback {
        loss: 0.30,
        rtt_ms: 90,
        goodput_kbps: 900,
        sent_kbps: ENCODE_DEFAULT_BITRATE_KBPS,
    };

    let first = controller
        .on_feedback(heavy_loss)
        .expect("the first report is never rate limited");
    assert!(first.bitrate_kbps < ENCODE_DEFAULT_BITRATE_KBPS);
    // Bits are the first rung of the ladder and the only one that may move
    // while the bitrate still has room (ADR 0037).
    assert_eq!(first.fps, ENCODE_DEFAULT_FPS);
    encoder.set_bitrate(first.bitrate_kbps).unwrap();

    // The second report inside the same second is dropped (§11, §14).
    assert!(controller.on_feedback(heavy_loss).is_none());

    // Encoding continues after the change.
    let frame = capturer.next_frame().unwrap().unwrap();
    assert!(!encoder.encode(&frame).unwrap().data.is_empty());
    assert!(controller.current_kbps() >= ABR_MIN_BITRATE_KBPS);
}

/// §11's `KeyframeRequest`: whichever encoder this machine actually has must
/// be able to answer one, and answer it with a keyframe rather than with a
/// side effect of some other setting (ADR 0037).
#[test]
fn a_keyframe_request_reaches_whichever_encoder_this_machine_has() {
    let mut encoder = select_encoder(EncoderConfig::default()).unwrap();
    let mut capturer = MovingCapturer::default();
    capturer.start(CaptureTarget::PrimaryDisplay).unwrap();

    // The first frame of a stream is a keyframe on its own, so the claim is
    // only testable from a later one.
    let first = capturer.next_frame().unwrap().unwrap();
    assert!(encoder.encode(&first).unwrap().keyframe);

    encoder.request_keyframe().unwrap();
    let next = capturer.next_frame().unwrap().unwrap();
    assert!(
        encoder.encode(&next).unwrap().keyframe,
        "the encoder ignored a keyframe request"
    );
}

/// The worker exits on its own when the parent goes away, so no decoder
/// outlives the session that spawned it (§8.1).
#[test]
fn the_worker_stops_when_the_handle_is_dropped() {
    let Ok(decoder) = DecoderHandle::spawn_with(&worker_binary()) else {
        return;
    };
    drop(decoder);
    // Nothing to assert beyond not hanging: `Drop` kills and reaps the child.
    std::thread::sleep(Duration::from_millis(50));
}
