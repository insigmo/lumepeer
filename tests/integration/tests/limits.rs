//! The two limit tests §17.2 demands as their own integration tests: the
//! concurrent-guest ceiling of §8.2 and the consent-queue overflow of §8.1.
//!
//! Both run over real QUIC connections between local endpoints, because the
//! point is that the refusal happens on the host before any `ConsentGrant`
//! reaches the guest, not merely inside `SessionManager`.

#![allow(clippy::unwrap_used, reason = "a failed assumption must fail the test")]

use std::time::Duration;

use lumepeer_core::CoreError;
use lumepeer_core::consent::Role;
use lumepeer_core::constants::MAX_PENDING_CONSENTS;
use lumepeer_core::license::Plan;
use lumepeer_core::protocol::MessageKind;
use lumepeer_core::session::SessionManager;
use lumepeer_net::endpoint::PeerEndpoint;
use lumepeer_net::{ControlConnection, guest_handshake, host_handshake};

const TIMEOUT: Duration = Duration::from_secs(20);

/// One connected guest, from the host's point of view and from its own.
struct Pair {
    host_side: ControlConnection,
    guest_side: ControlConnection,
    /// Kept alive so the guest endpoint is not dropped mid-test.
    _guest: PeerEndpoint,
}

async fn connect_guest(host: &PeerEndpoint, role: Role) -> Pair {
    let guest = PeerEndpoint::bind_local(iroh::SecretKey::generate())
        .await
        .unwrap();
    let addr = host.addr();

    let accepting = tokio::spawn({
        let host = host.clone();
        async move {
            let connection = host.accept().await.unwrap().unwrap();
            host_handshake(connection).await.unwrap()
        }
    });

    let connection = guest.connect_control(addr).await.unwrap();
    let guest_side = guest_handshake(connection, role, Vec::new(), Vec::new())
        .await
        .unwrap();
    let (host_side, _) = tokio::time::timeout(TIMEOUT, accepting)
        .await
        .unwrap()
        .unwrap();

    Pair {
        host_side,
        guest_side,
        _guest: guest,
    }
}

/// §8.2: Trial and Pro admit exactly one guest, whatever its role. The second
/// guest is refused before a `ConsentGrant` is ever sent.
#[tokio::test(flavor = "multi_thread")]
async fn the_second_guest_of_a_one_seat_plan_is_refused_before_any_grant() {
    let host = PeerEndpoint::bind_local(iroh::SecretKey::generate())
        .await
        .unwrap();
    let mut sessions = SessionManager::with_plan(Plan::Pro);

    let mut first = connect_guest(&host, Role::FullControl).await;
    let mut second = connect_guest(&host, Role::ViewOnly).await;

    for pair in [&mut first, &mut second] {
        pair.guest_side
            .send(MessageKind::ConsentRequest)
            .await
            .unwrap();
        pair.host_side.recv().await.unwrap();
    }

    let first_peer = first.host_side.peer();
    let second_peer = second.host_side.peer();
    sessions
        .request_consent_as(first_peer, Role::FullControl)
        .unwrap();
    sessions
        .request_consent_as(second_peer, Role::ViewOnly)
        .unwrap();

    sessions.grant(first_peer, Role::FullControl).unwrap();
    first
        .host_side
        .send(MessageKind::ConsentGrant(Role::FullControl))
        .await
        .unwrap();

    let refused = sessions.grant(second_peer, Role::ViewOnly);
    assert!(matches!(
        refused,
        Err(CoreError::ConcurrentGuestLimit { limit: 1 })
    ));
    assert_eq!(sessions.active_guest_count(), 1);
    // The refused guest is told, and what it is told is a revoke, never a grant.
    second
        .host_side
        .send(MessageKind::ConsentRevoke)
        .await
        .unwrap();

    assert_eq!(
        first.guest_side.recv().await.unwrap().kind,
        MessageKind::ConsentGrant(Role::FullControl)
    );
    assert_eq!(
        second.guest_side.recv().await.unwrap().kind,
        MessageKind::ConsentRevoke
    );

    // Only after a revoke may the seat be handed over.
    sessions.revoke(first_peer).unwrap();
    sessions.grant(second_peer, Role::ViewOnly).unwrap();
    assert_eq!(sessions.active_guest_count(), 1);

    host.close().await;
}

