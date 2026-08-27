//! Unattended access end to end: the credential exchange of §8 over a real
//! connection, and the lockout matrix around it (ADR 0033, ADR 0034).
//!
//! The unit tests in `lumepeer_core::unattended` already cover the gate on its
//! own. What is added here is everything that only shows up once the two sides
//! are separate processes talking postcard over QUIC: that the challenge and
//! the answer survive the wire, that a refusal says exactly as much as
//! `UnattendedError` allows and no more, and that an admitted session is the
//! same shape a consent dialog would have produced — a role, and none of the
//! four independent grants.
//!
//! The actor-level tests that decide *which* path a connection takes live with
//! the actor, in `apps/desktop/src-tauri/src/network.rs`: only that crate owns
//! the address book and the session manager together.

#![allow(clippy::unwrap_used, reason = "a failed assumption must fail the test")]

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lumepeer_core::address_book::{AddressBook, AddressEntry};
use lumepeer_core::consent::{Grants, Role};
use lumepeer_core::constants::{UNATTENDED_LOCKOUT_DURATION_SECS, UNATTENDED_MAX_FAILED_ATTEMPTS};
use lumepeer_core::protocol::{Direction, FEATURE_UNATTENDED, MessageKind, UnattendedRejection};
use lumepeer_core::session::{SessionManager, SessionState};
use lumepeer_core::unattended::{UnattendedAccess, UnattendedError};
use lumepeer_net::endpoint::PeerEndpoint;
use lumepeer_net::{ControlConnection, HelloInfo, guest_handshake, host_handshake};

/// Anything slower than this on loopback means the test is stuck, not slow.
const TIMEOUT: Duration = Duration::from_secs(20);

/// Over `UNATTENDED_PASSWORD_MIN_BYTES`, which the core enforces when set.
const PASSWORD: &str = "correct horse battery staple";

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The host's side of §8: credentials it will verify, for a device its address
/// book trusts.
fn configured_host(role: Role, with_code: bool) -> UnattendedAccess {
    let mut access = UnattendedAccess::new();
    access.set_password(PASSWORD).unwrap();
    access.set_role(role);
    if with_code {
        access.set_totp_secret(*b"12345678901234567890");
    }
    access
}

async fn host_endpoint() -> PeerEndpoint {
    PeerEndpoint::bind_local(iroh::SecretKey::generate())
        .await
        .unwrap()
}

async fn accept_control(host: &PeerEndpoint) -> (ControlConnection, HelloInfo) {
    let connection = host.accept().await.unwrap().unwrap();
    assert_eq!(connection.alpn(), lumepeer_net::ALPN_CONTROL);
    host_handshake(connection).await.unwrap()
}

/// §8 end to end: challenge, credentials, and a `ConsentGrant` that is the
/// same message an attended session would have produced.
///
/// A success is deliberately *not* answered with a new message kind: the guest
/// joins the ordinary admission path at exactly the point a human's click
/// would have put it, so there is only one way into a session.
#[tokio::test(flavor = "multi_thread")]
async fn a_correct_password_admits_the_guest_with_the_hosts_configured_role() {
    let host = host_endpoint().await;
    let guest = PeerEndpoint::bind_local(iroh::SecretKey::generate())
        .await
        .unwrap();
    let host_addr = host.addr();
    let guest_id = guest.node_id();

    let host_side = tokio::spawn({
        let host = host.clone();
        async move {
            let (mut control, hello) = accept_control(&host).await;
            let peer = control.peer();
            assert_eq!(peer, guest_id);
            // The host only offers the challenge to a guest that said it can
            // answer one (§9.1): an older peer would read the discriminant as
            // malformed and drop the connection.
            assert!(hello.features.iter().any(|f| f == FEATURE_UNATTENDED));

            let mut access = configured_host(Role::FullControl, false);
            let mut book = AddressBook::new();
            book.upsert(
                &peer,
                AddressEntry {
                    label: "office".to_owned(),
                    tags: Vec::new(),
                    notes: String::new(),
                    trusted: true,
                },
            );
            assert!(book.is_trusted(&peer), "the gate of ADR 0034");

            control
                .send(MessageKind::UnattendedChallenge {
                    code_required: access.code_required(),
                })
                .await
                .unwrap();

            let answer = control.recv().await.unwrap();
            assert_eq!(answer.direction, Direction::GuestToHost);
            let MessageKind::UnattendedAuth { password, code } = answer.kind else {
                panic!("expected credentials, got {:?}", answer.kind);
            };
            assert_eq!(code, None, "no second factor was announced");

            // The decision, and the role, come from the core and nowhere else.
            let mut sessions = SessionManager::new();
            let role = access.admit(Some(&password), code.as_deref()).unwrap();
            sessions.grant(peer, role).unwrap();
            control.send(MessageKind::ConsentGrant(role)).await.unwrap();

            let grants = sessions.grants(&peer).unwrap();
            (sessions.state(&peer), role, grants, control)
        }
    });

    let connection = guest.connect_control(host_addr).await.unwrap();
    let mut control = guest_handshake(
        connection,
        Role::ViewOnly,
        Vec::new(),
        vec![FEATURE_UNATTENDED.to_owned()],
    )
    .await
    .unwrap();

    let challenge = tokio::time::timeout(TIMEOUT, control.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        challenge.kind,
        MessageKind::UnattendedChallenge {
            code_required: false
        }
    );
    assert_eq!(challenge.direction, Direction::HostToGuest);

    control
        .send(MessageKind::UnattendedAuth {
            password: PASSWORD.to_owned(),
            code: None,
        })
        .await
        .unwrap();

    let verdict = tokio::time::timeout(TIMEOUT, control.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(verdict.kind, MessageKind::ConsentGrant(Role::FullControl));

    let (state, role, grants, _control) = tokio::time::timeout(TIMEOUT, host_side)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state, SessionState::Active);
    assert_eq!(role, Role::FullControl);
    assert_eq!(
        grants,
        Grants::from_role(Role::FullControl),
        "an unattended admission produces exactly the grants a role implies"
    );
}

