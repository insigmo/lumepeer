//! The shared-memory side channel a secure-desktop frame travels over
//! (ADR 0049).
//!
//! `crates/service/src/protocol.rs` fixes the mapping's name and size; this
//! file is the mechanics on both ends of it. [`Writer`] is the service's end
//! (session 0, `LocalSystem`): it creates the mapping and publishes one
//! frame at a time. [`Reader`] is the client's end (an interactive session,
//! unprivileged): it opens the existing mapping read-only and copies a
//! frame back out.
//!
//! No lock and no sequence number guard the mapping. That is not an
//! oversight: a request is only ever answered on a fresh pipe connection
//! (`windows_service.rs::accept_and_serve` disconnects after every reply),
//! the service finishes writing the mapping *before* it writes the pipe's
//! `STATUS_OK`, and a client never opens the mapping until *after* its
//! blocking read of that reply returns. The two `ReadFile`/`WriteFile`
//! kernel calls the pipe round trip already needs are a stronger ordering
//! guarantee than an application-level lock would add on top of them, so
//! this file does not invent one for a mapping the two sides never touch at
//! the same time (ADR 0049).

#![allow(
    unsafe_code,
    reason = "a named shared-memory mapping has no safe binding; same justification standard as SendInput (ADR 0012) and the rest of this crate's Win32 surface (ADR 0043)"
)]

use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, INVALID_HANDLE_VALUE, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_ALL_ACCESS, FILE_MAP_READ, FILE_MAP_WRITE, MapViewOfFile,
    PAGE_READWRITE, UnmapViewOfFile,
};
use windows::core::PCWSTR;

use crate::agent_protocol::{AGENT_FRAME_CAPACITY_BYTES, AGENT_FRAME_MAPPING_NAME};
use crate::protocol::{
    SECURE_DESKTOP_FRAME_CAPACITY_BYTES, SECURE_DESKTOP_FRAME_HEADER_BYTES,
    SECURE_DESKTOP_FRAME_MAPPING_BYTES, SECURE_DESKTOP_MAPPING_NAME,
};

/// Who may open the secure-desktop mapping, in SDDL — the same three trustees
/// the pipe's own DACL names (ADR 0043), but asymmetric where the pipe is not:
/// nothing on the client's side ever writes a frame, so interactive users get
/// generic *read* only.
const FRAME_SDDL: &str = "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GR;;;IU)";

/// Longest string form of a Windows SID this module will put in an access
/// list.
///
/// A SID string is `S-1-` plus an authority and up to fifteen sub-authorities
/// (`S-1-5-21-` plus three 32-bit values plus a relative id is the ordinary
/// user shape). Ninety characters is comfortably past every real one and
/// short enough that nothing long can be smuggled through
/// [`is_sid_string`] into an SDDL.
const MAX_SID_STRING_CHARS: usize = 90;

/// Whether `text` is the string form of a Windows SID and nothing else.
///
/// The one guard on [`Writer::create_for_session_agent`]: the SID it takes
/// comes from `ConvertSidToStringSidW` on a token the host service itself
/// obtained, so it is already an OS-produced string — but it is the only
/// value this file ever interpolates into an access list, and an access list
/// built by string formatting is exactly the shape that must not be able to
/// carry anything unexpected. `S`, digits and `-` cannot close an ACE or open
/// another one, so a string that passes this cannot change the list's
/// structure whatever else is wrong with it.
fn is_sid_string(text: &str) -> bool {
    text.len() <= MAX_SID_STRING_CHARS
        && text.starts_with("S-")
        && text
            .chars()
            .all(|c| c.is_ascii_digit() || c == '-' || c == 'S')
}

