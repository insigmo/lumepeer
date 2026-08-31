//! Negative protocol tests over a real connection (design doc §18).
//!
//! Phase 1 covers the two rows that the handshake itself owns: a protocol major
//! mismatch and an oversized frame. The remaining rows of §18 belong to phase 4.
//!
//! The file-transfer rows were added with `FileTransferStart` (ADR 0032): the
//! per-message bounds of §9.2 are checked on a live connection, where the only
//! thing wrong with the frame is its size.
//!
//! The cursor row is the same shape of test for a different payload: a bitmap
//! whose geometry contradicts its own pixel buffer is the one message on this
//! channel where believing the sender means indexing past the end of an
//! allocation.

#![allow(clippy::unwrap_used, reason = "a failed assumption must fail the test")]

use std::time::Duration;

use lumepeer_core::CoreError;
use lumepeer_core::consent::Role;
use lumepeer_core::constants::{
    FILE_NAME_MAX_BYTES, FILE_OFFER_MAX_BYTES, MAX_CONTROL_FRAME_BYTES, UNATTENDED_CODE_MAX_BYTES,
    UNATTENDED_PASSWORD_MAX_BYTES,
};
use lumepeer_core::protocol::{
    CursorShapeData, Direction, MessageEnvelope, MessageKind, PROTOCOL_MAJOR, PROTOCOL_MINOR,
};
use lumepeer_net::endpoint::PeerEndpoint;
use lumepeer_net::error::NetError;
use lumepeer_net::framing::FrameWriter;
use lumepeer_net::{guest_handshake, host_handshake};
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

/// §9.2, §18: the two bounds `FileTransferStart` repeats from the offer are
/// enforced on the wire, not merely documented (ADR 0032).
///
/// The message is sent *after* a completed handshake on purpose. A malformed
/// frame sent instead of `Hello` would be refused for being the wrong message,
/// which proves nothing about the limit; refused as the next message on an
/// established connection, the only thing wrong with it is its size.
#[tokio::test(flavor = "multi_thread")]
async fn a_transfer_start_past_its_bounds_is_refused_on_a_live_connection() {
    for kind in [
        MessageKind::FileTransferStart {
            transfer_id: 1,
            // One byte past what any filesystem this ships on can write.
            name: "n".repeat(FILE_NAME_MAX_BYTES + 1),
            size: 1,
            hash: [0u8; 32],
        },
        MessageKind::FileTransferStart {
            transfer_id: 1,
            name: "report.pdf".to_owned(),
            size: FILE_OFFER_MAX_BYTES + 1,
            hash: [0u8; 32],
        },
        // The same bound now covers the offer, which had only ever been
        // bounded on its size.
        MessageKind::FileOffer {
            name: "n".repeat(FILE_NAME_MAX_BYTES + 1),
            size: 1,
            hash: [0u8; 32],
        },
    ] {
        let (host, guest) = host_and_guest().await;
        let addr = host.addr();

        let host_side = tokio::spawn({
            let host = host.clone();
            async move {
                let connection = host.accept().await.unwrap().unwrap();
                let (mut control, _) = host_handshake(connection).await.unwrap();
                control.recv().await
            }
        });

        let connection = guest.connect_control(addr).await.unwrap();
        let mut control = guest_handshake(connection, Role::ViewOnly, Vec::new(), Vec::new())
            .await
            .unwrap();
        control.send(kind).await.unwrap();

        let refused = tokio::time::timeout(TIMEOUT, host_side)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(
            matches!(refused, NetError::Framing(CoreError::Malformed)),
            "an over-limit file message was accepted: {refused:?}"
        );

        control.connection().close(0u32.into(), b"done");
        guest.close().await;
        host.close().await;
    }
}

