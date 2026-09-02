//! Lumepeer's privileged helper service (design doc §11, §19; ADR 0043).
//!
//! One job: deliver the Secure Attention Sequence on behalf of the client
//! running in the interactive session. `crates/media/src/sas.rs` explains why
//! that needs privileges the session does not have — `SendSAS` is honoured
//! from a service in session 0, or from a process the user launched elevated,
//! and nothing else.
//!
//! The design rule here is that a privileged process reachable from an
//! unprivileged one is a local privilege escalation waiting to be written. So:
//!
//! - **One operation.** [`protocol`] can express `deliver the SAS` and nothing
//!   else. No paths, no strings, no lengths, no peer-driven allocation.
//! - **No network, no disk, no configuration.** This binary opens one endpoint
//!   and calls one Win32 function. It never reads a config file, so nothing a
//!   user can write changes what it does.
//! - **A DACL, not an honour system.** The pipe admits `LocalSystem`,
//!   administrators and interactive users. A network logon or a service
//!   account cannot connect at all.
//! - **Fixed frames.** Two bytes in, two bytes out, so a short read is an
//!   error rather than a state to reassemble.
//!
//! Off Windows this binary exits with an explanation. There is no SAS
//! mechanism on Linux or macOS, so a root daemon there would hold privileges
//! in order to do nothing, which is worse than not shipping one (ADR 0043).

#![cfg_attr(not(target_os = "windows"), forbid(unsafe_code))]
#![allow(
    unreachable_pub,
    reason = "binary crate: `pub` marks the service's own surface, not an API"
)]

#[cfg(target_os = "windows")]
mod install;
#[cfg(target_os = "windows")]
mod secure_desktop;
#[cfg(target_os = "windows")]
mod secure_desktop_input;
#[cfg(target_os = "windows")]
mod secure_desktop_launch;
#[cfg(target_os = "windows")]
mod windows_service;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    #[cfg(target_os = "windows")]
    {
        // `--console` runs the same listener in the foreground, as an ordinary
        // process. It is how the endpoint and the protocol are exercised
        // without registering anything with the SCM; it is *not* a way to get
        // the privileges, because an unelevated console run has exactly the
        // rights of the user who started it.
        // Registering and removing the service are the only things here that
        // need administrator rights, which is why they are flags on this
        // binary rather than a shell command line the client builds: the
        // client elevates *this*, and the elevated code is ours.
        let args: Vec<String> = std::env::args().collect();
        // The secure-desktop capture worker (ADR 0056): the service
        // re-executes this binary with this one argument into the console
        // session on `Winsta0\Winlogon`. It captures one frame and exits with
        // its outcome as the exit code, touching neither the SCM nor the
        // pipe. Checked first so it can never be confused with a normal run.
        if args
            .iter()
            .any(|arg| arg == lumepeer_service::SECURE_DESKTOP_WORKER_ARG)
        {
            std::process::exit(i32::try_from(secure_desktop_launch::run_worker()).unwrap_or(1));
        }
        // The secure-desktop *input* worker (ADR 0057): this binary,
        // re-executed onto `Winsta0\Winlogon` with the input arg followed by
        // four integers `kind logical x y`. It performs one event and exits
        // with the outcome. The integers were built by
        // `secure_desktop_launch::inject_via_worker` from a descriptor the
        // service already validated; `inject_from_args` validates them once
        // more here — the last gate before a `SendInput` — and a set that does
        // not parse exits non-zero, which the service reads as refusal.
        if let Some(position) = args
            .iter()
            .position(|arg| arg == lumepeer_service::SECURE_DESKTOP_INPUT_WORKER_ARG)
        {
            let action = args.get(position + 1..position + 5).and_then(|tail| {
                let parsed: Vec<u32> = tail.iter().filter_map(|a| a.parse().ok()).collect();
                match parsed.as_slice() {
                    &[kind, logical, x, y] => {
                        lumepeer_service::protocol::inject_from_args(kind, logical, x, y)
                    }
                    _ => None,
                }
            });
            let code = if let Some(action) = action {
                secure_desktop_launch::run_input_worker(action)
            } else {
                eprintln!("secure-desktop input worker: malformed arguments");
                1
            };
            std::process::exit(i32::try_from(code).unwrap_or(1));
        }
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
        if args.iter().any(|arg| arg == "--console") {
            tracing::info!("running in the foreground; SAS delivery has this process's rights");
            windows_service::serve_until_stopped(&std::sync::atomic::AtomicBool::new(false));
            return;
        }
        windows_service::dispatch();
    }

    #[cfg(not(target_os = "windows"))]
    {
        eprintln!(
            "lumepeer-service exists to deliver the Secure Attention Sequence, which only \
             Windows has. There is nothing for it to do on this platform."
        );
        std::process::exit(1);
    }
}
