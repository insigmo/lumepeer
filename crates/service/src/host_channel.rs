//! The pipe between the session host and the signed-in person's client
//! (ADR 0169 §4).
//!
//! `host_channel_protocol.rs` is what may be said; this is who may say it,
//! and how each side knows the other is who it claims.
//!
//! - **The access list** admits `LocalSystem` and the one signed-in user the
//!   session host serves, by SID — read and write only, never `WRITE_DAC`, so
//!   a program of that user's cannot widen the pipe from under the host (the
//!   rule `agent_channel.rs` keeps). Any program of theirs may connect: that
//!   is accepted, and it is why the protocol can only read and narrow.
//! - **The owner is `LocalSystem`.** The session host names itself the pipe's
//!   owner, and the client checks that before it believes a word
//!   ([`PanelLink::connect`]). A program of the user's could create a pipe of
//!   the same name first — it cannot make `LocalSystem` its owner — and a
//!   client that believed it would show "nobody is connected" while somebody
//!   is, which is the one lie that undoes §7.
//! - **`PIPE_REJECT_REMOTE_CLIENTS`**, as every pipe in this crate.
//!
//! Blocking, like `agent_channel.rs`, and for its reason: each side runs it on
//! a thread of its own. The client's half needs no `unsafe` beyond the owner
//! check, which is why it lives here and not in the desktop crate.

#![allow(
    unsafe_code,
    reason = "a DACL'd named pipe and reading a handle's owner have no safe \
              bindings; same justification standard as the rest of this \
              crate's Win32 surface (ADR 0043, ADR 0085)"
)]

use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _};
use std::time::Duration;

use windows::Win32::Foundation::{
    ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, HANDLE, HLOCAL, LocalFree,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
    SE_KERNEL_OBJECT,
};
use windows::Win32::Security::{
    IsWellKnownSid, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES,
    WinLocalSystemSid,
};
use windows::Win32::Storage::FileSystem::{
    FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAGS_AND_ATTRIBUTES, PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::core::PCWSTR;

use crate::host_channel_protocol::{
    Reply, Request, read_reply, read_request, write_reply, write_request,
};

/// Prefix of the pipe's name; the user's SID follows it, so two people signed
/// in on one machine never share a channel.
const ENDPOINT_PREFIX: &str = r"\\.\pipe\lumepeer-session-host-";

/// How many clients may hold the channel at once.
///
/// More than one because "any program the user runs may open it" includes the
/// client's own second window and a test probe; a handful because each is a
/// thread in a `LocalSystem` process.
const MAX_PANELS: u32 = 4;

/// Bytes the pipe buffers in each direction: one snapshot's worth, so a reply
/// does not trickle through a kernel buffer sized for two-byte frames.
const PIPE_BUFFER_BYTES: u32 = 64 * 1024;

/// How long a half-finished connection may sit, in milliseconds — the helper
/// pipe's five seconds.
const PIPE_TIMEOUT_MS: u32 = 5_000;

/// How long the host waits before trying again when every instance is taken.
const ALL_INSTANCES_BUSY_RETRY: Duration = Duration::from_millis(500);

/// A null-terminated UTF-16 copy of `text`, for the `W` entry points.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The channel's name for the user `user_sid`.
#[must_use]
pub fn endpoint_for(user_sid: &str) -> String {
    format!("{ENDPOINT_PREFIX}{user_sid}")
}

/// The access list for the user `user_sid`, in SDDL: owned by `LocalSystem`,
/// `LocalSystem` in full, the user read and write — `0x0012019b` is
/// `FILE_GENERIC_READ | FILE_GENERIC_WRITE` for a pipe — and nobody else.
///
/// `user_sid` must have passed [`crate::frame::is_sid_string`]; it is the only
/// value formatted in.
#[must_use]
fn sddl_for(user_sid: &str) -> String {
    format!("O:SYD:(A;;GA;;;SY)(A;;0x0012019b;;;{user_sid})")
}

/// The session host's end: hands out one [`PanelConnection`] per client.
#[derive(Debug)]
pub struct HostChannel {
    name: Vec<u16>,
    sddl: String,
    /// Whether the first instance has been created; it is created with
    /// `FILE_FLAG_FIRST_PIPE_INSTANCE`, so a name somebody else holds already
    /// is a failure rather than a pipe shared with them.
    first_created: bool,
}

impl HostChannel {
    /// The channel for the user `user_sid`; `None` for anything but a SID.
    #[must_use]
    pub fn new(user_sid: &str) -> Option<Self> {
        if !crate::frame::is_sid_string(user_sid) {
            tracing::error!("refusing to build the session host's channel from a non-SID");
            return None;
        }
        Some(Self {
            name: wide(&endpoint_for(user_sid)),
            sddl: sddl_for(user_sid),
            first_created: false,
        })
    }

    /// Waits for the next client to connect.
    ///
    /// `None` when the endpoint cannot be created — above all when another
    /// process already holds the name — which the caller treats as "nobody
    /// can attach", never as a reason to listen somewhere else.
    #[must_use]
    pub fn accept(&mut self) -> Option<PanelConnection> {
        loop {
            let pipe = match self.create_instance() {
                Created::Pipe(pipe) => pipe,
                Created::Busy => {
                    std::thread::sleep(ALL_INSTANCES_BUSY_RETRY);
                    continue;
                }
                Created::Failed => return None,
            };
            // SAFETY: `pipe` is a live named-pipe server handle just created.
            let connected = unsafe { ConnectNamedPipe(pipe, None) };
            if connected.is_err() {
                let error = windows::core::Error::from_thread();
                if error.code() != windows::core::HRESULT::from_win32(ERROR_PIPE_CONNECTED.0) {
                    tracing::warn!(?error, "the session host's channel stopped accepting");
                    // SAFETY: `pipe` is ours and is not used again.
                    unsafe {
                        let _ = windows::Win32::Foundation::CloseHandle(pipe);
                    }
                    return None;
                }
            }
            let mut client_pid = 0u32;
            // SAFETY: `pipe` is live and connected; `client_pid` is a local
            // the call writes.
            let _ = unsafe { GetNamedPipeClientProcessId(pipe, &raw mut client_pid) };
            // SAFETY: `pipe` is a live, connected handle nothing else owns;
            // `File` takes over closing it.
            let file = unsafe { std::fs::File::from_raw_handle(pipe.0.cast()) };
            return Some(PanelConnection {
                pipe: file,
                client_pid,
            });
        }
    }

    /// Creates one instance of the pipe.
    fn create_instance(&mut self) -> Created {
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        let encoded = wide(&self.sddl);
        // SAFETY: `encoded` is a null-terminated wide string that outlives the
        // call; the descriptor it allocates is freed below on every path.
        let converted = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(encoded.as_ptr()),
                SDDL_REVISION_1,
                &raw mut descriptor,
                None,
            )
        };
        if converted.is_err() {
            tracing::error!("cannot build the session host channel's access list");
            return Created::Failed;
        }
        let attributes = SECURITY_ATTRIBUTES {
            nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(0),
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: false.into(),
        };
        let mut open_mode = PIPE_ACCESS_DUPLEX.0;
        if !self.first_created {
            open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE.0;
        }
        // SAFETY: `self.name` and `attributes` outlive the call.
        let pipe = unsafe {
            CreateNamedPipeW(
                PCWSTR(self.name.as_ptr()),
                FILE_FLAGS_AND_ATTRIBUTES(open_mode),
                PIPE_TYPE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                MAX_PANELS,
                PIPE_BUFFER_BYTES,
                PIPE_BUFFER_BYTES,
                PIPE_TIMEOUT_MS,
                Some(&raw const attributes),
            )
        };
        let error = windows::core::Error::from_thread();
        // SAFETY: `descriptor.0` came from the conversion above and is not
        // used again; the pipe holds its own copy by this point.
        unsafe {
            let _ = LocalFree(Some(HLOCAL(descriptor.0)));
        }
        if pipe.is_invalid() {
            if self.first_created
                && error.code() == windows::core::HRESULT::from_win32(ERROR_PIPE_BUSY.0)
            {
                return Created::Busy;
            }
            tracing::error!(
                ?error,
                "cannot create the session host's channel; is another process holding its name?"
            );
            return Created::Failed;
        }
        self.first_created = true;
        Created::Pipe(pipe)
    }
}

