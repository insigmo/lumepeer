# ADR 0063 — The host is asked even when the device can let itself in

Status: accepted
Date: 2026-09-07

Amends ADR 0033 (unattended access) on where the consent dialog appears, and
retires three switches: the session row's `secure_desktop_input` toggle
(ADR 0061), the settings panel's helper-service Install/Remove pair (ADR 0043),
and the guest's secure-desktop banner (ADR 0049/0056).

Answers one host's report of a session where "the password box appears
instantly — that's great — but on the other side, in Lumepeer, nothing appears
at all."

## Context

### Only one side was being asked

ADR 0033 sends a *trusted* device down the credential path instead of queueing
a consent request: the reasoning was that a host with a device password
configured is a host with nobody at it. `on_handshaked` took that branch and
returned, so the host queued no request, emitted no `ConsentRequested`, and its
window — which `main.rs` raises on exactly that notification — stayed where it
was, showing nothing.

That reasoning is a guess about the room, not a fact about the machine.
Configuring a device password says a trusted device *may* let itself in. It
does not say the owner has stopped being able to decide, and when the guess was
wrong it was wrong in the worst direction: the guest waited on a password only
the owner knew while the owner sat in front of a window with nothing on it. The
report above is that exact state, and the buttons the reporter went looking for
— view only, full control, cancel — were never removed. They were on a dialog
this path never queued.

### Three switches nobody had a basis to answer

The same session produced three more reports, and they are one shape: a control
whose "off" position is not an answer anybody had a reason to give.

- **`secure_desktop_input`** ("Can control the admin prompt"). Since ADR 0061
  full control carries it, so the switch's only job was to *remove* a
  permission the host had just deliberately granted — while a modal UAC prompt
  was up on the far side, which is the worst possible moment to be reading a
  session row.
- **The Ctrl+Alt+Del helper service.** It admits nobody and holds exactly one
  capability. The installer registers it and the uninstaller removes it
  (`installer-hooks.nsh`), so the panel's Install/Remove pair only ever
  described a state the user was not choosing — and the sole symptom of it
  reading "not running" was a Ctrl+Alt+Del button that silently did nothing.
- **The secure-desktop banner.** The host throttles secure-desktop capture to
  500 ms while the encode loop runs at 30 Hz, so `ViewStatus::SecureDesktop`
  alternates with `Live` several times a second by construction. The sentence
  it rendered blinked at the top of the picture, over a picture of the UAC
  prompt that already said the same thing.

## Decision

**Both ways in are offered at once.** `on_handshaked` queues the consent
request *and*, for a trusted device, sends `UnattendedChallenge`. The guest
sees the password prompt; the person at the host sees the request, and the
window is raised in front of them as it is for any other guest.

Whichever answer lands first decides, and closes the other:

- `grant_role` drops the peer from `unattended_pending`, so a password arriving
  after the host has already chosen a role cannot grant a second, different one.
- `on_revoke` drops it too, so "no" is an answer to the challenge as well as to
  the dialog — a refused guest has nothing left to submit against.
- `SessionManager::grant` already removes the queued request, so a correct
  password takes the dialog off the host's screen.

A consent queue that is full or rate-limited no longer refuses a device that
can admit itself without the queue; the challenge is still offered, and only a
peer with neither way in has its connection closed.

**The three switches are gone.** `secure_desktop_input` keeps following full
control and stays independently revocable through `session_set_grant` — what is
removed is the button, not the grant. The helper service is registered at
start-up by `service_control::ensure_installed`, which needs no prompt because
the client already runs elevated (ADR 0057); `service_status` and `service_set`
are removed from the IPC surface. `viewOverlay` renders nothing for
`secure-desktop`.

## Consequences

- A host with unattended access configured is no longer treated as an empty
  chair. This is a real narrowing of ADR 0033: unattended admission still works
  with nobody present — nothing about the credential path changed — but it is
  no longer the *only* thing offered, and a guest can now be let in by a person
  who never types the password.
- Two answers race for one session. Both paths converge on `grant_role`, which
  is idempotent for an already-active peer, and each clears what the other was
  waiting on; the loser's answer is refused rather than applied late.
- The host sees a dialog for a device it has already trusted, including one
  signing in from a remembered password — where the dialog will appear and
  disappear within a round trip. That flicker is the cost of not guessing
  whether anybody is there.
- Removing the helper-service pair means the app installs a privileged service
  the app itself offers no way to remove. The uninstaller is where removal
  lives now, and that is stated in `service_control.rs`'s own header rather
  than left implicit.
- ADR 0061's consequence "the session row's toggle keeps both jobs" is
  superseded. Its decision — full control carries the grant — is not.
- The secure-desktop status still crosses the IPC boundary and still exists in
  `ViewStatus`. Nothing renders it; the picture is the disclosure, as ADR 0056
  intended it to be.
