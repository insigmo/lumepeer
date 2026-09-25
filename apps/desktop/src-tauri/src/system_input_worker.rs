//! This application, running as a machine's `LocalSystem` desktop injector
//! (ADR 0114).
//!
//! Launched by the helper service with one argument
//! ([`lumepeer_service::SYSTEM_INPUT_WORKER_ARG`]) into the console session's
//! `WinSta0\Default` desktop — but, unlike the session agent
//! (`session_agent.rs`), as `LocalSystem` rather than as the signed-in user.
//! That is the whole point of it: `SendInput` from `LocalSystem` reaches a
//! System-integrity foreground window that the unelevated host's own
//! `SendInput` is dropped in front of under UIPI, which is exactly what a
//! `VMware` guest's `MKSEmbedded` window is (docs/bugs/17-remote-hotkeys.md).
//!
//! **It injects and nothing else.** No actor, no endpoint, no keystore, no
//! tray, no window, and — the difference from the agent that earns it the
//! `LocalSystem` token — no capture. A `LocalSystem` process that read the
//! screen would be a machine-wide disclosure risk (ADR 0085 §1); this one only
//! performs input the host already authorized in `lumepeer-core`, so its
//! compromise is bounded by what that authorization already allows (ADR 0114).
//!
//! **Every event arriving here was authorized before the channel was touched.**
//! Nothing in this file consults a grant, a role or a peer, and nothing could —
//! the wire carries none. It reads a [`DesktopInjectEvent`], turns it back into
//! the [`InputEventPayload`] the host started with — `logical`, `scancode` *and*
//! `modifiers`, the two the agent's own wire drops and the ones the scan-code
//! chord path needs (the v0.0.92 fix) — and hands it to the very same
//! [`WindowsInjector`](lumepeer_media::capture) the host would have run
//! in-process, grab-and-relative pointer logic and all. One injector is held for
//! the whole run rather than rebuilt per event, so that state survives across
//! events exactly as it does in-process.

use lumepeer_core::protocol::{InputDetail, InputEventPayload};
use lumepeer_media::capture::{InputInjector, platform_injector};
use lumepeer_runtime::config;
use lumepeer_service::desktop_input_channel::InjectorLink;
use lumepeer_service::protocol::{DesktopInjectDetail, DesktopInjectEvent};

/// Runs this process as the desktop injector and never returns.
pub fn run() -> ! {
    // Logging first, so the injector's own view of a grab — the
    // "sending relative motion" line the host used to log in-process — lands in
    // the same file the operator reads. The appender opens in append mode, so a
    // second writer alongside the host is fine (§16.1).
    let (settings, _notes) = config::Settings::load();
    let _ = crate::logging::init(&settings);
    tracing::info!("starting as this machine's desktop injector (ADR 0114)");
    serve();
    std::process::exit(0);
}

/// Attaches to the service and performs what it forwards until the channel ends.
fn serve() {
    let Some(mut link) = InjectorLink::connect() else {
        tracing::error!(
            "no service is listening on the desktop injector channel, or this process is not \
             admitted to it. Exiting so the service can decide whether to start another one."
        );
        return;
    };
    let mut injector: Box<dyn InputInjector> = match platform_injector() {
        Ok(injector) => injector,
        Err(error) => {
            tracing::error!(%error, "this session has no input adapter; exiting");
            return;
        }
    };
    tracing::info!("desktop injector attached; performing forwarded input");

    while let Some(event) = link.recv() {
        if let Err(error) = injector.inject(&to_payload(event)) {
            tracing::warn!(%error, "an authorized input event was refused by this desktop");
        }
    }
    tracing::info!("the desktop injector channel closed; exiting");
}

/// Rebuilds the host's own [`InputEventPayload`] from the wire event.
///
/// The inverse of the host's `desktop_inject_event`: it carries `logical`,
/// `scancode` and `modifiers` precisely so this reconstruction is exact, and the
/// injector reproduces the same chord and grab behaviour it would in-process.
fn to_payload(event: DesktopInjectEvent) -> InputEventPayload {
    let detail = match event.detail {
        DesktopInjectDetail::Press => InputDetail::Press,
        DesktopInjectDetail::Release => InputDetail::Release,
        DesktopInjectDetail::Move { x, y } => InputDetail::PointerMove { x, y },
        DesktopInjectDetail::Wheel { dx, dy } => InputDetail::Wheel { dx, dy },
    };
    InputEventPayload {
        logical: event.logical,
        scancode: event.scancode,
        modifiers: event.modifiers,
        detail,
    }
}