/// §8.2: Team allows five connections in total, the controller included, and at
/// most one of them may control.
#[tokio::test(flavor = "multi_thread")]
async fn team_admits_five_in_total_with_at_most_one_controller() {
    let host = PeerEndpoint::bind_local(iroh::SecretKey::generate())
        .await
        .unwrap();
    let mut sessions = SessionManager::with_plan(Plan::Team);

    let mut pairs = Vec::new();
    for _ in 0..6u8 {
        pairs.push(connect_guest(&host, Role::ViewOnly).await);
    }
    let peers: Vec<_> = pairs.iter().map(|p| p.host_side.peer()).collect();

    sessions.grant(peers[0], Role::FullControl).unwrap();
    for peer in &peers[1..5] {
        sessions.grant(*peer, Role::ViewOnly).unwrap();
    }
    assert_eq!(sessions.active_guest_count(), 5);

    // A second controller inside the ceiling is still refused (§8.2).
    assert!(matches!(
        sessions.grant(peers[4], Role::FullControl),
        Err(CoreError::ControllerAlreadyGranted)
    ));
    // And the sixth connection exceeds the ceiling itself.
    assert!(matches!(
        sessions.grant(peers[5], Role::ViewOnly),
        Err(CoreError::ConcurrentGuestLimit { limit: 5 })
    ));

    pairs[5]
        .host_side
        .send(MessageKind::ConsentRevoke)
        .await
        .unwrap();
    assert_eq!(
        pairs[5].guest_side.recv().await.unwrap().kind,
        MessageKind::ConsentRevoke
    );

    host.close().await;
}

/// §8.1: the `MAX_PENDING_CONSENTS + 1`-th request is refused and nothing older
/// is evicted.
#[tokio::test(flavor = "multi_thread")]
async fn the_consent_queue_refuses_the_newcomer_and_keeps_the_older_requests() {
    let host = PeerEndpoint::bind_local(iroh::SecretKey::generate())
        .await
        .unwrap();
    let mut sessions = SessionManager::new();

    let mut pairs = Vec::new();
    for _ in 0..=MAX_PENDING_CONSENTS {
        pairs.push(connect_guest(&host, Role::ViewOnly).await);
    }

    let mut queued = Vec::new();
    for pair in &mut pairs[..MAX_PENDING_CONSENTS] {
        pair.guest_side
            .send(MessageKind::ConsentRequest)
            .await
            .unwrap();
        pair.host_side.recv().await.unwrap();
        let peer = pair.host_side.peer();
        sessions.request_consent_as(peer, Role::ViewOnly).unwrap();
        queued.push(peer);
    }
    assert_eq!(sessions.pending().len(), MAX_PENDING_CONSENTS);

    let overflow = &mut pairs[MAX_PENDING_CONSENTS];
    overflow
        .guest_side
        .send(MessageKind::ConsentRequest)
        .await
        .unwrap();
    overflow.host_side.recv().await.unwrap();
    let newcomer = overflow.host_side.peer();
    assert!(matches!(
        sessions.request_consent_as(newcomer, Role::ViewOnly),
        Err(CoreError::PendingConsentQueueFull)
    ));

    // Nothing was evicted and the newcomer did not take a slot.
    let still_queued: Vec<_> = sessions.pending().iter().map(|t| t.peer).collect();
    assert_eq!(still_queued, queued);
    assert_eq!(sessions.pending().len(), MAX_PENDING_CONSENTS);

    overflow
        .host_side
        .send(MessageKind::ConsentRevoke)
        .await
        .unwrap();
    assert_eq!(
        overflow.guest_side.recv().await.unwrap().kind,
        MessageKind::ConsentRevoke
    );

    host.close().await;
}
