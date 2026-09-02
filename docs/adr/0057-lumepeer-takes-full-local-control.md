# ADR 0057 — Lumepeer takes full local control: an elevated client and secure-desktop input

Status: accepted
Date: 2026-09-02

Extends ADR 0043 (one privileged helper), ADR 0049 (its second capability) and
ADR 0056 (secure-desktop capture crosses the session boundary). Answers the
same host's report those three do: the guest cannot see, and cannot click, the
administrator prompt — now widened to "I cannot interact with the Services
window" and "lumepeer must have maximum privileges to drive the machine."

## Context

Two distinct things a guest could not do, which the host read as one:

1. **Click a window that belongs to an elevated app** — `services.msc`,
   `regedit`, Task Manager, an installer's own UI. The picture of these
   arrives fine; input aimed at them vanishes. This is **UIPI**: a
   medium-integrity process (the client, launched `asInvoker`) cannot
   `SendInput` into a higher-integrity window. Nothing about the secure desktop
   is involved — these are ordinary windows on the ordinary desktop, only at a
   higher integrity level than the sender.

2. **Click the UAC prompt itself, or the lock screen.** ADR 0056 made the
   *picture* of the secure desktop reach the guest. Input still does not:
   `Winsta0\Winlogon` is a separate desktop object, and `SendInput` only ever
   reaches the desktop the calling thread is attached to. No integrity level
   changes which desktop a thread is on — so **elevation does not help here at
   all**, and the `LocalSystem` helper is the only process that can be put onto
   `Winlogon` (ADR 0056 already puts a capture worker there).

These need different answers, and conflating them is how the previous shape got
stuck. ADR 0043 deliberately kept the client unelevated and gave one narrow
capability to a service precisely to avoid "run the whole remote-access client
elevated all the time." That trade was right for Ctrl+Alt+Del. It is not enough
for a host who has decided this machine is theirs to drive remotely and wants a
guest to operate elevated apps and answer UAC.

## Decisions

### 1. The client ships with `requestedExecutionLevel = requireAdministrator`

`apps/desktop/src-tauri/build.rs` replaces tauri-build's default Windows
manifest with one that requests administrator, keeping the
`Microsoft.Windows.Common-Controls` v6 dependency verbatim (tauri's dialog APIs
need it). The client now runs at high integrity, so its existing
`SendInput` injector (`crates/media/src/capture/windows.rs`, `WindowsInjector`)
reaches elevated windows that UIPI used to drop. This is the whole of answer 1.

This reverses ADR 0043's "keep the client unelevated" for the client as a
whole, and the reversal is deliberately narrow in what it claims: elevation
buys **elevated-window input**, nothing on the secure desktop. `sas.rs` already
notes that an elevated process in the user's session can `SendSAS`, so with the
client elevated the helper is no longer the only path to Ctrl+Alt+Del — but the
helper stays, because it is still the only path to the secure desktop
(decisions 2–4), and `network.rs` already prefers it and falls back (ADR 0043
§5).

**What it costs, stated plainly:**

- A UAC prompt every time the client launches. On autostart that means a prompt
  at sign-in. This is the price of high integrity and there is no way to have
  the integrity without it.
- An elevated window cannot receive drag-and-drop from a non-elevated Explorer
  (UIPI, in the other direction). A host who dragged a file onto the window for
  a file transfer must use the in-app picker instead.
- `requireAdministrator` on a standard user account turns every launch into a
  credential prompt, not a yes/no. That is the OS working as intended for a
  process that asked for admin; it is the host's machine and their choice.

These are real regressions, chosen over the alternative (relaunch-on-demand
with a button) because the host asked for "maximum privileges" as the standing
state, not a per-task escalation. The relaunch path remains buildable later if
the cost proves not worth it; nothing here forecloses it.

### 2. The helper gets a third capability: inject one input event onto `Winlogon`

`OP_INJECT_SECURE_DESKTOP` joins `OP_DELIVER_SAS` and
`OP_CAPTURE_SECURE_DESKTOP` on the pipe. It is the mirror image of ADR 0056's
capture: where capture launches a `LocalSystem` worker onto the console
session's `Winlogon` desktop to *read* one frame, inject launches the same
short-lived worker there to *perform* one input event and exit. Same token
duplication, same `CreateProcessAsUserW` with `lpDesktop = "WinSta0\\Winlogon"`,
same "the worker exists for milliseconds then is gone" profile
(`secure_desktop_launch.rs`).

The worker is short-lived per event, not persistent, for the same reason ADR
0056's is: a `LocalSystem` process standing on the interactive secure desktop
is the thing whose compromise is worth the most, so it exists only for the one
operation. The cost is that continuous cursor *hover* is not reflected on the
secure desktop — but answering a UAC prompt is a click on a button at a known
point, and a click is one worker invocation (move-absolute, button down, button
up, in a single `SendInput` batch). Keystrokes for the lock screen are one
worker invocation per key. This is enough for the operations that exist on the
secure desktop and buys the smallest standing surface.

### 3. The wire carries a fixed-shape event, and still cannot express a second thing

