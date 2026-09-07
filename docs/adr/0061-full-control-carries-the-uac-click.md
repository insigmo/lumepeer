# ADR 0061 — Full control carries the UAC click

Status: accepted
Date: 2026-09-07

Amends ADR 0057 (Lumepeer takes full local control) on one point, and extends
ADR 0054 (full control carries every independent grant) to the one grant that
was excluded from it. Answers a host's report: "when the administrator window
opens, lumepeer cannot do anything with it — the UAC window is still out of
reach."

## Context

ADR 0057 built the whole path: a `LocalSystem` worker on `Winsta0\Winlogon`
that performs one `SendInput` per event, and a `secure_desktop_input` grant the
actor re-reads before every event. ADR 0056 had already made the *picture* of
the prompt arrive. Both work — the host reported seeing the UAC prompt and
being unable to click it, which is exactly the state where capture is fine and
injection is refused.

The refusal was the grant. ADR 0057 made `secure_desktop_input` deny-by-default
for **every** role, full control included, on the reasoning that approving an
elevation is a decision above handing over the ordinary keyboard. So a host who
had chosen "full control" still had to find a toggle in the session row before
the guest could answer a prompt — and the prompt is modal, so by the time
anyone notices, the guest is stuck looking at a screen that ignores them and
the host is looking at a dialog they were trying to hand over in the first
place.

The reasoning does not survive contact with what full control already means.
Since ADR 0057 the client itself runs elevated (`requireAdministrator`), which
is what lets a guest with `input` type into windows that *belong* to elevated
apps: `regedit`, Task Manager, an installer. A guest that can type into an
elevated window can already do the damage the withheld click was protecting
against — including typing an administrator password into the very prompt it
was not allowed to click. The switch bought no boundary; it only meant the
common case did not work until somebody found it.

## Decision

`Grants::from_role(Role::FullControl)` sets `secure_desktop_input`.

Nothing below full control does. Watching, and watching plus the narrowed
keyboard of `ControlLimited`, still cannot answer an elevation prompt: those
roles do not carry `input` either, so the argument above does not apply to
them.

It stays an `IndependentGrant`. The host can still withdraw it from a running
full-control session without ending the session or lowering the role, and the
actor still re-reads it before every single event, so a withdrawal lands on the
next one. What changed is only where it starts.

## Consequences

- "Full control" now means what it says on Windows: the guest can drive
  elevated windows *and* answer the prompts that create them. That is a real
  widening of the default, and it is the one the host asked for.
- The session row's toggle keeps both jobs: turning the grant off for a
  full-control guest, and turning it on for one that has it off. Only its
  starting position moved.
- `secure_desktop` (seeing the prompt) is unchanged and still on for every
  role, per ADR 0056. Seeing and clicking remain two different grants.
- ADR 0057's "no role turns it on" line is superseded. Its transport, worker
  and per-event re-check are all untouched.
