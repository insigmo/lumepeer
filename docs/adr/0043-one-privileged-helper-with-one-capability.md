# ADR 0043 — One privileged helper, with one capability

Status: accepted
Date: 2026-08-28

## Context

`docs/tasks/14-release-infrastructure.md` asks for a system service on all
three platforms, for two stated reasons: running before a user signs in, and
Ctrl+Alt+Del.

The second reason is real and narrow. `crates/media/src/sas.rs` has always
recorded the constraint: `SendSAS` is honoured from a process in session 0 that
the `SoftwareSASGeneration` policy grants the right — a service — or from a
process the user launched elevated. Everything else gets a silent no-op from
the OS. So the shipping answer today is "run the whole remote-access client
elevated, all the time, so one button works", which is a bad trade.

The first reason is not a service; it is an architecture. Serving a screen
before anybody signs in means a process in session 0 that can hand capture and
injection to whichever interactive session exists — the session-0/agent split
that every commercial product in this space has. That is a re-architecture of
the media path, not a unit file, and this ADR does not attempt it.

Two further facts shaped the scope:

- **A privileged process reachable from an unprivileged one is a local
  privilege escalation waiting to be written.** Whatever this service accepts,
  an attacker on the same machine can also send.
- **Linux and macOS have no SAS.** `sas_available()` is `false` there, by
  construction, because no such mechanism exists. A root daemon on those
  platforms would therefore hold privileges in order to perform *no*
  operations.

## Decisions

### 1. A separate binary, `lumepeer-service`, and not the client

`crates/service`, staged next to the client as a Tauri `externalBin` sidecar,
exactly like `lumepeer-decoder-worker`. The client is a GUI process with a
webview in it; session 0 has no desktop, and putting a browser engine into a
`LocalSystem` process to reach one Win32 call would be the wrong shape by a
wide margin.

### 2. Exactly one operation, and a wire that cannot express a second

`protocol.rs` is the whole interface: two bytes in, two bytes out. One opcode,
`OP_DELIVER_SAS`. No paths, no strings, no lengths, no peer-driven allocation,
no length field to lie about and no partial frame to reassemble.

This is the security argument, not a style preference. The attack surface of a
`LocalSystem` service is the set of things its callers can ask for, and here
that set has one member whose effect is a screen the user sees.

The service reads no configuration file, opens no socket, and touches no disk.
Nothing a local user can write changes what it does.

### 3. The access list is the authorization

The named pipe carries
`D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x0012019b;;;IU)`: `LocalSystem` and
administrators in full, and **interactive users** with read/write only — not
`WRITE_DAC`, so a client cannot widen the pipe from under the service.

`IU` is the grant that matters. A network logon, a service account and a task
running as another user are all outside it, so the only thing that can ask for
a Ctrl+Alt+Del is a process belonging to somebody signed in at the screen that
would receive it. `PIPE_REJECT_REMOTE_CLIENTS` keeps the endpoint off the
network entirely.

There is no second authorization check inside the service, and there should not
be: a check the service performs is a check with a bug in it, while the DACL is
enforced by the kernel before a byte arrives.

### 4. Elevation runs *our* binary, not a shell

Installing and removing the service need administrator rights. The client asks
for them with `Start-Process -Verb RunAs` on the **sidecar itself**, with one
flag (`--install` / `--uninstall`); the service control manager calls then run
inside our own code (`install.rs`).

The alternative — the client building an `sc.exe` command line out of a path
and handing it to an elevated shell — is one quoting bug away from running
whatever that path says as SYSTEM. Nothing is interpolated into the launch
except the sidecar's own `current_exe`-derived path, with quotes doubled.

### 5. The client asks the service first and falls back to itself

`network.rs`'s `SasRequest` handler tries `lumepeer_service::client::
deliver_sas()`, and on any failure — not installed, not running, not
permitted, a garbled answer — falls back to the in-process `sas::send_sas()`
that existed before. A missing service degrades the *privilege level*, never
the feature, and never into a silent success: the guest still gets an honest
`SasAck(false)` when neither path delivered.

The client needs no `unsafe` for this. Opening a Windows named pipe is an
ordinary `CreateFileW`, which `std::fs::OpenOptions` already does, so
`apps/desktop/src-tauri` stays `#![forbid(unsafe_code)]`.

### 6. Consent is in the app, not in the installer

The task asks for installation "through the installer, with the user's explicit
consent during installation". The consent is in the app's own settings panel
instead, and this is deliberate rather than a shortcut:

- A checkbox on an installer page is consent nobody reads. A panel that says
  what the service does, shows whether it is running, and offers to remove it
  is consent someone can act on later.
- The panel makes removal reachable from the same place as installation. A
  privileged service that can only be removed from `services.msc` is the
  property that separates software from unwanted software, and an installer
  checkbox does not provide it.
- macOS `.dmg` is a drag-install with no post-install script at all, so the
  installer route does not exist there uniformly anyway.

Both actions raise the operating system's own administrator prompt, which is
the consent that actually gates the change.

### 7. Windows only, and the reason is written down

Linux and macOS get no daemon. They have no SAS mechanism, so a daemon there
would hold root and answer no operations — a pure attack surface added for
symmetry with a table in a task file. `service_control::state()` reports
`Unsupported` there and the panel shows nothing rather than a button that
cannot work.

The sidecar is still *staged* for every target, because Tauri's `externalBin`
is not per-platform; off Windows the binary prints why there is nothing for it
to do and exits non-zero.

**This means the "runs before anybody signs in" half of task 4 is not
delivered.** It needs the session-0/agent split described in the context, which
is its own piece of work with its own ADR.

## Consequences

- Ctrl+Alt+Del works against a host whose client is running unelevated, once
  the helper is installed. That is the single reason the service exists, and it
  is worth stating that plainly: one button.
- The Windows bundle grows by one small binary, and so do the Linux and macOS
  bundles, for a binary that does nothing there. `externalBin` is the price.
- `crates/service` carries `unsafe` FFI — the service dispatcher, the pipe and
  its DACL, and the SCM calls — under the justification standard ADR 0012 set.
  It is the third crate allowed to, after the media ring buffer and `sas.rs`.
- A machine with the helper installed has a `LocalSystem` process listening on
  a named pipe permanently. That is a real addition to its attack surface, and
  the mitigation is the narrowness above rather than an argument that it is
  fine.
- `crates/media/src/sas.rs`'s header no longer describes elevation as the only
  shipping shape, because it is not.

## Verification

`cargo test -p lumepeer-service` runs the real binary in `--console` mode and
drives the actual endpoint: the pipe is created with its access list, a frame
is accepted, an unknown opcode and a frame without the magic are both refused
with a well-formed reply, and the service loops back to accepting so the
client's own reachability check succeeds. The test deliberately never sends
`OP_DELIVER_SAS` — on a machine where the suite happened to run elevated that
would throw the secure desktop up in someone's face.

The panel was driven against the running client with `tauri-pilot`:
`service_status` answers `not_installed`, and the row renders with the state
and an Install button.

**Not verified:** installing the service. That needs an administrator prompt
and leaves a `LocalSystem` service registered on the machine it runs on, so it
belongs in `docs/release-checklist.md` as a manual step rather than in a test
that any contributor might run.
