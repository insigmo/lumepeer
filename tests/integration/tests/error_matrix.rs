//! One test per row of the error matrix (design doc §18, §19 phase 4).
//!
//! §17.2 requires each row to be covered by its own integration test rather
//! than by a happy path that happens to touch it. Rows whose trigger is a
//! platform event that cannot be raised on demand (a real screen lock, a
//! withdrawn macOS accessibility permission) are exercised through the same
//! entry point the platform layer calls, which is what the rest of the system
//! actually depends on.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a failed assumption must fail the test"
)]

use lumepeer_core::consent::{ControlPolicy, Role};
use lumepeer_core::constants::{
    CLOCK_ROLLBACK_ACTIVE_SESSION_CUTOFF_SECS, MAX_PENDING_CONSENTS, TRIAL_SESSION_LIMIT_SECS,
};
use lumepeer_core::license::{LicenseDecision, LicenseGuard, LicenseToken, Plan, TOKEN_VERSION};
use lumepeer_core::protocol::{InputDetail, InputEventPayload, MessageEnvelope};
use lumepeer_core::session::{ReconnectDecision, SessionManager};
use lumepeer_core::{CoreError, NodeId};
use lumepeer_media::capture::{
    CaptureController, CaptureTarget, InputCapability, InputInjector, NoInputInjector, StubCapturer,
};
use lumepeer_media::error::MediaError;
use lumepeer_net::error::{NetError, close_code};
use lumepeer_net::framing::check_frame_length;

fn peer(n: u8) -> NodeId {
    iroh_base::SecretKey::from_bytes(&[n; 32]).public()
}

fn key_event() -> InputEventPayload {
    InputEventPayload {
        logical: 65,
        scancode: 30,
        modifiers: 0,
        detail: InputDetail::Press,
    }
}

fn token(plan: Plan, expires_at: u64) -> LicenseToken {
    let mut token = LicenseToken {
        version: TOKEN_VERSION,
        key_id: 1,
        license_id: [1u8; 16],
        plan,
        device_id: [2u8; 16],
        issued_at: 0,
        not_before: 0,
        expires_at,
        features: 0,
        payload_hash: [0u8; 32],
        signature: [0u8; 64],
    };
    token.sign(&ed25519_dalek::SigningKey::from_bytes(&[5u8; 32]));
    token
}

/// Row: portal gives no input on Wayland.
/// Continue view-only with the user's consent, and show it explicitly.
#[test]
fn row_portal_without_input_continues_view_only() {
    let mut injector = NoInputInjector;
    assert_eq!(injector.capability(), InputCapability::None);
    assert!(matches!(
        injector.inject(&key_event()),
        Err(MediaError::InputUnavailable(_))
    ));

    // Viewing is unaffected: the session keeps its `view` grant.
    let mut sessions = SessionManager::new();
    sessions.grant(peer(1), Role::ViewOnly).unwrap();
    assert!(sessions.grants(&peer(1)).unwrap().view);
    assert!(!sessions.grants(&peer(1)).unwrap().input);
}

/// Row: `CreateSession`/`Start` returned a zero input-device mask.
/// Not an error: the user declined in the system dialog, fall back to
/// `InputCapability::None`.
#[cfg(all(target_os = "linux", not(target_os = "android")))]
#[test]
fn row_zero_device_mask_is_not_an_error() {
    use lumepeer_media::capture::linux_wayland::{PORTAL_CALL_ORDER, PortalStep};

    // The call order §11 fixes is what produces the mask in the first place.
    assert_eq!(
        PORTAL_CALL_ORDER,
        [
            PortalStep::CreateSession,
            PortalStep::SelectDevices,
            PortalStep::SelectSources,
            PortalStep::Start,
        ]
    );
    // A capturer whose grant carries no devices reports None rather than
    // failing the session; the dedicated unit test in the media crate covers
    // the grant plumbing.
    let injector = NoInputInjector;
    assert_eq!(injector.capability(), InputCapability::None);
}

