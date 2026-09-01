//! Phase 1 acceptance test of design doc §19: two local instances establish a
//! P2P connection and run the full `Hello`/`HelloAck` -> `ConsentRequest` ->
//! `ConsentGrant` -> `ConsentRevoke` cycle.
//!
//! The endpoints bind with the `Minimal` preset, so no relay and no address
//! lookup service is involved: the guest dials the host's direct addresses.

#![allow(clippy::unwrap_used, reason = "a failed assumption must fail the test")]

use std::time::Duration;

use lumepeer_core::clipboard::{self, ClipboardFlow};
use lumepeer_core::consent::{IndependentGrant, Role};
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

/// §8.2: the independent grants belong to the session, not to the peer.
/// A revoke takes them with it, and the next `ConsentGrant` starts from a
/// role's implied grants — never from what the previous session had reached.
#[tokio::test(flavor = "multi_thread")]
async fn an_independent_grant_dies_with_the_session_that_held_it() {
    let host = host_endpoint().await;
    let guest = PeerEndpoint::bind_local(iroh::SecretKey::generate())
        .await
        .unwrap();
    let host_addr = host.addr();

    let host_side = tokio::spawn({
        let host = host.clone();
        async move {
            let (mut control, _) = accept_control(&host).await;
            let peer = control.peer();
            let mut sessions = SessionManager::new();

            let request = control.recv().await.unwrap();
            assert_eq!(request.kind, MessageKind::ConsentRequest);
            assert_eq!(request.direction, Direction::GuestToHost);
            sessions
                .request_consent_as(peer, Role::FullControl)
                .unwrap();
            sessions.grant(peer, Role::FullControl).unwrap();
            control
                .send(MessageKind::ConsentGrant(Role::FullControl))
                .await
                .unwrap();

            // Nothing is implied by the role: the host has to say so.
            assert!(!sessions.grants(&peer).unwrap().file_transfer);
            sessions
                .set_grant(peer, IndependentGrant::FileTransfer, true)
                .unwrap();
            assert!(sessions.grants(&peer).unwrap().file_transfer);

            sessions.revoke(peer).unwrap();
            control.send(MessageKind::ConsentRevoke).await.unwrap();
            assert_eq!(sessions.grants(&peer), None);
            assert_eq!(sessions.state(&peer), SessionState::Idle);

            // Granting the same role again is a new session. The file
            // transfer the previous one had reached does not come back with
            // it, and the host has to decide a second time.
            sessions.grant(peer, Role::FullControl).unwrap();
            assert!(!sessions.grants(&peer).unwrap().file_transfer);
            assert!(sessions.grants(&peer).unwrap().input);

            control
        }
    });

    let connection = guest.connect_control(host_addr).await.unwrap();
    let mut control = guest_handshake(connection, Role::FullControl, Vec::new(), Vec::new())
        .await
        .unwrap();
    control.send(MessageKind::ConsentRequest).await.unwrap();
    let granted = control.recv().await.unwrap();
    assert_eq!(granted.kind, MessageKind::ConsentGrant(Role::FullControl));
    let revoked = control.recv().await.unwrap();
    assert_eq!(revoked.kind, MessageKind::ConsentRevoke);

    let host_control = tokio::time::timeout(TIMEOUT, host_side)
        .await
        .unwrap()
        .unwrap();
    host_control.connection().close(0u32.into(), b"done");
    control.connection().close(0u32.into(), b"done");
    guest.close().await;
    host.close().await;
}

