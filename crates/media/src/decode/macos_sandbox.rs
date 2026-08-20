//! macOS `sandbox_init(3)` confinement for the decoder worker (§11.3).
//!
//! The macOS counterpart of the Linux seccomp filter in
//! `crates/decoder-worker/src/main.rs`: a running process fences *itself* in,
//! after it has opened everything it will ever need (the ring buffer of
//! §11.3, its stdin/stdout pipes and the `openh264` decoder) and before it
//! touches the first attacker-controlled byte. Unlike Windows `AppContainer`
//! (see `windows_sandbox`) nothing has to happen in the parent: the ordering
//! §11.3 asks for — map the ring, *then* confine, then decode — holds here
//! exactly as written.
//!
//! ## The profile
//!
//! [`PROFILE`] is Sandbox Profile Language: `(deny default)` and nothing
//! else. That is deliberately stricter than the Linux filter, which is a
//! deny *list* (there, an allow-list of syscall numbers would kill the
//! process on an unrelated libc version, while the syscalls that reach the
//! network or the filesystem are a short, stable set). Seatbelt does not
//! have that problem: it mediates named *operations* rather than syscall
//! numbers, and everything a pure-computation process actually does —
//! anonymous memory, `malloc` growth, thread creation, reads and writes on
//! descriptors it already holds, faulting in pages of libraries mapped
//! before confinement — is not a mediated operation at all. So here the
//! strict direction is also the one that survives an OS upgrade, and
//! deny-by-default is what the ground rules ask for anyway.
//!
//! Verified on macOS 26.6 (ADR 0019), and by
//! `the_profile_denies_the_filesystem_and_the_network` below on every run:
//! after `sandbox_init` the worker can neither open a path nor reach the
//! network, while the already-open ring, stdin/stdout and stderr keep
//! working and decoding proceeds normally.
//!
//! ## Why a deprecated API
//!
//! `sandbox_init` has carried a deprecation attribute since macOS 10.8 and
//! is still the only way for a process to confine *itself*. The supported
//! alternative — App Sandbox entitlements — is applied by the kernel at
//! `exec` time from the code signature, cannot be tightened afterwards, and
//! would make the confinement a property of how the bundle was signed rather
//! than of the worker binary, so a worker started outside a signed bundle
//! (`cargo test`, a developer build) would decode unconfined instead of
//! refusing. §11.3 wants the opposite. ADR 0019 records the trade-off; if
//! Apple ever removes the symbol the outcome is the failure §11.3 already
//! defines: no sandbox, no decoding.

#![allow(
    unsafe_code,
    reason = "sandbox_init(3) is a libSystem C entry point with no safe binding; the blocks below carry SAFETY notes, per §21"
)]

use std::ffi::{CStr, CString, c_char, c_int};

use crate::error::{MediaError, Result};

/// The Sandbox Profile Language profile the decoder worker confines itself
/// with: deny everything.
///
/// Public so a test — or an operator reading a bug report — sees the exact
/// text that was applied rather than a description of it.
pub const PROFILE: &str = "(version 1)\n(deny default)\n";

// SAFETY (declaration): both are libSystem entry points, present in every
// macOS process, with the signatures documented in `sandbox.h`.
// `sandbox_init` takes a NUL-terminated profile, a flag word (0 = the profile
// is an inline SBPL string rather than the name of a built-in one) and an
// out-pointer for an owned error string; `sandbox_free_error` frees exactly
// that string.
unsafe extern "C" {
    fn sandbox_init(profile: *const c_char, flags: u64, errorbuf: *mut *mut c_char) -> c_int;
    fn sandbox_free_error(errorbuf: *mut c_char);
}

