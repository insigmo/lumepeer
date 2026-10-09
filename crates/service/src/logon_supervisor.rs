//! The Linux sign-in screen's supervisor (ADR 0151).
//!
//! Run by systemd as `root` (`lumepeer-logon.service`). Two jobs, and nothing
//! else — no network, no capture, no input:
//!
//! - **Keep the owner's copy.** A client signed in as some account sends the
//!   entries a host needs (identity, device password, role, second factor,
//!   audit salt) over [`logon_enroll::SOCKET_PATH`] while its keyring is
//!   unlocked. The supervisor learns who is asking from the kernel
//!   (`SO_PEERCRED`), keeps one owner per machine, and writes the copy where
//!   only `root` can read it.
//! - **Decide when.** Once a second it asks logind which session is in front
//!   on `seat0`. When that is a sign-in screen, an owner is enrolled, and the
//!   owner has no graphical session of their own (their client would already
//!   be the host), it starts `lumepeer-desktop --logon-host` for that screen,
//!   with the environment that screen's display needs. When the screen goes —
//!   somebody signed in, or the seat switched — it stops it.
//!
//! The logon host itself decides whether it can host (ADR 0126 §3's split):
//! its exit code tells the supervisor whether to try again.

use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use lumepeer_service::logon_enroll::{
    self, OWNER_FILE, Reply, Request, SECRETS_FILE, SOCKET_PATH, STATE_DIR,
};
use lumepeer_service::logon_seat::{self, Session, XServer};
use lumepeer_service::{LOGON_HOST_ARG, LOGON_HOST_EXIT_NOT_ENABLED, LOGON_HOST_EXIT_ROLE_TAKEN};

/// How often the seat is looked at.
const TICK: Duration = Duration::from_secs(1);

/// How long a logon host is given to leave after `SIGTERM`.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// How long after a failed logon host the next one is started.
const RETRY_AFTER: Duration = Duration::from_secs(5);

/// The seat the console is.
const SEAT: &str = "seat0";

/// Runs the supervisor; returns only when it cannot start at all.
pub fn run() {
    if !rustix::process::geteuid().is_root() {
        tracing::error!("the sign-in screen's supervisor runs as root, started by systemd");
        std::process::exit(1);
    }
    let state = PathBuf::from(STATE_DIR);
    if let Err(error) = std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&state)
    {
        tracing::error!(%error, "cannot create {STATE_DIR}");
        std::process::exit(1);
    }
    let changed = Arc::new(AtomicBool::new(true));
    match listen() {
        Ok(listener) => {
            let changed = Arc::clone(&changed);
            let state = state.clone();
            if let Err(error) = std::thread::Builder::new()
                .name("logon-enroll".to_owned())
                .spawn(move || accept(&listener, &state, &changed))
            {
                tracing::error!(%error, "cannot start the enrolment listener");
            }
        }
        Err(error) => {
            tracing::error!(%error, "cannot listen on {SOCKET_PATH}; no account can enroll")
        }
    }
    tracing::info!("watching {SEAT} for the sign-in screen (ADR 0151)");
    supervise(&state, &changed);
}

/// Binds the enrolment socket, open to every local account: who asks is
/// read from the kernel per connection, not from the file's mode.
fn listen() -> std::io::Result<UnixListener> {
    let path = Path::new(SOCKET_PATH);
    if let Some(parent) = path.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o755)
            .create(parent)?;
    }
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o666))?;
    Ok(listener)
}

fn accept(listener: &UnixListener, state: &Path, changed: &AtomicBool) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                if serve_one(stream, state) {
                    changed.store(true, Ordering::Relaxed);
                }
            }
            Err(error) => tracing::warn!(%error, "enrolment connection failed"),
        }
    }
}

/// Answers one client; `true` when what is kept changed.
fn serve_one(mut stream: UnixStream, state: &Path) -> bool {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let uid = match rustix::net::sockopt::socket_peercred(&stream) {
        Ok(cred) => cred.uid.as_raw(),
        Err(error) => {
            tracing::warn!(%error, "cannot tell who is enrolling; refused");
            let _ = stream.write_all(&[Reply::Refused as u8]);
            return false;
        }
    };
    let (reply, changed) = match logon_enroll::read_request(&mut stream) {
        Ok(request) => handle(&request, uid, state),
        Err(error) => {
            tracing::warn!(uid, %error, "malformed enrolment request; refused");
            (Reply::Refused, false)
        }
    };
    let _ = stream.write_all(&[reply as u8]);
    changed
}

