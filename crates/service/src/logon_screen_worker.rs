//! This binary, running as the logon screen's worker (ADR 0088 §1).
//!
//! Launched by `LumepeerHost` with one argument
//! ([`lumepeer_service::LOGON_SCREEN_WORKER_ARG`]) onto the console session's
//! `WinSta0\Winlogon` desktop, as `LocalSystem` — because nobody is signed in
//! for it to run as, which is the entire situation this worker exists for.
//!
//! It is the session agent's counterpart for a machine with nobody in it, and
//! it is deliberately the smaller of the two:
//!
//! | | session agent (ADR 0085) | logon-screen worker (ADR 0088) |
//! | --- | --- | --- |
//! | Runs as | the signed-in user | `LocalSystem` |
//! | Desktop | `WinSta0\Default` | `WinSta0\Winlogon` |
//! | Publishes | an encoded media payload | raw `BGRA8` |
//! | Indicator | raises one, first | has nobody to show one to |
//! | Input | as that person | under `secure_desktop_input` (ADR 0061) |
//!
//! **It is told what to do and never asked what is permitted.** Every command
//! arriving here was authorized in `lumepeer-core`, in the host, before this
//! channel was touched: the controller role for the event itself and
//! `secure_desktop_input` for the fact that it lands on `Winlogon`. Nothing
//! in this file reads a grant, and nothing could — [`AgentCommand`] carries
//! no peer, role or grant to read.
//!
//! **What it must not grow.** A `LocalSystem` process on the desktop where
//! administrator passwords are typed may capture that desktop and perform the
//! events the host forwards to it. There is no path here that runs a program,
//! opens a file or reaches the network, and the whole set of things it can be
//! asked to do is the eight variants of [`AgentCommand`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use lumepeer_service::agent_channel::HostLink;
use lumepeer_service::agent_protocol::{AgentCommand, AgentEvent, CaptureUnavailableReason};
use lumepeer_service::frame::{FrameChannel, Writer};
use lumepeer_service::logon_screen::LOGON_SCREEN_CAPTURE_INTERVAL_MS;
use lumepeer_service::protocol::InjectAction;

/// The exit code that means "this worker did its job and was told to stop".
const WORKER_OK: u32 = 0;

/// Runs this process as the logon screen's worker until the host says stop or
/// the channel ends.
///
/// Returns the process exit code, the same one-bit outcome every other worker
/// in this binary reports: zero when the attachment ran and ended, non-zero
/// when it could not be made at all. The host does not read it — it watches
/// the process and the channel — but a person reading the log of a machine
/// that would not serve its logon screen does.
#[must_use]
pub fn run_worker() -> u32 {
    let Some(mut link) = HostLink::connect() else {
        tracing::error!(
            "logon-screen worker: no host is listening, or this process is not admitted to the \
             channel. Exiting so the host can decide whether to start another one."
        );
        return 1;
    };
    let Some(mut events) = link.events() else {
        return 1;
    };

    // Said before anything else: the host's state machine treats it as "there
    // is a logon screen now" and answers it with the one command this worker
    // waits for.
    let session = lumepeer_service::agent_launch::current_session().unwrap_or_default();
    if !events.send(AgentEvent::Attached { session }) {
        tracing::error!("logon-screen worker: cannot tell the host it attached");
        return 1;
    }
    tracing::info!(session, "logon-screen worker attached to the host");

    let mut capture: Option<Capture> = None;
    while let Some(command) = link.recv() {
        match command {
            // Nothing, and not an oversight (ADR 0088 §3). There is no signed-
            // in person on this desktop to show a banner to, and a
            // `LocalSystem` process drawing a window on `Winlogon` would be a
            // worse answer than the question deserves. What discloses this
            // capture is the audit record the host writes when a guest is
            // admitted to a machine nobody is signed in to, and the indicator
            // that goes up the instant somebody signs in.
            AgentCommand::ShowIndicator { on } => {
                tracing::debug!(
                    on,
                    "logon-screen worker: no desktop here to put an indicator on"
                );
            }
            AgentCommand::StartCapture { monitor } => {
                // The monitor index is ignored, and the host always sends zero
                // (ADR 0087 §6). `Winlogon` is one desktop however many
                // screens are attached to the machine, and the capture below
                // takes it whole; an index would be a number with nothing to
                // select between.
                let _ = monitor;
                // Replacing rather than adding: one capture at a time.
                capture = None;
                match Capture::start() {
                    Some(started) => capture = Some(started),
                    None => {
                        let _ = events.send(AgentEvent::CaptureUnavailable {
                            reason: CaptureUnavailableReason::Failed,
                        });
                    }
                }
            }
            AgentCommand::StopCapture => capture = None,
            AgentCommand::PointerMove { x, y } => inject(InjectAction::Move { x, y }),
            AgentCommand::Press { logical } => inject(InjectAction::Press { logical }),
            AgentCommand::Release { logical } => inject(InjectAction::Release { logical }),
            // The secure desktop has no scroll to perform: there is nothing
            // scrollable on it, and ADR 0057's descriptor has no shape for a
            // wheel. Dropped rather than approximated as a key.
            AgentCommand::Wheel { .. } => {
                tracing::debug!(
                    "logon-screen worker: a wheel has no meaning on the secure desktop"
                );
            }
            AgentCommand::Shutdown => {
                tracing::info!("logon-screen worker: the host asked this worker to stop");
                break;
            }
        }
    }

    // Capture stops before the channel does, so the last thing this process
    // does is not a frame published into a mapping nobody is reading any more.
    drop(capture);
    let _ = events.send(AgentEvent::Detaching);
    WORKER_OK
}