/// §8.2, §9.2: `clipboard_read` is the grant for the host's clipboard
/// travelling *to* the guest, and it carries nothing in the other direction.
///
/// One grant on, one off, and the payload has to go the way the grant is
/// named. The receiving side used to check the opposite flag from the sending
/// side, which was invisible while the four grants were unreachable and would
/// have made a host that switched on exactly the right permission watch
/// nothing happen (ADR 0030).
#[tokio::test(flavor = "multi_thread")]
async fn only_the_read_grant_carries_the_hosts_clipboard_to_the_guest() {
    let host = host_endpoint().await;
    let guest = PeerEndpoint::bind_local(iroh::SecretKey::generate())
        .await
        .unwrap();
    let host_addr = host.addr();

    let host_side = tokio::spawn({
        let host = host.clone();
        async move {
            let (mut control, _) = accept_control(&host).await;
            let peer = control.peer();
            let mut sessions = SessionManager::new();

            let request = control.recv().await.unwrap();
            assert_eq!(request.kind, MessageKind::ConsentRequest);
            sessions.request_consent_as(peer, Role::ViewOnly).unwrap();
            sessions.grant(peer, Role::ViewOnly).unwrap();
            control
                .send(MessageKind::ConsentGrant(Role::ViewOnly))
                .await
                .unwrap();

            sessions
                .set_grant(peer, IndependentGrant::ClipboardRead, true)
                .unwrap();
            let grants = sessions.grants(&peer).unwrap();
            assert!(clipboard::permits(grants, ClipboardFlow::HostToGuest));
            control
                .send(MessageKind::ClipboardSync {
                    data: b"host side text".to_vec(),
                })
                .await
                .unwrap();

            // The guest answers with a clipboard of its own. Writing the
            // host's clipboard is the *other* grant, and this session does
            // not hold it: the payload is dropped, not applied.
            let inbound = control.recv().await.unwrap();
            assert!(matches!(inbound.kind, MessageKind::ClipboardSync { .. }));
            assert!(!clipboard::permits(grants, ClipboardFlow::GuestToHost));

            control
        }
    });

    let connection = guest.connect_control(host_addr).await.unwrap();
    let mut control = guest_handshake(connection, Role::ViewOnly, Vec::new(), Vec::new())
        .await
        .unwrap();
    control.send(MessageKind::ConsentRequest).await.unwrap();
    assert_eq!(
        control.recv().await.unwrap().kind,
        MessageKind::ConsentGrant(Role::ViewOnly)
    );
    assert_eq!(
        control.recv().await.unwrap().kind,
        MessageKind::ClipboardSync {
            data: b"host side text".to_vec(),
        }
    );
    control
        .send(MessageKind::ClipboardSync {
            data: b"guest side text".to_vec(),
        })
        .await
        .unwrap();

    let host_control = tokio::time::timeout(TIMEOUT, host_side)
        .await
        .unwrap()
        .unwrap();
    host_control.connection().close(0u32.into(), b"done");
    control.connection().close(0u32.into(), b"done");
    guest.close().await;
    host.close().await;
}

/// §8.2, §9.2: the mirror image. `clipboard_write` is what lets a guest
/// change the host's clipboard, and holding it says nothing about whether the
/// host's own clipboard may travel the other way.
#[tokio::test(flavor = "multi_thread")]
async fn only_the_write_grant_lets_a_guest_change_the_hosts_clipboard() {
    let host = host_endpoint().await;
    let guest = PeerEndpoint::bind_local(iroh::SecretKey::generate())
        .await
        .unwrap();
    let host_addr = host.addr();

    let host_side = tokio::spawn({
        let host = host.clone();
        async move {
            let (mut control, _) = accept_control(&host).await;
            let peer = control.peer();
            let mut sessions = SessionManager::new();

            let request = control.recv().await.unwrap();
            assert_eq!(request.kind, MessageKind::ConsentRequest);
            sessions.request_consent_as(peer, Role::ViewOnly).unwrap();
            sessions.grant(peer, Role::ViewOnly).unwrap();
            control
                .send(MessageKind::ConsentGrant(Role::ViewOnly))
                .await
                .unwrap();

            sessions
                .set_grant(peer, IndependentGrant::ClipboardWrite, true)
                .unwrap();
            let grants = sessions.grants(&peer).unwrap();

            let inbound = control.recv().await.unwrap();
            assert_eq!(
                inbound.kind,
                MessageKind::ClipboardSync {
                    data: b"guest side text".to_vec(),
                }
            );
            // Applied: this is exactly the direction the grant names.
            assert!(clipboard::permits(grants, ClipboardFlow::GuestToHost));
            // And the host's own clipboard still stays where it is.
            assert!(!clipboard::permits(grants, ClipboardFlow::HostToGuest));

            control
        }
    });

    let connection = guest.connect_control(host_addr).await.unwrap();
    let mut control = guest_handshake(connection, Role::ViewOnly, Vec::new(), Vec::new())
        .await
        .unwrap();
    control.send(MessageKind::ConsentRequest).await.unwrap();
    assert_eq!(
        control.recv().await.unwrap().kind,
        MessageKind::ConsentGrant(Role::ViewOnly)
    );
    control
        .send(MessageKind::ClipboardSync {
            data: b"guest side text".to_vec(),
        })
        .await
        .unwrap();

    let host_control = tokio::time::timeout(TIMEOUT, host_side)
        .await
        .unwrap()
        .unwrap();
    host_control.connection().close(0u32.into(), b"done");
    control.connection().close(0u32.into(), b"done");
    guest.close().await;
    host.close().await;
}
