//! File transfer end to end (design doc §9.2, §4; ADR 0032).
//!
//! Two local endpoints, a real control connection and a real `rd/file/1`
//! connection, driven exactly the way the desktop actor drives them: the
//! offer/accept dance and the `FileTransferStart` that names the transfer
//! travel on the control channel, the bytes travel on the file channel, and
//! the receiver's `FileChunkAck` is both the progress report and the resume
//! point.
//!
//! What these tests are really about is the set of things that must *not*
//! happen. A declined offer must not open a connection at all. A cancelled
//! transfer must leave nothing on disk. A transfer whose bytes do not hash to
//! the offer must not be exported under the name the user was expecting. And
//! an offer from a session without the `file_transfer` grant must be refused
//! by the host's core, not by anything downstream of it.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a failed assumption must fail the test"
)]
#![allow(
    clippy::too_many_lines,
    reason = "one test is one protocol dance; splitting the halves apart would                   hide which message answers which"
)]

use std::path::PathBuf;
use std::time::Duration;

use lumepeer_core::consent::{IndependentGrant, Role};
use lumepeer_core::constants::FILE_CHUNK_MAX_BYTES;
use lumepeer_core::protocol::{FEATURE_FILE_TRANSFER, MessageKind};
use lumepeer_core::session::SessionManager;
use lumepeer_net::endpoint::PeerEndpoint;
use lumepeer_net::file_transfer::{
    ReceiveTracker, StagedReceive, TransferId, hash_file, read_chunk, safe_file_name, send_file,
};
use lumepeer_net::{ALPN_FILE, ControlConnection, guest_handshake, host_handshake};

/// Anything slower than this on loopback means the test is stuck, not slow.
const TIMEOUT: Duration = Duration::from_secs(20);

/// The transfer id the sender picks in these tests. Any number does: what
/// matters is that both sides use the *same* one, which is the whole reason
/// `FileTransferStart` exists (ADR 0032).
const TRANSFER: TransferId = 7;

/// A scratch directory that removes itself.
struct Scratch(PathBuf);

