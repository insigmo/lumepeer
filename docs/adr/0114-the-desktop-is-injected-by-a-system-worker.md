# ADR 0114 — The whole desktop's input is performed by a `LocalSystem` worker

Status: accepted
Date: 2026-09-25

Answers `docs/bugs/17-remote-hotkeys.md`'s remaining half: a guest can drive an
ordinary window on the host, but not a **`VMware` guest** running on it. Ctrl+V
into the VM pastes nothing, the pointer will not move inside it, and Ctrl+Alt
does not release its grab — while RuDesktop and the vendor "Assistant" on the
very same machine drive that same VM without trouble.

Extends [ADR 0057](0057-lumepeer-takes-full-local-control.md) (the worker that
performs one event on the secure desktop) and [ADR 0085](0085-the-host-can-be-a-service-and-the-screen-belongs-to-an-agent.md)
(the session agent that runs as the signed-in user). It **narrows** ADR 0085's
§1 rule — see the Decision — for injection alone.

## Context

Measured on the host `beta`, a Debian guest in `VMware` Workstation, with the
guest's interrupt counters as the instrument (IRQ 1 keyboard, IRQ 12 VMMouse):

- RuDesktop and Assistant, both installed and both injecting **from
  `LocalSystem`** in the interactive session, reach the guest: keyboard
  13 → 89, mouse 132 → ~11600.
- Lumepeer v0.0.92, injecting from its own host process — `requireAdministrator`,
  High integrity — reaches the guest with **nothing** over three minutes. Its
  own log records the pointer refusing to land and the grab detection switching
  to relative motion, all correct and all landing nowhere.
- A probe from a High-integrity process in the console session confirmed it:
  with `VMware` in the foreground, `SetCursorPos` returns FALSE and neither a
  key nor a click reaches the guest; with anything else in the foreground, the
  same calls work.

The window that holds the guest (`MKSEmbedded`, inside the `vmware.exe` frame)
is owned by `vmware-vmx.exe`, which runs at **System integrity**. Windows' UIPI
silently drops synthesized input aimed at a window owned by a higher-integrity
process, and no integrity level an ordinary host can reach is above System. The
only sender UIPI does not drop is one running **as `LocalSystem`** — which is
exactly what RuDesktop and Assistant do, and what this project already does for
the secure desktop (ADR 0057) but not for the ordinary one.

The host, in both shapes it can take, performs input below `LocalSystem`: the
desktop application injects in-process at High integrity, and the session-0
service's agent (ADR 0085) deliberately drops to the signed-in user's token.
Neither can put input in front of the `VMware` window.

## Decision

**The whole session's input — the ordinary desktop, not only `Winlogon` — is
performed by a `LocalSystem` worker in the console session, and the host falls
back to its own in-process injector only when that worker is not available.**

Concretely:

- A new **desktop injector** worker (`SYSTEM_INPUT_WORKER_ARG`) is the desktop
  binary launched by the service, with the service's own `LocalSystem` token
  re-stamped into the console session (the same restamp
  `secure_desktop_launch` already does), onto `WinSta0\Default`, for the whole
  session. It is a third worker shape beside the two that already exist:
  `LocalSystem` like the secure-desktop worker, `Default` and long-lived like
  the agent. It **only injects** — no capture, no window, no endpoint.
- It reuses the host's own `WindowsInjector` (`crates/media`) unchanged, so the
  scan-code chord path (the v0.0.92 fix, needed for Ctrl+V into a VM) and the
  grab/relative pointer state machine are exactly the host's, running one
  integrity level up. The wire that reaches it therefore carries the guest's
  `scancode` and `modifiers` as well as its `logical` code — the two the
  agent's own protocol drops — over its own authenticated, `LocalSystem`-only
  channel (`desktop_input_channel`).
- The host actor routes every ordinary-desktop event through the service to
  this worker (`OP_INJECT_DESKTOP`), and performs it in-process only when the
  service says it could not (`STATUS_REFUSED`: no service, the worker still
  starting, its channel gone). So a machine without the service, or one whose
  worker has just died, behaves exactly as it did before this ADR.

### What this narrows in ADR 0085

ADR 0085 §1 argues that the agent must **not** be `LocalSystem`, because "a
`LocalSystem` process that calls `SendInput` is a process whose compromise is
the machine." That argument stands for the **agent**, which also *captures the
screen* — a `LocalSystem` capturer is a machine-wide disclosure. It does not
stand for a process that **only injects** already-authorized input:

- Every event was authorized in `lumepeer-core`, in the host, before the
  channel was touched — the same authorization the in-process injector passes,
  re-read per event so a revoke lands on the next one.
- The worker takes input only from the authenticated desktop client over the
  existing IPC, behind a DACL that admits `LocalSystem` and administrators and
  **no interactive user** (`SYSTEM_ONLY_SDDL`), with the launched worker's pid
  checked on connect. No new way in from outside is created.
- It runs only while a control session is active, on the console session's
  ordinary desktop, and does nothing a process on that desktop could not — it
  presses keys and moves a pointer, which is what the person there could do.

So a `LocalSystem` process that *only* performs authorized input is bounded by
that authorization, and this ADR permits it for injection while leaving §1's
rule in force for the agent's capture.

### What this does not change

- The host role does **not** move to session 0. `claim_host_role` still keeps
  the desktop application as host; frame relay out of session 0 is not built,
  and moving the role would stop the machine sending a picture. **Only
  injection is delegated.**
- `requireAdministrator` on the host is untouched.
- The secure-desktop path (ADR 0057) is untouched: UAC and `Winlogon` still go
  through the short-lived per-event worker; this worker is the ordinary desktop's.

## Consequences

- Ctrl+V, the pointer and Ctrl+Alt reach a `VMware` guest, because the injector
  is one integrity level above the window that was dropping them.
- Every ordinary-desktop event now makes a synchronous round trip to the
  service before it is performed, the same shape the secure-desktop path
  already has. If that latency is felt under a fast mouse, the next step is a
  direct host↔injector channel that keeps the service out of the hot path; it
  was left out of this first version deliberately.
- The `LocalSystem` surface grows by one long-lived process. It is justified
  above and enumerated the way ADR 0043 asks: it injects, over one channel, for
  one console session, and nothing else.
