//! This application, running as a machine's session agent (ADR 0085 §1, §3).
//!
//! Started by `LumepeerHost` with one argument
//! ([`lumepeer_service::SESSION_AGENT_ARG`]) into the console session, as the
//! signed-in user. It is the same binary as the desktop client and shares none
//! of its behaviour: no actor, no endpoint, no keystore, no tray and no main
//! window. What it has is a screen, which is the one thing the privileged host
//! does not.
//!
//! **The agent is told what to do and never asked what is permitted.** Every
//! command arriving here was authorized in `lumepeer-core`, inside the host,
//! before the channel was touched; nothing in this file consults a grant, a
//! role or a peer, and nothing could — the protocol carries none of them. The
//! events going the other way are facts about a desktop and nothing else.
//!
//! **Order, not intent, is what keeps "no hidden capture" true.** This module
//! does not decide when the indicator goes up. The host's [`SessionScreen`]
//! answers an attachment with `ShowIndicator` and *then* `StartCapture`, and
//! this loop carries the two out in the order it receives them. A guest cannot
//! arrange for a frame to leave a machine whose banner is not up, because the
//! command that starts capture is the second of the two and this side does
//! nothing until it is told to.
//!
//! **What this file must not grow.** It runs as a signed-in human, on their
//! desktop, driven by a `LocalSystem` process. It presses keys, moves a
//! pointer and reads a screen — and it must stay that and nothing more. There
//! is no path here that runs a program, opens a file or reaches the network:
//! the entire set of things it can be asked to do is
//! [`AgentCommand`]'s variants, and adding to that list is a visible change to
//! a protocol whose whole argument is that the list is short.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use lumepeer_media::capture::{CaptureTarget, InputInjector, platform_backend, platform_injector};
use lumepeer_media::encode::{EncoderConfig, select_encoder};
use lumepeer_service::agent_channel::HostLink;
use lumepeer_service::agent_protocol::{
    AGENT_FRAME_CAPACITY_BYTES, AgentCommand, AgentEvent, CaptureUnavailableReason,
};
use lumepeer_service::frame::{FrameChannel, Writer};

/// Label of the indicator window.
///
/// Its own, never the host bar's: the host bar is the client's surface, with
/// the revoke on it and an actor behind it, and this process has neither. A
/// shared label would mean the two could not both exist on one machine, which
/// is exactly the moment a handover happens (ADR 0085 §4).
const INDICATOR_LABEL: &str = "session-indicator";

/// Size of the indicator, in logical pixels.
const INDICATOR_WIDTH: f64 = 320.0;
/// See [`INDICATOR_WIDTH`].
const INDICATOR_HEIGHT: f64 = 52.0;

/// Gap between the indicator and the top edge of the screen, in logical
/// pixels.
const INDICATOR_TOP_MARGIN: f64 = 12.0;

/// How long the capture loop waits between frames when the screen has not
/// changed.
///
/// The backend already answers `None` for an unchanged screen (§11.1), so this
/// is the idle poll rather than a frame budget: fast enough that the first
/// frame after something moves is not visibly late, slow enough that a machine
/// nobody is using is not encoding at full rate for nothing.
const IDLE_POLL: std::time::Duration = std::time::Duration::from_millis(16);

/// Runs this process as a session agent and never returns.
///
/// A Tauri application rather than a bare loop, for one reason: the indicator
/// has to be a window, this crate is `#![forbid(unsafe_code)]`, and a window
/// without a toolkit would mean raw Win32 here. The application it builds has
/// no tray, no menu, no IPC surface and no main window — only whatever
/// [`indicator`] puts up.
pub fn run() -> ! {
    tracing::info!("starting as this machine's session agent");
    let app = tauri::Builder::default()
        .setup(|app| {
            let handle = app.handle().clone();
            // The channel is three blocking calls deep — connect, read, write
            // — so it gets a thread rather than a task on the UI loop.
            std::thread::spawn(move || {
                serve(&handle);
                // The host is gone, or told this agent to stop. Either way the
                // window has nothing left to say, and an indicator standing
                // over a session that ended would be the exact lie §2.2 is
                // about, pointing the other way.
                handle.exit(0);
            });
            Ok(())
        })
        .build(tauri::generate_context!())
        .unwrap_or_else(|error| {
            tracing::error!(%error, "cannot build the session agent application");
            std::process::exit(1);
        });
    app.run(|_, _| {});
    std::process::exit(0);
}