/// The owner policy (ADR 0126 §3, kept on Linux): the first account to enroll
/// owns the sign-in screen; only it can change or withdraw its copy, and
/// `root` can withdraw any.
fn handle(request: &Request, uid: u32, state: &Path) -> (Reply, bool) {
    let owner = read_owner(state);
    match request {
        Request::Status => match owner {
            Some(owner) if owner == uid && state.join(SECRETS_FILE).is_file() => {
                (Reply::Yours, false)
            }
            Some(owner) if owner != uid => (Reply::OtherOwner, false),
            _ => (Reply::NotEnrolled, false),
        },
        Request::Enroll(entries) => {
            if owner.is_some_and(|owner| owner != uid) {
                tracing::info!(
                    uid,
                    "another account hosts the sign-in screen; enrolment refused"
                );
                return (Reply::OtherOwner, false);
            }
            if uid == 0 {
                // root has no keyring and no client; nothing it enrolls is a
                // host anybody saved.
                return (Reply::Refused, false);
            }
            match write_copy(state, uid, entries) {
                Ok(()) => {
                    tracing::info!(
                        uid,
                        entries = entries.len(),
                        "the sign-in screen will be hosted with this account's copy"
                    );
                    (Reply::Yours, true)
                }
                Err(error) => {
                    tracing::error!(uid, %error, "cannot keep the enrolled copy");
                    (Reply::Failed, false)
                }
            }
        }
        Request::Withdraw => match owner {
            None => (Reply::NotEnrolled, false),
            Some(owner) if owner != uid && uid != 0 => (Reply::OtherOwner, false),
            Some(_) => {
                let removed = remove_if_present(&state.join(SECRETS_FILE))
                    && remove_if_present(&state.join(OWNER_FILE));
                if removed {
                    tracing::info!(uid, "hosting the sign-in screen was withdrawn");
                    (Reply::Yours, true)
                } else {
                    (Reply::Failed, false)
                }
            }
        },
    }
}

fn read_owner(state: &Path) -> Option<u32> {
    std::fs::read_to_string(state.join(OWNER_FILE))
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn write_copy(state: &Path, uid: u32, entries: &[(String, Vec<u8>)]) -> std::io::Result<()> {
    let encoded = logon_enroll::encode_entries(entries)?;
    write_private(&state.join(SECRETS_FILE), &encoded)?;
    write_private(&state.join(OWNER_FILE), format!("{uid}\n").as_bytes())
}

/// Writes `bytes` to `path` through a fresh `0600` file renamed over it, so a
/// reader sees the old copy or the new one and never half of either.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let temporary = path.with_extension("new");
    let _ = std::fs::remove_file(&temporary);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&temporary, path)
}

fn remove_if_present(path: &Path) -> bool {
    match std::fs::remove_file(path) {
        Ok(()) => true,
        Err(error) => error.kind() == std::io::ErrorKind::NotFound,
    }
}

/// A logon host this supervisor started.
struct Hosting {
    child: Child,
    /// The sign-in screen's session it serves.
    session: String,
}

/// What the last logon host said about trying again.
enum Backoff {
    /// Nothing to wait for.
    None,
    /// It refused for a reason only a change fixes: wait for the screen or
    /// the enrolment to change.
    UntilChange { session: String },
    /// It failed: try again after a while.
    Until(Instant),
}

fn supervise(state: &Path, changed: &AtomicBool) {
    let mut hosting: Option<Hosting> = None;
    let mut backoff = Backoff::None;
    loop {
        if changed.swap(false, Ordering::Relaxed) {
            // A new copy, or none: whatever runs was started with the old one.
            if let Some(running) = hosting.take() {
                stop(running);
            }
            backoff = Backoff::None;
        }
        if let Some(running) = &mut hosting
            && let Ok(Some(status)) = running.child.try_wait()
        {
            let session = running.session.clone();
            hosting = None;
            backoff = after_exit(status, session);
        }

        let screen = enrolled(state).then(sign_in_screen).flatten();
        match (screen, &hosting) {
            (Some((session, _)), Some(running)) if running.session == session.id => {}
            (Some((session, environment)), _) => {
                if let Some(running) = hosting.take() {
                    stop(running);
                }
                let waiting = match &backoff {
                    Backoff::None => false,
                    Backoff::UntilChange { session: refused } => *refused == session.id,
                    Backoff::Until(when) => Instant::now() < *when,
                };
                if !waiting {
                    backoff = Backoff::None;
                    hosting = start(&session, &environment);
                }
            }
            (None, Some(_)) => {
                if let Some(running) = hosting.take() {
                    tracing::info!(session = %running.session, "the sign-in screen is gone; stopping its host");
                    stop(running);
                }
            }
            (None, None) => {
                if matches!(backoff, Backoff::UntilChange { .. }) {
                    backoff = Backoff::None;
                }
            }
        }
        std::thread::sleep(TICK);
    }
}