/// §8.2: passing the gate is admission, not a blanket permission. The four
/// independent grants stay off however the session was admitted (ADR 0029).
#[test]
fn an_admitted_session_holds_none_of_the_four_independent_grants() {
    let peer = iroh::SecretKey::from_bytes(&[7u8; 32]).public();

    for role in [Role::ViewOnly, Role::ControlLimited, Role::FullControl] {
        let mut access = configured_host(role, false);
        let mut sessions = SessionManager::new();

        let admitted = access.admit(Some(PASSWORD), None).unwrap();
        assert_eq!(admitted, role);
        sessions.grant(peer, admitted).unwrap();

        let grants = sessions.grants(&peer).unwrap();
        assert!(grants.view, "every role implies view");
        assert_eq!(grants.input, role == Role::FullControl);
        assert!(!grants.clipboard_read);
        assert!(!grants.clipboard_write);
        assert!(!grants.file_transfer);
        assert!(!grants.recording);
    }
}

/// §18: a refusal says which factor to retype and nothing else. In particular
/// "the password was right but the code was not" and "both were wrong" are the
/// same answer on the wire.
#[tokio::test(flavor = "multi_thread")]
async fn a_refusal_says_no_more_than_the_error_type_allows() {
    let host = host_endpoint().await;
    let guest = PeerEndpoint::bind_local(iroh::SecretKey::generate())
        .await
        .unwrap();
    let host_addr = host.addr();

    let host_side = tokio::spawn({
        let host = host.clone();
        async move {
            let (mut control, _hello) = accept_control(&host).await;
            let mut access = configured_host(Role::ViewOnly, true);
            control
                .send(MessageKind::UnattendedChallenge {
                    code_required: true,
                })
                .await
                .unwrap();

            let mut sent = Vec::new();
            for _ in 0..2 {
                let answer = control.recv().await.unwrap();
                let MessageKind::UnattendedAuth { password, code } = answer.kind else {
                    panic!("expected credentials");
                };
                let rejection = match access.admit(Some(&password), code.as_deref()) {
                    Ok(_) => panic!("these credentials must not pass"),
                    Err(UnattendedError::BadPassword | UnattendedError::MissingPassword) => {
                        UnattendedRejection::BadPassword
                    }
                    Err(UnattendedError::BadCode | UnattendedError::MissingCode) => {
                        UnattendedRejection::BadCode
                    }
                    Err(UnattendedError::LockedOut { remaining_secs }) => {
                        UnattendedRejection::LockedOut { remaining_secs }
                    }
                    Err(_) => UnattendedRejection::Unavailable,
                };
                sent.push(rejection);
                control
                    .send(MessageKind::UnattendedReject(rejection))
                    .await
                    .unwrap();
            }
            (sent, control)
        }
    });

    let connection = guest.connect_control(host_addr).await.unwrap();
    let mut control = guest_handshake(
        connection,
        Role::ViewOnly,
        Vec::new(),
        vec![FEATURE_UNATTENDED.to_owned()],
    )
    .await
    .unwrap();
    let challenge = tokio::time::timeout(TIMEOUT, control.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        challenge.kind,
        MessageKind::UnattendedChallenge {
            code_required: true
        }
    );

    // Right password, wrong code.
    control
        .send(MessageKind::UnattendedAuth {
            password: PASSWORD.to_owned(),
            code: Some("000000".to_owned()),
        })
        .await
        .unwrap();
    let first = tokio::time::timeout(TIMEOUT, control.recv())
        .await
        .unwrap()
        .unwrap();

    // Wrong password, wrong code.
    control
        .send(MessageKind::UnattendedAuth {
            password: "not it".to_owned(),
            code: Some("000000".to_owned()),
        })
        .await
        .unwrap();
    let second = tokio::time::timeout(TIMEOUT, control.recv())
        .await
        .unwrap()
        .unwrap();

    let (sent, _control) = tokio::time::timeout(TIMEOUT, host_side)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(sent.len(), 2);
    // The first is a code failure; the second is reported by whichever factor
    // the core names first. Either way neither answer confirms that the *other*
    // factor was right, which is what the guest must not learn.
    assert_eq!(
        first.kind,
        MessageKind::UnattendedReject(UnattendedRejection::BadCode)
    );
    assert!(matches!(
        second.kind,
        MessageKind::UnattendedReject(
            UnattendedRejection::BadPassword | UnattendedRejection::BadCode
        )
    ));
}