/// Attaches to the host and carries out what it asks until the channel ends.
fn serve(app: &tauri::AppHandle) {
    let Some(mut link) = HostLink::connect() else {
        tracing::error!(
            "no host is listening on the session agent channel, or this process is not admitted \
             to it. Exiting so the host can decide whether to start another one."
        );
        return;
    };
    let Some(mut events) = link.events() else {
        return;
    };

    // Said before anything else, because the host's state machine treats it as
    // "there is a screen now" and answers it with the indicator.
    let session = lumepeer_service::agent_launch::current_session().unwrap_or_default();
    if !events.send(AgentEvent::Attached { session }) {
        tracing::error!("cannot tell the host this agent attached");
        return;
    }
    tracing::info!(session, "attached to the host");

    let mut capture: Option<Capture> = None;
    while let Some(command) = link.recv() {
        match command {
            AgentCommand::ShowIndicator { on } => indicator(app, on),
            AgentCommand::StartCapture { monitor } => {
                // Replacing rather than adding: one capture at a time, so a
                // second `StartCapture` — a monitor change — stops the first.
                capture = None;
                match Capture::start(monitor, &link) {
                    Ok(started) => capture = Some(started),
                    Err(reason) => {
                        // The attachment stands and the indicator stays up:
                        // what this says is that *this session* has no picture
                        // to give, which is the honest `MediaUnavailable` the
                        // guest already knows how to receive (§18; ADR 0024).
                        let _ = events.send(AgentEvent::CaptureUnavailable { reason });
                    }
                }
            }
            AgentCommand::StopCapture => capture = None,
            AgentCommand::PointerMove { .. }
            | AgentCommand::Press { .. }
            | AgentCommand::Release { .. }
            | AgentCommand::Wheel { .. } => inject(command),
            AgentCommand::Shutdown => {
                tracing::info!("the host asked this agent to stop");
                break;
            }
        }
    }

    // Stop capture before the indicator comes down, which is the same ordering
    // rule as start, read backwards: a banner that dropped while pixels were
    // still leaving would be the window §2.2 forbids, just at the other end.
    drop(capture);
    indicator(app, false);
    let _ = events.send(AgentEvent::Detaching);
}

/// Raises or lowers the indicator the person at this machine cannot dismiss.
///
/// Its own window, undecorated, always on top, out of the taskbar and never
/// focused — it appears while somebody is working in another application, and
/// taking their keyboard at that moment would be worse than the problem it
/// solves. It is closable only by this process, which closes it only when the
/// attachment it stands for is over.
fn indicator(app: &tauri::AppHandle, on: bool) {
    use tauri::Manager as _;

    let app = app.clone();
    let queued = app.clone().run_on_main_thread(move || {
        match (on, app.get_webview_window(INDICATOR_LABEL)) {
            (false, None) => {}
            (true, Some(window)) => {
                // Already up. `show` is a no-op on a visible window and undoes
                // anything that hid rather than closed it.
                let _ = window.show();
            }
            (false, Some(window)) => {
                if let Err(error) = window.destroy() {
                    tracing::warn!(%error, "cannot take the session indicator down");
                }
            }
            (true, None) => open_indicator(&app),
        }
    });
    if let Err(error) = queued {
        tracing::warn!(%error, "cannot reach the main thread to move the session indicator");
    }
}

/// Builds the indicator, centred against the top edge of the primary screen.
fn open_indicator(app: &tauri::AppHandle) {
    let placement = app.primary_monitor().ok().flatten().map(|monitor| {
        let scale = monitor.scale_factor();
        let size = monitor.size().to_logical::<f64>(scale);
        let origin = monitor.position().to_logical::<f64>(scale);
        (
            origin.x + (size.width - INDICATOR_WIDTH) / 2.0,
            origin.y + INDICATOR_TOP_MARGIN,
        )
    });

    let mut builder = tauri::WebviewWindowBuilder::new(
        app,
        INDICATOR_LABEL,
        tauri::WebviewUrl::App("agentbar.html".into()),
    )
    .title("Lumepeer")
    .inner_size(INDICATOR_WIDTH, INDICATOR_HEIGHT)
    .decorations(false)
    .resizable(false)
    .always_on_top(true)
    .skip_taskbar(true)
    .focused(false);
    if let Some((x, y)) = placement {
        builder = builder.position(x, y);
    }
    match builder.build() {
        Ok(_) => tracing::info!("session indicator raised"),
        // Loud, and worth being loud about: the indicator failing is the one
        // failure that would let capture run with nothing on screen saying so.
        // The host is told by the `CaptureUnavailable` that follows, because
        // `Capture::start` is only reached after this.
        Err(error) => tracing::error!(%error, "cannot raise the session indicator"),
    }
}