/// §9.2, §18: a clipboard file list is untrusted input exactly like a
/// `FileOffer`, and the same two bounds are enforced on a live connection
/// (docs/bugs/14-clipboard-files.md #2; ADR 0046).
#[tokio::test(flavor = "multi_thread")]
async fn a_clipboard_file_offer_past_its_bounds_is_refused_on_a_live_connection() {
    use lumepeer_core::constants::CLIPBOARD_FILE_LIST_MAX_ENTRIES;
    use lumepeer_core::protocol::ClipboardFileEntry;

    for kind in [
        MessageKind::ClipboardFileOffer {
            files: (0..=CLIPBOARD_FILE_LIST_MAX_ENTRIES)
                .map(|i| ClipboardFileEntry {
                    name: format!("file-{i}.bin"),
                    size: 1,
                })
                .collect(),
        },
        MessageKind::ClipboardFileOffer {
            files: vec![ClipboardFileEntry {
                name: "n".repeat(FILE_NAME_MAX_BYTES + 1),
                size: 1,
            }],
        },
        MessageKind::ClipboardFileOffer {
            files: vec![ClipboardFileEntry {
                name: "big.bin".to_owned(),
                size: FILE_OFFER_MAX_BYTES + 1,
            }],
        },
    ] {
        let (host, guest) = host_and_guest().await;
        let addr = host.addr();

        let host_side = tokio::spawn({
            let host = host.clone();
            async move {
                let connection = host.accept().await.unwrap().unwrap();
                let (mut control, _) = host_handshake(connection).await.unwrap();
                control.recv().await
            }
        });

        let connection = guest.connect_control(addr).await.unwrap();
        let mut control = guest_handshake(connection, Role::ViewOnly, Vec::new(), Vec::new())
            .await
            .unwrap();
        control.send(kind).await.unwrap();

        let refused = tokio::time::timeout(TIMEOUT, host_side)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(
            matches!(refused, NetError::Framing(CoreError::Malformed)),
            "an over-limit clipboard file offer was accepted: {refused:?}"
        );

        control.connection().close(0u32.into(), b"done");
        guest.close().await;
        host.close().await;
    }
}

/// The mirror of the test above: at the bound, both messages are ordinary
/// traffic. A limit that also refused the largest legal value would be a
/// different limit.
#[tokio::test(flavor = "multi_thread")]
async fn a_transfer_start_exactly_at_its_bounds_is_ordinary_traffic() {
    let (host, guest) = host_and_guest().await;
    let addr = host.addr();

    let host_side = tokio::spawn({
        let host = host.clone();
        async move {
            let connection = host.accept().await.unwrap().unwrap();
            let (mut control, _) = host_handshake(connection).await.unwrap();
            control.recv().await
        }
    });

    let connection = guest.connect_control(addr).await.unwrap();
    let mut control = guest_handshake(connection, Role::ViewOnly, Vec::new(), Vec::new())
        .await
        .unwrap();
    let at_bound = MessageKind::FileTransferStart {
        transfer_id: 1,
        name: "n".repeat(FILE_NAME_MAX_BYTES),
        size: FILE_OFFER_MAX_BYTES,
        hash: [7u8; 32],
    };
    control.send(at_bound.clone()).await.unwrap();

    let received = tokio::time::timeout(TIMEOUT, host_side)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(received.kind, at_bound);

    control.connection().close(0u32.into(), b"done");
    guest.close().await;
    host.close().await;
}

/// §9.1: device credentials arrive from a peer that has not been admitted
/// yet, which makes them the least trusted payload the control channel
/// carries. Both fields are bounded while decoding, before anything
/// downstream allocates (ADR 0033).
#[tokio::test(flavor = "multi_thread")]
async fn oversized_device_credentials_are_refused_while_decoding() {
    for kind in [
        MessageKind::UnattendedAuth {
            password: "p".repeat(UNATTENDED_PASSWORD_MAX_BYTES + 1),
            code: None,
        },
        MessageKind::UnattendedAuth {
            password: "fine".to_owned(),
            code: Some("1".repeat(UNATTENDED_CODE_MAX_BYTES + 1)),
        },
    ] {
        let (host, guest) = host_and_guest().await;
        let addr = host.addr();

        let host_side = tokio::spawn({
            let host = host.clone();
            async move {
                let connection = host.accept().await.unwrap().unwrap();
                let (mut control, _) = host_handshake(connection).await.unwrap();
                control.recv().await
            }
        });

        let connection = guest.connect_control(addr).await.unwrap();
        let mut control = guest_handshake(connection, Role::ViewOnly, Vec::new(), Vec::new())
            .await
            .unwrap();
        control.send(kind).await.unwrap();

        let refused = tokio::time::timeout(TIMEOUT, host_side)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(
            matches!(refused, NetError::Framing(CoreError::Malformed)),
            "over-limit credentials were accepted: {refused:?}"
        );

        control.connection().close(0u32.into(), b"done");
        guest.close().await;
        host.close().await;
    }
}