impl Scratch {
    fn new(what: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "lumepeer-ft-{what}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A payload spanning more than one chunk, so offsets and resume points are
/// exercised rather than assumed.
fn payload(bytes: usize) -> Vec<u8> {
    (0..bytes)
        .map(|i| u8::try_from(i % 251).unwrap_or(0))
        .collect()
}

async fn endpoints() -> (PeerEndpoint, PeerEndpoint) {
    let host = PeerEndpoint::bind_local(iroh::SecretKey::generate())
        .await
        .unwrap();
    let guest = PeerEndpoint::bind_local(iroh::SecretKey::generate())
        .await
        .unwrap();
    (host, guest)
}

/// The guest half of the handshake, advertising what this build understands.
async fn guest_control(guest: &PeerEndpoint, addr: iroh::EndpointAddr) -> ControlConnection {
    let connection = guest.connect_control(addr).await.unwrap();
    guest_handshake(
        connection,
        Role::ViewOnly,
        Vec::new(),
        vec![FEATURE_FILE_TRANSFER.to_owned()],
    )
    .await
    .unwrap()
}

/// Receives one transfer off a chunk stream into staging and, if the hash of
/// the offer matches, exports it.
///
/// This is the receiver the desktop actor runs, with the actor's bookkeeping
/// collapsed into locals: the accounting decides before anything is written,
/// the running hash is fed the same bytes, and nothing leaves staging until
/// `finish` says the file is the file that was offered.
async fn receive_one(
    file_connection: &iroh::endpoint::Connection,
    tracker: &mut ReceiveTracker,
    staged: &mut StagedReceive,
    expected: [u8; 32],
    stop_after: Option<u64>,
) -> (u64, Option<bool>) {
    let mut recv = file_connection.accept_uni().await.unwrap();
    loop {
        let Ok((id, offset, bytes)) = read_chunk(&mut recv).await else {
            // The stream ended. Whether that is completion or a drop is the
            // tracker's answer, not the stream's.
            break;
        };
        assert_eq!(id, TRANSFER, "a chunk arrived under a different id");
        tracker.apply_chunk(id, offset, bytes.len()).unwrap();
        staged.append(&bytes).await.unwrap();
        tracker.hash_chunk(id, &bytes);
        let state = tracker.state(id).unwrap().clone();
        if let Some(limit) = stop_after
            && state.received >= limit
        {
            // Simulates the connection dying mid-transfer: stop reading and
            // leave everything that arrived exactly where it is.
            return (state.received, None);
        }
        if state.received == state.total {
            break;
        }
    }
    let received = tracker.state(TRANSFER).unwrap().received;
    let verified = tracker.finish(TRANSFER, expected);
    (received, Some(verified))
}

/// §9.2 end to end: a guest offers, the host accepts, the bytes ride
/// `rd/file/1`, the hash is checked, and only then does the file appear under
/// the name the receiving user agreed to — in the directory *they* chose.
#[tokio::test(flavor = "multi_thread")]
async fn a_whole_file_travels_and_is_exported_only_after_its_hash_matches() {
    let scratch = Scratch::new("whole");
    let source = scratch.join("report.pdf");
    let bytes = payload(FILE_CHUNK_MAX_BYTES * 2 + 33);
    tokio::fs::write(&source, &bytes).await.unwrap();
    let expected = hash_file(&source).await.unwrap();
    let size = bytes.len() as u64;

    let (host, guest) = endpoints().await;
    let host_addr = host.addr();
    let inbox = scratch.join("inbox");
    tokio::fs::create_dir_all(&inbox).await.unwrap();

    let host_side = tokio::spawn({
        let host = host.clone();
        let inbox = inbox.clone();
        async move {
            let connection = host.accept().await.unwrap().unwrap();
            let (mut control, hello) = host_handshake(connection).await.unwrap();
            let peer = control.peer();
            // The host only ever sends `FileTransferStart` to a peer that
            // said it understands one (§9.1).
            assert!(hello.features.iter().any(|f| f == FEATURE_FILE_TRANSFER));

            let mut sessions = SessionManager::new();
            sessions.request_consent_as(peer, Role::ViewOnly).unwrap();
            sessions.grant(peer, Role::ViewOnly).unwrap();
            sessions
                .set_grant(peer, IndependentGrant::FileTransfer, true)
                .unwrap();
            control
                .send(MessageKind::ConsentGrant(Role::ViewOnly))
                .await
                .unwrap();

            let MessageKind::FileOffer { name, size, hash } = control.recv().await.unwrap().kind
            else {
                panic!("expected an offer");
            };
            assert!(sessions.grants(&peer).unwrap().file_transfer);
            let name = safe_file_name(&name).expect("a plain basename");
            let destination = inbox.join(&name);
            control.send(MessageKind::FileAccept(true)).await.unwrap();

            let MessageKind::FileTransferStart {
                transfer_id,
                name: started,
                size: started_size,
                hash: started_hash,
            } = control.recv().await.unwrap().kind
            else {
                panic!("expected a transfer start");
            };
            // The start restates the offer so this check is possible at all.
            assert_eq!((started, started_size, started_hash), (name, size, hash));

            let file_connection = host.accept().await.unwrap().unwrap();
            assert_eq!(file_connection.alpn(), ALPN_FILE);

            let mut tracker = ReceiveTracker::default();
            tracker.begin_with(transfer_id, size).unwrap();
            let mut staged = StagedReceive::create(&inbox, transfer_id).await.unwrap();
            let staging_path = staged.path().to_path_buf();
            let (received, verified) =
                receive_one(&file_connection, &mut tracker, &mut staged, hash, None).await;
            assert_eq!(received, size);
            assert_eq!(verified, Some(true));
            staged.export(&destination).await.unwrap();
            assert!(!staging_path.exists(), "staging outlived the export");

            control
                .send(MessageKind::FileChunkAck {
                    transfer_id,
                    offset: size,
                })
                .await
                .unwrap();
            (control, destination)
        }
    });

    let mut control = guest_control(&guest, host_addr.clone()).await;
    assert_eq!(
        control.recv().await.unwrap().kind,
        MessageKind::ConsentGrant(Role::ViewOnly)
    );
    control
        .send(MessageKind::FileOffer {
            name: "report.pdf".to_owned(),
            size,
            hash: expected,
        })
        .await
        .unwrap();
    assert_eq!(
        control.recv().await.unwrap().kind,
        MessageKind::FileAccept(true)
    );
    control
        .send(MessageKind::FileTransferStart {
            transfer_id: TRANSFER,
            name: "report.pdf".to_owned(),
            size,
            hash: expected,
        })
        .await
        .unwrap();

    // §4: the file connection is opened here, after the accept, and never
    // before it.
    let file_connection = guest.connect(host_addr, ALPN_FILE).await.unwrap();
    let mut send = file_connection.open_uni().await.unwrap();
    send_file(&mut send, TRANSFER, &source, 0, |_| {})
        .await
        .unwrap();
    send.finish().unwrap();

    // The ack at the full size is the receiver saying "verified and on disk",
    // which is the only completion a sender can honestly report.
    assert_eq!(
        control.recv().await.unwrap().kind,
        MessageKind::FileChunkAck {
            transfer_id: TRANSFER,
            offset: size,
        }
    );

    let (host_control, destination) = tokio::time::timeout(TIMEOUT, host_side)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(tokio::fs::read(&destination).await.unwrap(), bytes);

    host_control.connection().close(0u32.into(), b"done");
    control.connection().close(0u32.into(), b"done");
    guest.close().await;
    host.close().await;
}

/// §4: a declined offer opens no file connection at all. Not "opens one and
/// closes it" — the whole point of the lazy connection is that a session that
/// never agreed to a transfer never has a second connection to be delayed by.
#[tokio::test(flavor = "multi_thread")]
async fn a_declined_offer_never_opens_the_file_connection() {
    let (host, guest) = endpoints().await;
    let host_addr = host.addr();

    let host_side = tokio::spawn({
        let host = host.clone();
        async move {
            let connection = host.accept().await.unwrap().unwrap();
            let (mut control, _) = host_handshake(connection).await.unwrap();
            let peer = control.peer();
            let mut sessions = SessionManager::new();
            sessions.request_consent_as(peer, Role::ViewOnly).unwrap();
            sessions.grant(peer, Role::ViewOnly).unwrap();
            sessions
                .set_grant(peer, IndependentGrant::FileTransfer, true)
                .unwrap();

            let MessageKind::FileOffer { .. } = control.recv().await.unwrap().kind else {
                panic!("expected an offer");
            };
            // The receiving user said no.
            control.send(MessageKind::FileAccept(false)).await.unwrap();

            // Nothing else must arrive on this endpoint.
            let nothing = tokio::time::timeout(Duration::from_secs(2), host.accept()).await;
            assert!(
                nothing.is_err(),
                "a connection arrived after the offer was declined"
            );
            control
        }
    });

    let mut control = guest_control(&guest, host_addr).await;
    control
        .send(MessageKind::FileOffer {
            name: "report.pdf".to_owned(),
            size: 10,
            hash: [0u8; 32],
        })
        .await
        .unwrap();
    assert_eq!(
        control.recv().await.unwrap().kind,
        MessageKind::FileAccept(false)
    );

    let host_control = tokio::time::timeout(TIMEOUT, host_side)
        .await
        .unwrap()
        .unwrap();
    host_control.connection().close(0u32.into(), b"done");
    control.connection().close(0u32.into(), b"done");
    guest.close().await;
    host.close().await;
}

/// §9.2: a transfer cancelled halfway leaves nothing on disk — not a
/// truncated file under the destination name, and not the staging file
/// either.
#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_transfer_leaves_nothing_behind() {
    let scratch = Scratch::new("cancel");
    let source = scratch.join("big.bin");
    let bytes = payload(FILE_CHUNK_MAX_BYTES * 3);
    tokio::fs::write(&source, &bytes).await.unwrap();
    let expected = hash_file(&source).await.unwrap();
    let size = bytes.len() as u64;
    let inbox = scratch.join("inbox");
    tokio::fs::create_dir_all(&inbox).await.unwrap();

    let (host, guest) = endpoints().await;
    let host_addr = host.addr();

    let host_side = tokio::spawn({
        let host = host.clone();
        let inbox = inbox.clone();
        async move {
            let connection = host.accept().await.unwrap().unwrap();
            let (mut control, _) = host_handshake(connection).await.unwrap();
            let peer = control.peer();
            let mut sessions = SessionManager::new();
            sessions.request_consent_as(peer, Role::ViewOnly).unwrap();
            sessions.grant(peer, Role::ViewOnly).unwrap();
            sessions
                .set_grant(peer, IndependentGrant::FileTransfer, true)
                .unwrap();

            let MessageKind::FileOffer { name, .. } = control.recv().await.unwrap().kind else {
                panic!("expected an offer");
            };
            let destination = inbox.join(safe_file_name(&name).unwrap());
            control.send(MessageKind::FileAccept(true)).await.unwrap();
            let MessageKind::FileTransferStart { transfer_id, .. } =
                control.recv().await.unwrap().kind
            else {
                panic!("expected a transfer start");
            };

            let file_connection = host.accept().await.unwrap().unwrap();
            let mut tracker = ReceiveTracker::default();
            tracker.begin_with(transfer_id, size).unwrap();
            let mut staged = StagedReceive::create(&inbox, transfer_id).await.unwrap();
            let staging_path = staged.path().to_path_buf();

            // Stop one chunk in, exactly as an abort mid-flight would.
            let (received, _) = receive_one(
                &file_connection,
                &mut tracker,
                &mut staged,
                expected,
                Some(1),
            )
            .await;
            assert!(received > 0 && received < size, "nothing to cancel");
            assert!(staging_path.exists(), "the partial bytes went nowhere");

            control
                .send(MessageKind::FileAbort { transfer_id })
                .await
                .unwrap();
            tracker.cancel(transfer_id);
            staged.discard().await;

            assert!(!staging_path.exists(), "a cancelled transfer left staging");
            assert!(
                !destination.exists(),
                "a cancelled transfer produced a file under the destination name"
            );
            (control, destination)
        }
    });

    let mut control = guest_control(&guest, host_addr.clone()).await;
    control
        .send(MessageKind::FileOffer {
            name: "big.bin".to_owned(),
            size,
            hash: expected,
        })
        .await
        .unwrap();
    assert_eq!(
        control.recv().await.unwrap().kind,
        MessageKind::FileAccept(true)
    );
    control
        .send(MessageKind::FileTransferStart {
            transfer_id: TRANSFER,
            name: "big.bin".to_owned(),
            size,
            hash: expected,
        })
        .await
        .unwrap();

    let file_connection = guest.connect(host_addr, ALPN_FILE).await.unwrap();
    let mut send = file_connection.open_uni().await.unwrap();
    // The sender is still pushing when the abort lands; whether it finishes
    // writing is irrelevant to what must be true on the receiving disk.
    let pushing = tokio::spawn(async move {
        let _ = send_file(&mut send, TRANSFER, &source, 0, |_| {}).await;
    });

    assert_eq!(
        control.recv().await.unwrap().kind,
        MessageKind::FileAbort {
            transfer_id: TRANSFER
        }
    );
    pushing.abort();

    let (host_control, destination) = tokio::time::timeout(TIMEOUT, host_side)
        .await
        .unwrap()
        .unwrap();
    assert!(!destination.exists());

    host_control.connection().close(0u32.into(), b"done");
    control.connection().close(0u32.into(), b"done");
    guest.close().await;
    host.close().await;
}

/// §10: the file connection drops mid-transfer and comes back inside the
/// reconnect window. The sender picks up from the last `FileChunkAck` it saw,
/// not from zero, and the file that lands is byte-for-byte the one offered.
#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_file_connection_resumes_from_the_last_ack() {
    let scratch = Scratch::new("resume");
    let source = scratch.join("resume.bin");
    let bytes = payload(FILE_CHUNK_MAX_BYTES * 3 + 11);
    tokio::fs::write(&source, &bytes).await.unwrap();
    let expected = hash_file(&source).await.unwrap();
    let size = bytes.len() as u64;
    let inbox = scratch.join("inbox");
    tokio::fs::create_dir_all(&inbox).await.unwrap();

