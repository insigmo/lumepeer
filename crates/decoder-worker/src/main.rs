//! Sandboxed decoder worker (design doc §4, §11.3).
//!
//! Runs as its own OS process with a platform sandbox and no network or
//! filesystem access beyond the descriptors handed to it. Frames go back to the
//! main process over a shared memory ring buffer, never per-frame
//! serialization (§11.3, §15).
//!
//! It refuses to decode when the sandbox cannot be applied: better no video
//! than an unconfined decoder in the trust boundary.
//!
//! Startup order is normative and must not be reordered for convenience:
//!
//! 1. Map the ring: by the path in `argv[1]` everywhere but Windows, where
//!    `AppContainer` blocks opening it by path at all, so the parent hands
//!    over an already-open handle instead (its raw value, in `argv[2]`) and
//!    the sandbox is already applied by the time this process starts (see
//!    point 2).
//! 2. Apply the sandbox. After this the process can open nothing. On
//!    Windows this only *verifies* confinement rather than applying it,
//!    since `AppContainer` is process-creation-time only and the parent
//!    already did it before spawning this process.
//! 3. Emit the readiness byte, then decode until told to stop.

// `deny` rather than `forbid`: the decode loop touches the shared mapping of
// §11.3 through `lumepeer-media`, which owns the only `unsafe` in that path.
// This binary itself contains none.
#![deny(unsafe_code)]
#![allow(
    unreachable_pub,
    reason = "binary crate: nothing here is a library API"
)]

use std::io::{Read as _, Write as _};

use lumepeer_media::decode::{
    ERROR_BYTE, FRAME_BYTE, PENDING_BYTE, READY_BYTE, STOP_BYTE, SharedRing, WAKE_BYTE,
    platform_sandbox,
};

/// Platform confinement of this process (§11.3).
///
/// Inline module so the file list of §6 stays exact.
mod sandbox {
    use lumepeer_media::decode::SandboxKind;

    /// Applies the sandbox for `kind`.
    ///
    /// # Errors
    /// Fails when the platform confinement cannot be established. The caller
    /// must treat that as fatal: §11.3 forbids decoding unconfined.
    pub fn apply(kind: SandboxKind) -> anyhow::Result<()> {
        match kind {
            SandboxKind::LinuxSeccomp => linux_seccomp(),
            SandboxKind::WindowsAppContainer => windows_app_container(),
            other => anyhow::bail!(
                "the {other:?} sandbox is not implemented yet; refusing to decode unconfined"
            ),
        }
    }

    /// Confinement was already applied by the parent at `CreateProcessW`
    /// time (`lumepeer_media::decode::windows_sandbox::spawn_confined`):
    /// `AppContainer` is a process-*creation*-time restriction, so by the
    /// time this binary's `main` runs it is too late to apply it, only to
    /// check it. This is that check, so that running the worker binary
    /// directly (bypassing `DecoderHandle::spawn`) fails closed instead of
    /// decoding unconfined.
    #[cfg(windows)]
    fn windows_app_container() -> anyhow::Result<()> {
        lumepeer_media::decode::windows_verify_confined().map_err(|e| anyhow::anyhow!("{e}"))
    }

    #[cfg(not(windows))]
    fn windows_app_container() -> anyhow::Result<()> {
        anyhow::bail!("AppContainer is only available on Windows")
    }

    #[cfg(all(target_os = "linux", not(target_os = "android")))]
    fn linux_seccomp() -> anyhow::Result<()> {
        use std::collections::BTreeMap;

        use seccompiler::{BpfProgram, SeccompAction, SeccompFilter, TargetArch};

        // Deny list rather than allow list: the allocator, the runtime and the
        // codec touch a long tail of harmless syscalls, and an incomplete allow
        // list would kill the process on an unrelated libc version. What §11.3
        // actually requires is that the decoder reaches neither the network nor
        // the filesystem, and those are a short, stable set.
        let denied: &[i64] = &[
            libc::SYS_socket,
            libc::SYS_socketpair,
            libc::SYS_connect,
            libc::SYS_bind,
            libc::SYS_listen,
            libc::SYS_accept4,
            libc::SYS_sendto,
            libc::SYS_sendmsg,
            libc::SYS_recvfrom,
            libc::SYS_recvmsg,
            libc::SYS_openat,
            libc::SYS_execve,
            libc::SYS_execveat,
            libc::SYS_ptrace,
            libc::SYS_unlinkat,
            libc::SYS_renameat2,
            libc::SYS_mkdirat,
        ];
        let rules: BTreeMap<i64, Vec<seccompiler::SeccompRule>> =
            denied.iter().map(|call| (*call, Vec::new())).collect();

        let arch = if cfg!(target_arch = "x86_64") {
            TargetArch::x86_64
        } else if cfg!(target_arch = "aarch64") {
            TargetArch::aarch64
        } else {
            anyhow::bail!("no seccomp target architecture for this build; refusing to decode");
        };

        let filter = SeccompFilter::new(
            rules,
            // Everything not named above stays allowed.
            SeccompAction::Allow,
            // Everything named above fails with EPERM instead of killing the
            // process, so a refused syscall is a decode error, not a crash.
            SeccompAction::Errno(libc::EPERM as u32),
            arch,
        )?;
        let program: BpfProgram = filter.try_into()?;
        seccompiler::apply_filter_all_threads(&program)?;
        Ok(())
    }

