//! Negative protocol tests over a real connection (design doc §18).
//!
//! Phase 1 covers the two rows that the handshake itself owns: a protocol major
//! mismatch and an oversized frame. The remaining rows of §18 belong to phase 4.

#![allow(clippy::unwrap_used, reason = "a failed assumption must fail the test")]

use std::time::Duration;

use lumepeer_core::CoreError;
use lumepeer_core::consent::Role;
use lumepeer_core::constants::MAX_CONTROL_FRAME_BYTES;
use lumepeer_core::protocol::{
    Direction, MessageEnvelope, MessageKind, PROTOCOL_MAJOR, PROTOCOL_MINOR,
};
use lumepeer_net::endpoint::PeerEndpoint;
use lumepeer_net::error::NetError;
use lumepeer_net::framing::FrameWriter;
use lumepeer_net::host_handshake;
use tokio::io::AsyncWriteExt as _;

const TIMEOUT: Duration = Duration::from_secs(20);

async fn host_and_guest() -> (PeerEndpoint, PeerEndpoint) {
    let host = PeerEndpoint::bind_local(iroh::SecretKey::generate())
        .await
        .unwrap();
    let guest = PeerEndpoint::bind_local(iroh::SecretKey::generate())
        .await
        .unwrap();
    (host, guest)
}

/// §9.1, §18: a different `major` in `Hello` ends the connection immediately,
/// before consent is ever offered.
#[tokio::test(flavor = "multi_thread")]
async fn a_major_mismatch_is_refused_before_consent() {
    let (host, guest) = host_and_guest().await;
    let addr = host.addr();

    let host_side = tokio::spawn({
        let host = host.clone();
        async move {
            let connection = host.accept().await.unwrap().unwrap();
            host_handshake(connection).await
        }
    });

    let connection = guest.connect_control(addr).await.unwrap();
    let (send, _recv) = connection.open_bi().await.unwrap();
    let mut writer = FrameWriter::new(send);
    let mut hello = MessageEnvelope {
        session_id: [0u8; 16],
        direction: Direction::GuestToHost,
        seq: 0,
        kind: MessageKind::Hello {
            major: PROTOCOL_MAJOR + 1,
            minor: PROTOCOL_MINOR,
            role_request: Role::ViewOnly,
            features: Vec::new(),
            invite_proof: Vec::new(),
        },
        body: Vec::new(),
    };
    writer.write_frame(&mut hello).await.unwrap();

    let refused = tokio::time::timeout(TIMEOUT, host_side)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(matches!(
        refused,
        NetError::Framing(CoreError::IncompatibleVersion {
            local: 1,
            remote: 2
        })
    ));

    guest.close().await;
    host.close().await;
}

/// §9.1, §18: an oversized frame is refused on the length prefix, before the
/// payload is allocated, and the stream is closed with `FRAME_SIZE`.
#[tokio::test(flavor = "multi_thread")]
async fn an_oversized_frame_is_refused_on_its_length_prefix() {
    let (host, guest) = host_and_guest().await;
    let addr = host.addr();

    let host_side = tokio::spawn({
        let host = host.clone();
        async move {
            let connection = host.accept().await.unwrap().unwrap();
            host_handshake(connection).await
        }
    });

    let connection = guest.connect_control(addr).await.unwrap();
    let (mut send, _recv) = connection.open_bi().await.unwrap();
    // Announce one byte more than the maximum and send nothing else: the host
    // must refuse on the prefix alone and never wait for the body.
    let announced = u32::try_from(MAX_CONTROL_FRAME_BYTES + 1).unwrap();
    send.write_all(&announced.to_be_bytes()).await.unwrap();
    send.flush().await.unwrap();

    let refused = tokio::time::timeout(TIMEOUT, host_side)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(matches!(
        refused,
        NetError::Framing(CoreError::FrameSize { size }) if size == MAX_CONTROL_FRAME_BYTES + 1
    ));

    guest.close().await;
    host.close().await;
}
