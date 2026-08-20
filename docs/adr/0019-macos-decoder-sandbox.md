# ADR 0019 — macOS decoder sandbox via `sandbox_init(3)`

Status: accepted
Date: 2026-08-20

## Context

§11.3 decodes out of process, and the worker refuses to decode when it cannot
confine itself: better no video than an unconfined decoder chewing on an
attacker-controlled bitstream inside the trust boundary. Linux (seccomp-BPF)
and Windows (`AppContainer`, ADR 0007) have had that confinement for a while.
The macOS arm of `crates/decoder-worker/src/main.rs`'s `sandbox::apply` was
never written, so it fell through to `the {other:?} sandbox is not implemented
yet; refusing to decode unconfined`.

That refusal was correct and also fatal in practice: a Mac could not be a
**guest** at all. Connection, pairing and consent all succeeded, the remote
view window opened, and then the first frame killed the worker with that
message and nothing was ever painted. ADR 0007 listed the macOS
`sandbox_init` decoder sandbox as one of the documented gaps because no Mac
was reachable from the development machine. One is now (macOS 26.6.2,
x86_64) — the same argument ADR 0013 made for capture.

## Decision

`crates/media/src/decode/macos_sandbox.rs` implements the confinement with
`sandbox_init(3)`, and `sandbox::apply` calls it for
`SandboxKind::MacosSandbox`.

Unlike Windows, nothing changes in the parent: `sandbox_init` confines the
*calling* process, so the ordering §11.3 mandates — map the ring, then
confine, then touch the first untrusted byte — holds literally, exactly as it
does for Linux seccomp. `DecoderHandle::spawn_with`'s non-Windows path spawns
the worker unchanged.

The file lives in `crates/media` rather than in the worker because
`crates/decoder-worker` is `#![deny(unsafe_code)]` and `sandbox_init` is a C
entry point with no safe binding. That makes `decode::macos_sandbox` the
sixth (and last) module in `lumepeer-media` to opt back into `unsafe_code`,
with a `reason` and a SAFETY note per block, as §21 requires. It is a
separate file rather than an inline module for the same reason
`windows_sandbox.rs` is one.

### The profile

Sandbox Profile Language, in full:

```
(version 1)
(deny default)
```

Deny-by-default, unlike the Linux filter, which is a deny *list*. The
asymmetry is deliberate and comes from what each mechanism mediates. seccomp
sees syscall *numbers*, so an allow-list there kills the process on an
unrelated libc version and buys nothing over denying the short, stable set of
calls that reach the network or the filesystem. Seatbelt mediates named
*operations*, and everything a pure-computation process does — anonymous
memory, `malloc` growth, thread creation, reads and writes on descriptors it
already holds, faulting in pages of libraries mapped before confinement — is
not a mediated operation at all. So on macOS the strict direction is also the
one that survives an OS upgrade, and it is what the deny-by-default ground
rule asks for.

Measured on macOS 26.6.2 with the profile applied: `open` of any path fails
with `EPERM`, `connect` fails with `EPERM`, while the pre-opened ring, the
inherited stdin/stdout/stderr pipes, `malloc`, `mmap` and `pthread_create`
all keep working and `openh264` decodes normally. `socket(2)` itself still
succeeds — socket *creation* is not a mediated operation; reaching the
network is, and that is what is denied. Nothing is decoded through a
socket, so the distinction is cosmetic; it is recorded here so that a future
reader does not mistake a non-null socket fd for a hole.

The profile is a compile-time constant. It is deliberately not host-editable
and not in `config/`: a file that can loosen the decoder sandbox from outside
the TCB is exactly what deny-by-default forbids.

### Why a deprecated API

`sandbox_init` has carried a deprecation attribute since macOS 10.8 and is
still the only way for a process to confine *itself*. The supported
alternative, App Sandbox entitlements, is applied by the kernel at `exec`
time from the code signature, cannot be tightened afterwards, and would make
confinement a property of how the bundle was signed rather than of the worker
binary — so a worker run outside a signed bundle (`cargo test`, a developer
build, a sidecar someone copied out) would decode *unconfined* instead of
refusing, which inverts §11.3. The named built-in profiles
(`kSBXProfilePureComputation` and friends) are deprecated on exactly the same
schedule and are coarser and unreadable at the call site, so they buy nothing
over the two-line profile above.

If Apple ever removes the symbol, the worker fails to link or `sandbox_init`
fails at runtime, and the outcome is the one §11.3 already defines: no
sandbox, no decoding, with an error naming what is missing.

## Testing

- `crates/media/src/decode/macos_sandbox.rs`'s
  `the_profile_denies_the_filesystem_and_the_network` re-executes the test
  binary with `--exact` and an env marker, because the sandbox is
  irreversible and process-wide and would otherwise confine every test that
  ran after it. The child applies the profile and reports what it can and
  cannot do; the parent asserts on that report. No second helper binary, and
  the check runs on every `cargo test -p lumepeer-media` on macOS rather than
  behind an opt-in env var.
- `tests/integration/tests/media_pipeline.rs`'s
  `a_captured_frame_reaches_the_guest_through_the_sandboxed_decoder` now
  treats `SandboxUnavailable` on macOS as a failure rather than a documented
  skip, the same as Linux and Windows. It passes on macOS 26.6.2: the worker
  logs `decoder worker confined kind=MacosSandbox` and a captured frame makes
  it through encode → confined decode → picture.

## Consequences

- macOS can be a guest. The `MacosSandbox`-is-not-implemented refusal is
  gone, and the remote view window on a Mac now gets frames.
- ADR 0007's macOS gap list loses the `sandbox_init` decoder sandbox entry;
  capture (ADR 0013) and input closed earlier, so phase 4 on macOS is down to
  running the §18 error matrix on the machine itself.
- iOS still refuses. `platform_sandbox()` reports `MacosSandbox` there too,
  but `sandbox_init` is not available to an iOS app (the kernel applies the
  container sandbox at `exec`), so the worker's `macos_sandbox` arm is
  `#[cfg(target_os = "macos")]` and iOS gets the §11.3 refusal until an iOS
  build exists to confine.
- The decoder worker on macOS can no longer open files, which includes crash
  reports and any future diagnostics it might want to write. Anything the
  worker needs to touch has to be opened before `confine()` and handed to it
  as an already-open descriptor, exactly as the ring buffer is — the same
  discipline `AppContainer` already forces on Windows.