Injection needs parameters — where, which button, which key — which
`OP_DELIVER_SAS` and `OP_CAPTURE_SECURE_DESKTOP` deliberately have none of. The
protocol keeps ADR 0043's property that "everything the wire cannot express is
something an attacker cannot ask for" by making the parameters a **fixed-layout
descriptor**, not a length-prefixed message: on seeing `OP_INJECT_SECURE_DESKTOP`
in the two-byte frame, the service reads exactly `INJECT_PAYLOAD_LEN` more
bytes — a size fixed by the opcode at compile time, never named by the peer.
There is still no length field to lie about, no string, no path, and no
peer-driven allocation. The descriptor is: event kind (move / button / key),
a normalized `x`/`y` (`0..=65535`, absolute over the virtual screen, exactly
what `MOUSEEVENTF_ABSOLUTE` takes), a button-or-key code from a **closed enum
the service validates**, and a down/up bit. The service clamps and validates
every field before it reaches the worker; a descriptor that does not parse is
`STATUS_REFUSED`, the same one-bit answer as every other refusal.

The service passes the validated descriptor to the worker as a handful of
bounded integers on the worker's command line — never a peer string. The
worker re-parses them under the same closed enum. `secure_desktop_launch.rs`
already refuses an executable path containing a quote; integers cannot carry
one.

### 4. A new independent grant, and — unlike viewing — it is *not* on for every role

ADR 0056 turned `secure_desktop` (viewing) on for every role, reasoning that
seeing a machine ask for administrator consent is part of watching its screen.
**Controlling** that prompt is categorically different: a guest who can click
`Yes` on a UAC dialog can elevate arbitrary code on the host — the single most
consequential thing a remote guest can do, and the exact capability that makes
remote-access tools the tool of choice for the person on the other end of a
support scam. So `secure_desktop_input`:

- is a new [`IndependentGrant`], deny-by-default;
- is **not** derived from any role, including `FullControl`. This diverges from
  ADR 0054 ("full control carries every independent grant") on this one grant,
  and the divergence is the point: handing over the keyboard and mouse of the
  ordinary desktop is a different decision from handing over the ability to
  approve elevation, and it should take its own switch even for a guest the
  host already trusts with everything else. This restores, for this one grant,
  the deny-by-default ADR 0049 originally gave `secure_desktop` before ADR 0056
  relaxed the viewing half;
- requires `secure_desktop` (viewing) to be meaningful — you cannot aim a click
  at a picture you are not being shown — but is a separate flag, separately
  revocable, re-read by the host actor before every event, so a revoke lands on
  the next event exactly like `input` does;
- is shown to the host as an unremovable indicator whenever it is on, the same
  §2.2 "no hidden control" mechanism the recording indicator uses. A guest
  driving the secure desktop is never invisible to the person in front of it.

Authorization stays in `lumepeer-core` in the main process (the ground rule):
the service's DACL and its `caller_is_in_active_console_session` check bound
*who can reach* the operation; whether *this guest* may inject is decided by the
actor re-reading the grant, before the pipe is ever touched.

## Consequences

- A guest the host has explicitly granted `secure_desktop_input` can approve UAC
  and unlock the host. That is the feature, and it is worth stating without
  euphemism: it is remote local privilege escalation, offered because the host
  asked for it, gated behind a switch that no role turns on for them.
- The helper now has three operations instead of one. ADR 0043's "a wire that
  cannot express a second thing" is now "a wire that expresses exactly three
  fixed-shape things"; the security argument moves from "one operation" to "each
  operation is fixed-shape, parameter-closed, and console-session-bound," which
  is the property that actually mattered.
- The client is elevated. Its attack surface is now a high-integrity process; in
  exchange, `forbid(unsafe_code)` in `apps/desktop/src-tauri` is untouched
  (a manifest is not code) and the injector that reaches elevated windows is the
  one that already existed.
- `crates/service` carries more `unsafe` FFI (a second `CreateProcessAsUserW`
  path and a `SendInput` in the worker), under ADR 0012's justification
  standard, as ADR 0049/0056 already established for this crate.

## Verification

- **Elevated-window input (decision 1):** manual on Windows. With the client
  running elevated, a guest with `input` moves the mouse and clicks in an open
  `services.msc` / `regedit` window and the clicks land. Before this change the
  same clicks are dropped by UIPI. Belongs in `docs/release-checklist.md`,
  not CI: it needs a real elevated app and a real second machine.
- **Grant (decision 4):** `cargo test -p lumepeer-core` — `secure_desktop_input`
  is deny-by-default, is derived from no role (not even `FullControl`), is set
  and revoked independently, and reads back exhaustively with no `_` arm.
- **Protocol (decision 3):** `cargo test -p lumepeer-service` — a well-formed
  inject frame parses to a clamped descriptor; an out-of-range field, an unknown
  kind, and a short read are each `STATUS_REFUSED`. The suite never launches the
  worker against a live `Winlogon`, exactly as ADR 0043's suite never sends
  `OP_DELIVER_SAS`.
- **Secure-desktop injection end to end (decisions 2–3):** manual on Windows,
  with the service installed and the grant on: a guest clicks `Yes` on a real
  UAC prompt and the prompt is answered. **Not verified in CI** — it needs a
  `LocalSystem` service, a live secure desktop, and a second machine, so it is a
  `docs/release-checklist.md` step. This is the same honesty ADR 0056's own
  verification section applied to capture.