/// Performs one already-authorized input event.
///
/// The injector is built per event rather than held. That is not a
/// micro-optimization in reverse: input on a service host arrives in bursts
/// with long gaps, `platform_injector` is cheap on Windows, and an adapter
/// held across a desktop switch is one that goes on pointing at a desktop
/// nobody is at. Failing to build one is a refusal to inject this event and
/// nothing more — the host learns from the guest's own session, which already
/// reports input faults (§18).
fn inject(command: AgentCommand) {
    use lumepeer_core::protocol::{InputDetail, InputEventPayload};

    let (logical, detail) = match command {
        AgentCommand::PointerMove { x, y } => (0, InputDetail::PointerMove { x, y }),
        AgentCommand::Press { logical } => (logical, InputDetail::Press),
        AgentCommand::Release { logical } => (logical, InputDetail::Release),
        AgentCommand::Wheel { dx, dy } => (0, InputDetail::Wheel { dx, dy }),
        // Not reachable: the caller matched these four. Doing nothing rather
        // than guessing is what every other refusal on this wire does.
        AgentCommand::ShowIndicator { .. }
        | AgentCommand::StartCapture { .. }
        | AgentCommand::StopCapture
        | AgentCommand::Shutdown => return,
    };

    let mut injector: Box<dyn InputInjector> = match platform_injector() {
        Ok(injector) => injector,
        Err(error) => {
            tracing::warn!(%error, "this session has no input adapter");
            return;
        }
    };
    if let Err(error) = injector.inject(&InputEventPayload {
        logical,
        scancode: 0,
        modifiers: 0,
        detail,
    }) {
        tracing::warn!(%error, "an authorized input event was refused by this desktop");
    }
}

/// A running capture-and-encode loop, stopped by dropping it.
struct Capture {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Capture {
    /// Starts publishing encoded frames of `monitor` into the shared mapping.
    ///
    /// The mapping is *opened*, never created: the host creates it with an
    /// access list naming this one user, and an agent that created its own
    /// would be publishing into something whose access list it chose.
    fn start(monitor: u32, link: &HostLink) -> Result<Self, CaptureUnavailableReason> {
        let Some(writer) = Writer::open(FrameChannel::SessionAgent) else {
            tracing::error!("the host has not published a frame mapping for this agent");
            return Err(CaptureUnavailableReason::Failed);
        };
        let (mut capturer, _) = platform_backend().map_err(|error| {
            tracing::warn!(%error, "this build has no capture backend");
            CaptureUnavailableReason::NoBackend
        })?;
        let encoder = select_encoder(EncoderConfig::default()).map_err(|error| {
            tracing::warn!(%error, "no encoder available in this session");
            CaptureUnavailableReason::Failed
        })?;
        capturer
            .start(CaptureTarget::Display(monitor))
            .map_err(|error| {
                tracing::warn!(%error, monitor, "cannot capture this display");
                CaptureUnavailableReason::NoDisplay
            })?;

        // A writing handle of its own: `serve` is blocked in `recv` with one
        // and this loop announces frames from another thread.
        let Some(mut events) = link.events() else {
            tracing::error!("cannot split the host channel for the capture loop");
            return Err(CaptureUnavailableReason::Failed);
        };
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let sequence = AtomicU32::new(0);
                let mut encoder = encoder;
                while !stop.load(Ordering::SeqCst) {
                    match capturer.next_frame() {
                        // The screen has not changed (§11.1). Nothing to
                        // publish and nothing to announce: a host that saw a
                        // sequence move would go and read the same bytes
                        // again.
                        Ok(None) => std::thread::sleep(IDLE_POLL),
                        Ok(Some(frame)) => {
                            let (width, height) = (frame.width, frame.height);
                            match encoder.encode(&frame) {
                                Ok(bitstream) => {
                                    let payload =
                                        lumepeer_runtime::view::encode_media_payload(&bitstream);
                                    if payload.len() > AGENT_FRAME_CAPACITY_BYTES {
                                        // Refused rather than truncated: a
                                        // partial bitstream is a corrupt one,
                                        // not a smaller one.
                                        tracing::warn!(
                                            bytes = payload.len(),
                                            "an encoded frame is too large for the mapping"
                                        );
                                        continue;
                                    }
                                    if !writer.write(width, height, &payload) {
                                        tracing::warn!("cannot publish a frame into the mapping");
                                        continue;
                                    }
                                    let next = sequence.fetch_add(1, Ordering::SeqCst);
                                    if !events.send(AgentEvent::FramePublished { sequence: next }) {
                                        // The channel has gone, so there is no
                                        // host to publish for.
                                        break;
                                    }
                                }
                                Err(error) => {
                                    tracing::warn!(%error, "dropping a frame this session could not encode");
                                }
                            }
                        }
                        Err(error) => {
                            // A screen lock, a desktop switch, a user switch.
                            // The host is told there is no picture and this
                            // loop ends; the supervision loop on the other
                            // side decides what happens next.
                            tracing::info!(%error, "capture was interrupted");
                            let _ = events.send(AgentEvent::CaptureUnavailable {
                                reason: CaptureUnavailableReason::Failed,
                            });
                            break;
                        }
                    }
                }
                capturer.stop();
            })
        };
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Joined rather than abandoned: the loop is what stops the backend,
        // and a second `StartCapture` arriving while the first was still
        // running would be two capturers on one desktop.
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            tracing::warn!("the capture loop panicked on its way out");
        }
    }
}
