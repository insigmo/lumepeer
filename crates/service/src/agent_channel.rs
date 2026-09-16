//! The pipe the privileged host and its session agent talk over (ADR 0085).
//!
//! `agent_protocol.rs` is what may be said; this is who may say it.
//!
//! Three things bound the far end, and none of them is the protocol:
//!
//! 1. **The access list.** `LocalSystem`, administrators, and the one signed-in
//!    user the host started an agent for — named by SID, not by the `IU` alias
//!    the helper's own request pipe uses. `IU` would admit *every* interactive
//!    user, and on a machine with a second person signed in that is a way to
//!    stand in for the agent.
//! 2. **`PIPE_REJECT_REMOTE_CLIENTS`.** The same flag the helper's pipe
//!    carries, for the same reason: this endpoint is not on the network and
//!    never becomes one.
//! 3. **The process id.** The host launched the agent and knows its pid
//!    ([`crate::agent_launch::SessionAgent::pid`]), so the first thing it does
//!    with a connection is ask the kernel who is on the other end
//!    (`GetNamedPipeClientProcessId`) and hang up on anybody else. The access
//!    list narrows it to a user; this narrows it to the process that user's
//!    host actually started. It is the same shape as ADR 0049's session
//!    binding — a mechanical fact about who is talking, not a policy decision
//!    about who is allowed to.
//!
//! **Blocking, on purpose.** Every call here blocks, exactly like
//! `windows_service.rs`'s accept loop, and the caller is expected to run it on
//! a thread of its own. An async wrapper would need cancellation, and a pipe
//! wait has none — the way to end one is to connect to it, which the caller
//! can do without this module growing a second mechanism.
//!
//! The agent's own half ([`HostLink`]) needs no `unsafe`: opening a named pipe
//! is an ordinary `CreateFileW`, which the standard library already does. That
//! matters because the agent is the desktop binary, which is
//! `#![forbid(unsafe_code)]`.

use std::io::{Read as _, Write as _};

use crate::agent_protocol::{
    AGENT_ENDPOINT, AGENT_MESSAGE_LEN, AgentCommand, AgentEvent, encode_event, parse_command,
};

/// The agent's end of the channel.
///
/// Reads commands, writes events, and can do neither of the opposite two —
/// there is no method here that sends a command, so an agent cannot instruct a
/// host even by accident.
#[derive(Debug)]
pub struct HostLink {
    pipe: std::fs::File,
}

impl HostLink {
    /// Opens the channel the host is listening on.
    ///
    /// `None` when there is nothing listening, or when this process is not
    /// admitted by the access list — one answer for both, because the agent's
    /// next move is the same either way: exit, and let the host notice and
    /// decide whether to start another one.
    #[must_use]
    pub fn connect() -> Option<Self> {
        let pipe = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(AGENT_ENDPOINT)
            .ok()?;
        Some(Self { pipe })
    }

    /// Tells the host something about this desktop.
    ///
    /// Returns whether it went out whole. A partial write is a failure, not a
    /// smaller message: the messages are fixed-size and a host that read half
    /// of one would be parsing the next one from the wrong offset forever.
    #[must_use]
    pub fn send(&mut self, event: AgentEvent) -> bool {
        self.pipe.write_all(&encode_event(event)).is_ok() && self.pipe.flush().is_ok()
    }

    /// A half of this channel that can only write.
    ///
    /// The mirror of [`AgentLink::commands`], and needed for the mirror
    /// reason: an agent is blocked in [`recv`](Self::recv) waiting for the
    /// host while its capture loop has frames to announce, and one handle
    /// cannot be both. `None` when the handle cannot be duplicated, which
    /// leaves the agent able to hear and not answer — an attachment worth
    /// ending rather than serving half of.
    #[must_use]
    pub fn events(&self) -> Option<HostEvents> {
        self.pipe
            .try_clone()
            .inspect_err(
                |error| tracing::warn!(%error, "cannot split the host channel for writing"),
            )
            .ok()
            .map(|pipe| HostEvents { pipe })
    }

