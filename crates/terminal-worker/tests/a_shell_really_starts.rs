//! The whole Windows path, end to end, in whichever shape this process can
//! take (ADR 0079, ADR 0102).
//!
//! The same test covers both arrangements, and which one it covers is decided
//! by how the test binary was started rather than by anything it does:
//!
//! - **unelevated** — `CreateProcessAsUserW` succeeds and the pseudo-console
//!   is in this process;
//! - **elevated** — that call comes back `ERROR_PRIVILEGE_NOT_HELD`, exactly
//!   as the shipped client of ADR 0057 always sees, and the shell comes up
//!   through the worker instead.
//!
//! So running it both ways is the coverage, and neither way is a special
//! build. It lives in the worker's crate because `CARGO_BIN_EXE_` only names
//! binaries of the crate the test belongs to, and the worker has to be beside
//! the executable doing the spawning for the client to find it at all.

#![cfg(target_os = "windows")]
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "a test")]

use std::io::{Read, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use lumepeer_terminal::{Shell, ShellError, ShellSize};

/// What the shell is asked to say, and what the test looks for coming back.
///
/// Distinctive on purpose: a `ConPTY` echoes the command as well as its
/// output, and both carry escape sequences, so the check is "this string
/// appeared" rather than anything about the shape of the stream.
const PROBE: &str = "lumepeer-probe-9f3a";

/// How long a shell gets to say it, before the test gives up on it.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(20);

/// Puts the worker beside this test binary, which is where the client looks.
///
/// `cargo` builds the worker into `target/<profile>/` and test binaries into
/// `target/<profile>/deps/`, and `locate_worker_binary` looks next to the
/// running executable — the arrangement an installed build has and a test tree
/// does not. Copying is the whole fixture. A copy that fails because the file
/// is already there and in use by another test binary is not a failure.
fn stage_worker() {
    let built = std::path::Path::new(env!("CARGO_BIN_EXE_lumepeer-terminal-worker"));
    let beside = std::env::current_exe()
        .expect("this test's own path")
        .with_file_name(
            built
                .file_name()
                .expect("the worker binary has a file name"),
        );
    if beside == built {
        return;
    }
    let _ = std::fs::copy(built, &beside);
}

#[test]
fn a_shell_starts_answers_resizes_and_dies_with_its_control() {
    stage_worker();

    let shell = match Shell::spawn(ShellSize { cols: 80, rows: 24 }) {
        Ok(shell) => shell,
        // No interactive desktop to drop to — a headless runner, or a session
        // with no Explorer in it. That is the correct answer on such a
        // machine (ADR 0079) and there is no shell here to test.
        Err(ShellError::CannotDropPrivileges) => return,
        Err(error) => panic!("no shell started: {error}"),
    };

    let control = shell.control();
    let (reader, mut writer, _) = shell.into_parts();

    // The read is blocking and the shell may say nothing at all, so it goes
    // on its own thread and the test waits on the channel instead.
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut reader = reader;
        let mut buffer = vec![0u8; 8192];
        while let Ok(read) = reader.read(&mut buffer) {
            if read == 0 || tx.send(buffer[..read].to_vec()).is_err() {
                return;
            }
        }
    });

    writer
        .write_all(format!("echo {PROBE}\r\n").as_bytes())
        .expect("what was typed to reach the shell");
    writer.flush().expect("the keystrokes to go out");

    let deadline = Instant::now() + ANSWER_TIMEOUT;
    let mut said = String::new();
    while Instant::now() < deadline && !said.contains(PROBE) {
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(chunk) => said.push_str(&String::from_utf8_lossy(&chunk)),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    assert!(
        said.contains(PROBE),
        "the shell never echoed the probe back; it said {} bytes",
        said.len()
    );

    // A resize reaches the console whichever side of the drop it is on: in
    // this process it is a call, through the worker it is a frame.
    control.resize(ShellSize {
        cols: 120,
        rows: 40,
    });

    // And the kill is final. Through the worker that means killing the worker
    // and letting its job take the shell with it, which is the property
    // ADR 0102 has to preserve across the extra process: what the reader sees
    // is its end of the pipe closing, either way.
    control.kill();
    let closed = loop {
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break true,
            Err(mpsc::RecvTimeoutError::Timeout) => break false,
        }
    };
    assert!(closed, "the shell outlived the control that killed it");
}
