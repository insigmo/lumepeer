//! Phase 5 resource gate (design doc §15, §16.2, §19).
//!
//! `ci/resource-budget.yml` names the reference hardware and the thresholds;
//! this test is the part of the gate this repo can drive without the actual
//! self-hosted runner of §16.2, which does not exist yet. It samples the
//! sandboxed decoder worker's own RSS against the `active_extra_rss_mib` gate
//! of §15: that worker is one component of the "active session, hardware
//! encode" budget, so if it alone blows the whole-app budget the gate must
//! fail, even though a full accounting also needs the host process and a real
//! capture/encode loop that this CI environment cannot produce.
//!
//! Opt-in via `LUMEPEER_TEST_RESOURCE_BUDGET` so a hosted runner with
//! unrelated CPU/RAM pressure never flakes this on a PR: the resource-budget
//! CI job is the only caller (see the `resource-budget` job in
//! `.github/workflows/ci.yml`).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a failed assumption must fail the test"
)]

#[cfg(target_os = "linux")]
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::time::Duration;

#[cfg(target_os = "linux")]
use lumepeer_media::capture::{CaptureTarget, Frame, InputCapability, PixelFormat, ScreenCapturer};
#[cfg(target_os = "linux")]
use lumepeer_media::decode::DecoderHandle;
#[cfg(target_os = "linux")]
use lumepeer_media::encode::{EncoderConfig, select_encoder};
#[cfg(target_os = "linux")]
use lumepeer_media::error::MediaError;

/// Path of the decoder worker built into the same target directory, same
/// approach as `media_pipeline.rs`.
#[cfg(target_os = "linux")]
fn worker_binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop();
    path.pop();
    path.push("lumepeer-decoder-worker");
    path
}

/// Reads the `gate_p95` value under the given top-level budget key straight
/// out of `ci/resource-budget.yml`, so the number in the test always tracks
/// the design doc's single point of truth instead of drifting from a copy.
/// Hand-rolled rather than a YAML crate: the file is a flat, hand-written
/// table and a new dependency for one number is not worth an ADR.
#[cfg(target_os = "linux")]
fn gate_p95_mib(budget_key: &str) -> f64 {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest_dir
        .parent()
        .and_then(std::path::Path::parent)
        .expect("tests/integration sits two levels under the repo root");
    let yaml = std::fs::read_to_string(repo_root.join("ci/resource-budget.yml"))
        .expect("ci/resource-budget.yml must exist (§15, §16.2)");

    let mut lines = yaml.lines();
    let in_key = lines.by_ref().skip_while(|line| {
        line.trim_start()
            .strip_prefix(budget_key)
            .and_then(|rest| rest.trim_start().strip_prefix(':'))
            .is_none()
    });
    for line in in_key.skip(1) {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("gate_p95:") {
            let number = rest.split('#').next().unwrap().trim();
            return number.parse().expect("gate_p95 must be a plain number");
        }
        // Left the key's own indented block without finding it.
        if !line.starts_with(' ') || trimmed.ends_with(':') {
            break;
        }
    }
    panic!("no gate_p95 found under {budget_key} in ci/resource-budget.yml");
}

/// Peak `VmRSS` of a process in MiB, sampled a few times over a short window
/// since decoder RSS settles after the first few keyframes.
#[cfg(target_os = "linux")]
fn peak_rss_mib(pid: u32) -> f64 {
    let mut peak = 0.0f64;
    for _ in 0..10 {
        if let Some(sample) = read_vmrss_mib(pid) {
            peak = peak.max(sample);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    peak
}

#[cfg(target_os = "linux")]
fn read_vmrss_mib(pid: u32) -> Option<f64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kib: f64 = rest.trim().trim_end_matches("kB").trim().parse().ok()?;
            return Some(kib / 1024.0);
        }
    }
    None
}

/// Synthetic capturer, same shape as `media_pipeline.rs`: it exists so this
/// test does not depend on a live display being present on the runner.
#[cfg(target_os = "linux")]
#[derive(Debug, Default)]
struct MovingCapturer {
    tick: u8,
}

#[cfg(target_os = "linux")]
impl ScreenCapturer for MovingCapturer {
    fn start(&mut self, _target: CaptureTarget) -> Result<(), MediaError> {
        Ok(())
    }

    fn next_frame(&mut self) -> Result<Option<Frame>, MediaError> {
        self.tick = self.tick.wrapping_add(0x20);
        let (width, height) = (256u32, 256u32);
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

    fn stop(&mut self) {}

    fn input_capability(&self) -> InputCapability {
        InputCapability::Full
    }
}

/// §15/§19 phase 5: the sandboxed decoder worker, decoding a real session's
/// worth of frames, must not by itself exceed the active-session extra RSS
/// gate.
#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "VmRSS sampling is implemented for Linux only, matching ADR 0007's scope"
)]
fn the_decoder_worker_stays_within_the_active_extra_rss_gate() {
    #[cfg(target_os = "linux")]
    {
        if std::env::var_os("LUMEPEER_TEST_RESOURCE_BUDGET").is_none() {
            eprintln!(
                "skipping: set LUMEPEER_TEST_RESOURCE_BUDGET=1 (the resource-budget CI job does)"
            );
            return;
        }

        let mut decoder = match DecoderHandle::spawn_with(&worker_binary()) {
            Ok(decoder) => decoder,
            Err(MediaError::SandboxUnavailable(e)) => {
                panic!("seccomp is implemented on Linux, so the worker must confine itself: {e}")
            }
            Err(e) => panic!("decoder worker did not start: {e}"),
        };

        let mut encoder = select_encoder(EncoderConfig::default()).unwrap();
        let mut capturer = MovingCapturer::default();
        capturer.start(CaptureTarget::PrimaryDisplay).unwrap();

        for _ in 0..60 {
            let frame = capturer.next_frame().unwrap().unwrap();
            let bitstream = encoder.encode(&frame).unwrap();
            let _ = decoder.decode(&bitstream).unwrap();
        }

        let rss = peak_rss_mib(decoder.pid());
        let gate = gate_p95_mib("active_extra_rss_mib");
        assert!(
            rss <= gate,
            "decoder worker alone used {rss:.1} MiB RSS, over the {gate:.1} MiB \
             active_extra_rss_mib gate_p95 of §15; that gate covers the whole \
             active session, so this component must stay well under it"
        );

        decoder.shutdown();
    }
}