/// Which of this crate's two frame mappings a [`Writer`] or [`Reader`] is on.
///
/// Enumerated rather than parameterized by a name and a size: the mappings a
/// `LocalSystem` process publishes are part of what it exposes, and "whatever
/// the caller named" is not a list anyone can review. There are two, both are
/// here, and adding a third is a visible change to this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameChannel {
    /// One frame of the secure desktop, written by the short-lived worker the
    /// helper launches onto `Winlogon` (ADR 0049, ADR 0056).
    SecureDesktop,
    /// Encoded frames from the long-lived session agent (ADR 0085).
    SessionAgent,
}

impl FrameChannel {
    /// The mapping's name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::SecureDesktop => SECURE_DESKTOP_MAPPING_NAME,
            Self::SessionAgent => AGENT_FRAME_MAPPING_NAME,
        }
    }

    /// Bytes of payload the mapping can hold after its header.
    #[must_use]
    pub const fn capacity(self) -> usize {
        match self {
            Self::SecureDesktop => SECURE_DESKTOP_FRAME_CAPACITY_BYTES,
            Self::SessionAgent => AGENT_FRAME_CAPACITY_BYTES,
        }
    }

    /// Bytes of the fixed header both channels share: `width:u32 |
    /// height:u32 | payload_len:u32`, little-endian.
    ///
    /// One layout for both, deliberately. The header is read and written as a
    /// plain byte slice either way, and a second layout would be a second
    /// thing to get subtly wrong across a process boundary for no gain.
    #[must_use]
    pub const fn header_bytes(self) -> usize {
        SECURE_DESKTOP_FRAME_HEADER_BYTES
    }

    /// Total size of the mapping. Neither side ever asks the other how big it
    /// is — both compute it the same way from the same two numbers.
    #[must_use]
    pub const fn mapping_bytes(self) -> usize {
        self.header_bytes() + self.capacity()
    }
}

/// The secure-desktop mapping's total size, asserted against the constant
/// `protocol.rs` publishes so the two cannot drift apart silently.
const _: () = assert!(
    FrameChannel::SecureDesktop.mapping_bytes() == SECURE_DESKTOP_FRAME_MAPPING_BYTES,
    "the secure-desktop channel must be the size protocol.rs says it is"
);

/// A null-terminated UTF-16 copy of `text`, for the `W` entry points.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Builds the mapping's DACL. Shared by [`Writer::create`] and left unused
/// by [`Reader::open`], which only ever opens a mapping that already exists
/// and so never supplies one of its own.
fn security_attributes(sddl: &str) -> Option<(SECURITY_ATTRIBUTES, Vec<u16>)> {
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    let encoded = wide(sddl);
    // SAFETY: `encoded` is a null-terminated wide string that outlives the
    // call; the descriptor it allocates is owned by the caller from here.
    let converted = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(encoded.as_ptr()),
            SDDL_REVISION_1,
            &raw mut descriptor,
            None,
        )
    };
    if converted.is_err() {
        return None;
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(0),
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: false.into(),
    };
    // The descriptor itself has to outlive the `CreateFileMappingW` call that
    // borrows `attributes`, so it is intentionally leaked here rather than
    // freed — `Writer::create` frees it right after that call returns.
    Some((attributes, encoded))
}

/// The service's end: creates the mapping and publishes one frame at a time
/// into it.
///
/// Kept alive for the service's whole lifetime once created; the accept loop
/// serves one connection at a time (`windows_service.rs`), so nothing here
/// needs to be `Sync`, only movable onto the single thread that drives it.
#[derive(Debug)]
pub struct Writer {
    handle: HANDLE,
    base: *mut u8,
    channel: FrameChannel,
}

// SAFETY: `handle` and `base` are used exclusively from the service's single
// accept-loop thread (`windows_service.rs::serve_until_stopped` is a serial
// loop); nothing here is ever touched from two threads at once, which is all
// `Send` promises.
unsafe impl Send for Writer {}

impl Writer {
    /// Creates the secure-desktop mapping with [`FRAME_SDDL`], page-file
    /// backed rather than disk-backed — this crate touches no disk
    /// (ADR 0043) — sized at exactly [`FrameChannel::mapping_bytes`], never
    /// larger and never resized later.
    #[must_use]
    pub fn create() -> Option<Self> {
        Self::create_with(FrameChannel::SecureDesktop, FRAME_SDDL)
    }

