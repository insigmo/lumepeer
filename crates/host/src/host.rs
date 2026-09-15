//! What `LumepeerHost` actually does when it runs (ADR 0085).
//!
//! Six steps, and the order is load-bearing:
//!
//! 1. **Take the host role**, or do not host. Two hosts on one machine is not
//!    a degraded mode, it is a contradiction (ADR 0085 §4), and the token is
//!    asked for before anything else so that a machine whose desktop client is
//!    already hosting never has a second `SessionManager` built for it at all.
//! 2. **Protect the machine store**, or do not host. A host that wrote a
//!    device-password hash into a directory whose access list it could not
//!    apply would be offering exactly the protection it failed to establish.
//! 3. **Open the identity and bind the endpoint.**
//! 4. **Spawn the actor**, with the agent screen as its window seam and with
//!    no capture backend of its own.
//! 5. **Supervise the agent** until asked to stop.
//! 6. **Give the role back**, which happens by the guard dropping — including
//!    when this process is killed, because the kernel releases a mutex whose
//!    holder died.
//!
//! Steps 1 and 2 are refusals rather than warnings, and that is the difference
//! between this and the desktop client. A client that cannot do something
//! still has a person in front of it who can see that it did not; a service
//! has nobody, so a half-configured one that started anyway would be a machine
//! quietly listening on the network in a state nobody chose.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ed25519_dalek::SigningKey;
use lumepeer_net::PeerEndpoint;
use lumepeer_net::keystore::load_or_create;
use lumepeer_runtime::network::{ActorPolicy, default_capture, spawn_actor_with};
use lumepeer_service::host_role::HostRoleClaim;

pub use crate::agent::SUPERVISION_TICK;
use crate::view::{AgentScreen, AgentViewWindows};

/// Hosts this machine until `stopping` is set.
///
/// Returns on every refusal too, which is what makes a misconfigured host a
/// service that stops rather than one that runs and admits nobody without
/// saying why.
pub fn run(stopping: &AtomicBool) {
    // Before the runtime, before the endpoint, before the store: if something
    // else on this machine is hosting, none of the rest should be built.
    let role = match lumepeer_service::host_role::claim() {
        HostRoleClaim::Held(role) => role,
        HostRoleClaim::Taken => {
            tracing::warn!(
                "something else on this machine already holds the host role — most likely the \
                 Lumepeer window, which a person is signed in to. This service will not be a \
                 second host. Close that window, or ask it to hand the role over, and start \
                 this service again."
            );
            return;
        }
        HostRoleClaim::Unavailable => {
            // A shipped service runs as `LocalSystem` and never lands here;
            // an unelevated `--console` run always does. Refusing rather than
            // hosting anyway, unlike the desktop client, which treats the same
            // answer as permission: a client that cannot read the token is one
            // process on a machine with a person at it, and a service that
            // cannot read it is a process that also cannot launch an agent,
            // reach the machine store or serve a screen. Hosting on that would
            // be a listening endpoint and nothing behind it.
            tracing::error!(
                "cannot read this machine's host role. A service run as LocalSystem can; an \
                 unelevated --console run cannot, and there is nothing useful it could do if \
                 it hosted anyway."
            );
            return;
        }
    };

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::error!(%error, "cannot start the async runtime");
            return;
        }
    };
    runtime.block_on(host(stopping));
    // Said after the runtime is done rather than left to `Drop`, so the log
    // records the moment the machine became hostable again.
    drop(role);
    tracing::info!("the host role is free again");
}

