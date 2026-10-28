# ADR 0169 — The host is a `LocalSystem` process in the session, and the client runs unelevated

Status: accepted — the measurements of pack `01` held on 2026-10-28
(`docs/research/session-host-probe.md`)
Date: 2026-10-28

Supersedes [ADR 0057](0057-lumepeer-takes-full-local-control.md) §1 (the
client ships `requireAdministrator`). Amends
[ADR 0085](0085-the-host-can-be-a-service-and-the-screen-belongs-to-an-agent.md) §1,
[ADR 0114](0114-the-desktop-is-injected-by-a-system-worker.md),
[ADR 0123](0123-the-password-stays-home-and-so-does-the-policy.md) §5,
[ADR 0126](0126-the-logon-screen-is-hosted-by-the-same-host.md),
[ADR 0140](0140-windows-autostart-is-a-logon-task.md) and
[ADR 0164](0164-signing-in-from-the-logon-screen-hands-the-guest-to-the-client.md).
Leaves [ADR 0122](0122-nothing-below-the-host-and-nobody-but-the-host.md) §1
exactly as it is.

The owner's request: Lumepeer must start without administrator rights and
still do everything it does with them — the way the vendor "Assistant" on the
same machine does.

## Context

### What the elevated client buys today

The Windows client is `requireAdministrator` (`apps/desktop/src-tauri/build.rs`).
Since ADR 0057 a long list of things leans on that, most of it added later
and each correct on its own:

- **The helper answers elevated callers only.** `LumepeerHelper`'s pipe, its
  frame mappings and the desktop-input channel admit `SY` and `BA`
  (`D:(A;;GA;;;SY)(A;;GA;;;BA)`, ADR 0122 §1). `BA` is enabled only in an
  elevated token. So Ctrl+Alt+Del, the secure desktop (picture and input) and
  the `LocalSystem` injector (ADR 0114) reach only an elevated client.
- **Input into elevated windows.** Without the injector the client falls back
  to its in-process `SendInput`, and UIPI drops what a medium-integrity
  sender aims at `services.msc`, `regedit`, Task Manager, an installer, or a
  `VMware` guest (`vmware-vmx.exe` runs at System integrity).
- **The stores.** An elevated run keeps the endpoint identity, the device
  password, the trusted devices, the invite, the history, the remembered
  passwords and the audit log in `%ProgramData%\Lumepeer\users\<SID>`, which
  only `LocalSystem` and administrators can open, and deleted the profile's
  copy on its first run (ADR 0123 §5, `placement.rs`).
- **The host role.** `Global\lumepeer-host-role` and its release event admit
  `SY` and `BA` only (`crates/service/src/host_role.rs`).
- **Updates.** The bundle is `installMode: perMachine`; the elevated client
  installs an update with no prompt.
- **Files.** The file manager and received files act with the elevated half
  of the account's token.
- **Autostart and the sign-in handover** start the client through its
  elevated token (`HighestAvailable` task, ADR 0140; `TokenLinkedToken` in
  `client_launch.rs`, ADR 0164).

So flipping the manifest to `asInvoker` alone gives a client that has none of
the above, and — because the stores are where it cannot read them — a new
endpoint identity: every guest that saved this machine would find a stranger.

### Why the service cannot simply listen to an unelevated client

Whatever an unelevated client may ask, any program the same user runs may ask
too. A service that presses "Yes" on a UAC prompt because a medium-integrity
process asked is a UAC bypass; ADR 0122 §1 found exactly that and closed it.
The same goes for the stores: if a program with no rights can set the device
password or trust a device, then through the `LocalSystem` injector it drives
elevated windows. **The decisions about who gets in and what they may do have
to be taken on the privileged side, never by the window.**

### How the products beside it do it — measured on this PC, 2026-10-28

- **Assistant (SAFIB).** Service `AstService` runs as `LocalSystem`.
  `assistant.exe` runs in the console session (session 1) under an account
  that is not the user's: `query process` names it `(unknown)` exactly as it
  names `winlogon.exe`, `csrss.exe` and `dwm.exe`, and `explorer.exe`
  `beta_win`; an unelevated shell can read neither its path nor its owner.
  Its manifest is `asInvoker`, Authenticode-signed by SAFIB LLC. ADR 0114
  had already measured it injecting from `LocalSystem` on `beta`.
- **RuDesktop.** `rudesktop.exe --service` runs as `LocalSystem` in session 0;
  its child in session 1 runs under a non-user account; only
  `rudesktop.exe --tray`, that child's child, runs as `beta_win`.

Neither "runs without administrator rights". Administrator rights are spent
once, at install, on a `LocalSystem` service, and that service starts the
working process into the user's session with its own token. What the user
starts has no rights and needs none.

Lumepeer already has every piece of that shape and uses them in two narrower
places: `--logon-host` (ADR 0126) is the whole host, as `LocalSystem`, in the
console session — but only on `Winlogon` and only while nobody is signed in;
and the ADR 0114 injector is `LocalSystem` on `WinSta0\Default` — but only
injects.

## Decision

