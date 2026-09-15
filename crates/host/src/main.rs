//! `LumepeerHost` — the session-0 host service (ADR 0085).
//!
//! The other half of `docs/tasks/14-release-infrastructure.md` task 4, the
//! half [ADR 0043](../../../docs/adr/0043-one-privileged-helper-with-one-capability.md)
//! deliberately did not deliver: a host that survives a sign-out, because it
//! never lived inside anybody's session in the first place.
//!
//! **This is not `LumepeerHelper` with a network bolted on.** ADR 0085 §1's
//! table is the whole argument for why they are two services, and the shape of
//! this binary is what makes the table true rather than aspirational:
//!
//! - It has **no local request endpoint**. Nothing an unprivileged process on
//!   this machine can open, connect to or write a byte at. The helper's own
//!   pipe admits interactive users by design, because the one thing they can
//!   ask for there is a Ctrl+Alt+Del on their own screen; there is no
//!   equivalently narrow thing to ask a host that holds the network, so it
//!   answers nobody locally.
//! - It **draws no pixels and presses no keys**. Not as a promise: this crate
//!   turns on none of `lumepeer-runtime`'s capture, encode or decode features,
//!   so there is no backend compiled into it to call. Pictures and keystrokes
//!   belong to the session agent, which runs as the signed-in user and can do
//!   nothing that user could not.
//! - It **authorizes exactly as the client does**, through the one
//!   `lumepeer-core` both link. A second implementation of who may do what,
//!   kept in step by review, is how a machine ends up admitting two different
//!   things depending on which process answered.
//!
//! Off Windows this binary exits with an explanation. ADR 0085's "what this
//! decision does not decide" says why there is no Linux or macOS daemon: those
//! platforms have their own session mechanisms, and a root daemon built by
//! analogy with this one would hold privileges for a design nobody has worked
//! out yet.

#![cfg_attr(not(target_os = "windows"), forbid(unsafe_code))]
#![allow(
    unreachable_pub,
    reason = "binary crate: `pub` marks this service's own surface, not an API"
)]

#[cfg(target_os = "windows")]
mod agent;
#[cfg(target_os = "windows")]
mod host;
#[cfg(target_os = "windows")]
mod input;
#[cfg(target_os = "windows")]
mod install;
#[cfg(target_os = "windows")]
mod service;
#[cfg(target_os = "windows")]
mod stores;
#[cfg(target_os = "windows")]
mod view;

fn main() {
    #[cfg(target_os = "windows")]
    {
        // A file, not stdout, for `crates/service`'s `log` module's reason: a
        // process the service control manager starts has no stdout, and a host
        // that cannot say why it refused an admission is a host nobody can
        // operate. Its own file rather than the helper's — one long-lived
        // host's sessions interleaved with a helper's per-click workers is a
        // log that tells neither story.
        let log_path = lumepeer_service::log::init(lumepeer_service::log::HOST_LOG_FILE);
        let args: Vec<String> = std::env::args().collect();

        if args.iter().any(|arg| arg == "--install") {
            match install::install() {
                Ok(()) => return,
                Err(error) => {
                    eprintln!("{error}");
                    std::process::exit(1);
                }
            }
        }
        if args.iter().any(|arg| arg == "--uninstall") {
            match install::uninstall() {
                Ok(()) => return,
                Err(error) => {
                    eprintln!("{error}");
                    std::process::exit(1);
                }
            }
        }

        if let Some(path) = &log_path {
            tracing::info!(path = %path.display(), "lumepeer-host is logging here");
        }

        // `--console` runs the same host in the foreground, as an ordinary
        // process, and is how everything below the privilege line gets
        // exercised without registering anything with the SCM. It is *not* a
        // way to get the privileges: an unelevated console run cannot take the
        // host role's `Global\` token and cannot call `WTSQueryUserToken`, so
        // it reports both honestly and hosts nothing. That is the point — the
        // failure is visible rather than silently degraded (§18).
        if args.iter().any(|arg| arg == "--console") {
            tracing::info!(
                "running in the foreground; this run has exactly the rights of the user who \
                 started it, which is not enough to host a machine"
            );
            host::run(&std::sync::atomic::AtomicBool::new(false));
            return;
        }

        service::dispatch();
    }

    #[cfg(not(target_os = "windows"))]
    {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            )
            .init();
        eprintln!(
            "lumepeer-host is a Windows service. Linux and macOS have their own session \
             mechanisms, and ADR 0085 deliberately does not build a root daemon by analogy \
             with this one."
        );
        std::process::exit(1);
    }
}
