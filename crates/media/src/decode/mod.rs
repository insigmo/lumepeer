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

use crate::error::{MediaError, Result};

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

/// Handle to the out-of-process decoder.
#[derive(Debug)]
pub struct DecoderHandle {
    sandbox: SandboxKind,
}

impl DecoderHandle {
    /// Spawns the decoder worker inside the platform sandbox.
    ///
    /// # Errors
    /// [`MediaError::SandboxUnavailable`] if the platform cannot confine the
    /// worker: in that case no decoding starts at all (§11.3), and
    /// [`MediaError::DecoderWorker`] if the process cannot be spawned.
    pub fn spawn() -> Result<Self> {
        let Some(sandbox) = platform_sandbox() else {
            return Err(MediaError::SandboxUnavailable(
                "no sandbox mechanism for this platform".to_owned(),
            ));
        };
        let _ = sandbox;
        Err(MediaError::DecoderWorker(
            "phase 2: spawn decoder-worker with shared memory ring buffer per §11.3".to_owned(),
        ))
    }

    /// Sandbox confining the worker.
    #[must_use]
    pub const fn sandbox(&self) -> SandboxKind {
        self.sandbox
    }
}
