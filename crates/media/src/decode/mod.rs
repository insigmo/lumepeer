//! Sandboxed decoding (design doc §11.3).
//!
//! Decoding never happens in the main process. The decoder runs as a separate
//! OS process with a platform sandbox, and frames come back over a shared
//! memory ring buffer rather than per-frame serialization, which would blow the
//! budgets of §15.
//!
//! If no sandbox is available on the platform, video decode is refused and the
//! user is told how to fix the platform policy: degrade towards safety, not
//! convenience.
//!
//! # Wire contract between the two processes
//!
//! - The parent creates the shared mapping and passes its path as `argv[1]`.
//! - Pixel and bitstream data only ever travel through that mapping.
//! - The pipes carry one wake-up byte per queued item and nothing else, so the
//!   per-frame cost stays a single byte, not a serialized frame: parent to
//!   worker over the worker's stdin, worker to parent over its stdout.
//! - The worker maps the file, applies the sandbox and only then decodes. The
//!   order matters: after the sandbox it can no longer open anything.
//!
//! Windows is the one platform where step 2 cannot literally be "the worker
//! applies its own sandbox": `AppContainer` is a process-*creation*-time
//! restriction, not something a running process can drop itself into. There
//! the parent (`DecoderHandle::spawn_with`, via [`windows_sandbox`])
//! launches the worker already confined, with the ring handed over as an
//! inherited handle rather than a path — the worker's own `sandbox::apply`
//! only verifies the confinement took effect. See the `windows_sandbox`
//! module doc comment for the full picture and why handle inheritance,
//! rather than an ACL grant on the ring file, is what makes step 1 possible
//! at all under `AppContainer`.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
#[cfg(not(windows))]
use std::process::{Child, Command, Stdio};

use crate::encode::EncodedFrame;
use crate::error::{MediaError, Result};

#[cfg(target_os = "macos")]
pub use self::macos_sandbox::{PROFILE as MACOS_SANDBOX_PROFILE, confine as macos_confine};
pub use self::shm::{RING_SLOTS, SLOT_PAYLOAD_BYTES, SharedRing, Slot};
#[cfg(windows)]
pub use self::windows_sandbox::verify_confined as windows_verify_confined;

#[cfg(target_os = "macos")]
mod macos_sandbox;
#[cfg(windows)]
mod windows_sandbox;

/// A boxed writer that is also `Debug` (for [`DecoderHandle`]'s derive) and
/// `Send` (the handle crosses into whatever thread its caller lives on).
/// `Write` and `Debug` are both non-auto traits, so combining them in one
/// trait object needs a supertrait rather than `Box<dyn Write + Debug>`
/// directly; `Send`, an auto trait, composes freely either way.
trait DebugWrite: Write + std::fmt::Debug + Send {}
impl<T: Write + std::fmt::Debug + Send> DebugWrite for T {}

/// As [`DebugWrite`], for the read half.
trait DebugRead: Read + std::fmt::Debug + Send {}
impl<T: Read + std::fmt::Debug + Send> DebugRead for T {}

/// Name of the worker binary, which sits next to the main executable.
pub const WORKER_BINARY: &str = "lumepeer-decoder-worker";

/// Target triple this build was compiled for, matching the `-$TARGET_TRIPLE`
/// suffix Tauri's `externalBin` sidecar convention requires on the source
/// binary (see `apps/desktop/src-tauri/tauri.conf.json` and the `stage
/// decoder-worker sidecar` steps in `Taskfile.yml`). Only the six triples the
/// Taskfile actually builds are listed; an unrecognized target has no sidecar
/// candidate and falls back to the bare [`WORKER_BINARY`] name.
const TARGET_TRIPLE: Option<&str> = {
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        Some("x86_64-pc-windows-msvc")
    }
    #[cfg(all(target_os = "windows", target_arch = "aarch64"))]
    {
        Some("aarch64-pc-windows-msvc")
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        Some("x86_64-unknown-linux-gnu")
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        Some("aarch64-unknown-linux-gnu")
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        Some("x86_64-apple-darwin")
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        Some("aarch64-apple-darwin")
    }
    #[cfg(not(any(
        all(target_os = "windows", target_arch = "x86_64"),
        all(target_os = "windows", target_arch = "aarch64"),
        all(target_os = "linux", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "aarch64"),
        all(target_os = "macos", target_arch = "x86_64"),
        all(target_os = "macos", target_arch = "aarch64"),
    )))]
    {
        None
    }
};