    /// Waits for the next command from the host.
    ///
    /// `None` means the channel is gone or the host said something this agent
    /// does not understand. Both end the attachment: an agent that skipped a
    /// message it could not parse would be carrying out a privileged
    /// instruction approximately, and an agent whose host has gone has nothing
    /// left to serve.
    #[must_use]
    pub fn recv(&mut self) -> Option<AgentCommand> {
        let mut message = [0u8; AGENT_MESSAGE_LEN];
        // `read_exact`, not `read`: a short message is not a message.
        self.pipe.read_exact(&mut message).ok()?;
        parse_command(&message)
    }
}

/// The writing half of a [`HostLink`].
///
/// Events only, the same way [`crate::agent_channel::AgentCommands`] is
/// commands only: neither peer can reach the direction that is not its own.
#[derive(Debug)]
pub struct HostEvents {
    pipe: std::fs::File,
}

impl HostEvents {
    /// Tells the host something about this desktop.
    ///
    /// Returns whether it went out whole; a partial write is a failure for the
    /// same reason it is everywhere else on this wire.
    #[must_use]
    pub fn send(&mut self, event: AgentEvent) -> bool {
        self.pipe.write_all(&encode_event(event)).is_ok() && self.pipe.flush().is_ok()
    }
}

/// The privileged host's end of the channel.
#[cfg(target_os = "windows")]
pub use windows_impl::{AgentCommands, AgentLink};

