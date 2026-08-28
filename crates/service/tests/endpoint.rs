//! The service endpoint, end to end against the real binary (ADR 0043).
//!
//! Runs `lumepeer-service --console`, opens the pipe the way the client does,
//! and checks the frame contract. The one thing it deliberately never sends is
//! `OP_DELIVER_SAS`: on a machine where the test happens to run elevated that
//! would throw the real Ctrl+Alt+Del screen up in someone's face. An unknown
//! opcode exercises the same path — create the pipe with its access list,
//! accept, read a fixed frame, dispatch, write a fixed frame — and stops one
//! `match` arm short.

#![cfg(target_os = "windows")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a failed assumption must fail the test"
)]

use std::io::{Read as _, Write as _};
use std::time::{Duration, Instant};

use lumepeer_service::protocol::{ENDPOINT, FRAME_LEN, MAGIC, STATUS_REFUSED, request};

/// Anything slower than this and the service never came up.
const READY_TIMEOUT: Duration = Duration::from_secs(10);

/// An opcode the service does not know, and must never come to know: 0xFF is
/// left unassigned precisely so this test has something safe to send.
const OP_UNKNOWN: u8 = 0xFF;

struct Service(std::process::Child);

impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start() -> Service {
    let exe = std::path::Path::new(env!("CARGO_BIN_EXE_lumepeer-service"));
    let child = std::process::Command::new(exe)
        .arg("--console")
        .spawn()
        .expect("the service binary must be runnable");
    Service(child)
}

fn connect_when_ready() -> std::fs::File {
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        if let Ok(pipe) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(ENDPOINT)
        {
            return pipe;
        }
        assert!(
            Instant::now() < deadline,
            "the service never opened its endpoint"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The endpoint, end to end, in one test.
///
/// One test rather than three because the pipe is single-instance by design:
/// two services cannot listen on it at once, and `cargo test` runs test
/// functions in parallel. Splitting these would make them race for the one
/// endpoint the service is allowed to have.
#[test]
fn the_endpoint_answers_and_refuses() {
    assert!(
        !lumepeer_service::client::is_reachable(),
        "nothing must be listening before the service starts"
    );
    let _service = start();

    // An unknown opcode: the whole path — create the pipe with its access
    // list, accept, read a fixed frame, dispatch, write a fixed frame — one
    // `match` arm short of the real thing.
    {
        let mut pipe = connect_when_ready();
        pipe.write_all(&request(OP_UNKNOWN)).unwrap();
        pipe.flush().unwrap();
        let mut reply = [0u8; FRAME_LEN];
        pipe.read_exact(&mut reply).unwrap();
        assert_eq!(
            reply,
            [MAGIC, STATUS_REFUSED],
            "an unknown opcode is refused"
        );
    }

    // A frame that is not a request at all is refused, not guessed at.
    {
        let mut pipe = connect_when_ready();
        pipe.write_all(&[0x00, 0x01]).unwrap();
        pipe.flush().unwrap();
        let mut reply = [0u8; FRAME_LEN];
        pipe.read_exact(&mut reply).unwrap();
        assert_eq!(reply, [MAGIC, STATUS_REFUSED]);
    }

    // And the service loops back to accepting after each one, which is what
    // the client's own reachability check depends on.
    let deadline = Instant::now() + READY_TIMEOUT;
    while !lumepeer_service::client::is_reachable() {
        assert!(Instant::now() < deadline, "the service stopped accepting");
        std::thread::sleep(Duration::from_millis(50));
    }
}
