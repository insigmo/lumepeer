//! What the Linux supervisor reads about the seat (ADR 0151), as pure
//! functions.
//!
//! The supervisor asks `loginctl` which session is in front on `seat0` and
//! what it is, and reads `/proc` for the X server of an X11 sign-in screen.
//! Everything that turns that text into a decision is here, apart from the
//! processes and files, so it is tested on every platform with the exact text
//! those tools print.

use std::collections::HashMap;
use std::path::PathBuf;

/// One logind session, as far as the supervisor cares.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Session {
    /// The session id.
    pub id: String,
    /// `greeter`, `user`, `lock-screen`, `background`, `manager`, …
    pub class: String,
    /// `x11`, `wayland`, `tty`, `unspecified`, …
    pub kind: String,
    /// The account the session runs as.
    pub uid: Option<u32>,
    /// The X display, for an X11 session that has one.
    pub display: Option<String>,
    /// The virtual terminal it sits on.
    pub vt: Option<u32>,
    /// `online`, `active` or `closing`.
    pub state: String,
}

impl Session {
    /// Whether this session draws a screen: an X11 or Wayland session that is
    /// not on its way out. A `tty` session — an SSH login among them — is not
    /// one, and must never count as somebody being signed in at the machine.
    #[must_use]
    pub fn graphical(&self) -> bool {
        matches!(self.kind.as_str(), "x11" | "wayland" | "mir") && self.state != "closing"
    }

    /// Whether this is a sign-in screen the logon host can stand in front of.
    #[must_use]
    pub fn sign_in_screen(&self) -> bool {
        self.class == "greeter" && self.graphical()
    }
}

/// Reads `Key=Value` lines, as `loginctl show-*` prints them.
#[must_use]
pub fn properties(text: &str) -> HashMap<&str, &str> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.trim(), value.trim()))
        .collect()
}

/// The session `loginctl show-session <id> -p Id -p Class -p Type -p User -p
/// Display -p VTNr -p State` described.
#[must_use]
pub fn session(text: &str) -> Option<Session> {
    let props = properties(text);
    let id = props.get("Id").filter(|id| !id.is_empty())?;
    let optional = |key: &str| {
        props
            .get(key)
            .filter(|value| !value.is_empty())
            .map(|value| (*value).to_owned())
    };
    Some(Session {
        id: (*id).to_owned(),
        class: optional("Class").unwrap_or_default(),
        kind: optional("Type").unwrap_or_default(),
        uid: props.get("User").and_then(|uid| uid.parse().ok()),
        display: optional("Display"),
        // logind prints 0 for a session on no terminal.
        vt: props
            .get("VTNr")
            .and_then(|vt| vt.parse().ok())
            .filter(|vt| *vt != 0),
        state: optional("State").unwrap_or_default(),
    })
}

/// The session ids `loginctl show-user <uid> -p Sessions --value` printed.
#[must_use]
pub fn session_ids(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_owned).collect()
}

/// An X server, from its command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XServer {
    /// `:0` and the like, when the command line names one. A server started
    /// with `-displayfd` picks its own and does not.
    pub display: Option<String>,
    /// The `vtN` it was started on.
    pub vt: Option<u32>,
    /// Its `-auth` file.
    pub auth: Option<PathBuf>,
}

/// Reads an X server's `/proc/<pid>/cmdline` (arguments separated by NUL), or
/// `None` when it is not an `Xorg`/`X` server at all.
#[must_use]
pub fn x_server(cmdline: &[u8]) -> Option<XServer> {
    let mut args = cmdline
        .split(|byte| *byte == 0)
        .filter(|arg| !arg.is_empty())
        .map(String::from_utf8_lossy);
    let program = args.next()?;
    let name = program.rsplit('/').next().unwrap_or(&program);
    if name != "Xorg" && name != "X" {
        return None;
    }
    let mut server = XServer {
        display: None,
        vt: None,
        auth: None,
    };
    let mut expect_auth = false;
    for arg in args {
        if expect_auth {
            server.auth = Some(PathBuf::from(arg.as_ref()));
            expect_auth = false;
        } else if arg == "-auth" {
            expect_auth = true;
        } else if let Some(number) = arg.strip_prefix(':') {
            if !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
                server.display = Some(arg.into_owned());
            }
        } else if let Some(vt) = arg.strip_prefix("vt") {
            server.vt = vt.parse().ok();
        }
    }
    Some(server)
}

/// The X server of a sign-in screen on `display` and `vt`: the one naming that
/// display, else the one on that terminal, else the only one there is.
#[must_use]
pub fn pick_x_server<'a>(
    servers: &'a [XServer],
    display: Option<&str>,
    vt: Option<u32>,
) -> Option<&'a XServer> {
    let with_auth: Vec<&XServer> = servers.iter().filter(|s| s.auth.is_some()).collect();
    if let Some(display) = display
        && let Some(server) = with_auth
            .iter()
            .find(|s| s.display.as_deref() == Some(display))
    {
        return Some(server);
    }
    if let Some(vt) = vt
        && let Some(server) = with_auth.iter().find(|s| s.vt == Some(vt))
    {
        return Some(server);
    }
    match with_auth.as_slice() {
        [only] => Some(only),
        _ => None,
    }
}