#[cfg(target_os = "windows")]
mod windows_impl {
    #![allow(
        unsafe_code,
        reason = "a named pipe with a DACL has no safe binding; same \
                  justification standard as the rest of this crate's Win32 \
                  surface (ADR 0043, ADR 0049)"
    )]

    use std::io::{Read as _, Write as _};
    use std::os::windows::io::FromRawHandle as _;

    use windows::Win32::Foundation::{
        CloseHandle, ERROR_PIPE_CONNECTED, HANDLE, HLOCAL, LocalFree,
    };
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    use windows::Win32::Storage::FileSystem::{FILE_FLAGS_AND_ATTRIBUTES, PIPE_ACCESS_DUPLEX};
    use windows::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId,
        PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
    };
    use windows::core::PCWSTR;

    use crate::agent_protocol::{
        AGENT_ENDPOINT, AGENT_MESSAGE_LEN, AgentCommand, AgentEvent, encode_command, parse_event,
    };

    /// Bytes the pipe buffers in each direction.
    ///
    /// The kernel's minimum granularity rather than headroom anyone asked for,
    /// exactly as `windows_service.rs`'s own pipe sizes it: the messages are
    /// twelve bytes and nothing here ever queues.
    const PIPE_BUFFER_BYTES: u32 = 64;

    /// How long a half-finished connection may sit before the kernel gives up
    /// on it, in milliseconds.
    ///
    /// The same five seconds the helper's request pipe uses, for the same
    /// reason: nothing legitimate takes this long, and it exists so one stuck
    /// client cannot hold the single-instance pipe forever.
    const PIPE_TIMEOUT_MS: u32 = 5_000;

    /// A null-terminated UTF-16 copy of `text`, for the `W` entry points.
    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// The access list for one agent, in SDDL.
    ///
    /// `SY` and `BA` in full, and the named user with read and write only —
    /// never `WRITE_DAC`, so an agent cannot widen the pipe from under the
    /// host, which is the property ADR 0043 §3 gives the helper's own pipe.
    /// `0x0012019b` is `FILE_GENERIC_READ | FILE_GENERIC_WRITE` for a pipe.
    ///
    /// `user_sid` must already have passed [`crate::frame::is_sid_string`]; it
    /// is the only value formatted in, and it comes from a token this process
    /// obtained itself.
    #[must_use]
    pub(super) fn sddl_for(user_sid: &str) -> String {
        format!("D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x0012019b;;;{user_sid})")
    }

    /// The access list for the logon-screen worker (ADR 0088 §1).
    ///
    /// The same list with the user's half left out, because there is no user:
    /// nobody is signed in, and the worker is `LocalSystem` on `Winlogon`. A
    /// list that admitted an interactive user here would be admitting whoever
    /// happens to sign in *next* to the channel that drives the screen they
    /// are about to type a password on.
    pub(super) const SYSTEM_ONLY_SDDL: &str = "D:(A;;GA;;;SY)(A;;GA;;;BA)";

    /// The privileged host's end of the channel.
    ///
    /// Writes commands, reads events, and can do neither of the opposite two.
    #[derive(Debug)]
    pub struct AgentLink {
        pipe: std::fs::File,
    }

    impl AgentLink {
        /// Creates the channel and waits for the agent with `expected_pid` to
        /// connect to it.
        ///
        /// Blocks until somebody connects. A connection from any other process
        /// is closed and the wait starts again, so a local process that is
        /// admitted by the access list still cannot stand in for the agent the
        /// host started — it can only make the host wait through one more
        /// round.
        ///
        /// `None` when the endpoint itself cannot be created, which is the
        /// only failure worth giving up on: everything else is another
        /// connection.
        #[must_use]
        pub fn accept_from(expected_pid: u32, user_sid: &str) -> Option<Self> {
            Self::accept_from_while(expected_pid, user_sid, &|| true)
        }

        /// [`accept_from`](Self::accept_from), for a caller that must be able
        /// to stop waiting.
        ///
        /// `keep_waiting` is asked between rounds — after a stranger has been
        /// turned away, never in the middle of one — and a `false` gives up
        /// and answers `None`.
        ///
        /// It exists because the blocking half of this is genuinely
        /// unbounded. `ConnectNamedPipe` waits for a connection and no flag
        /// interrupts it, so a host whose agent died between being launched
        /// and connecting would wait for it forever and never look at the
        /// machine again — a screen lost permanently to one process that
        /// failed to start. The caller breaks the wait the way
        /// `windows_service.rs` already breaks its own: by connecting to the
        /// pipe, which costs one turned-away stranger and one more round, and
        /// this is the question asked on that round.
        #[must_use]
        pub fn accept_from_while(
            expected_pid: u32,
            user_sid: &str,
            keep_waiting: &dyn Fn() -> bool,
        ) -> Option<Self> {
            if !crate::frame::is_sid_string(user_sid) {
                tracing::error!("refusing to build an agent channel access list from a non-SID");
                return None;
            }
            Self::accept_with(&sddl_for(user_sid), expected_pid, keep_waiting)
        }

        /// [`accept_from_while`](Self::accept_from_while), for the
        /// logon-screen worker (ADR 0088 §1).
        ///
        /// The same channel, the same process check, and an access list with
        /// no user on it: the worker runs as `LocalSystem` because nobody is
        /// signed in for it to run as. Its own entry point rather than a flag
        /// on the one above, so that "this channel admitted a process with no
        /// user behind it" is a call a reader can find rather than a parameter
        /// they have to trace.
        #[must_use]
        pub fn accept_from_system_while(
            expected_pid: u32,
            keep_waiting: &dyn Fn() -> bool,
        ) -> Option<Self> {
            Self::accept_with(SYSTEM_ONLY_SDDL, expected_pid, keep_waiting)
        }

        /// The accept loop both entry points run, with whichever access list
        /// they built.
        #[must_use]
        fn accept_with(
            sddl: &str,
            expected_pid: u32,
            keep_waiting: &dyn Fn() -> bool,
        ) -> Option<Self> {
            loop {
                if !keep_waiting() {
                    tracing::info!("no longer waiting for a session agent to connect");
                    return None;
                }
                let pipe = create_pipe(sddl)?;
                match accept_one(pipe, expected_pid) {
                    Accepted::Agent(link) => return Some(link),
                    Accepted::Stranger => {
                        // SAFETY: `pipe` came from `create_pipe` and nothing
                        // took ownership of it on this path.
                        unsafe {
                            let _ = CloseHandle(pipe);
                        }
                    }
                    Accepted::Broken => {
                        // SAFETY: same.
                        unsafe {
                            let _ = CloseHandle(pipe);
                        }
                        return None;
                    }
                }
            }
        }

        /// A half of this channel that can only write.
        ///
        /// `None` when the handle cannot be duplicated, which leaves the
        /// caller with a link it can read and not write — an attachment worth
        /// abandoning rather than serving half of.
        ///
        /// A host needs this because the two directions are driven by
        /// different things: events arrive when the agent has something to
        /// say, and commands leave when a guest does something, and one
        /// thread cannot be blocked in `recv` and ready to `send` at the same
        /// time. Duplicating the handle is the whole mechanism — both halves
        /// are the same pipe, so a write still cannot be read as a command by
        /// this side and an event still cannot be written by it.
        #[must_use]
        pub fn commands(&self) -> Option<AgentCommands> {
            self.pipe
                .try_clone()
                .inspect_err(
                    |error| tracing::warn!(%error, "cannot split the agent channel for writing"),
                )
                .ok()
                .map(|pipe| AgentCommands { pipe })
        }

        /// Tells the agent to do one already-authorized thing.
        ///
        /// Returns whether it went out whole; a partial write is a failure for
        /// the same reason it is on the agent's side.
        #[must_use]
        pub fn send(&mut self, command: AgentCommand) -> bool {
            self.pipe.write_all(&encode_command(command)).is_ok() && self.pipe.flush().is_ok()
        }

        /// Waits for the next thing the agent has to say.
        ///
        /// `None` means the agent is gone or said something this host does not
        /// understand, and both mean the same thing to the caller: this
        /// attachment is over, there is no screen, and the guest gets the
        /// honest "no picture" state rather than the last frame forever
        /// (ADR 0085 §3).
        #[must_use]
        pub fn recv(&mut self) -> Option<AgentEvent> {
            let mut message = [0u8; AGENT_MESSAGE_LEN];
            self.pipe.read_exact(&mut message).ok()?;
            parse_event(&message)
        }
    }

    /// The writing half of an [`AgentLink`].
    ///
    /// Commands only. There is no `recv` here and no way to add one without
    /// saying so in the type, which is the same property `HostLink` has in the
    /// other direction: neither peer can reach the direction that is not its
    /// own by getting the framing right.
    #[derive(Debug)]
    pub struct AgentCommands {
        pipe: std::fs::File,
    }

    impl AgentCommands {
        /// Tells the agent to do one already-authorized thing.
        ///
        /// Returns whether it went out whole; a partial write is a failure for
        /// the same reason it is everywhere else on this wire.
        #[must_use]
        pub fn send(&mut self, command: AgentCommand) -> bool {
            self.pipe.write_all(&encode_command(command)).is_ok() && self.pipe.flush().is_ok()
        }
    }

    /// What one turn of the accept loop produced.
    enum Accepted {
        /// The process the host launched.
        Agent(AgentLink),
        /// Somebody else. Hang up and wait again.
        Stranger,
        /// The endpoint itself failed. Give up.
        Broken,
    }

    /// Creates the single-instance pipe with `sddl` on it.
    fn create_pipe(sddl: &str) -> Option<HANDLE> {
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        let encoded = wide(sddl);
        // SAFETY: `encoded` is a null-terminated wide string that outlives the
        // call; the descriptor it allocates is freed on every path out.
        let converted = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(encoded.as_ptr()),
                SDDL_REVISION_1,
                &raw mut descriptor,
                None,
            )
        };
        if converted.is_err() {
            tracing::error!("cannot build the agent channel's access list");
            return None;
        }
        let attributes = SECURITY_ATTRIBUTES {
            nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(0),
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: false.into(),
        };
        let name = wide(AGENT_ENDPOINT);
        // SAFETY: `name` and `attributes` outlive the call. One instance, so a
        // second listener cannot be squatted alongside this one, and
        // `PIPE_REJECT_REMOTE_CLIENTS` keeps the endpoint off the network.
        let pipe = unsafe {
            CreateNamedPipeW(
                PCWSTR(name.as_ptr()),
                FILE_FLAGS_AND_ATTRIBUTES(PIPE_ACCESS_DUPLEX.0),
                PIPE_TYPE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                PIPE_BUFFER_BYTES,
                PIPE_BUFFER_BYTES,
                PIPE_TIMEOUT_MS,
                Some(&raw const attributes),
            )
        };
        // SAFETY: `descriptor.0` came from the conversion above and is not
        // used again; the pipe holds its own copy by this point.
        unsafe {
            let _ = LocalFree(Some(HLOCAL(descriptor.0)));
        }
        if pipe.is_invalid() {
            tracing::error!("cannot create the agent channel");
            return None;
        }
        Some(pipe)
    }

    /// Waits for one connection and decides whether it is the agent.
    fn accept_one(pipe: HANDLE, expected_pid: u32) -> Accepted {
        // SAFETY: `pipe` is a live named-pipe server handle from `create_pipe`.
        let connected = unsafe { ConnectNamedPipe(pipe, None) };
        if connected.is_err() {
            // `ERROR_PIPE_CONNECTED` means a client arrived before the wait
            // started, which is a connection, not a failure.
            let error = windows::core::Error::from_win32();
            if error.code() != windows::core::HRESULT::from_win32(ERROR_PIPE_CONNECTED.0) {
                tracing::warn!(?error, "the agent channel stopped accepting");
                return Accepted::Broken;
            }
        }
        let mut client_pid = 0u32;
        // SAFETY: `pipe` is live and connected; `client_pid` is a local that
        // outlives the call and is only written.
        if unsafe { GetNamedPipeClientProcessId(pipe, &raw mut client_pid) }.is_err() {
            // Fails closed, like every other identity check in this crate: a
            // caller whose identity cannot be established is not the agent.
            tracing::warn!("agent channel: cannot identify the process that connected");
            return Accepted::Stranger;
        }
        if client_pid != expected_pid {
            // Named in the log, never in a reply — there is no reply to give.
            tracing::warn!(
                client_pid,
                expected_pid,
                "agent channel: refusing a connection from a process this host did not start"
            );
            return Accepted::Stranger;
        }
        // SAFETY: `pipe` is a live, connected pipe handle this function owns
        // and does not touch again; `File` takes over closing it, and every
        // read and write from here on is the standard library's.
        let file = unsafe { std::fs::File::from_raw_handle(pipe.0.cast()) };
        Accepted::Agent(AgentLink { pipe: file })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// The access list admits `LocalSystem`, administrators and one named
        /// user — never the `IU` alias, which would be every interactive user
        /// at once, and never everyone or authenticated users.
        #[test]
        fn the_channel_admits_one_named_user_and_no_alias() {
            let sddl = sddl_for("S-1-5-21-1111111111-2222222222-3333333333-1001");
            assert!(sddl.contains(";;;SY)"));
            assert!(sddl.contains(";;;BA)"));
            assert!(
                sddl.contains("0x0012019b;;;S-1-5-21-1111111111-2222222222-3333333333-1001)"),
                "the agent's own user gets read and write, and nothing else"
            );
            assert!(
                !sddl.contains(";;;IU)"),
                "IU would admit every interactive user, not the one the host started an agent for"
            );
            assert!(!sddl.contains(";;;WD)") && !sddl.contains(";;;AU)"));
        }

        /// The logon-screen worker's list has no user on it at all — not the
        /// `IU` alias, and no SID: nobody is signed in, and admitting whoever
        /// signs in next would be admitting them to the channel that drives
        /// the screen they are about to type a password on (ADR 0088 §1).
        #[test]
        fn the_logon_screen_channel_admits_no_user_at_all() {
            assert!(SYSTEM_ONLY_SDDL.contains(";;;SY)"));
            assert!(SYSTEM_ONLY_SDDL.contains(";;;BA)"));
            assert!(!SYSTEM_ONLY_SDDL.contains(";;;IU)"));
            assert!(!SYSTEM_ONLY_SDDL.contains("S-1-"));
            assert!(!SYSTEM_ONLY_SDDL.contains(";;;WD)") && !SYSTEM_ONLY_SDDL.contains(";;;AU)"));
        }

        /// A SID that would not survive [`crate::frame::is_sid_string`] never
        /// reaches `CreateNamedPipeW`.
        #[test]
        fn a_non_sid_never_becomes_an_access_list() {
            assert!(AgentLink::accept_from(0, "BA").is_none());
            assert!(AgentLink::accept_from(0, "S-1-5-18)(A;;GA;;;WD").is_none());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nothing is listening on a developer machine, and asking says so rather
    /// than hanging or panicking — the same property `client.rs` pins for the
    /// helper's own pipe.
    #[test]
    fn an_absent_host_is_simply_unreachable() {
        // If something *is* listening here it is this machine's real host, and
        // connecting to it is harmless: the link is dropped without a word
        // being sent, and the host goes back to waiting for its agent.
        let _ = HostLink::connect();
    }

    /// The endpoint is a local pipe and is not the helper's. Two endpoints
    /// with one name would put an agent's commands in front of the helper's
    /// two-byte parser.
    #[test]
    fn the_endpoint_is_local_and_its_own() {
        assert!(AGENT_ENDPOINT.starts_with(r"\\.\pipe"));
        assert_ne!(AGENT_ENDPOINT, crate::protocol::ENDPOINT);
    }
}