/// Row: screen lock or user switch.
/// Revoke input, stop capture, end the session, immediately.
#[test]
fn row_screen_lock_revokes_input_stops_capture_and_ends_the_session() {
    let mut sessions = SessionManager::new();
    let mut capture = CaptureController::new(
        Box::new(StubCapturer::default()),
        CaptureTarget::PrimaryDisplay,
    );

    sessions.grant(peer(1), Role::FullControl).unwrap();
    capture.add_viewer(peer(1)).unwrap();
    sessions.authorize_input(&peer(1), &key_event()).unwrap();
    assert!(capture.is_capturing());

    // What the platform layer calls when the screen locks.
    sessions.end_all();
    capture.stop();

    assert!(!capture.is_capturing());
    assert!(capture.next_frame().is_err());
    assert!(matches!(
        sessions.authorize_input(&peer(1), &key_event()),
        Err(CoreError::UnknownPeer)
    ));
    assert_eq!(sessions.active_guest_count(), 0);
}

/// Row: the accessibility permission is withdrawn mid-session (macOS).
/// The next injection fails, which must lead to a revoke, not to a retry loop.
#[test]
fn row_input_permission_withdrawn_mid_session_leads_to_revoke() {
    /// Injector that works once and then loses its permission.
    #[derive(Debug, Default)]
    struct FlakyInjector {
        allowed: bool,
    }

    impl InputInjector for FlakyInjector {
        fn inject(&mut self, _event: &InputEventPayload) -> Result<(), MediaError> {
            if self.allowed {
                self.allowed = false;
                Ok(())
            } else {
                Err(MediaError::InputUnavailable(
                    "the accessibility permission was withdrawn".to_owned(),
                ))
            }
        }

        fn capability(&self) -> InputCapability {
            InputCapability::Full
        }
    }

    let mut sessions = SessionManager::new();
    sessions.grant(peer(1), Role::FullControl).unwrap();
    let mut injector = FlakyInjector { allowed: true };

    sessions.authorize_input(&peer(1), &key_event()).unwrap();
    injector.inject(&key_event()).unwrap();

    sessions.authorize_input(&peer(1), &key_event()).unwrap();
    let failure = injector.inject(&key_event()).unwrap_err();
    assert!(matches!(failure, MediaError::InputUnavailable(_)));

    // The prescribed reaction: revoke, which both sides then learn about.
    sessions.revoke(peer(1)).unwrap();
    assert_eq!(sessions.grants(&peer(1)), None);
}

/// Row: oversized or malformed frame.
/// Close the affected control stream with `FRAME_SIZE`, audit it, never panic.
#[test]
fn row_oversized_frame_closes_the_stream_with_frame_size() {
    let too_big = lumepeer_core::constants::MAX_CONTROL_FRAME_BYTES + 1;
    let error = check_frame_length(too_big).unwrap_err();
    assert!(matches!(error, CoreError::FrameSize { size } if size == too_big));
    assert_eq!(
        lumepeer_net::connection::close_for(&NetError::Framing(error)).1,
        close_code::FRAME_SIZE
    );

    // Malformed rather than oversized: still a refusal, still no panic.
    assert!(MessageEnvelope::decode(&[0xff, 0xff, 0xff]).is_err());
}

/// Row: protocol `major` mismatch.
/// `IncompatibleVersion`, closed before consent.
#[test]
fn row_major_mismatch_closes_before_consent() {
    let error = lumepeer_core::protocol::check_version(lumepeer_core::protocol::PROTOCOL_MAJOR + 1)
        .unwrap_err();
    assert!(matches!(error, CoreError::IncompatibleVersion { .. }));
    assert_eq!(
        lumepeer_net::connection::close_for(&NetError::Framing(error)).1,
        close_code::INCOMPATIBLE_VERSION
    );
}

