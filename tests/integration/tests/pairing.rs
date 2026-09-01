//! End-to-end host/guest pairing over two local endpoints (design doc §7,
//! §9.1). First test that exercises `host_handshake` + `guest_handshake`
//! together against a live `TicketRegistry`, rather than each in isolation.

#![allow(clippy::expect_used, reason = "a failed assumption must fail the test")]

use ed25519_dalek::SigningKey;
use lumepeer_core::consent::Role;
use lumepeer_core::session::SessionManager;
use lumepeer_net::ticket::{InviteTicket, TicketRegistry};
use lumepeer_net::{PeerEndpoint, guest_handshake, host_handshake};

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock before epoch")
        .as_secs()
}

#[tokio::test(flavor = "multi_thread")]
async fn full_pairing_round_trip_grants_and_the_guest_sees_it() {
    let host_secret = iroh::SecretKey::from_bytes(&[1u8; 32]);
    let host_identity = SigningKey::from_bytes(&host_secret.to_bytes());
    let host = PeerEndpoint::bind_local(host_secret)
        .await
        .expect("host bind");

    let guest_secret = iroh::SecretKey::from_bytes(&[2u8; 32]);
    let guest = PeerEndpoint::bind_local(guest_secret)
        .await
        .expect("guest bind");

    let now = unix_now();
    let ticket = InviteTicket::issue(
        &host_identity,
        &host.addr(),
        Role::ViewOnly,
        now,
        None,
        None,
    )
    .expect("issue ticket");
    let mut registry = TicketRegistry::new();
    registry.register(&ticket);

    let host_addr = host.addr();
    let accept = tokio::spawn({
        let host = host.clone();
        async move {
            let incoming = host.accept().await.expect("accepted")?;
            let (control, hello) = host_handshake(incoming).await?;
            Ok::<_, lumepeer_net::NetError>((control, hello))
        }
    });

    let proof = postcard::to_allocvec(&ticket).expect("encode proof");
    let connection = guest
        .connect_control(host_addr)
        .await
        .expect("guest dials host");
    let guest_control = guest_handshake(connection, Role::ViewOnly, proof, Vec::new())
        .await
        .expect("guest handshake");

    let (host_control, hello) = accept.await.expect("join").expect("host handshake");
    let claimed_ticket: InviteTicket =
        postcard::from_bytes(&hello.invite_proof).expect("decode proof");
    registry
        .claim(&claimed_ticket, now)
        .expect("first claim succeeds");
    registry
        .claim(&claimed_ticket, now)
        .expect("a live invite is a way back in, so a repeat claim is allowed (ADR 0016)");
    registry.retire_all();
    assert!(
        registry.claim(&claimed_ticket, now).is_err(),
        "an invite the host has replaced must stop working at once"
    );

    let mut sessions = SessionManager::new();
    sessions
        .request_consent_as(host_control.peer(), hello.role_request)
        .expect("queued");
    sessions
        .grant(host_control.peer(), Role::ViewOnly)
        .expect("granted");

    let mut host_control = host_control;
    host_control
        .send(lumepeer_core::protocol::MessageKind::ConsentGrant(
            Role::ViewOnly,
        ))
        .await
        .expect("send grant");

    let mut guest_control = guest_control;
    let envelope = guest_control.recv().await.expect("guest receives grant");
    assert!(matches!(
        envelope.kind,
        lumepeer_core::protocol::MessageKind::ConsentGrant(Role::ViewOnly)
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_expired_ticket_is_refused_by_the_registry_after_a_real_handshake() {
    let host_secret = iroh::SecretKey::from_bytes(&[3u8; 32]);
    let host_identity = SigningKey::from_bytes(&host_secret.to_bytes());
    let host = PeerEndpoint::bind_local(host_secret)
        .await
        .expect("host bind");

    let past = 1_000u64;
    let ticket = InviteTicket::issue(
        &host_identity,
        &host.addr(),
        Role::ViewOnly,
        past,
        None,
        None,
    )
    .expect("issue ticket");
    let mut registry = TicketRegistry::new();
    registry.register(&ticket);

    let far_future = past + lumepeer_core::constants::INVITE_TICKET_TTL_SECS + 1;
    assert!(registry.claim(&ticket, far_future).is_err());
}