/// The mirror: a password and a code exactly at their bounds are ordinary
/// traffic, and the gate refuses them on their merits rather than the wire
/// refusing them on their size.
#[tokio::test(flavor = "multi_thread")]
async fn credentials_exactly_at_their_bounds_are_ordinary_traffic() {
    let (host, guest) = host_and_guest().await;
    let addr = host.addr();

    let host_side = tokio::spawn({
        let host = host.clone();
        async move {
            let connection = host.accept().await.unwrap().unwrap();
            let (mut control, _) = host_handshake(connection).await.unwrap();
            control.recv().await
        }
    });

    let connection = guest.connect_control(addr).await.unwrap();
    let mut control = guest_handshake(connection, Role::ViewOnly, Vec::new(), Vec::new())
        .await
        .unwrap();
    let at_bound = MessageKind::UnattendedAuth {
        password: "p".repeat(UNATTENDED_PASSWORD_MAX_BYTES),
        code: Some("1".repeat(UNATTENDED_CODE_MAX_BYTES)),
    };
    control.send(at_bound.clone()).await.unwrap();

    let received = tokio::time::timeout(TIMEOUT, host_side)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(received.kind, at_bound);

    control.connection().close(0u32.into(), b"done");
    guest.close().await;
    host.close().await;
}

/// §11: a cursor whose geometry contradicts its own pixel buffer is refused
/// while decoding, and no truncation of a valid one gets through either.
///
/// The bytes are built by hand and then damaged, because `encode` cannot
/// produce them: the check that matters is the one on the receiving side,
/// where the numbers came from someone else. `MessageEnvelope::decode` is the
/// same function the control connection runs on every inbound frame, so what
/// is proven here is what happens on the wire — and what must never happen is
/// a panic, an allocation sized from the sender's claim, or a shape that is
/// believed.
#[test]
fn a_damaged_cursor_frame_is_refused_rather_than_believed() {
    let honest = MessageEnvelope {
        session_id: [3u8; 16],
        direction: Direction::HostToGuest,
        seq: 0,
        kind: MessageKind::CursorShape {
            shape: CursorShapeData {
                width: 8,
                height: 8,
                hotspot_x: 1,
                hotspot_y: 1,
                rgba: vec![0x5A; 8 * 8 * 4],
            },
        },
        body: Vec::new(),
    };
    let bytes = honest.encode().unwrap();
    assert_eq!(MessageEnvelope::decode(&bytes).unwrap(), honest);

    // Every prefix of a valid frame: each one is either an incomplete value
    // or a shape whose payload no longer matches its geometry.
    for cut in 1..bytes.len() {
        assert!(
            MessageEnvelope::decode(&bytes[..cut]).is_err(),
            "a cursor frame truncated to {cut} bytes was accepted"
        );
    }

    // And every single-byte corruption of the header, which is where the
    // geometry and the payload length live. Anything that still parses must
    // parse into a shape whose pixels match what it claims — the one thing
    // the rest of the pipeline is allowed to assume.
    for index in 0..bytes.len().min(64) {
        for flip in [0x01u8, 0x80, 0xFF] {
            let mut damaged = bytes.clone();
            damaged[index] ^= flip;
            if let Ok(envelope) = MessageEnvelope::decode(&damaged)
                && let MessageKind::CursorShape { shape } = envelope.kind
            {
                assert_eq!(
                    shape.rgba.len(),
                    usize::from(shape.width) * usize::from(shape.height) * 4,
                    "a decoded cursor disagreed with its own geometry"
                );
                assert!(shape.hotspot_x < shape.width && shape.hotspot_y < shape.height);
            }
        }
    }
}