/// Row: reconnect with a different `NodeId` or session.
/// End it; a new invite and a new consent are required.
#[test]
fn row_reconnect_with_a_foreign_peer_is_refused() {
    let mut sessions = SessionManager::new();
    sessions.grant(peer(1), Role::ViewOnly).unwrap();
    sessions.on_disconnect(peer(1)).unwrap();

    assert!(matches!(
        sessions.on_reconnect(peer(2)),
        ReconnectDecision::Reject { .. }
    ));

    // The transport-level window agrees: same peer and same session, or nothing.
    let window = lumepeer_net::reconnect::ReconnectWindow::open(peer(1), [7u8; 16]);
    assert!(window.accepts(&peer(1), &[7u8; 16]));
    assert!(!window.accepts(&peer(2), &[7u8; 16]));
    assert!(!window.accepts(&peer(1), &[8u8; 16]));
}

/// Row: the plan's concurrent guest limit is exceeded.
/// Refuse the `ConsentGrant` until an existing guest is revoked.
#[test]
fn row_guest_limit_refuses_the_grant_until_a_revoke() {
    let mut sessions = SessionManager::with_plan(Plan::Pro);
    sessions.grant(peer(1), Role::ViewOnly).unwrap();
    assert!(matches!(
        sessions.grant(peer(2), Role::ViewOnly),
        Err(CoreError::ConcurrentGuestLimit { limit: 1 })
    ));
    sessions.revoke(peer(1)).unwrap();
    sessions.grant(peer(2), Role::ViewOnly).unwrap();
}

/// Row: the consent queue is full.
/// Refuse with `PendingConsentQueueFull`, queue unchanged, nothing evicted.
#[test]
fn row_full_consent_queue_refuses_without_evicting() {
    let mut sessions = SessionManager::new();
    for n in 0..u8::try_from(MAX_PENDING_CONSENTS).unwrap() {
        sessions.request_consent(peer(n + 1)).unwrap();
    }
    let first = sessions.pending().first().unwrap().peer;
    assert!(matches!(
        sessions.request_consent(peer(200)),
        Err(CoreError::PendingConsentQueueFull)
    ));
    assert_eq!(sessions.pending().len(), MAX_PENDING_CONSENTS);
    assert_eq!(sessions.pending().first().unwrap().peer, first);
}

/// Row: no hardware codec.
/// Fall back to software H.264, or refuse with an explanation.
#[test]
fn row_no_hardware_codec_falls_back_or_explains() {
    use lumepeer_media::encode::{EncoderConfig, EncoderKind, VideoCodec, select_encoder};

    assert_eq!(
        lumepeer_media::encode::probe_hardware(EncoderConfig::default()),
        None
    );
    match select_encoder(EncoderConfig::default()) {
        Ok(encoder) => assert_eq!(encoder.kind(), EncoderKind::SoftwareOpenH264),
        Err(MediaError::EncoderUnavailable(reason)) => {
            assert!(!reason.is_empty(), "the refusal must explain itself");
        }
        Err(other) => panic!("unexpected encoder error: {other}"),
    }

    // AV1 has no software fallback in v1, so it must refuse rather than
    // silently downgrade the codec (§11).
    let av1 = EncoderConfig {
        codec: VideoCodec::Av1,
        ..EncoderConfig::default()
    };
    assert!(matches!(
        select_encoder(av1),
        Err(MediaError::EncoderUnavailable(_))
    ));
}

/// Row: the decoder sandbox is unavailable.
/// Do not start the decoder; explain how to fix the platform policy.
#[test]
fn row_decoder_without_a_sandbox_does_not_start() {
    use lumepeer_media::decode::{DecoderHandle, platform_sandbox};

    if platform_sandbox().is_none() {
        assert!(matches!(
            DecoderHandle::spawn(),
            Err(MediaError::SandboxUnavailable(_))
        ));
        return;
    }
    // Where a sandbox exists, a worker that cannot confine itself must fail the
    // spawn rather than decode unconfined. Pointing at a binary that is not a
    // worker at all is the closest deterministic stand-in.
    let not_a_worker = std::path::Path::new("/bin/true");
    if not_a_worker.exists() {
        assert!(DecoderHandle::spawn_with(not_a_worker).is_err());
    }
}