/// Performs one already-authorized event on the secure desktop.
///
/// The same [`crate::secure_desktop_input::perform`] the one-shot input worker
/// of ADR 0057 calls, from a process that is already standing on `Winlogon`
/// rather than one launched to press a single key. What changes is the cost —
/// no process per event — and nothing else: the event was authorized by the
/// host, and this call is the last few metres of carrying it out.
fn inject(action: InjectAction) {
    if !crate::secure_desktop_input::perform(action) {
        tracing::warn!("logon-screen worker: the event was not accepted");
    }
}

/// A running capture of the logon screen.
///
/// A thread rather than a loop in `run_worker`, for the reason the session
/// agent has the same split: the command channel is a blocking read, and a
/// worker that only captured between commands would publish a frame whenever
/// a guest happened to type and never otherwise.
#[derive(Debug)]
struct Capture {
    running: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Capture {
    /// Starts publishing frames of the logon screen on a tick.
    ///
    /// `None` when the mapping the host created cannot be opened, which is the
    /// one failure worth reporting as "this screen has no picture": everything
    /// else — a capture that found nothing this tick, a frame that did not fit
    /// — is a tick that publishes nothing and tries again.
    fn start() -> Option<Self> {
        let writer = Writer::open(FrameChannel::LogonScreen)?;
        let running = Arc::new(AtomicBool::new(true));
        let thread = {
            let running = Arc::clone(&running);
            std::thread::spawn(move || publish_until_stopped(&writer, &running))
        };
        Some(Self {
            running,
            thread: Some(thread),
        })
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            tracing::warn!("logon-screen worker: the capture thread panicked on its way out");
        }
    }
}

/// Captures and publishes the logon screen until `running` is cleared.
///
/// The frame counter is this attachment's own and starts at one, exactly like
/// the session agent's: it exists so a reader can tell a fresh frame from one
/// it has already copied, and it means nothing across attachments.
fn publish_until_stopped(writer: &Writer, running: &AtomicBool) {
    let sequence = AtomicU32::new(0);
    let interval = Duration::from_millis(LOGON_SCREEN_CAPTURE_INTERVAL_MS);
    while running.load(Ordering::SeqCst) {
        let started = std::time::Instant::now();
        if let Some((width, height, pixels)) = crate::secure_desktop::capture() {
            if writer.write(width, height, &pixels) {
                sequence.fetch_add(1, Ordering::Relaxed);
            } else {
                // The only way this happens is a screen bigger than the
                // mapping, which `secure_desktop::capture` already fits down.
                // Publishing nothing is the honest answer: a partial frame is
                // a corrupt one, not a smaller one.
                tracing::warn!(width, height, "logon-screen worker: the frame did not fit");
            }
        }
        if let Some(remaining) = interval.checked_sub(started.elapsed()) {
            std::thread::sleep(remaining);
        }
    }
    // Nothing is serving this mapping any more, so nothing in it is the
    // current screen. The host clears it too — this is the copy that runs when
    // the worker stops on its own rather than being stopped (ADR 0088 §2).
    writer.clear();
}