    /// Creates the session agent's mapping, writable by the one signed-in
    /// user the host launched an agent for and by nobody else (ADR 0085).
    ///
    /// The agent runs as that user, not as `LocalSystem`, so it has to be
    /// admitted to write — and `IU` would admit **every** interactive user,
    /// which on a machine with a second person signed in is a way to feed a
    /// guest a picture the real agent never published. Naming the one SID is
    /// the difference between enumerating who may write and describing a
    /// category that happens to contain them.
    ///
    /// `user_sid` is the string form of the SID from the token the host
    /// service itself obtained for the console session. A value that is not a
    /// SID string is refused outright rather than formatted into an access
    /// list ([`is_sid_string`]).
    #[must_use]
    pub fn create_for_session_agent(user_sid: &str) -> Option<Self> {
        if !is_sid_string(user_sid) {
            tracing::error!("refusing to build an agent mapping access list from a non-SID");
            return None;
        }
        // `GR` plus `GW`: map the view for reading and writing, and nothing
        // else — notably not `WRITE_DAC`, so the agent cannot widen this from
        // under the host, the same property ADR 0043 gives the request pipe.
        let sddl = format!("D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;{user_sid})");
        Self::create_with(FrameChannel::SessionAgent, &sddl)
    }

    /// The shared body of [`create`](Self::create) and
    /// [`create_for_session_agent`](Self::create_for_session_agent).
    #[must_use]
    fn create_with(channel: FrameChannel, sddl: &str) -> Option<Self> {
        let Some((attributes, _encoded)) = security_attributes(sddl) else {
            tracing::error!(?channel, "cannot build the frame mapping's access list");
            return None;
        };
        let name = wide(channel.name());
        let size = u32::try_from(channel.mapping_bytes()).ok()?;
        // SAFETY: `attributes` and `name` are locals that outlive the call;
        // `INVALID_HANDLE_VALUE` with `PAGE_READWRITE` is the documented way
        // to request a page-file-backed mapping rather than one tied to a
        // file on disk.
        let handle = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                Some(&raw const attributes),
                PAGE_READWRITE,
                0,
                size,
                PCWSTR(name.as_ptr()),
            )
        };
        // SAFETY: `attributes.lpSecurityDescriptor` came from
        // `ConvertStringSecurityDescriptorToSecurityDescriptorW` above and is
        // not read again after this point; the mapping holds its own copy of
        // the descriptor by now.
        unsafe {
            let _ = LocalFree(Some(HLOCAL(attributes.lpSecurityDescriptor)));
        }
        let handle = match handle {
            Ok(handle) if !handle.is_invalid() => handle,
            Err(error) => {
                // `Global\` needs `SeCreateGlobalPrivilege`, which an
                // unelevated caller (including this crate's own unelevated
                // test runs) will not hold — a clean, expected failure the
                // real service running as `LocalSystem` does not hit.
                tracing::error!(?channel, %error, "cannot create the frame mapping");
                return None;
            }
            Ok(_invalid) => {
                tracing::error!(?channel, "the frame mapping handle is invalid");
                return None;
            }
        };
        // SAFETY: `handle` was just created above with `PAGE_READWRITE` and
        // is owned by this call; the requested range is within the size the
        // mapping was created with.
        let view = unsafe { MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, 0) };
        if view.Value.is_null() {
            tracing::error!(?channel, "cannot map the frame mapping");
            // SAFETY: `handle` is live and owned here.
            unsafe {
                let _ = CloseHandle(handle);
            }
            return None;
        }
        Some(Self {
            handle,
            base: view.Value.cast::<u8>(),
            channel,
        })
    }

    /// Opens the mapping the service already [`create`](Self::create)d, for
    /// write (ADR 0056).
    ///
    /// This is the worker's end: a second `LocalSystem` process, launched by
    /// the service into the console session on the secure desktop, that only
    /// ever *fills* a mapping the long-lived service is holding open — never
    /// creates one of its own, so nothing keeps the mapping alive but the
    /// service, and it stays alive exactly as long as ADR 0049's ordering
    /// needs (the service holds it past the pipe reply the client reads
    /// after). `None` if the mapping does not exist yet (the service never
    /// created it) or cannot be opened for write (the caller is not
    /// `LocalSystem`, which the DACL's `SY:GA` is the only writer for).
    #[must_use]
    pub fn open(channel: FrameChannel) -> Option<Self> {
        let name = wide(channel.name());
        // Map for read and write rather than every access: a writer fills the
        // view and never changes the mapping itself. The session agent is not
        // `LocalSystem`, and its own access list admits it to no more than
        // this either.
        let access = FILE_MAP_WRITE.0 | FILE_MAP_READ.0;
        // SAFETY: `name` is a null-terminated wide string that outlives the
        // call; opening an existing mapping by name needs no security
        // attributes of its own.
        let handle = unsafe {
            windows::Win32::System::Memory::OpenFileMappingW(access, false, PCWSTR(name.as_ptr()))
        }
        .ok()?;
        // SAFETY: `handle` was just opened with exactly the access this
        // writable view requests.
        let view = unsafe {
            MapViewOfFile(
                handle,
                windows::Win32::System::Memory::FILE_MAP(access),
                0,
                0,
                0,
            )
        };
        if view.Value.is_null() {
            // SAFETY: `handle` is live and owned here.
            unsafe {
                let _ = CloseHandle(handle);
            }
            return None;
        }
        Some(Self {
            handle,
            base: view.Value.cast::<u8>(),
            channel,
        })
    }

    /// Publishes one frame, overwriting whatever was there before.
    ///
    /// Refuses rather than truncates a payload larger than
    /// [`SECURE_DESKTOP_FRAME_CAPACITY_BYTES`]: a partial BGRA8 image is a
    /// corrupt one, not a smaller one, and the mapping never resizes at
    /// runtime.
    #[must_use]
    pub fn write(&self, width: u32, height: u32, payload: &[u8]) -> bool {
        if payload.len() > self.channel.capacity() {
            tracing::warn!(
                channel = ?self.channel,
                len = payload.len(),
                capacity = self.channel.capacity(),
                "refusing to publish a frame larger than the mapping's capacity"
            );
            return false;
        }
        let Ok(payload_len) = u32::try_from(payload.len()) else {
            return false;
        };
        // SAFETY: `self.base` addresses `SECURE_DESKTOP_FRAME_MAPPING_BYTES`
        // writable bytes for as long as `self` lives (`MapViewOfFile` above);
        // the header is 12 bytes and `payload.len()` was just checked to fit
        // in the capacity after it, so every write below lands inside the
        // mapping.
        unsafe {
            std::ptr::copy_nonoverlapping(width.to_le_bytes().as_ptr(), self.base, 4);
            std::ptr::copy_nonoverlapping(height.to_le_bytes().as_ptr(), self.base.add(4), 4);
            std::ptr::copy_nonoverlapping(payload_len.to_le_bytes().as_ptr(), self.base.add(8), 4);
            std::ptr::copy_nonoverlapping(
                payload.as_ptr(),
                self.base.add(self.channel.header_bytes()),
                payload.len(),
            );
        }
        true
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        // SAFETY: `self.base` came from the `MapViewOfFile` in `create` and
        // is not used again after this; `self.handle` is live and owned.
        unsafe {
            let _ = UnmapViewOfFile(windows::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.base.cast(),
            });
            let _ = CloseHandle(self.handle);
        }
    }
}