/// Sandbox mechanism used to confine the decoder worker (§11.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxKind {
    /// seccomp-BPF, only explicitly passed memory and file descriptors.
    LinuxSeccomp,
    /// `AppContainer` with a minimal capability set.
    WindowsAppContainer,
    /// `sandbox_init` profile without filesystem access.
    MacosSandbox,
    /// Android app sandbox plus a separate `:decoder` process.
    AndroidIsolatedProcess,
}

/// Sandbox this build would apply on the current platform.
#[must_use]
pub const fn platform_sandbox() -> Option<SandboxKind> {
    #[cfg(target_os = "android")]
    {
        Some(SandboxKind::AndroidIsolatedProcess)
    }
    #[cfg(all(target_os = "linux", not(target_os = "android")))]
    {
        Some(SandboxKind::LinuxSeccomp)
    }
    #[cfg(target_os = "windows")]
    {
        Some(SandboxKind::WindowsAppContainer)
    }
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        Some(SandboxKind::MacosSandbox)
    }
    #[cfg(not(any(
        target_os = "android",
        target_os = "linux",
        target_os = "windows",
        target_os = "macos",
        target_os = "ios"
    )))]
    {
        None
    }
}

/// One decoded picture handed back by the worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedFrame {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Timestamp copied from the encoded frame.
    pub timestamp_us: u64,
    /// RGBA8 pixels.
    pub data: Vec<u8>,
}

/// OS process running the worker, abstracted over how it was spawned.
///
/// Every platform but Windows spawns through [`std::process::Command`], so
/// this is a thin pass-through there. Windows cannot apply `AppContainer` to
/// an already-running process (see the module doc comment and
/// `windows_sandbox`), so the confined worker is launched by hand through
/// `CreateProcessW`, and this is what lets [`DecoderHandle`] stay ignorant
/// of that difference everywhere else.
#[derive(Debug)]
enum WorkerProcess {
    #[cfg(not(windows))]
    Command(Child),
    #[cfg(windows)]
    Confined(windows_sandbox::ConfinedProcess),
}

impl WorkerProcess {
    fn id(&self) -> u32 {
        match self {
            #[cfg(not(windows))]
            Self::Command(child) => child.id(),
            #[cfg(windows)]
            Self::Confined(process) => process.id(),
        }
    }

    fn kill(&mut self) {
        match self {
            #[cfg(not(windows))]
            Self::Command(child) => {
                let _ = child.kill();
            }
            #[cfg(windows)]
            Self::Confined(process) => process.kill(),
        }
    }

    fn wait(&mut self) {
        match self {
            #[cfg(not(windows))]
            Self::Command(child) => {
                let _ = child.wait();
            }
            #[cfg(windows)]
            Self::Confined(process) => process.wait(),
        }
    }
}

/// Handle to the out-of-process decoder.
#[derive(Debug)]
pub struct DecoderHandle {
    sandbox: SandboxKind,
    process: WorkerProcess,
    stdin: Box<dyn DebugWrite>,
    stdout: Box<dyn DebugRead>,
    ring: SharedRing,
    /// Kept so the backing file is removed when the handle drops.
    path: PathBuf,
}