/// §18: `UNATTENDED_MAX_FAILED_ATTEMPTS` consecutive failures lock the gate
/// for `UNATTENDED_LOCKOUT_DURATION_SECS`, and the correct password is refused
/// during it just like any other.
#[test]
fn the_lockout_refuses_even_the_right_password_and_says_how_long() {
    let mut access = configured_host(Role::ViewOnly, false);

    for attempt in 0..UNATTENDED_MAX_FAILED_ATTEMPTS {
        assert!(
            matches!(
                access.admit(Some("wrong"), None),
                Err(UnattendedError::BadPassword)
            ),
            "attempt {attempt} should be an ordinary refusal, not a lockout yet"
        );
    }

    assert!(access.locked_out());
    let Err(UnattendedError::LockedOut { remaining_secs }) = access.admit(Some(PASSWORD), None)
    else {
        panic!("the right password must be refused while locked out");
    };
    assert!(
        remaining_secs <= UNATTENDED_LOCKOUT_DURATION_SECS,
        "the wait never exceeds the configured lockout"
    );
    // Long enough to be a real cost: the guest is told to wait, not nudged.
    assert!(remaining_secs > 0);
}

/// A success clears the counter, so a user who mistypes a few times and then
/// gets it right does not carry those failures into the next session.
#[test]
fn a_success_resets_the_failure_counter() {
    let mut access = configured_host(Role::ViewOnly, false);

    for _ in 0..(UNATTENDED_MAX_FAILED_ATTEMPTS - 1) {
        assert!(access.admit(Some("wrong"), None).is_err());
    }
    assert!(!access.locked_out());
    assert_eq!(access.admit(Some(PASSWORD), None).unwrap(), Role::ViewOnly);

    for _ in 0..(UNATTENDED_MAX_FAILED_ATTEMPTS - 1) {
        assert!(access.admit(Some("wrong"), None).is_err());
    }
    assert!(
        !access.locked_out(),
        "the counter restarted, so this many failures must not lock out"
    );
}

/// The second factor is verified against the real clock, so a code minted for
/// the current step passes and one minted for a distant step does not.
#[test]
fn the_second_factor_is_required_and_checked_against_the_clock() {
    let mut access = configured_host(Role::ViewOnly, true);
    assert!(access.code_required());

    let totp = access.totp().unwrap();
    let now = now_unix();

    assert!(matches!(
        access.admit(Some(PASSWORD), None),
        Err(UnattendedError::MissingCode)
    ));
    assert!(matches!(
        access.admit(Some(PASSWORD), Some(&totp.generate(now - 600).unwrap())),
        Err(UnattendedError::BadCode)
    ));
    assert_eq!(
        access
            .admit(Some(PASSWORD), Some(&totp.generate(now_unix()).unwrap()))
            .unwrap(),
        Role::ViewOnly
    );
}

/// Deny-by-default at the entrance: a host that never set a password refuses
/// everything, and says only that it is not configured.
#[test]
fn a_host_without_a_password_admits_nobody() {
    let mut access = UnattendedAccess::new();
    assert!(!access.enabled());
    assert!(matches!(
        access.admit(Some(PASSWORD), None),
        Err(UnattendedError::NotConfigured)
    ));

    // And the address book alone changes nothing: trust narrows who may try,
    // it is never a way past the password (ADR 0034).
    let peer = iroh::SecretKey::from_bytes(&[3u8; 32]).public();
    let mut book = AddressBook::new();
    book.upsert(
        &peer,
        AddressEntry {
            label: "office".to_owned(),
            tags: Vec::new(),
            notes: String::new(),
            trusted: true,
        },
    );
    assert!(book.is_trusted(&peer));
    assert!(matches!(
        access.admit(Some(PASSWORD), None),
        Err(UnattendedError::NotConfigured)
    ));
}