/// The client's end: opens the mapping the service already created,
/// read-only, and copies a frame back out of it.
#[derive(Debug)]
pub struct Reader {
    handle: HANDLE,
    base: *const u8,
    channel: FrameChannel,
}

impl Reader {
    /// Opens the mapping. `None` if the service has never created it (not
    /// installed, or never asked to capture) — every failure here collapses
    /// to the same "nothing to read", matching every other client-side
    /// failure in this crate (`client.rs`).
    #[must_use]
    pub fn open(channel: FrameChannel) -> Option<Self> {
        let name = wide(channel.name());
        // SAFETY: `name` is a null-terminated wide string that outlives the
        // call; opening an existing mapping by name needs no security
        // attributes of its own.
        let handle = unsafe {
            windows::Win32::System::Memory::OpenFileMappingW(
                FILE_MAP_READ.0,
                false,
                PCWSTR(name.as_ptr()),
            )
        }
        .ok()?;
        // SAFETY: `handle` was just opened with `FILE_MAP_READ`, which is
        // exactly the access this view requests.
        let view = unsafe { MapViewOfFile(handle, FILE_MAP_READ, 0, 0, 0) };
        if view.Value.is_null() {
            // SAFETY: `handle` is live and owned here.
            unsafe {
                let _ = CloseHandle(handle);
            }
            return None;
        }
        Some(Self {
            handle,
            base: view.Value.cast::<u8>().cast_const(),
            channel,
        })
    }