fn after_exit(status: ExitStatus, session: String) -> Backoff {
    let code = status.code().and_then(|code| u32::try_from(code).ok());
    match code {
        Some(0) => {
            tracing::info!(%session, "the logon host left");
            Backoff::UntilChange { session }
        }
        Some(LOGON_HOST_EXIT_NOT_ENABLED | LOGON_HOST_EXIT_ROLE_TAKEN) => {
            tracing::info!(%session, ?code, "the logon host declined; waiting for the screen or the copy to change");
            Backoff::UntilChange { session }
        }
        _ => {
            tracing::warn!(%session, %status, "the logon host failed; trying again shortly");
            Backoff::Until(Instant::now() + RETRY_AFTER)
        }
    }
}

fn enrolled(state: &Path) -> bool {
    read_owner(state).is_some() && state.join(SECRETS_FILE).is_file()
}

/// The sign-in screen in front on the seat and the environment its logon host
/// needs, or `None` when there is none to serve — including when the owner
/// is signed in somewhere graphical, whose own client is the host then.
fn sign_in_screen() -> Option<(Session, Vec<(String, String)>)> {
    let active = loginctl(&["show-seat", SEAT, "-p", "ActiveSession", "--value"])?;
    let active = active.trim();
    if active.is_empty() {
        return None;
    }
    let session = show_session(active)?;
    if !session.sign_in_screen() {
        return None;
    }
    let owner = read_owner(Path::new(STATE_DIR))?;
    if owner_signed_in(owner) {
        return None;
    }
    let environment = environment(&session)?;
    Some((session, environment))
}

fn show_session(id: &str) -> Option<Session> {
    let text = loginctl(&[
        "show-session",
        id,
        "-p",
        "Id",
        "-p",
        "Class",
        "-p",
        "Type",
        "-p",
        "User",
        "-p",
        "Display",
        "-p",
        "VTNr",
        "-p",
        "State",
    ])?;
    logon_seat::session(&text)
}

/// Whether `uid` has a graphical session anywhere on this machine — a
/// background one after a user switch included. An SSH login does not count.
fn owner_signed_in(uid: u32) -> bool {
    let Some(ids) = loginctl(&["show-user", &uid.to_string(), "-p", "Sessions", "--value"]) else {
        // No such user is logged in at all.
        return false;
    };
    logon_seat::session_ids(&ids)
        .iter()
        .filter_map(|id| show_session(id))
        .any(|session| session.graphical())
}

fn loginctl(args: &[&str]) -> Option<String> {
    let output = Command::new("loginctl").args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The environment the logon host draws on `session`'s screen with: the
/// display and its credentials for X11, the session's runtime directory and
/// bus for Wayland.
fn environment(session: &Session) -> Option<Vec<(String, String)>> {
    let uid = session.uid?;
    let runtime = PathBuf::from(format!("/run/user/{uid}"));
    let mut environment = vec![
        ("LUMEPEER_LOGON_SESSION".to_owned(), session.id.clone()),
        ("XDG_SESSION_TYPE".to_owned(), session.kind.clone()),
    ];
    match session.kind.as_str() {
        "x11" => {
            let servers = x_servers();
            let server =
                logon_seat::pick_x_server(&servers, session.display.as_deref(), session.vt)?;
            let display = session.display.clone().or_else(|| server.display.clone())?;
            environment.push(("DISPLAY".to_owned(), display));
            environment.push((
                "XAUTHORITY".to_owned(),
                server.auth.as_ref()?.to_string_lossy().into_owned(),
            ));
        }
        "wayland" => {
            let names: Vec<String> = std::fs::read_dir(&runtime)
                .ok()?
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect();
            let socket = logon_seat::wayland_socket(names.iter().map(String::as_str))?;
            let gid = std::fs::metadata(&runtime).ok()?.gid();
            environment.push(("WAYLAND_DISPLAY".to_owned(), socket));
            environment.push((
                "XDG_RUNTIME_DIR".to_owned(),
                runtime.to_string_lossy().into_owned(),
            ));
            environment.push((
                "DBUS_SESSION_BUS_ADDRESS".to_owned(),
                format!("unix:path={}/bus", runtime.display()),
            ));
            // The logon host starts its screen-side helper as this account
            // (ADR 0151): the sign-in screen's compositor answers its own
            // account on its own bus, and nobody else.
            environment.push(("LUMEPEER_GREETER_UID".to_owned(), uid.to_string()));
            environment.push(("LUMEPEER_GREETER_GID".to_owned(), gid.to_string()));
        }
        _ => return None,
    }
    Some(environment)
}

/// Every running `Xorg`, read from `/proc`.
fn x_servers() -> Vec<XServer> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.bytes().all(|b| b.is_ascii_digit()))
        })
        .filter_map(|entry| std::fs::read(entry.path().join("cmdline")).ok())
        .filter_map(|cmdline| logon_seat::x_server(&cmdline))
        .collect()
}