### 1. On Windows the host is `lumepeer-desktop.exe --session-host`, as `LocalSystem`, in the console session

`LumepeerHelper` starts the desktop binary with a new constant argument
(`SESSION_HOST_ARG`) into the console session's `WinSta0\Default`, with its
own token stamped for that session (`duplicate_own_token_for_session`, the
launch ADR 0114 and ADR 0126 already do). The path is this binary's own
directory and the command line carries nothing else (ADR 0043 §4).

That process is `logon_host.rs` generalised: headless (no Tauri, no WebView),
`spawn_actor_with`, the stores of the **console user's** SID under
`%ProgramData%\Lumepeer\users\<SID>` — the same directory the elevated client
fills today, so no host-side store moves and the identity every guest saved
is the one it serves. It holds the host role. It is a `LocalSystem` thread on
`Default`, which is exactly the ADR 0114 injector's position, so it injects
in-process with the existing `WindowsInjector` and reaches elevated windows
and `VMware`. For the secure desktop it calls the helper's existing
operations, which admit `SY` — the media path does not change.

Lifecycle:

- The service starts it when somebody signs in at the console and stops it at
  sign-out or console switch, as it does the logon host the other way round.
- It opens its endpoint when the user's store holds an identity **and** either
  "accept connections after sign-in" is on in that store or a client is
  attached (decision 4). A user who never opened Lumepeer has no store, and
  nothing listens for them; the identity is minted on the first attach.
- At the logon screen ADR 0126 is unchanged. At sign-in the logon host hands
  the role to this process instead of to the client; ADR 0164's "the service
  starts the owner's client" becomes "the service starts the session host"
  (it would have anyway). Merging the two into one process that follows the
  console across sign-in is left for later.

This takes ADR 0085 §1's "the privileged side draws no pixels" back for the
console session, as ADR 0126 already did for the logon screen, for the same
reason: the process it replaces is an elevated client holding the network,
capture and injection together, and an administrator token is not a boundary
Windows defends against `LocalSystem`.

### 2. The client is `asInvoker` and does not host on Windows

`build.rs` requests `asInvoker`. On Windows a release client never hosts in
process. It is the guest, and the host's control panel. Without a running
service it says this computer cannot be reached and offers to start the
service, which is one UAC prompt. A debug build, and `LUMEPEER_KEYSTORE=file` for the
end-to-end rigs, keep hosting in process exactly as an unelevated run does
today, so `e2e/matrix` keeps working.

### 3. The client has its own guest identity

One actor per process owns one endpoint (`crates/net/src/endpoint.rs`), and
two endpoints with one key fight over the relay and the DHT record. So the
client cannot borrow the host's key. And it must not route its guest
sessions through the session host: a guest decodes and parses what a remote
host sends, and that parsing belongs at the user's level, not in a
`LocalSystem` process any host on the internet can feed.

So the host keeps the existing identity (saved guests keep working), and the
client mints a guest identity in the profile. The guest-side stores — the
remembered-hosts list and the passwords remembered for other hosts — move
from the admin tree to the profile, once, done by the session host on the
client's first attach. The host-side stores — identity, device password and
its second factor, address book with the trusted devices, invite, audit —
stay where ADR 0123 put them.

### 4. The channel between them: the unelevated client reads and narrows, never widens

The session host creates a pipe whose list admits `SY` and the console user's
SID. Any program that user runs can open it; that is accepted, and it is why
the rule is about **what the channel can say**, not who says it:

- **Read:** sessions and their grants, chat, transfers, connection state,
  update state.
- **Narrow:** end a session, revoke or lower a grant, refuse a pending
  request, turn hosting off, send chat.
- **Nothing that widens.** There is no message on this pipe that admits a
  guest, raises a grant, turns on `secure_desktop_input`, trusts a device or
  changes the device password. A malicious caller can disconnect a guest; it
  cannot let one in.

### 5. Widening is decided where the user's programs cannot reach

- **Inside a session — admission, view to control, `secure_desktop_input`,
  "trust this device":** a native dialog the session host draws itself
  (`TaskDialogIndirect`; the manifest already carries Common-Controls v6). It
  is a System-integrity window, so UIPI stops a program running as the user
  from clicking, typing into or reading it. Peer-supplied strings (name,
  device) are shown as plain text. A WebView in the session host is rejected:
  it would render remote-controlled strings in a browser engine inside a
  `LocalSystem` process, and WebView2 under `LocalSystem` is not something to
  depend on.
- **Policy — device password, second factor, unattended role, logon screen,
  the trusted-device list:** the client starts itself elevated in a
  settings-only mode (`--access-settings`). That costs one UAC prompt, at the
  moment of the change. That process is today's elevated client minus hosting
  (the role is held, so `claim_host_role` already says "not a host"). It writes the admin-only store
  as today, then signals an event only administrators can signal, and the
  session host re-reads. On a standard account this prompt asks for an
  administrator's credentials, as Windows does for any machine-wide security
  setting.

### 6. Work done as a person runs with that person's token

