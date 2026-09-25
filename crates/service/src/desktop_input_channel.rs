//! The pipe the privileged service and its `LocalSystem` desktop injector talk
//! over (ADR 0114).
//!
//! The mirror of `agent_channel.rs`, narrowed to one job and one direction. The
//! session agent is a two-way channel because the agent has a screen to report
//! on; this injector has nothing to say back — it presses keys and moves a
//! pointer and that is all — so the wire runs one way, service to injector, and
//! there is no event half for a compromised injector to lie over.
//!
//! Three things bound the far end, exactly as they bound the agent's, and none
//! of them is the protocol:
//!
//! 1. **The access list.** `LocalSystem` and administrators only
//!    ([`SYSTEM_ONLY_SDDL`]) — the same list the logon-screen worker's channel
//!    carries (ADR 0088 §1), because this injector is `LocalSystem` too and no
//!    signed-in user should be able to stand in front of the channel that
//!    performs the whole session's input.
//! 2. **`PIPE_REJECT_REMOTE_CLIENTS`.** This endpoint is not on the network and
//!    never becomes one.
//! 3. **The process id.** The service launched the injector and knows its pid
//!    ([`crate::system_injector_launch::SystemInjector::pid`]), so it asks the
//!    kernel who connected (`GetNamedPipeClientProcessId`) and hangs up on
//!    anybody else.
//!
//! The message on this wire is a [`crate::protocol::DesktopInjectEvent`], the
//! only shape it carries, encoded by [`crate::protocol::encode_desktop_inject`]
//! and validated by [`crate::protocol::parse_desktop_inject`] — no length
//! field, no string, fixed size, so a short read is an error rather than a
//! state to reassemble.
//!
//! The injector's own half ([`InjectorLink`]) needs no `unsafe`: opening a named
//! pipe is an ordinary `CreateFileW` the standard library already does. That
//! matters because the injector is the desktop binary, which is
//! `#![forbid(unsafe_code)]`.

use std::io::Read as _;

use crate::protocol::{DESKTOP_INJECT_PAYLOAD_LEN, DesktopInjectEvent, parse_desktop_inject};

/// Name of the pipe the service and its desktop injector talk over.
///
/// A local pipe, and its own: distinct from the session agent's
/// ([`crate::agent_protocol::AGENT_ENDPOINT`]) and from the helper's request
/// endpoint ([`crate::protocol::ENDPOINT`]), so no one parser is ever handed
/// another's messages.
pub const DESKTOP_INPUT_ENDPOINT: &str = r"\\.\pipe\lumepeer-desktop-injector";

/// The injector's end of the channel.
///
/// Reads events and does nothing else — there is no method here that sends
/// anything, so the injector cannot instruct the service even by accident.
#[derive(Debug)]
pub struct InjectorLink {
    pipe: std::fs::File,
}

impl InjectorLink {
    /// Opens the channel the service is listening on.
    ///
    /// `None` when there is nothing listening, or when this process is not
    /// admitted by the access list — one answer for both, because the
    /// injector's next move is the same either way: exit, and let the service
    /// notice and start another one.
    #[must_use]
    pub fn connect() -> Option<Self> {
        let pipe = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(DESKTOP_INPUT_ENDPOINT)
            .ok()?;
        Some(Self { pipe })
    }

    /// Waits for the next event from the service.
    ///
    /// `None` means the channel is gone or the service sent something this
    /// injector does not understand. Both end the attachment: an injector that
    /// skipped a message it could not parse would be performing input
    /// approximately, and one whose service has gone has nothing left to do.
    #[must_use]
    pub fn recv(&mut self) -> Option<DesktopInjectEvent> {
        let mut message = [0u8; DESKTOP_INJECT_PAYLOAD_LEN];
        // `read_exact`, not `read`: a short message is not a message.
        self.pipe.read_exact(&mut message).ok()?;
        parse_desktop_inject(&message)
    }
}

/// The privileged service's end of the channel.
#[cfg(target_os = "windows")]
pub use windows_impl::InjectorHost;