/// What one try at creating an instance produced.
enum Created {
    /// A fresh instance to wait on.
    Pipe(HANDLE),
    /// Every instance is in use; wait and try again.
    Busy,
    /// The endpoint cannot be had.
    Failed,
}

/// The session host's end of one client's connection: reads requests, writes
/// replies, and nothing else.
#[derive(Debug)]
pub struct PanelConnection {
    pipe: std::fs::File,
    client_pid: u32,
}

impl PanelConnection {
    /// The process on the other end, for the log; `0` when the kernel would
    /// not say.
    #[must_use]
    pub const fn client_pid(&self) -> u32 {
        self.client_pid
    }

    /// The next request; `None` ends the connection.
    #[must_use]
    pub fn recv(&mut self) -> Option<Request> {
        read_request(&mut self.pipe)
    }

    /// Answers the last request; whether the answer went out whole.
    #[must_use]
    pub fn send(&mut self, reply: &Reply) -> bool {
        write_reply(&mut self.pipe, reply)
    }
}

/// The client's end: asks, and is answered.
#[derive(Debug)]
pub struct PanelLink {
    pipe: std::fs::File,
}

impl PanelLink {
    /// Opens the channel of the session host serving the user `user_sid`.
    ///
    /// `None` when nothing is listening, when this process is not admitted, or
    /// when what answers is not owned by `LocalSystem` — a pipe of that name
    /// another program made, whose snapshot would be whatever it liked.
    #[must_use]
    pub fn connect(user_sid: &str) -> Option<Self> {
        if !crate::frame::is_sid_string(user_sid) {
            return None;
        }
        let pipe = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(endpoint_for(user_sid))
            .ok()?;
        if !owned_by_local_system(&pipe) {
            tracing::warn!(
                "the session host's channel is not LocalSystem's; not believing what it says"
            );
            return None;
        }
        Some(Self { pipe })
    }