/// The host, once the role is held and there is a runtime to build it in.
async fn host(stopping: &AtomicBool) {
    let Some((stores, directory)) = crate::stores::open().await else {
        tracing::error!(
            "cannot protect this machine's Lumepeer store, so there is nowhere safe to keep a \
             device password. Not hosting."
        );
        return;
    };
    tracing::info!(path = %directory.display(), "machine store ready");

    let secret_key = match load_or_create(stores.keystore.as_ref()) {
        Ok(key) => key,
        Err(error) => {
            tracing::error!(%error, "cannot open this machine's identity; not hosting");
            return;
        }
    };
    let identity = SigningKey::from_bytes(&secret_key.to_bytes());

    // The settings a service host reads are the machine's, and there are none
    // yet: `lumepeer_runtime::config` resolves per-user paths, which is
    // exactly what a `LocalSystem` process must not read. The defaults are the
    // same relay and the same transport preference the client ships with, and
    // ADR 0087 records making them configurable as not done.
    let settings = lumepeer_runtime::config::Settings::default();
    let endpoint = match PeerEndpoint::bind_with_lan(secret_key, settings.relay_url()).await {
        Ok(endpoint) => endpoint,
        Err(error) => {
            tracing::error!(%error, "cannot bind this machine's endpoint; not hosting");
            return;
        }
    };

    let screen = Arc::new(AgentScreen::new());
    // No capture backend is compiled into this binary (see the crate header),
    // so this resolves to the stub capturer and to `MediaHealth::
    // without_capture` — the honest state for a process with no screen, and
    // what a guest is told until an agent attaches.
    let mut media = default_capture();
    // The injector is the exception, and the one place this host reaches a
    // desktop at all. It is not an injector: it forwards an event the actor
    // already authorized to the agent, which performs it as the signed-in user
    // (`input.rs`). `default_capture` leaves this `None` on a build with no
    // backend, which would make every session on a service host view-only.
    media.injector = Some(Box::new(crate::input::AgentInjector::new(Arc::clone(
        &screen,
    ))));
    let handle = spawn_actor_with(
        endpoint.clone(),
        identity,
        Arc::new(AgentViewWindows::new(Arc::clone(&screen))),
        media,
        lumepeer_runtime::clipboard_os::platform_clipboard(),
        stores,
        // This process holds the token, which it took above and holds until it
        // exits. Nothing else may host while it does.
        ActorPolicy::hosting(settings.obfuscated()),
    );

    tokio::spawn({
        let online = handle.online_flag();
        async move {
            endpoint.online().await;
            online.store(true, Ordering::Relaxed);
            tracing::info!(
                "endpoint reached a relay; this machine is dialable from outside the LAN"
            );
        }
    });

    // The supervision loop is three blocking threads deep (`agent.rs`), so it
    // gets a thread of its own rather than a task: a `ConnectNamedPipe` on a
    // runtime worker would hold that worker for as long as nobody is signed
    // in, which on a machine at its logon screen is forever.
    let supervision = {
        let screen = Arc::clone(&screen);
        // The flag is `service.rs`'s `'static` static under the SCM, and a
        // local that outlives this call under `--console`. Neither can be
        // borrowed into a `spawn`, so the flag the thread reads is its own and
        // this task mirrors the other one onto it.
        let mirror = Arc::new(AtomicBool::new(false));
        let thread = {
            let mirror = Arc::clone(&mirror);
            let screen = Arc::clone(&screen);
            std::thread::spawn(move || crate::agent::supervise(&screen, &mirror))
        };
        (thread, mirror)
    };
    let (supervision_thread, supervision_stop) = supervision;

    tracing::info!("hosting this machine");
    while !stopping.load(Ordering::SeqCst) {
        tokio::time::sleep(SUPERVISION_TICK).await;
    }

    tracing::info!("stopping");
    supervision_stop.store(true, Ordering::SeqCst);
    // Joined, not abandoned: the thread is what tells the agent to drop its
    // indicator and stop capture, and a process that exited before it ran
    // would leave a banner on somebody's screen until they signed out.
    if supervision_thread.join().is_err() {
        tracing::warn!("the supervision thread panicked on its way out");
    }
    // Nothing left with a screen, so nothing left that could be believed to
    // have one.
    screen.attach_commands(None);
}