#[cfg(target_os = "windows")]
mod windows_impl {
    #![allow(
        unsafe_code,
        reason = "a named pipe with a DACL has no safe binding; same \
                  justification standard as the rest of this crate's Win32 \
                  surface (ADR 0043, ADR 0049, ADR 0085)"
    )]

    use std::io::Write as _;
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

    use super::DESKTOP_INPUT_ENDPOINT;
    use crate::protocol::{DesktopInjectEvent, encode_desktop_inject};

    /// Bytes the pipe buffers in each direction.
    ///
    /// One descriptor is [`crate::protocol::DESKTOP_INJECT_PAYLOAD_LEN`] bytes
    /// and nothing here queues; 64 is the kernel's own minimum, the same size
    /// `agent_channel` gives its pipe.
    const PIPE_BUFFER_BYTES: u32 = 64;

    /// How long a half-finished connection may sit before the kernel gives up on
    /// it, in milliseconds — the same five seconds every other pipe in this
    /// crate uses.
    const PIPE_TIMEOUT_MS: u32 = 5_000;

    /// The access list: `LocalSystem` and administrators, and no user at all.
    ///
    /// The injector runs as `LocalSystem` because that is the only integrity
    /// level whose `SendInput` a System-integrity foreground window does not
    /// drop; admitting an interactive user to the channel that drives it would
    /// be handing that user the whole session's input. `GA` for both, and
    /// `WRITE_DAC` for neither, so the injector cannot widen the pipe from under
    /// the service — the property ADR 0085's own channel keeps.
    const SYSTEM_ONLY_SDDL: &str = "D:(A;;GA;;;SY)(A;;GA;;;BA)";

    /// A null-terminated UTF-16 copy of `text`, for the `W` entry points.
    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// The service's end of the channel.
    ///
    /// Writes events, and cannot read: there is no `recv` here and no way to add
    /// one without saying so in the type, so the injector can never make the
    /// service parse something as input to itself.
    #[derive(Debug)]
    pub struct InjectorHost {
        pipe: std::fs::File,
    }

    impl InjectorHost {
        /// Creates the channel and waits for the injector with `expected_pid` to
        /// connect to it.
        ///
        /// `keep_waiting` is asked between rounds — after a stranger has been
        /// turned away, never in the middle of one — and a `false` gives up and
        /// answers `None`, the same interruption mechanism `agent_channel` uses
        /// for a wait no flag can break.
        ///
        /// `None` when the endpoint itself cannot be created; every other
        /// failure is another connection, so the loop tries again.
        #[must_use]
        pub fn accept_from_system_while(
            expected_pid: u32,
            keep_waiting: &dyn Fn() -> bool,
        ) -> Option<Self> {
            loop {
                if !keep_waiting() {
                    tracing::info!("no longer waiting for the desktop injector to connect");
                    return None;
                }
                let pipe = create_pipe()?;
                match accept_one(pipe, expected_pid) {
                    Accepted::Injector(link) => return Some(link),
                    Accepted::Stranger => {
                        // SAFETY: `pipe` came from `create_pipe` and nothing took
                        // ownership of it on this path.
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

        /// Tells the injector to perform one already-authorized event.
        ///
        /// Returns whether it went out whole; a partial write is a failure, not
        /// a smaller message, for the same reason it is everywhere else on this
        /// crate's wires — the messages are fixed-size and a reader that took
        /// half of one would parse the next from the wrong offset forever.
        #[must_use]
        pub fn send(&mut self, event: DesktopInjectEvent) -> bool {
            self.pipe.write_all(&encode_desktop_inject(event)).is_ok() && self.pipe.flush().is_ok()
        }
    }

    /// What one turn of the accept loop produced.
    enum Accepted {
        /// The process the service launched.
        Injector(InjectorHost),
        /// Somebody else. Hang up and wait again.
        Stranger,
        /// The endpoint itself failed. Give up.
        Broken,
    }

    /// Creates the single-instance pipe with [`SYSTEM_ONLY_SDDL`] on it.
    fn create_pipe() -> Option<HANDLE> {
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        let encoded = wide(SYSTEM_ONLY_SDDL);
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
            tracing::error!("cannot build the desktop injector channel's access list");
            return None;
        }
        let attributes = SECURITY_ATTRIBUTES {
            nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(0),
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: false.into(),
        };
        let name = wide(DESKTOP_INPUT_ENDPOINT);
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
        // SAFETY: `descriptor.0` came from the conversion above and is not used
        // again; the pipe holds its own copy by this point.
        unsafe {
            let _ = LocalFree(Some(HLOCAL(descriptor.0)));
        }
        if pipe.is_invalid() {
            tracing::error!("cannot create the desktop injector channel");
            return None;
        }
        Some(pipe)
    }

    /// Waits for one connection and decides whether it is the injector.
    fn accept_one(pipe: HANDLE, expected_pid: u32) -> Accepted {
        // SAFETY: `pipe` is a live named-pipe server handle from `create_pipe`.
        let connected = unsafe { ConnectNamedPipe(pipe, None) };
        if connected.is_err() {
            // `ERROR_PIPE_CONNECTED` means a client arrived before the wait
            // started, which is a connection, not a failure.
            let error = windows::core::Error::from_win32();
            if error.code() != windows::core::HRESULT::from_win32(ERROR_PIPE_CONNECTED.0) {
                tracing::warn!(?error, "the desktop injector channel stopped accepting");
                return Accepted::Broken;
            }
        }
        let mut client_pid = 0u32;
        // SAFETY: `pipe` is live and connected; `client_pid` is a local that
        // outlives the call and is only written.
        if unsafe { GetNamedPipeClientProcessId(pipe, &raw mut client_pid) }.is_err() {
            // Fails closed, like every other identity check in this crate.
            tracing::warn!("desktop injector channel: cannot identify the process that connected");
            return Accepted::Stranger;
        }
        if client_pid != expected_pid {
            tracing::warn!(
                client_pid,
                expected_pid,
                "desktop injector channel: refusing a connection from a process this service did \
                 not start"
            );
            return Accepted::Stranger;
        }
        // SAFETY: `pipe` is a live, connected pipe handle this function owns and
        // does not touch again; `File` takes over closing it, and every write
        // from here on is the standard library's.
        let file = unsafe { std::fs::File::from_raw_handle(pipe.0.cast()) };
        Accepted::Injector(InjectorHost { pipe: file })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// The access list admits `LocalSystem` and administrators and no user
        /// — not the `IU` alias, and no SID: the channel drives the whole
        /// session's input, and admitting whoever signs in next would hand it to
        /// them.
        #[test]
        fn the_channel_admits_no_user_at_all() {
            assert!(SYSTEM_ONLY_SDDL.contains(";;;SY)"));
            assert!(SYSTEM_ONLY_SDDL.contains(";;;BA)"));
            assert!(!SYSTEM_ONLY_SDDL.contains(";;;IU)"));
            assert!(!SYSTEM_ONLY_SDDL.contains("S-1-"));
            assert!(!SYSTEM_ONLY_SDDL.contains(";;;WD)") && !SYSTEM_ONLY_SDDL.contains(";;;AU)"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nothing is listening on a developer machine, and asking says so rather
    /// than hanging or panicking — the same property `agent_channel` pins.
    #[test]
    fn an_absent_service_is_simply_unreachable() {
        let _ = InjectorLink::connect();
    }

    /// The endpoint is a local pipe and is its own — not the agent's and not the
    /// helper's request pipe.
    #[test]
    fn the_endpoint_is_local_and_its_own() {
        assert!(DESKTOP_INPUT_ENDPOINT.starts_with(r"\\.\pipe"));
        assert_ne!(
            DESKTOP_INPUT_ENDPOINT,
            crate::agent_protocol::AGENT_ENDPOINT
        );
    }
}