    /// Asks one thing; `None` when the channel is gone or answered nonsense,
    /// either of which ends this link.
    #[must_use]
    pub fn ask(&mut self, request: &Request) -> Option<Reply> {
        if !write_request(&mut self.pipe, request) {
            return None;
        }
        read_reply(&mut self.pipe)
    }
}

/// Whether the kernel object behind `pipe` is owned by `LocalSystem`.
fn owned_by_local_system(pipe: &std::fs::File) -> bool {
    let handle = HANDLE(pipe.as_raw_handle().cast());
    let mut owner = PSID::default();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: `handle` is a live pipe handle `pipe` owns and outlives the
    // call; `owner` and `descriptor` are locals the call writes, the first
    // pointing into the second, which is freed below.
    let read = unsafe {
        GetSecurityInfo(
            handle,
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION,
            Some(&raw mut owner),
            None,
            None,
            None,
            Some(&raw mut descriptor),
        )
    };
    let system = read.is_ok() && {
        // SAFETY: `owner` points into `descriptor`, which is still alive.
        unsafe { IsWellKnownSid(owner, WinLocalSystemSid) }.as_bool()
    };
    if !descriptor.0.is_null() {
        // SAFETY: allocated by `GetSecurityInfo` above and not used again.
        unsafe {
            let _ = LocalFree(Some(HLOCAL(descriptor.0)));
        }
    }
    system
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;

    const USER: &str = "S-1-5-21-1111111111-2222222222-3333333333-1001";

    /// ADR 0169 §4: `LocalSystem` and exactly one named user — read and
    /// write, never `WRITE_DAC` — owned by `LocalSystem`; no alias that would
    /// admit anybody else, and not administrators either.
    #[test]
    fn the_channel_admits_system_and_one_user_and_nobody_else() {
        let sddl = sddl_for(USER);
        assert!(sddl.starts_with("O:SY"), "the client checks this owner");
        assert!(sddl.contains("(A;;GA;;;SY)"));
        assert!(sddl.contains(&format!("(A;;0x0012019b;;;{USER})")));
        assert_eq!(sddl.matches("S-1-").count(), 1, "exactly one SID");
        for alias in [";;;IU)", ";;;WD)", ";;;AU)", ";;;BA)", ";;;BU)"] {
            assert!(!sddl.contains(alias), "{alias} must not be admitted");
        }
    }

    /// A string that is not a SID never becomes a pipe name or a list.
    #[test]
    fn a_non_sid_never_becomes_a_channel() {
        assert!(HostChannel::new("BA").is_none());
        assert!(HostChannel::new("S-1-5-18)(A;;GA;;;WD").is_none());
        assert!(PanelLink::connect("..\\..\\x").is_none());
    }

    /// Each user has a channel of their own, local, and not the helper's.
    #[test]
    fn the_endpoint_is_local_and_per_user() {
        let name = endpoint_for(USER);
        assert!(name.starts_with(r"\\.\pipe\"));
        assert!(name.ends_with(USER));
        assert_ne!(name, crate::protocol::ENDPOINT);
        assert_ne!(endpoint_for(USER), endpoint_for("S-1-5-21-1-2-3-1002"));
    }

    /// Nobody is listening for a user who does not exist, and asking says so
    /// rather than hanging.
    #[test]
    fn an_absent_host_is_simply_unreachable() {
        assert!(PanelLink::connect("S-1-5-21-0-0-0-4294967295").is_none());
    }

    /// End to end on this machine: a channel this test creates, a client that
    /// connects to it. Unelevated the pipe's owner cannot be `LocalSystem`, so
    /// the client must refuse to believe it — the squatting case; a run as
    /// `LocalSystem` is the only one that gets an answer.
    #[test]
    fn a_client_believes_only_a_channel_local_system_owns() {
        let Some((sid, _)) = crate::program_data::this_process() else {
            return;
        };
        let Some(mut channel) = HostChannel::new(&sid) else {
            return;
        };
        let server = std::thread::spawn(move || {
            let Some(mut panel) = channel.accept() else {
                return;
            };
            if panel.recv() == Some(Request::Snapshot) {
                let _ = panel.send(&Reply::Snapshot(b"{}".to_vec()));
            }
        });
        // Give the accept a moment to be waiting.
        std::thread::sleep(Duration::from_millis(100));
        let link = PanelLink::connect(&sid);
        if sid == "S-1-5-18" {
            let mut link = link.expect("LocalSystem's own channel");
            assert_eq!(
                link.ask(&Request::Snapshot),
                Some(Reply::Snapshot(b"{}".to_vec()))
            );
        } else {
            // `O:SY` cannot be set by anyone but LocalSystem, so this test's
            // pipe was refused at creation or carries this user as its owner.
            assert!(link.is_none(), "a channel this user made was believed");
            // Unblock the accept so the thread ends.
            let _ = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(endpoint_for(&sid));
        }
        let _ = server.join();
    }
}
