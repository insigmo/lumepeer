# ADR 0126 — The logon screen is hosted by the same host

Status: accepted
Date: 2026-09-28
Amended by: ADR 0133 (§3: on whenever a device password is set; the switch records a refusal)

The user's report: after a reboot the host cannot be reached until somebody
signs in and types the password at the machine, while other remote-desktop
products connect at the sign-in screen.

Takes a different road from [ADR 0085](0085-the-host-can-be-a-service-and-the-screen-belongs-to-an-agent.md),
[ADR 0087](0087-assembling-the-session-zero-host.md) and
[ADR 0088](0088-the-logon-screen-is-a-capture-target-and-a-switch-is-a-state.md).
Their session-0 host (`crates/host`) still has no path from its frame mapping
to a guest, has an identity of its own — a different host from the one every
guest has saved — and is not in the installer. Nothing in it is removed; it is
simply not what serves the logon screen.

## Decision

### 1. The desktop binary hosts the logon screen, as `LocalSystem`, on `Winlogon`

`LumepeerHelper` — already installed, already starting with Windows — gains a
supervisor thread (`crates/service/src/logon_host_launch.rs`). Whenever the
console session has nobody signed in (`WTSUserName` is empty), it launches
`lumepeer-desktop.exe --logon-host` into that session's `WinSta0\Winlogon`
desktop with its own `LocalSystem` token stamped for the session — the launch
ADR 0114 already does for the input worker, with the desktop and the argument
changed.

That process (`apps/desktop/src-tauri/src/logon_host.rs`) runs the same
runtime as the client: the same actor, the same capture, encoder and injector.
Every thread it starts is on `Winlogon`, and a `LocalSystem` thread there may
duplicate that desktop and `SendInput` into it, so nothing is relayed and
nothing is new in the media path.

This reverses ADR 0085 §1's "the privileged side draws no pixels" for this one
process, deliberately. The client it stands in for already runs elevated
(ADR 0057) with the network, capture and injection in one process, so the
distance between that and `LocalSystem` is small; and it only exists while
nobody is signed in.

### 2. Same identity, same password, same address book

The logon host opens the stores of the account that turned the feature on,
where the elevated client keeps them since ADR 0123:
`%ProgramData%\Lumepeer\users\<SID>`, which only `LocalSystem` and
administrators can open. So the guest dials the host it has saved, and a guest
that remembers the device password gets in the way it always does.

It refuses — exit code `LOGON_HOST_EXIT_NOT_ENABLED` — unless the feature is on,
the owner's store already holds an identity (minting one would be a different
host), and a device password is set.

### 3. Off by default, one owner per machine

"Also at the Windows sign-in screen" is a checkbox in the unattended-access
panel, disabled until a device password exists. Turning it on writes the
account's SID to `%ProgramData%\Lumepeer\users\logon-host-owner`; turning it
off removes it. A file there that an ordinary user owns is removed rather than
read. The helper itself still reads no configuration (ADR 0043): it decides
only *when*, and the logon host decides whether.

### 4. Who gets in, and what they get

Nobody is present, so `ViewWindows::attendance` is `Unattended` and the device
password is the only way in (ADR 0085 §2); each admission is audited as
`EmptyMachineLogin` (ADR 0088 §3). The whole screen is the secure desktop, so
`on_secure_desktop` is `true`: a keystroke needs `secure_desktop_input`, and the
picture needs `secure_desktop` — checked when the media connection is accepted,
the check ADR 0088 left to whoever built a frame path. A full-control role
carries both.

Input from such a host never goes to the ADR 0114 injector: that one lives on
`Default`, where an event would land on a desktop nobody is looking at.

There is no banner on the logon screen, the known gap ADR 0088 §3 already
named.

### 5. Signing in hands the machine to the client

The moment somebody is signed in, the supervisor raises the host-role release
event (ADR 0085 §4); the logon host returns, releasing the role, and is
terminated if it has not left within five seconds. The guest's connection
drops and it dials again (ADR 0106). A client starting while the role is still
held now raises the same request and waits up to eight seconds for the role
before settling on not hosting. A logon host that finds the role taken — a
client still running in a disconnected session after a fast user switch —
exits with `LOGON_HOST_EXIT_ROLE_TAKEN`, and the console is left alone until
it changes.

## Not done

- **The owner's settings.** Their relay and transport choices live in their
  profile, which the logon host cannot read; it runs on the defaults, as
  ADR 0087 §6 decided for the session-0 host.
- **The other surfaces.** A terminal opens nothing here: it takes the token of
  the interactive user's shell (ADR 0079, ADR 0102), and there is none. The file
  manager drops no privileges anywhere, so on this host it browses as
  `LocalSystem` rather than as the elevated administrator the client is — a
  full-control guest with the device password, who could sign in anyway.
- **A seamless sign-in.** The session ends at sign-in and a new one starts
  with the client, so the guest sees a reconnect of a few seconds.
- **Starting the client after sign-in.** The client is `requireAdministrator`
  (ADR 0057), and Windows does not start such a program from the
  `HKCU\...\Run` entry autostart writes (ADR 0103). If that holds, the guest
  who signed the machine in reaches nobody afterwards until the client is
  started by hand. To be confirmed on a real sign-in, and fixed separately.

## Verification

Tests that run anywhere: the supervisor's decisions (`serve_now`, `ended_by`),
the owner file (elevated runs only), the logon screen's seam answers, and the
settings toggle.

Needs a real machine, and none of it has been run yet — see
`docs/release-checklist.md`, "Hosting the sign-in screen".