/// The Wayland socket of a runtime directory's listing: the lowest
/// `wayland-N`, never its `.lock`.
#[must_use]
pub fn wayland_socket<'a>(names: impl IntoIterator<Item = &'a str>) -> Option<String> {
    let mut sockets: Vec<&str> = names
        .into_iter()
        .filter(|name| {
            name.starts_with("wayland-")
                && !std::path::Path::new(name)
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("lock"))
        })
        .collect();
    sockets.sort_unstable();
    sockets.first().map(|name| (*name).to_owned())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// What `loginctl show-session` prints for GDM's sign-in screen on
    /// Debian 13, with the properties the supervisor asks for.
    const GDM_GREETER: &str =
        "Id=c1\nUser=111\nDisplay=\nVTNr=1\nState=active\nClass=greeter\nType=wayland\n";

    #[test]
    fn a_wayland_greeter_is_a_sign_in_screen_and_a_user_session_is_not() {
        let greeter = session(GDM_GREETER).unwrap();
        assert_eq!(greeter.id, "c1");
        assert_eq!(greeter.uid, Some(111));
        assert_eq!(greeter.display, None);
        assert_eq!(greeter.vt, Some(1));
        assert!(greeter.sign_in_screen());

        let user =
            session("Id=50\nUser=1000\nClass=user\nType=wayland\nState=active\nVTNr=2\n").unwrap();
        assert!(user.graphical());
        assert!(!user.sign_in_screen());
    }

    #[test]
    fn an_ssh_login_is_not_somebody_at_the_machine() {
        let ssh =
            session("Id=106\nUser=1000\nClass=user\nType=tty\nState=online\nVTNr=0\n").unwrap();
        assert!(!ssh.graphical());
        assert_eq!(ssh.vt, None);
    }

    #[test]
    fn a_closing_greeter_is_not_served() {
        let closing = session("Id=c2\nUser=111\nClass=greeter\nType=x11\nState=closing\n").unwrap();
        assert!(!closing.sign_in_screen());
    }

    #[test]
    fn no_id_is_no_session() {
        assert_eq!(session("Class=greeter\nType=x11\n"), None);
        assert_eq!(session(""), None);
    }

    #[test]
    fn session_ids_are_split_on_whitespace() {
        assert_eq!(session_ids("50 51 106\n"), vec!["50", "51", "106"]);
        assert!(session_ids("").is_empty());
    }

    fn cmdline(args: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        for arg in args {
            out.extend_from_slice(arg.as_bytes());
            out.push(0);
        }
        out
    }

    #[test]
    fn the_x_servers_of_lightdm_sddm_and_gdm_are_read() {
        let lightdm = x_server(&cmdline(&[
            "/usr/lib/xorg/Xorg",
            ":0",
            "-seat",
            "seat0",
            "-auth",
            "/var/run/lightdm/root/:0",
            "-nolisten",
            "tcp",
            "vt7",
            "-novtswitch",
        ]))
        .unwrap();
        assert_eq!(lightdm.display.as_deref(), Some(":0"));
        assert_eq!(lightdm.vt, Some(7));
        assert_eq!(
            lightdm.auth.as_deref(),
            Some(std::path::Path::new("/var/run/lightdm/root/:0"))
        );

        let sddm = x_server(&cmdline(&[
            "/usr/lib/xorg/Xorg",
            "-nolisten",
            "tcp",
            "-background",
            "none",
            "-seat",
            "seat0",
            "vt1",
            "-auth",
            "/run/sddm/xauth_AbCdEf",
            "-noreset",
            "-displayfd",
            "16",
        ]))
        .unwrap();
        assert_eq!(sddm.display, None);
        assert_eq!(sddm.vt, Some(1));

        let gdm = x_server(&cmdline(&[
            "/usr/lib/xorg/Xorg",
            "vt1",
            "-displayfd",
            "3",
            "-auth",
            "/run/user/111/gdm/Xauthority",
            "-nolisten",
            "tcp",
        ]))
        .unwrap();
        assert_eq!(gdm.vt, Some(1));
        assert!(gdm.auth.is_some());
    }

    #[test]
    fn other_programs_are_not_x_servers() {
        assert_eq!(x_server(&cmdline(&["/usr/bin/Xwayland", ":0"])), None);
        assert_eq!(x_server(&cmdline(&["/usr/bin/bash"])), None);
        assert_eq!(x_server(b""), None);
    }

    #[test]
    fn the_server_is_picked_by_display_then_terminal_then_alone() {
        let a = XServer {
            display: Some(":0".to_owned()),
            vt: Some(7),
            auth: Some("/a".into()),
        };
        let b = XServer {
            display: None,
            vt: Some(1),
            auth: Some("/b".into()),
        };
        let servers = [a.clone(), b.clone()];
        assert_eq!(pick_x_server(&servers, Some(":0"), Some(1)), Some(&a));
        assert_eq!(pick_x_server(&servers, Some(":5"), Some(1)), Some(&b));
        assert_eq!(pick_x_server(&servers, None, None), None);
        assert_eq!(
            pick_x_server(std::slice::from_ref(&b), None, None),
            Some(&b)
        );
        let no_auth = XServer { auth: None, ..b };
        assert_eq!(pick_x_server(&[no_auth], None, None), None);
    }

    #[test]
    fn the_lowest_wayland_socket_is_chosen_and_never_its_lock() {
        assert_eq!(
            wayland_socket([
                "bus",
                "wayland-1.lock",
                "wayland-1",
                "wayland-0.lock",
                "wayland-0"
            ]),
            Some("wayland-0".to_owned())
        );
        assert_eq!(wayland_socket(["bus", "pipewire-0"]), None);
    }
}