/// Applies [`PROFILE`] to this process, irreversibly.
///
/// Everything the worker needs must already be open when this is called:
/// afterwards it can open nothing (§11.3).
///
/// # Errors
/// [`MediaError::SandboxUnavailable`] if the profile cannot be compiled or
/// applied. The caller must treat that as fatal: §11.3 forbids decoding
/// unconfined.
pub fn confine() -> Result<()> {
    let profile = CString::new(PROFILE).map_err(|e| {
        MediaError::SandboxUnavailable(format!(
            "the decoder sandbox profile is not a C string: {e}"
        ))
    })?;
    let mut error: *mut c_char = std::ptr::null_mut();

    // SAFETY: `profile` is a live NUL-terminated string for the duration of
    // the call and is only read by it. `error` is a valid, writable
    // out-pointer; `sandbox_init` either leaves it null or stores a string it
    // owns, released below through `sandbox_free_error` and never touched
    // after that.
    let rc = unsafe { sandbox_init(profile.as_ptr(), 0, &raw mut error) };
    if rc == 0 {
        return Ok(());
    }

    let detail = if error.is_null() {
        format!("sandbox_init failed with {rc} and no detail")
    } else {
        // SAFETY: on failure `sandbox_init` stores a NUL-terminated string it
        // allocated. It is copied out before being handed straight back to
        // `sandbox_free_error`, the only correct way to release it, and the
        // raw pointer is not used again afterwards.
        let message = unsafe { CStr::from_ptr(error) }
            .to_string_lossy()
            .into_owned();
        // SAFETY: as above — `error` is non-null and came from this same
        // `sandbox_init` call, and this is its single release.
        unsafe { sandbox_free_error(error) };
        message
    };
    Err(MediaError::SandboxUnavailable(format!(
        "sandbox_init refused the decoder profile: {detail}; refusing to decode unconfined"
    )))
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "a failed assumption must fail the test"
    )]

    use super::*;

    /// Marks the re-executed child of the test below. The sandbox is
    /// irreversible and process-wide, so it cannot be applied inside the test
    /// process itself without confining every test that runs after it: the
    /// check needs a process of its own, and re-running this same test binary
    /// is how to get one without shipping a second helper binary.
    const CHILD: &str = "LUMEPEER_TEST_MACOS_SANDBOX_CHILD";
    /// Path of the test below, for the child's `--exact` filter.
    const TEST_NAME: &str =
        "decode::macos_sandbox::tests::the_profile_denies_the_filesystem_and_the_network";

    #[test]
    fn the_profile_denies_the_filesystem_and_the_network() {
        if std::env::var_os(CHILD).is_some() {
            child();
        }

        let exe = std::env::current_exe().expect("the test binary has a path");
        let output = std::process::Command::new(exe)
            .args(["--exact", TEST_NAME, "--nocapture", "--test-threads=1"])
            .env(CHILD, "1")
            .output()
            .expect("the test binary re-executes");
        let report = String::from_utf8_lossy(&output.stdout);

        for expected in [
            "confined=ok",
            "open=denied",
            "connect=denied",
            "already-open-fd=ok",
            "compute=ok",
        ] {
            assert!(
                report.contains(expected),
                "the confined child did not report {expected}\nstdout: {report}\nstderr: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    /// The confined half: apply the profile, then prove the two things §11.3
    /// actually cares about — no filesystem, no network — while descriptors
    /// opened *before* confinement keep working.
    fn child() -> ! {
        use std::io::{ErrorKind, Read as _};

        // Opened before the sandbox, exactly as the ring buffer is.
        let mut already_open = std::fs::File::open("/etc/hosts").expect("/etc/hosts is readable");

        match confine() {
            Ok(()) => println!("confined=ok"),
            Err(e) => println!("confined=FAILED ({e})"),
        }

        match std::fs::File::open("/etc/hosts") {
            Ok(_) => println!("open=ALLOWED"),
            Err(e) if e.kind() == ErrorKind::PermissionDenied => println!("open=denied"),
            Err(e) => println!("open=OTHER ({e})"),
        }
        // Nothing listens on discard/9, so only the *kind* of failure
        // distinguishes a sandbox denial from an ordinary refused connection.
        match std::net::TcpStream::connect("127.0.0.1:9") {
            Ok(_) => println!("connect=ALLOWED"),
            Err(e) if e.kind() == ErrorKind::PermissionDenied => println!("connect=denied"),
            Err(e) => println!("connect=OTHER ({:?}: {e})", e.kind()),
        }

        let mut first = [0u8; 1];
        match already_open.read(&mut first) {
            Ok(_) => println!("already-open-fd=ok"),
            Err(e) => println!("already-open-fd=BROKEN ({e})"),
        }

        // A stand-in for decoding: allocate and touch a frame-sized buffer,
        // which besides the ring is all the confined worker ever does.
        let mut frame = vec![0u8; 1920 * 1080 * 4];
        let last = frame.len() - 1;
        frame[last] = 1;
        let checksum: u64 = frame.iter().map(|b| u64::from(*b)).sum();
        println!("compute=ok ({checksum})");

        std::process::exit(0);
    }
}
