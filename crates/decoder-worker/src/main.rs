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
//! 1. Map the ring file named by `argv[1]`.
//! 2. Apply the sandbox. After this the process can open nothing.
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
            other => anyhow::bail!(
                "the {other:?} sandbox is not implemented yet; refusing to decode unconfined"
            ),
        }
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

    let path = std::env::args_os()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("usage: lumepeer-decoder-worker <ring-path>"))?;

    // 1. Everything that needs the filesystem happens before the sandbox.
    let ring = SharedRing::open(std::path::Path::new(&path))?;
    let mut decoder = Decoder::new()?;

    // 2. Confine. A failure here is fatal on purpose (§11.3).
    sandbox::apply(kind)?;
    tracing::info!(?kind, "decoder worker confined");

    // 3. Only now is any untrusted bitstream touched.
    run(ring, &mut decoder)
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