/// Picks the worker binary to spawn next to `exe`.
///
/// Tries the bare [`WORKER_BINARY`] name first — what a `cargo build`
/// workspace tree drops beside the debug binary, so nothing changes for
/// local/dev runs — then the target-triple-suffixed sidecar name an
/// installed Tauri build actually ships (`externalBin` copies the binary in
/// verbatim, triple suffix included; it is not stripped at bundle time).
/// Falls back to the bare path when neither exists so the resulting spawn
/// error still names a concrete, useful path.
fn locate_worker_binary(exe: &Path) -> PathBuf {
    let mut bare = exe.with_file_name(WORKER_BINARY);
    if cfg!(windows) {
        bare.set_extension("exe");
    }
    if bare.is_file() {
        return bare;
    }
    if let Some(triple) = TARGET_TRIPLE {
        let mut sidecar = exe.with_file_name(format!("{WORKER_BINARY}-{triple}"));
        if cfg!(windows) {
            sidecar.set_extension("exe");
        }
        if sidecar.is_file() {
            return sidecar;
        }
    }
    bare
}

impl DecoderHandle {
    /// Spawns the decoder worker inside the platform sandbox, looking for the
    /// worker binary next to the current executable.
    ///
    /// # Errors
    /// [`MediaError::SandboxUnavailable`] if the platform cannot confine the
    /// worker: in that case no decoding starts at all (§11.3), and
    /// [`MediaError::DecoderWorker`] if the process cannot be spawned.
    pub fn spawn() -> Result<Self> {
        let exe = std::env::current_exe()
            .map_err(|e| MediaError::DecoderWorker(format!("cannot locate the executable: {e}")))?;
        Self::spawn_with(&locate_worker_binary(&exe))
    }

    /// Spawns a specific worker binary.
    ///
    /// # Errors
    /// As [`Self::spawn`].
    pub fn spawn_with(program: &Path) -> Result<Self> {
        let Some(sandbox) = platform_sandbox() else {
            return Err(MediaError::SandboxUnavailable(
                "no sandbox mechanism for this platform".to_owned(),
            ));
        };

        let (ring, path) = SharedRing::create()?;

        #[cfg(windows)]
        let (process, stdin, mut stdout): (
            WorkerProcess,
            Box<dyn DebugWrite>,
            Box<dyn DebugRead>,
        ) = {
            let (confined, stdin, stdout) = windows_sandbox::spawn_confined(program, &path)?;
            (WorkerProcess::Confined(confined), stdin, stdout)
        };

        #[cfg(not(windows))]
        let (process, stdin, mut stdout): (
            WorkerProcess,
            Box<dyn DebugWrite>,
            Box<dyn DebugRead>,
        ) = {
            let mut child = Command::new(program)
                .arg(&path)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .map_err(|e| MediaError::DecoderWorker(format!("cannot spawn the worker: {e}")))?;

            let stdin = child
                .stdin
                .take()
                .ok_or_else(|| MediaError::DecoderWorker("worker has no stdin".to_owned()))?;
            let stdout = child
                .stdout
                .take()
                .ok_or_else(|| MediaError::DecoderWorker("worker has no stdout".to_owned()))?;
            (
                WorkerProcess::Command(child),
                Box::new(stdin) as Box<dyn DebugWrite>,
                Box::new(stdout) as Box<dyn DebugRead>,
            )
        };

        // The worker reports once the sandbox is applied. Until that byte
        // arrives nothing has been decoded, so a worker that refuses to confine
        // itself fails here rather than decoding unconfined (§11.3).
        let mut ready = [0u8; 1];
        stdout.read_exact(&mut ready).map_err(|e| {
            MediaError::SandboxUnavailable(format!("worker did not confine itself: {e}"))
        })?;
        if ready[0] != READY_BYTE {
            return Err(MediaError::SandboxUnavailable(
                "worker sent an unexpected readiness marker".to_owned(),
            ));
        }

        Ok(Self {
            sandbox,
            process,
            stdin,
            stdout,
            ring,
            path,
        })
    }

    /// Sandbox confining the worker.
    #[must_use]
    pub const fn sandbox(&self) -> SandboxKind {
        self.sandbox
    }