    let (host, guest) = endpoints().await;
    let host_addr = host.addr();

    let host_side = tokio::spawn({
        let host = host.clone();
        let inbox = inbox.clone();
        async move {
            let connection = host.accept().await.unwrap().unwrap();
            let (mut control, _) = host_handshake(connection).await.unwrap();
            let peer = control.peer();
            let mut sessions = SessionManager::new();
            sessions.request_consent_as(peer, Role::ViewOnly).unwrap();
            sessions.grant(peer, Role::ViewOnly).unwrap();
            sessions
                .set_grant(peer, IndependentGrant::FileTransfer, true)
                .unwrap();

            let MessageKind::FileOffer { name, .. } = control.recv().await.unwrap().kind else {
                panic!("expected an offer");
            };
            let destination = inbox.join(safe_file_name(&name).unwrap());
            control.send(MessageKind::FileAccept(true)).await.unwrap();
            let MessageKind::FileTransferStart { transfer_id, .. } =
                control.recv().await.unwrap().kind
            else {
                panic!("expected a transfer start");
            };

            // The receiver's tracker and its staging file survive the drop:
            // that is what makes a resume a resume. The control session is
            // untouched throughout, which is the §10 window.
            let mut tracker = ReceiveTracker::default();
            tracker.begin_with(transfer_id, size).unwrap();
            let mut staged = StagedReceive::create(&inbox, transfer_id).await.unwrap();

            let first = host.accept().await.unwrap().unwrap();
            let (received, _) =
                receive_one(&first, &mut tracker, &mut staged, expected, Some(1)).await;
            assert!(received > 0 && received < size);
            first.close(0u32.into(), b"dropped");
            control
                .send(MessageKind::FileChunkAck {
                    transfer_id,
                    offset: received,
                })
                .await
                .unwrap();

            let second = host.accept().await.unwrap().unwrap();
            let (total, verified) =
                receive_one(&second, &mut tracker, &mut staged, expected, None).await;
            assert_eq!(total, size, "the resume did not reach the end");
            assert_eq!(verified, Some(true), "the resumed file did not verify");
            staged.export(&destination).await.unwrap();
            (control, destination)
        }
    });