fn start(session: &Session, environment: &[(String, String)]) -> Option<Hosting> {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe.with_file_name("lumepeer-desktop"),
        Err(error) => {
            tracing::error!(%error, "cannot find this binary, so not the desktop binary beside it");
            return None;
        }
    };
    let mut command = Command::new(&exe);
    command
        .arg(LOGON_HOST_ARG)
        .env_clear()
        .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
        .env("HOME", STATE_DIR)
        .env("LANG", "C.UTF-8")
        .envs(environment.iter().map(|(key, value)| (key, value)))
        .current_dir(STATE_DIR);
    match command.spawn() {
        Ok(child) => {
            tracing::info!(session = %session.id, kind = %session.kind, pid = child.id(), "started the logon host for the sign-in screen");
            Some(Hosting {
                child,
                session: session.id.clone(),
            })
        }
        Err(error) => {
            tracing::error!(exe = %exe.display(), %error, "cannot start the logon host");
            None
        }
    }
}

/// Asks the logon host to leave, and makes it if it does not.
fn stop(mut hosting: Hosting) {
    if let Ok(Some(_)) = hosting.child.try_wait() {
        return;
    }
    if let Some(pid) = i32::try_from(hosting.child.id())
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    {
        let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
    }
    let deadline = Instant::now() + STOP_TIMEOUT;
    while Instant::now() < deadline {
        if let Ok(Some(_)) = hosting.child.try_wait() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    tracing::warn!(session = %hosting.session, "the logon host did not leave in time; killing it");
    let _ = hosting.child.kill();
    let _ = hosting.child.wait();
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn fresh(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lumepeer-logon-supervisor-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn entries() -> Vec<(String, Vec<u8>)> {
        vec![("lumepeer.endpoint.identity".to_owned(), vec![1; 64])]
    }

    #[test]
    fn the_first_account_owns_and_another_cannot_take_over() {
        let state = fresh("owner");
        assert_eq!(handle(&Request::Status, 1000, &state).0, Reply::NotEnrolled);
        assert_eq!(
            handle(&Request::Enroll(entries()), 1000, &state),
            (Reply::Yours, true)
        );
        assert_eq!(handle(&Request::Status, 1000, &state).0, Reply::Yours);
        assert_eq!(handle(&Request::Status, 1001, &state).0, Reply::OtherOwner);
        assert_eq!(
            handle(&Request::Enroll(entries()), 1001, &state),
            (Reply::OtherOwner, false)
        );
        assert_eq!(
            handle(&Request::Withdraw, 1001, &state).0,
            Reply::OtherOwner
        );
        assert_eq!(read_owner(&state), Some(1000));
        let kept = logon_enroll::decode_entries(&std::fs::read(state.join(SECRETS_FILE)).unwrap())
            .unwrap();
        assert_eq!(kept, entries());
    }

    #[test]
    fn the_owner_or_root_withdraws_and_then_anybody_may_enroll() {
        let state = fresh("withdraw");
        handle(&Request::Enroll(entries()), 1000, &state);
        assert_eq!(handle(&Request::Withdraw, 0, &state), (Reply::Yours, true));
        assert!(!enrolled(&state));
        assert_eq!(
            handle(&Request::Withdraw, 1000, &state).0,
            Reply::NotEnrolled
        );
        assert_eq!(
            handle(&Request::Enroll(entries()), 1001, &state).0,
            Reply::Yours
        );
        assert_eq!(
            handle(&Request::Withdraw, 1001, &state),
            (Reply::Yours, true)
        );
    }

    #[test]
    fn root_enrolls_nothing() {
        let state = fresh("root");
        assert_eq!(
            handle(&Request::Enroll(entries()), 0, &state).0,
            Reply::Refused
        );
        assert!(!enrolled(&state));
    }

    #[test]
    fn the_copy_is_private() {
        let state = fresh("mode");
        handle(&Request::Enroll(entries()), 1000, &state);
        for file in [SECRETS_FILE, OWNER_FILE] {
            let mode = std::fs::metadata(state.join(file)).unwrap().mode() & 0o777;
            assert_eq!(mode, 0o600, "{file}");
        }
    }
}