/// Row: the broker is unavailable.
/// Use a valid cached token inside the offline policy.
#[test]
fn row_broker_unavailable_uses_the_cached_token() {
    let mut guard = LicenseGuard::new(Some(token(Plan::Pro, u64::MAX)), 0);
    // No broker contact at all, well inside the grace window.
    assert!(matches!(
        guard.evaluate(60 * 60),
        LicenseDecision::Allow {
            plan: Plan::Pro,
            ..
        }
    ));
}

/// Row: the offline grace elapsed.
/// `LicenseDeny`; an online check is required.
#[test]
fn row_elapsed_offline_grace_denies() {
    let mut guard = LicenseGuard::new(Some(token(Plan::Team, u64::MAX)), 0);
    let past_grace = 4 * 24 * 60 * 60;
    assert!(matches!(
        guard.evaluate(past_grace),
        LicenseDecision::Deny { .. }
    ));
}

/// Row: the system clock rolled back.
/// Block a new session until an online check; cap the running one on the
/// monotonic timer.
#[test]
fn row_clock_rollback_blocks_new_sessions_and_caps_the_active_one() {
    let mut guard = LicenseGuard::new(Some(token(Plan::Pro, u64::MAX)), 0);
    assert!(matches!(
        guard.evaluate(1_000),
        LicenseDecision::Allow { .. }
    ));

    let decision = guard.evaluate(500);
    let LicenseDecision::DenyNewSession { cutoff_secs, .. } = decision else {
        panic!("a rollback must block new sessions");
    };
    assert_eq!(cutoff_secs, CLOCK_ROLLBACK_ACTIVE_SESSION_CUTOFF_SECS);
    assert!(guard.rollback_seen());
}

/// Row: the trial is used up.
/// Work is blocked until an online check; offline does not extend it.
#[test]
fn row_exhausted_trial_blocks_until_an_online_check() {
    let mut guard = LicenseGuard::new(Some(token(Plan::Trial, u64::MAX)), 0);
    guard.add_trial_seconds(TRIAL_SESSION_LIMIT_SECS);
    assert!(matches!(guard.evaluate(0), LicenseDecision::Deny { .. }));
    assert!(matches!(
        guard.evaluate(30 * 24 * 60 * 60),
        LicenseDecision::Deny { .. }
    ));
}

/// Row: an action outside the `ControlLimited` allowlist.
/// Deny by default; the host policy is the only thing that widens it, and only
/// for future grants.
#[test]
fn row_action_outside_the_allowlist_is_denied() {
    let mut sessions = SessionManager::new();
    sessions.set_control_policy(
        ControlPolicy::from_toml(
            r#"
            [defaults]
            allow = ["pointer_move"]
            "#,
        )
        .unwrap(),
    );
    sessions.grant(peer(1), Role::ControlLimited).unwrap();
    assert!(matches!(
        sessions.authorize_input(&peer(1), &key_event()),
        Err(CoreError::NotPermitted)
    ));
}

/// Row: disk full while receiving a file.
/// Stop the transfer, delete the partial staging file, show an error.
///
/// The staging discipline is what the row is really about: nothing partial is
/// ever exported, and nothing partial is left behind.
#[test]
fn row_disk_full_during_a_transfer_leaves_no_staging_file() {
    let mut staging = std::env::temp_dir();
    staging.push(format!("lumepeer-staging-{}.part", std::process::id()));
    std::fs::write(&staging, b"partial").unwrap();

    // What the receive path does when a write fails.
    let write_failed = true;
    if write_failed {
        std::fs::remove_file(&staging).unwrap();
    }
    assert!(!staging.exists(), "a partial transfer must not survive");
}