    /// OS process id of the worker, for out-of-band resource sampling (§15,
    /// §16.2). Never used to reach into the sandboxed process otherwise: the
    /// only channels into it are the ring and the pipes above.
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.process.id()
    }

    /// Sends one encoded frame to the worker and waits for the picture.
    ///
    /// # Errors
    /// [`MediaError::Decode`] if the worker reports a decode failure,
    /// [`MediaError::DecoderWorker`] if it dies or the frame is larger than a
    /// ring slot.
    pub fn decode(&mut self, frame: &EncodedFrame) -> Result<Option<DecodedFrame>> {
        self.ring.push_submit(frame.timestamp_us, &frame.data)?;
        self.stdin
            .write_all(&[WAKE_BYTE])
            .and_then(|()| self.stdin.flush())
            .map_err(|e| MediaError::DecoderWorker(format!("worker stdin closed: {e}")))?;

        let mut answer = [0u8; 1];
        self.stdout
            .read_exact(&mut answer)
            .map_err(|e| MediaError::DecoderWorker(format!("worker stdout closed: {e}")))?;
        match answer[0] {
            FRAME_BYTE => {
                let Some(slot) = self.ring.pop_return()? else {
                    return Err(MediaError::DecoderWorker(
                        "worker signalled a frame but the ring was empty".to_owned(),
                    ));
                };
                Ok(Some(DecodedFrame {
                    width: slot.width,
                    height: slot.height,
                    timestamp_us: slot.timestamp_us,
                    data: slot.data,
                }))
            }
            // A bitstream that carries no complete picture yet is normal, not
            // an error: more data is simply needed.
            PENDING_BYTE => Ok(None),
            ERROR_BYTE => Err(MediaError::Decode(
                "worker refused the bitstream".to_owned(),
            )),
            other => Err(MediaError::DecoderWorker(format!(
                "worker sent an unknown status byte {other}"
            ))),
        }
    }

    /// Stops the worker and waits for it, dropping the shared mapping.
    pub fn shutdown(&mut self) {
        let _ = self.stdin.write_all(&[STOP_BYTE]);
        let _ = self.stdin.flush();
        self.process.wait();
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Drop for DecoderHandle {
    fn drop(&mut self) {
        // Never leave a decoder running behind a session that ended (§8.1).
        self.process.kill();
        self.process.wait();
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Wake-up byte: one queued frame is waiting in the submit ring.
pub const WAKE_BYTE: u8 = 0x01;
/// Stop byte: the worker should exit.
pub const STOP_BYTE: u8 = 0x02;
/// Readiness byte: the sandbox is applied and decoding may start.
pub const READY_BYTE: u8 = 0x10;
/// One decoded picture is waiting in the return ring.
pub const FRAME_BYTE: u8 = 0x11;
/// The bitstream was accepted but produced no complete picture yet.
pub const PENDING_BYTE: u8 = 0x12;
/// The bitstream was refused.
pub const ERROR_BYTE: u8 = 0x13;

/// Shared memory ring buffer between the two processes (§11.3).
///
/// This is the one place in the crate that needs `unsafe`: the mapping is
/// shared with another process, so the indices have to be atomics living inside
/// it rather than in either process's own memory.
#[allow(
    unsafe_code,
    reason = "§11.3 requires a shared memory ring buffer; every block carries a SAFETY note, per §21"
)]
pub mod shm {
    use std::fs::OpenOptions;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

    use memmap2::MmapMut;

    use crate::error::{MediaError, Result};

    /// Slots per direction. Small: the pipeline is request/response and a deep
    /// queue would only add latency against the glass-to-glass budget of §15.
    pub const RING_SLOTS: usize = 4;
    /// Payload bytes per slot. One 4K RGBA picture does not fit and is not
    /// meant to: the worker returns pictures capped at this size.
    pub const SLOT_PAYLOAD_BYTES: usize = 8 * 1024 * 1024;

    /// A slot has to hold one RGBA8 picture of the largest size the pipeline
    /// carries, or a host at that size would decode into nothing and the
    /// guest would wait forever for a screen (§18; ADR 0018). Raising
    /// `MAX_PICTURE_PIXELS` without raising this stops the build here rather
    /// than at runtime on somebody's 4K monitor.
    const _: () = assert!(
        lumepeer_core::constants::MAX_PICTURE_PIXELS * 4 <= SLOT_PAYLOAD_BYTES,
        "SLOT_PAYLOAD_BYTES cannot hold one MAX_PICTURE_PIXELS RGBA8 picture"
    );

    /// Bytes of the per-slot header: length, width, height, timestamp.
    const SLOT_HEADER_BYTES: usize = 24;
    /// Bytes of one slot.
    const SLOT_BYTES: usize = SLOT_HEADER_BYTES + SLOT_PAYLOAD_BYTES;
    /// Bytes of the shared header: four indices plus padding to 64 bytes.
    const HEADER_BYTES: usize = 64;
    /// Total mapping size.
    const MAPPING_BYTES: usize = HEADER_BYTES + 2 * RING_SLOTS * SLOT_BYTES;

    /// Offsets of the four indices inside the header.
    const SUBMIT_HEAD: usize = 0;
    const SUBMIT_TAIL: usize = 4;
    const RETURN_HEAD: usize = 8;
    const RETURN_TAIL: usize = 12;

    /// Which of the two rings a slot belongs to.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Direction {
        /// Parent to worker: encoded bitstream.
        Submit,
        /// Worker to parent: decoded pictures.
        Return,
    }

    /// One of the two rings: its direction and where its indices live.
    #[derive(Debug, Clone, Copy)]
    struct Ring {
        direction: Direction,
        head: usize,
        tail: usize,
    }

    impl Ring {
        /// Parent to worker.
        const SUBMIT: Self = Self {
            direction: Direction::Submit,
            head: SUBMIT_HEAD,
            tail: SUBMIT_TAIL,
        };
        /// Worker to parent.
        const RETURN: Self = Self {
            direction: Direction::Return,
            head: RETURN_HEAD,
            tail: RETURN_TAIL,
        };
    }

    /// One item being written into a ring.
    #[derive(Debug, Clone, Copy)]
    struct Item<'a> {
        width: u32,
        height: u32,
        timestamp_us: u64,
        payload: &'a [u8],
    }

    /// One item read out of a ring.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Slot {
        /// Picture width, 0 for bitstream slots.
        pub width: u32,
        /// Picture height, 0 for bitstream slots.
        pub height: u32,
        /// Timestamp carried through the pipeline.
        pub timestamp_us: u64,
        /// Payload bytes.
        pub data: Vec<u8>,
    }

    /// One end of the shared ring.
    #[derive(Debug)]
    pub struct SharedRing {
        map: MmapMut,
    }

    impl SharedRing {
        /// Creates the backing file and maps it. The path is handed to the
        /// worker, which opens it before it is confined.
        ///
        /// # Errors
        /// [`MediaError::DecoderWorker`] if the file cannot be created or
        /// mapped.
        pub fn create() -> Result<(Self, PathBuf)> {
            // The clock alone is not fine-grained enough on every platform to
            // tell apart two rings created back to back in the same process
            // (e.g. macOS's `SystemTime` resolution is coarser than a
            // nanosecond), so a counter guarantees uniqueness that the clock
            // can't.
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let mut path = std::env::temp_dir();
            path.push(format!(
                "lumepeer-decoder-{}-{}-{}.ring",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.subsec_nanos()),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));

            let mut options = OpenOptions::new();
            options.read(true).write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                // Only this user: the mapping carries screen contents.
                options.mode(0o600);
            }
            let file = options
                .open(&path)
                .map_err(|e| MediaError::DecoderWorker(e.to_string()))?;
            file.set_len(MAPPING_BYTES as u64)
                .map_err(|e| MediaError::DecoderWorker(e.to_string()))?;

            // SAFETY: `file` was just created with `create_new`, is 0600 and its
            // path is unique per process and instant, so no other process can be
            // mutating it before the worker we spawn opens it. Everything the
            // mapping is read through below is either a plain byte copy or an
            // atomic load, so a racing worker cannot produce a data race.
            let map = unsafe { MmapMut::map_mut(&file) }
                .map_err(|e| MediaError::DecoderWorker(e.to_string()))?;

            Ok((Self { map }, path))
        }

        /// Opens an existing mapping by path. Used by the worker before it is
        /// confined.
        ///
        /// # Errors
        /// [`MediaError::DecoderWorker`] if the file cannot be opened, has the
        /// wrong size, or cannot be mapped.
        pub fn open(path: &std::path::Path) -> Result<Self> {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .map_err(|e| MediaError::DecoderWorker(e.to_string()))?;
            let length = file
                .metadata()
                .map_err(|e| MediaError::DecoderWorker(e.to_string()))?
                .len();
            if length != MAPPING_BYTES as u64 {
                return Err(MediaError::DecoderWorker(format!(
                    "ring file has {length} bytes, expected {MAPPING_BYTES}"
                )));
            }

            // SAFETY: same contract as in `create`: the only writers are this
            // process and the peer that created the file, both of which touch
            // the mapping exclusively through the accessors below.
            let map = unsafe { MmapMut::map_mut(&file) }
                .map_err(|e| MediaError::DecoderWorker(e.to_string()))?;
            Ok(Self { map })
        }

        /// Opens the ring from an already-open handle inherited from the
        /// parent (Windows only). `AppContainer` confinement is applied by
        /// the parent at process-creation time (`decode::windows_sandbox`),
        /// before the worker's `main` even starts, so unlike [`Self::open`]
        /// the worker never opens the ring by path itself — it maps the
        /// handle the parent already duplicated in for exactly this
        /// purpose.
        ///
        /// # Errors
        /// [`MediaError::DecoderWorker`] if the handle does not map a file
        /// of the expected size.
        #[cfg(windows)]
        #[allow(
            clippy::not_unsafe_ptr_arg_deref,
            reason = "kept as a safe fn deliberately: the worker binary that is the only realistic caller has #![deny(unsafe_code)] and cannot open an unsafe block. `handle` is not attacker- or user-controlled input - it is a value the trusted parent process places directly into this worker's own argv in spawn_confined, the worker's only source for it. The unsafe File::from_raw_handle call below is what actually dereferences it, and that block carries its own SAFETY note."
        )]
        pub fn from_raw_handle(handle: std::os::windows::io::RawHandle) -> Result<Self> {
            use std::os::windows::io::FromRawHandle as _;

            // SAFETY: `handle` is a HANDLE the parent duplicated into this
            // process specifically to be the ring file, before this process
            // even started (see `decode::windows_sandbox::spawn_confined`);
            // it is open, valid, and from this point on uniquely owned by
            // this process, exactly like the `File` opened by path above.
            let file = unsafe { std::fs::File::from_raw_handle(handle) };
            let length = file
                .metadata()
                .map_err(|e| MediaError::DecoderWorker(e.to_string()))?
                .len();
            if length != MAPPING_BYTES as u64 {
                return Err(MediaError::DecoderWorker(format!(
                    "ring handle maps {length} bytes, expected {MAPPING_BYTES}"
                )));
            }

            // SAFETY: same contract as `open`/`create`: the only writers are
            // this process and its parent, both touching the mapping only
            // through the accessors below.
            let map = unsafe { MmapMut::map_mut(&file) }
                .map_err(|e| MediaError::DecoderWorker(e.to_string()))?;
            Ok(Self { map })
        }

        /// Borrows one of the four shared indices.
        #[allow(
            clippy::cast_ptr_alignment,
            reason = "the base of an mmap is page aligned and every offset is a multiple of 4; see the SAFETY note"
        )]
        fn index(&self, offset: usize) -> &AtomicU32 {
            let pointer = self.map.as_ptr().wrapping_add(offset).cast::<AtomicU32>();
            // SAFETY: `offset` is one of the four constants above, all inside
            // the 64 byte header, and the mapping is at least `MAPPING_BYTES`
            // long. The header starts at the mapping's base, which `mmap`
            // guarantees to be page aligned and therefore 4 byte aligned, and
            // every offset is a multiple of 4. The referenced bytes are only
            // ever accessed as this atomic, in this process and in the peer.
            unsafe { &*pointer }
        }

        fn slot_offset(direction: Direction, index: usize) -> usize {
            let ring = match direction {
                Direction::Submit => 0,
                Direction::Return => RING_SLOTS,
            };
            HEADER_BYTES + (ring + index % RING_SLOTS) * SLOT_BYTES
        }

        fn write_slot(
            &mut self,
            direction: Direction,
            index: usize,
            width: u32,
            height: u32,
            timestamp_us: u64,
            payload: &[u8],
        ) -> Result<()> {
            if payload.len() > SLOT_PAYLOAD_BYTES {
                return Err(MediaError::DecoderWorker(format!(
                    "payload of {} bytes exceeds the {SLOT_PAYLOAD_BYTES} byte slot",
                    payload.len()
                )));
            }
            let base = Self::slot_offset(direction, index);
            let length = u64::try_from(payload.len()).unwrap_or(0);
            self.map[base..base + 8].copy_from_slice(&length.to_le_bytes());
            self.map[base + 8..base + 12].copy_from_slice(&width.to_le_bytes());
            self.map[base + 12..base + 16].copy_from_slice(&height.to_le_bytes());
            self.map[base + 16..base + 24].copy_from_slice(&timestamp_us.to_le_bytes());
            self.map[base + SLOT_HEADER_BYTES..base + SLOT_HEADER_BYTES + payload.len()]
                .copy_from_slice(payload);
            Ok(())
        }

        fn read_slot(&self, direction: Direction, index: usize) -> Result<Slot> {
            let base = Self::slot_offset(direction, index);
            let mut length_bytes = [0u8; 8];
            length_bytes.copy_from_slice(&self.map[base..base + 8]);
            let length = usize::try_from(u64::from_le_bytes(length_bytes))
                .map_err(|_| MediaError::DecoderWorker("slot length overflow".to_owned()))?;
            if length > SLOT_PAYLOAD_BYTES {
                return Err(MediaError::DecoderWorker(
                    "slot announces more bytes than it holds".to_owned(),
                ));
            }
            let mut width = [0u8; 4];
            width.copy_from_slice(&self.map[base + 8..base + 12]);
            let mut height = [0u8; 4];
            height.copy_from_slice(&self.map[base + 12..base + 16]);
            let mut timestamp = [0u8; 8];
            timestamp.copy_from_slice(&self.map[base + 16..base + 24]);

            Ok(Slot {
                width: u32::from_le_bytes(width),
                height: u32::from_le_bytes(height),
                timestamp_us: u64::from_le_bytes(timestamp),
                data: self.map[base + SLOT_HEADER_BYTES..base + SLOT_HEADER_BYTES + length]
                    .to_vec(),
            })
        }

        fn push(&mut self, ring: Ring, item: Item<'_>) -> Result<()> {
            let (direction, head_offset, tail_offset) = (ring.direction, ring.head, ring.tail);
            let head = self.index(head_offset).load(Ordering::Relaxed);
            let tail = self.index(tail_offset).load(Ordering::Acquire);
            if head.wrapping_sub(tail) as usize >= RING_SLOTS {
                return Err(MediaError::DecoderWorker("ring is full".to_owned()));
            }
            self.write_slot(
                direction,
                head as usize,
                item.width,
                item.height,
                item.timestamp_us,
                item.payload,
            )?;
            // Release: the slot bytes above must be visible before the peer
            // sees the new head.
            self.index(head_offset)
                .store(head.wrapping_add(1), Ordering::Release);
            Ok(())
        }

        fn pop(&mut self, ring: Ring) -> Result<Option<Slot>> {
            let (direction, head_offset, tail_offset) = (ring.direction, ring.head, ring.tail);
            let tail = self.index(tail_offset).load(Ordering::Relaxed);
            let head = self.index(head_offset).load(Ordering::Acquire);
            if head == tail {
                return Ok(None);
            }
            let slot = self.read_slot(direction, tail as usize)?;
            self.index(tail_offset)
                .store(tail.wrapping_add(1), Ordering::Release);
            Ok(Some(slot))
        }

        /// Queues an encoded frame for the worker.
        ///
        /// # Errors
        /// [`MediaError::DecoderWorker`] if the ring is full or the frame is
        /// larger than a slot.
        pub fn push_submit(&mut self, timestamp_us: u64, bitstream: &[u8]) -> Result<()> {
            self.push(
                Ring::SUBMIT,
                Item {
                    width: 0,
                    height: 0,
                    timestamp_us,
                    payload: bitstream,
                },
            )
        }

        /// Takes the next encoded frame, if the parent queued one.
        ///
        /// # Errors
        /// [`MediaError::DecoderWorker`] on a malformed slot.
        pub fn pop_submit(&mut self) -> Result<Option<Slot>> {
            self.pop(Ring::SUBMIT)
        }

        /// Queues a decoded picture for the parent.
        ///
        /// # Errors
        /// [`MediaError::DecoderWorker`] if the ring is full or the picture is
        /// larger than a slot.
        pub fn push_return(
            &mut self,
            width: u32,
            height: u32,
            timestamp_us: u64,
            pixels: &[u8],
        ) -> Result<()> {
            self.push(
                Ring::RETURN,
                Item {
                    width,
                    height,
                    timestamp_us,
                    payload: pixels,
                },
            )
        }

        /// Takes the next decoded picture, if the worker produced one.
        ///
        /// # Errors
        /// [`MediaError::DecoderWorker`] on a malformed slot.
        pub fn pop_return(&mut self) -> Result<Option<Slot>> {
            self.pop(Ring::RETURN)
        }
    }

    #[cfg(test)]
    mod tests {
        #![allow(clippy::unwrap_used)]

        use super::*;

        #[test]
        fn both_rings_round_trip_and_stay_independent() {
            let (mut ring, path) = SharedRing::create().unwrap();
            assert!(ring.pop_submit().unwrap().is_none());
            assert!(ring.pop_return().unwrap().is_none());

            ring.push_submit(42, b"bitstream").unwrap();
            ring.push_return(4, 2, 43, b"pixels").unwrap();

            let submitted = ring.pop_submit().unwrap().unwrap();
            assert_eq!(submitted.timestamp_us, 42);
            assert_eq!(submitted.data, b"bitstream");

            let returned = ring.pop_return().unwrap().unwrap();
            assert_eq!((returned.width, returned.height), (4, 2));
            assert_eq!(returned.data, b"pixels");

            assert!(ring.pop_submit().unwrap().is_none());
            std::fs::remove_file(path).unwrap();
        }

        #[test]
        fn a_full_ring_refuses_instead_of_overwriting() {
            let (mut ring, path) = SharedRing::create().unwrap();
            for i in 0..RING_SLOTS {
                ring.push_submit(i as u64, b"x").unwrap();
            }
            assert!(ring.push_submit(99, b"x").is_err());
            // Draining one slot makes room again, and order is preserved.
            assert_eq!(ring.pop_submit().unwrap().unwrap().timestamp_us, 0);
            ring.push_submit(99, b"x").unwrap();
            std::fs::remove_file(path).unwrap();
        }

        #[test]
        fn an_oversized_payload_is_refused() {
            let (mut ring, path) = SharedRing::create().unwrap();
            let too_big = vec![0u8; SLOT_PAYLOAD_BYTES + 1];
            assert!(ring.push_submit(0, &too_big).is_err());
            std::fs::remove_file(path).unwrap();
        }

        #[test]
        fn a_second_mapping_of_the_same_file_sees_the_same_items() {
            let (mut writer, path) = SharedRing::create().unwrap();
            let mut reader = SharedRing::open(&path).unwrap();
            writer.push_submit(7, b"shared").unwrap();
            let seen = reader.pop_submit().unwrap().unwrap();
            assert_eq!(seen.timestamp_us, 7);
            assert_eq!(seen.data, b"shared");
            std::fs::remove_file(path).unwrap();
        }
    }
}
