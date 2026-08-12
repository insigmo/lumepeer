//! Phase 1 acceptance test of design doc §19: two local instances establish a
//! P2P connection and run the full `Hello`/`HelloAck` -> `ConsentRequest` ->
//! `ConsentGrant` -> `ConsentRevoke` cycle.
//!
//! The endpoints bind with the `Minimal` preset, so no relay and no address
//! lookup service is involved: the guest dials the host's direct addresses.

#![allow(clippy::unwrap_used, reason = "a failed assumption must fail the test")]

use std::time::Duration;

use lumepeer_core::consent::Role;
use lumepeer_core::protocol::{Direction, MessageKind};
use lumepeer_core::session::{SessionManager, SessionState};
use lumepeer_net::endpoint::PeerEndpoint;
use lumepeer_net::{ControlConnection, HelloInfo, guest_handshake, host_handshake};

/// Anything slower than this on loopback means the test is stuck, not slow.
const TIMEOUT: Duration = Duration::from_secs(20);

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

#[tokio::test(flavor = "multi_thread")]
async fn full_consent_cycle_between_two_local_instances() {
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
            let mut sessions = SessionManager::new();

            // Guest asks. Only the host's core may answer (§2.3).
            let request = control.recv().await.unwrap();
            assert_eq!(request.kind, MessageKind::ConsentRequest);
            assert_eq!(request.direction, Direction::GuestToHost);
            let ticket = sessions
                .request_consent_as(peer, hello.role_request)
                .unwrap();
            assert_eq!(ticket.queue_position, 0);
            assert_eq!(sessions.pending().len(), 1);

            // Host user grants a lower role than the guest asked for.
            sessions.grant(peer, Role::ViewOnly).unwrap();
            assert_eq!(sessions.state(&peer), SessionState::Active);
            assert!(sessions.pending().is_empty());
            control
                .send(MessageKind::ConsentGrant(Role::ViewOnly))
                .await
                .unwrap();

            // Host user revokes; every grant drops with it (§8.1).
            sessions.revoke(peer).unwrap();
            assert_eq!(sessions.grants(&peer), None);
            assert_eq!(sessions.active_guest_count(), 0);
            control.send(MessageKind::ConsentRevoke).await.unwrap();

            // The connection lives as long as this value: dropping a
            // ControlConnection closes the QUIC connection under it.
            (peer, control.session_id(), control)
        }
    });

    let connection = guest.connect_control(host_addr).await.unwrap();
    let mut control = guest_handshake(connection, Role::FullControl, Vec::new(), Vec::new())
        .await
        .unwrap();

    // The host assigned a session id in its HelloAck (§9.1).
    let session_id = control.session_id();
    assert_ne!(session_id, [0u8; 16]);

    control.send(MessageKind::ConsentRequest).await.unwrap();

    let grant = control.recv().await.unwrap();
    // The guest asked for FullControl and got ViewOnly: the host decides.
    assert_eq!(grant.kind, MessageKind::ConsentGrant(Role::ViewOnly));
    assert_eq!(grant.direction, Direction::HostToGuest);
    assert_eq!(grant.session_id, session_id);
    assert_eq!(grant.seq, 1);

    let revoke = control.recv().await.unwrap();
    assert_eq!(revoke.kind, MessageKind::ConsentRevoke);
    assert_eq!(revoke.seq, 2);

    let (peer, host_session_id, _host_control) = tokio::time::timeout(TIMEOUT, host_side)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(peer, guest_id);
    assert_eq!(host_session_id, session_id);

    control.connection().close(0u32.into(), b"done");
    guest.close().await;
    host.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn each_session_gets_its_own_random_id() {
    let host = host_endpoint().await;
    let host_addr = host.addr();

    let host_side = tokio::spawn({
        let host = host.clone();
        async move {
            let mut controls = Vec::new();
            for _ in 0..2 {
                let (control, _) = accept_control(&host).await;
                controls.push(control);
            }
            controls
        }
    });

    let mut guest_ids = Vec::new();
    let mut guests = Vec::new();
    for _ in 0..2 {
        let guest = PeerEndpoint::bind_local(iroh::SecretKey::generate())
            .await
            .unwrap();
        let connection = guest.connect_control(host_addr.clone()).await.unwrap();
        let control = guest_handshake(connection, Role::ViewOnly, Vec::new(), Vec::new())
            .await
            .unwrap();
        guest_ids.push(control.session_id());
        guests.push((guest, control));
    }

    let host_controls = tokio::time::timeout(TIMEOUT, host_side)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(host_controls[0].session_id(), host_controls[1].session_id());
    assert_ne!(guest_ids[0], guest_ids[1]);

    for (guest, control) in guests {
        control.connection().close(0u32.into(), b"done");
        guest.close().await;
    }
    host.close().await;
}