    let mut control = guest_control(&guest, host_addr.clone()).await;
    control
        .send(MessageKind::FileOffer {
            name: "resume.bin".to_owned(),
            size,
            hash: expected,
        })
        .await
        .unwrap();
    assert_eq!(
        control.recv().await.unwrap().kind,
        MessageKind::FileAccept(true)
    );
    control
        .send(MessageKind::FileTransferStart {
            transfer_id: TRANSFER,
            name: "resume.bin".to_owned(),
            size,
            hash: expected,
        })
        .await
        .unwrap();

    let first = guest.connect(host_addr.clone(), ALPN_FILE).await.unwrap();
    let mut send = first.open_uni().await.unwrap();
    let pushing = tokio::spawn(async move {
        let _ = send_file(&mut send, TRANSFER, &source.clone(), 0, |_| {}).await;
        source
    });
    let MessageKind::FileChunkAck {
        transfer_id,
        offset,
    } = control.recv().await.unwrap().kind
    else {
        panic!("expected an ack");
    };
    assert_eq!(transfer_id, TRANSFER);
    assert!(offset > 0 && offset < size, "nothing to resume from");
    let source = pushing.await.unwrap();
    first.close(0u32.into(), b"dropped");

    // The resume: a new file connection, and the sender starts at the offset
    // the receiver acked rather than at zero.
    let second = guest.connect(host_addr, ALPN_FILE).await.unwrap();
    let mut send = second.open_uni().await.unwrap();
    send_file(&mut send, TRANSFER, &source, offset, |_| {})
        .await
        .unwrap();
    send.finish().unwrap();