    #[cfg(not(all(target_os = "linux", not(target_os = "android"))))]
    fn linux_seccomp() -> anyhow::Result<()> {
        anyhow::bail!("seccomp is only available on Linux")
    }
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();

    let Some(kind) = platform_sandbox() else {
        anyhow::bail!("no sandbox mechanism available on this platform; refusing to decode");
    };

    // 1. Everything that needs the filesystem happens before the sandbox.
    let ring = open_ring()?;
    let mut decoder = Decoder::new()?;

    // 2. Confine (or, on Windows, verify the parent already confined this
    // process before spawning it - see sandbox::apply's doc comment). A
    // failure here is fatal on purpose (§11.3).
    sandbox::apply(kind)?;
    tracing::info!(?kind, "decoder worker confined");

    // 3. Only now is any untrusted bitstream touched.
    run(ring, &mut decoder)
}

/// Maps the ring buffer the parent created.
///
/// Every platform but Windows opens it by the path in `argv[1]`. Windows
/// never opens the ring by path at all: `AppContainer` blocks path-based
/// filesystem access even to a file the process was already granted, so the
/// parent hands over an already-open handle instead (see
/// `lumepeer_media::decode::windows_sandbox`'s module doc comment), passed
/// as the raw handle value in `argv[2]`.
#[cfg(not(windows))]
fn open_ring() -> anyhow::Result<SharedRing> {
    let path = std::env::args_os()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("usage: lumepeer-decoder-worker <ring-path>"))?;
    Ok(SharedRing::open(std::path::Path::new(&path))?)
}

#[cfg(windows)]
fn open_ring() -> anyhow::Result<SharedRing> {
    let handle_arg = std::env::args_os().nth(2).ok_or_else(|| {
        anyhow::anyhow!("usage: lumepeer-decoder-worker <ring-path> <ring-handle>")
    })?;
    let handle_value: isize = handle_arg
        .to_str()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow::anyhow!("ring handle argument is not a valid integer"))?;
    let handle = handle_value as std::os::windows::io::RawHandle;
    Ok(SharedRing::from_raw_handle(handle)?)
}

/// Reads wake-up bytes, decodes what the ring holds, answers with a status byte.
fn run(mut ring: SharedRing, decoder: &mut Decoder) -> anyhow::Result<()> {
    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();

    stdout.write_all(&[READY_BYTE])?;
    stdout.flush()?;

    let mut command = [0u8; 1];
    loop {
        match stdin.read_exact(&mut command) {
            Ok(()) => {}
            // The parent went away: exit quietly, there is nothing to decode
            // for anyone.
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e.into()),
        }
        match command[0] {
            STOP_BYTE => return Ok(()),
            WAKE_BYTE => {}
            other => anyhow::bail!("unknown command byte {other}"),
        }

        let Some(slot) = ring.pop_submit()? else {
            stdout.write_all(&[PENDING_BYTE])?;
            stdout.flush()?;
            continue;
        };

        let status = match decoder.decode(&slot.data) {
            Ok(Some(picture)) => {
                ring.push_return(
                    picture.width,
                    picture.height,
                    slot.timestamp_us,
                    &picture.rgba,
                )?;
                FRAME_BYTE
            }
            Ok(None) => PENDING_BYTE,
            Err(error) => {
                // The bitstream is attacker-controlled: a refusal is a normal
                // outcome and must never take the process down (§2.4).
                tracing::warn!(%error, "refusing a bitstream");
                ERROR_BYTE
            }
        };
        stdout.write_all(&[status])?;
        stdout.flush()?;
    }
}

/// One decoded picture in RGBA8.
struct Picture {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

/// H.264 decoder. Software `openh264` for now; the platform hardware decoders
/// of §5.1 arrive with the rest of the platform work in phase 4.
struct Decoder {
    inner: openh264::decoder::Decoder,
}

impl Decoder {
    fn new() -> anyhow::Result<Self> {
        let inner = openh264::decoder::Decoder::new()
            .map_err(|e| anyhow::anyhow!("cannot create the decoder: {e}"))?;
        Ok(Self { inner })
    }

    fn decode(&mut self, bitstream: &[u8]) -> anyhow::Result<Option<Picture>> {
        use openh264::formats::YUVSource as _;

        let Some(yuv) = self
            .inner
            .decode(bitstream)
            .map_err(|e| anyhow::anyhow!("{e}"))?
        else {
            return Ok(None);
        };
        let (width, height) = yuv.dimensions();
        let mut rgba = vec![0u8; width * height * 4];
        yuv.write_rgba8(&mut rgba);
        Ok(Some(Picture {
            width: u32::try_from(width)?,
            height: u32::try_from(height)?,
            rgba,
        }))
    }
}