    /// Copies the current frame out of the mapping, or `None` if the header
    /// names a payload larger than the mapping's own capacity — which can
    /// only mean this side and the service disagree about the layout, never
    /// a real frame, since [`Writer::write`] refuses to publish one that
    /// does not fit.
    #[must_use]
    pub fn read(&self) -> Option<(u32, u32, Vec<u8>)> {
        // SAFETY: `self.base` addresses `SECURE_DESKTOP_FRAME_MAPPING_BYTES`
        // readable bytes for as long as `self` lives (`MapViewOfFile` in
        // `open`); the three header reads are within the first 12 of those
        // bytes.
        let (width, height, payload_len) = unsafe {
            let mut buf4 = [0u8; 4];
            std::ptr::copy_nonoverlapping(self.base, buf4.as_mut_ptr(), 4);
            let width = u32::from_le_bytes(buf4);
            std::ptr::copy_nonoverlapping(self.base.add(4), buf4.as_mut_ptr(), 4);
            let height = u32::from_le_bytes(buf4);
            std::ptr::copy_nonoverlapping(self.base.add(8), buf4.as_mut_ptr(), 4);
            let payload_len = u32::from_le_bytes(buf4);
            (width, height, payload_len)
        };
        let payload_len = usize::try_from(payload_len).ok()?;
        if payload_len > self.channel.capacity() {
            return None;
        }
        let mut data = vec![0u8; payload_len];
        // SAFETY: `payload_len` was just checked to fit within the capacity
        // that follows the 12-byte header, so this range is inside the
        // mapping; `data` is a freshly allocated buffer of exactly that
        // length.
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.base.add(self.channel.header_bytes()),
                data.as_mut_ptr(),
                payload_len,
            );
        }
        Some((width, height, data))
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        // SAFETY: `self.base` came from the `MapViewOfFile` in `open` and is
        // not used again after this; `self.handle` is live and owned.
        unsafe {
            let _ = UnmapViewOfFile(windows::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.base.cast_mut().cast(),
            });
            let _ = CloseHandle(self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// The access list names exactly three trustees, with interactive users
    /// held to read-only — nothing on the client's side ever writes a frame.
    #[test]
    fn the_mapping_admits_only_local_interactive_readers() {
        assert!(FRAME_SDDL.contains(";;;SY)"));
        assert!(FRAME_SDDL.contains(";;;BA)"));
        assert!(
            FRAME_SDDL.contains("GR;;;IU)"),
            "interactive users get read only"
        );
        assert!(
            !FRAME_SDDL.contains("GA;;;IU)"),
            "interactive users must not get write"
        );
    }

    /// A round trip through the real mapping: create, write one frame,
    /// open it independently, and read the same bytes back.
    ///
    /// This exercises the actual `CreateFileMappingW`/`MapViewOfFile`/
    /// `OpenFileMappingW` machinery on this machine, not a fake — the two
    /// ends are genuinely separate handles, the way the service and the
    /// client are two separate processes.
    #[test]
    fn a_written_frame_reads_back_unchanged() {
        let Some(writer) = Writer::create() else {
            // No local mapping support in this sandbox (e.g. a locked-down
            // CI runner without `SeCreateGlobalPrivilege`) is a real failure
            // mode this test tolerates rather than panics on, the same way
            // `client.rs`'s tests tolerate a missing service.
            eprintln!("skipping: could not create the secure-desktop mapping here");
            return;
        };
        let payload: Vec<u8> = (0..64u8).collect();
        assert!(writer.write(8, 2, &payload));

        let reader = Reader::open(FrameChannel::SecureDesktop)
            .expect("the mapping the writer just created must be openable");
        let (width, height, data) = reader.read().expect("a frame was just published");
        assert_eq!((width, height), (8, 2));
        assert_eq!(data, payload);
    }

    /// The two channels are two mappings, not one served twice: different
    /// names, so nothing a worker writes for one can be read out of the
    /// other, and different capacities, because one holds a screenshot and
    /// the other an encoded frame.
    #[test]
    fn the_two_channels_are_separate_mappings() {
        assert_ne!(
            FrameChannel::SecureDesktop.name(),
            FrameChannel::SessionAgent.name()
        );
        assert!(FrameChannel::SecureDesktop.name().starts_with(r"Global\"));
        assert!(FrameChannel::SessionAgent.name().starts_with(r"Global\"));
        assert_eq!(
            FrameChannel::SecureDesktop.capacity(),
            SECURE_DESKTOP_FRAME_CAPACITY_BYTES
        );
        assert_eq!(
            FrameChannel::SessionAgent.capacity(),
            AGENT_FRAME_CAPACITY_BYTES
        );
        for channel in [FrameChannel::SecureDesktop, FrameChannel::SessionAgent] {
            assert_eq!(
                channel.mapping_bytes(),
                channel.header_bytes() + channel.capacity()
            );
        }
    }

    /// The one value this file ever formats into an access list is checked
    /// first, and the check refuses anything that could change the list's
    /// structure rather than trying to escape it.
    #[test]
    fn only_a_sid_string_reaches_an_access_list() {
        assert!(is_sid_string(
            "S-1-5-21-1111111111-2222222222-3333333333-1001"
        ));
        assert!(is_sid_string("S-1-5-18"));
        // Not a SID at all.
        assert!(!is_sid_string(""));
        assert!(!is_sid_string("BA"));
        assert!(!is_sid_string("1-5-18"));
        // Shapes that would end one ACE and open another, or name a trustee
        // by an alias instead of a SID.
        assert!(!is_sid_string("S-1-5-18)(A;;GA;;;WD"));
        assert!(!is_sid_string("S-1-5-18;;;WD"));
        assert!(!is_sid_string("S-1-5-18 "));
        // Longer than any real SID.
        assert!(!is_sid_string(&format!("S-1-{}", "5-".repeat(60))));
    }

    /// A SID that does not pass the check never reaches `CreateFileMappingW`.
    #[test]
    fn an_agent_mapping_is_refused_for_a_non_sid() {
        assert!(Writer::create_for_session_agent("A;;GA;;;WD").is_none());
        assert!(Writer::create_for_session_agent("").is_none());
    }

    /// A payload larger than the capacity is refused outright, never
    /// truncated into a corrupt image.
    #[test]
    fn an_oversized_payload_is_refused_not_truncated() {
        let Some(writer) = Writer::create() else {
            eprintln!("skipping: could not create the secure-desktop mapping here");
            return;
        };
        let oversized = vec![0u8; FrameChannel::SecureDesktop.capacity() + 1];
        assert!(!writer.write(1, 1, &oversized));
    }
}