`LocalSystem` holds `SeTcbPrivilege`, so the session host gets the console
user's token from `WTSQueryUserToken` and its elevated half from
`TokenLinkedToken` (a primary token for a TCB caller, as `client_launch.rs`
already uses):

- **Terminal:** `lumepeer-terminal-worker` is started with the user's own
  token through `CreateProcessAsUserW`. ADR 0079 and ADR 0102 are unchanged:
  the shell runs as the person, never as the host.
- **File manager and received files:** each file operation impersonates the
  user's token. Where the account has an elevated half, that half is used,
  which is the token the elevated client acted with, so the guest sees the
  same files as today. Otherwise the user's own token is used.
- **Recording export:** written as the user. **Tunnels:** connect as the
  session host. A raw TCP connection carries no Windows identity.

### 7. A session somebody can see

When a session starts and no client is attached, the session host starts the
client in the session with the user's token, the way `client_launch.rs` does
today, but with the limited half. The host bar stays in the client. If no
client attaches within a bounded time, the session is ended with a reason. A
session nobody can see does not continue, which keeps §2.2's "no hidden
control" true.

### 8. Updates are installed by the session host

The session host checks for an update, downloads it into
`%ProgramData%\Lumepeer\updates` (administrators only), verifies it against
the updater public key compiled into the binary, and runs the installer
silently. ADR 0148's restart close code is kept. The client shows the state
and may ask "install now". That is not a widening request: the caller does
not choose the package.

### 9. Autostart

The client's tray goes back to the per-user `Run` key, which works for an
`asInvoker` binary. The ADR 0140 logon task is removed on the first run of
the new build. "Accept connections after sign-in" is now a setting in the
user's store, read by the session host (decision 1), and no longer a side
effect of the client starting.

## What does not change

- `LumepeerHelper`'s pipe, frame mappings and channels stay `SY` + `BA`
  (ADR 0122 §1). The session host is `SY`, and the `--access-settings` process
  is `BA`. No unelevated process reaches the helper.
- The secure-desktop workers (ADR 0056/0057 §2–4) and `secure_desktop_input`
  as a grant no role carries.
- The logon host (ADR 0126/0133/0164) at the logon screen.
- Linux and macOS: there is no elevated half there to protect anything.

## Consequences

- No UAC prompt when Lumepeer starts. A standard account runs it without an
  administrator's password, and drag-and-drop from Explorer reaches the window.
- A guest still drives elevated windows, `VMware`, UAC (with
  `secure_desktop_input`), Ctrl+Alt+Del and the logon screen.
- A network-facing `LocalSystem` process runs for the whole signed-in session.
  Today that process is an elevated client, and ADR 0126 already runs one at
  the logon screen.
- Anything running as the user can now drive the client. That means it can
  watch sessions, disconnect guests, chat, and dial out as this user's guest
  identity, including to hosts that trust it. It cannot admit anyone here or
  widen a grant. This is the position every unelevated client is in, and the
  one the Linux and macOS clients are in today.
- Guest-side secrets (the remembered passwords for other hosts) live in the
  profile again, as on Linux and macOS.
- Each host that trusted this machine as a guest sees a new key once. One
  device-password entry there re-trusts it.
- The consent prompt is a plain system dialog, not the Braun window
  (ADR 0168), because nothing that only looks like it is safe in that place.
- Changing the access policy takes a UAC prompt at the moment of the change.
- The ADR 0114 injector worker and `OP_INJECT_DESKTOP` lose their caller once
  the client stops hosting. They are removed in the last pack, not before.

## Not done

- One process that follows the console from the logon screen into the session
  without a redial (decision 1).
- Following the input desktop inside the session host instead of the helper's
  per-event secure-desktop worker.
- Sessions other than the console (RDP, fast user switching beyond the
  console).
- A native, non-UAC policy editor for standard accounts.
- A signed "this guest key succeeds that one" statement, which would spare the
  one-time re-trust.

## Verification

- **Pack 01, before any code:** a `LocalSystem` process started into
  session 1 on `Default` captures the desktop (DXGI), captures loopback audio,
  reads and writes the clipboard, shows a `TaskDialog` that a medium-integrity
  `SendInput` probe cannot press, starts the terminal worker with
  `WTSQueryUserToken`'s token, and lists an administrator-only folder while
  impersonating the linked token. If any of these fails, this ADR is revised
  before pack 02.
- **Live, on `beta` with the service installed:** the client process is medium
  integrity (no UAC at launch); a guest clicks in `services.msc` and
  `regedit`, types into a `VMware` guest, answers a UAC prompt with
  `secure_desktop_input`, sends Ctrl+Alt+Del, and reaches the logon screen
  after a reboot; a saved guest reaches the same identity as before the
  update.
- **Standard account:** Lumepeer starts and hosts without credentials.
- **Refusals:** a medium-integrity program cannot press the consent dialog,
  cannot write the policy store, and gets no widening message accepted on the
  client channel (`cargo test -p lumepeer-service`, plus a live probe).
- **Updates:** an update installs with no prompt while nobody is at the
  machine.

Packs: `docs/unelevated-client/README.md`.