    let (host_control, destination) = tokio::time::timeout(TIMEOUT, host_side)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(tokio::fs::read(&destination).await.unwrap(), bytes);

    host_control.connection().close(0u32.into(), b"done");
    control.connection().close(0u32.into(), b"done");
    guest.close().await;
    host.close().await;
}

/// §8.2: an offer from a session that does not hold `file_transfer` is
/// declined by the host's core.
///
/// The role is `FullControl`, which is the point: the most powerful role
/// there is still implies nothing about files (§2.2), and the check that
/// refuses this is `Grants::file_transfer` and not anything about the role.
#[tokio::test(flavor = "multi_thread")]
async fn an_offer_without_the_grant_is_declined() {
    let (host, guest) = endpoints().await;
    let host_addr = host.addr();

    let host_side = tokio::spawn({
        let host = host.clone();
        async move {
            let connection = host.accept().await.unwrap().unwrap();
            let (mut control, _) = host_handshake(connection).await.unwrap();
            let peer = control.peer();
            let mut sessions = SessionManager::new();
            sessions
                .request_consent_as(peer, Role::FullControl)
                .unwrap();
            sessions.grant(peer, Role::FullControl).unwrap();

            let MessageKind::FileOffer { .. } = control.recv().await.unwrap().kind else {
                panic!("expected an offer");
            };
            let grants = sessions.grants(&peer).unwrap();
            assert!(grants.input, "the role did imply input");
            assert!(!grants.file_transfer, "a role implied file transfer");
            control.send(MessageKind::FileAccept(false)).await.unwrap();

            // And once the host does decide, the same offer is takeable.
            sessions
                .set_grant(peer, IndependentGrant::FileTransfer, true)
                .unwrap();
            let MessageKind::FileOffer { .. } = control.recv().await.unwrap().kind else {
                panic!("expected a second offer");
            };
            assert!(sessions.grants(&peer).unwrap().file_transfer);
            control.send(MessageKind::FileAccept(true)).await.unwrap();
            control
        }
    });

    let mut control = guest_control(&guest, host_addr).await;
    let offer = MessageKind::FileOffer {
        name: "report.pdf".to_owned(),
        size: 4,
        hash: [1u8; 32],
    };
    control.send(offer.clone()).await.unwrap();
    assert_eq!(
        control.recv().await.unwrap().kind,
        MessageKind::FileAccept(false)
    );
    control.send(offer).await.unwrap();
    assert_eq!(
        control.recv().await.unwrap().kind,
        MessageKind::FileAccept(true)
    );

    let host_control = tokio::time::timeout(TIMEOUT, host_side)
        .await
        .unwrap()
        .unwrap();
    host_control.connection().close(0u32.into(), b"done");
    control.connection().close(0u32.into(), b"done");
    guest.close().await;
    host.close().await;
}
