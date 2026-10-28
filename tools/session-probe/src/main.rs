//! Pack-01 session-host probe (throwaway; docs/unelevated-client/
//! 01-system-in-session-probe.md). Measures what a `LocalSystem` process the
//! service launches into the console session on `WinSta0\Default` can actually
//! do, for ADR 0169. Not part of the lumepeer workspace or any release build.
//!
//! Roles, by first argument:
//!   --run           elevated controller (the owner's one UAC click): installs
//!                   a LocalSystem service, starts it, waits, removes it.
//!   --service       the service body (SCM starts it as LocalSystem, session 0).
//!   --measure       the nine measurements (LocalSystem, console session).
//!   --medium-poke T the medium-integrity poke against the 2.5 dialog titled T.

#[cfg(windows)]
mod measure;
#[cfg(windows)]
mod poke;
#[cfg(windows)]
mod svc;
#[cfg(windows)]
mod tray;
#[cfg(windows)]
mod win;

#[cfg(windows)]
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let flag = args.get(1).map(String::as_str);
    match flag {
        Some("--service") => svc::dispatch(),
        Some("--measure") => std::process::exit(i32::try_from(measure::run()).unwrap_or(1)),
        Some("--medium-poke") => {
            let title = args.get(2).map(String::as_str).unwrap_or("Lumepeer Session Probe");
            std::process::exit(i32::try_from(poke::run(title)).unwrap_or(1));
        }
        Some("--run") => match svc::run_controller() {
            Ok(()) => {
                println!("done — report: {}", win::report_path().display());
            }
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        },
        _ => {
            eprintln!(
                "session-probe (pack 01). Run elevated:\n  session-probe.exe --run\n\
                 It installs a temporary LocalSystem service, measures from the \
                 user's session, writes the report, and removes itself."
            );
            std::process::exit(2);
        }
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("session-probe is a Windows-only pack-01 tool");
    std::process::exit(1);
}
