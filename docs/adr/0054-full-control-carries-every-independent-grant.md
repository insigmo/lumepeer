# ADR 0054 — Full control carries every independent grant

Status: accepted
Date: 2026-09-02
Amends: ADR 0029 (the four independent grants), ADR 0048 (`DisplayMode`),
ADR 0049 (`SecureDesktop`), design doc §8.2

## Context

Since ADR 0029 a session started at `Grants::from_role(role)` with every
independent grant off, whatever the role. A host that had just decided "this
person may use my keyboard and mouse" then met a six-row checklist on the
connections panel and had to tick, one at a time, the clipboard both ways,
file transfer, recording, display mode and the secure desktop before the
session could do anything but move a pointer.

The checklist did not protect much. Every one of those six is reachable by a
guest who already holds `input`: a peer that can type can open the host's own
file manager, copy from and paste into the host's own clipboard, drive the
host's own display settings, and click through the host's own UAC prompt.
Nothing on the list is a capability `FullControl` withholds outright; the
switches mostly decided whether the guest had to do it the slow way, through
the remote desktop, instead of the fast way, through Lumepeer. What the panel
produced in practice was six clicks of ceremony that train the host to tick
everything without reading it — the failure mode a consent surface exists to
avoid.

The distinction that does carry weight is the one the consent dialog already
draws, between watching and controlling. A guest holding only `view` genuinely
cannot reach any of the six on its own.

## Decision

**`Grants::from_role` gives `Role::FullControl` every independent grant, and
gives the lesser roles none.** One `let full = matches!(role,
Role::FullControl);` drives `input`, `clipboard_read`, `clipboard_write`,
`file_transfer`, `recording`, `display_mode` and `secure_desktop` together.
`ViewOnly` and `ControlLimited` are unchanged: `view` and nothing else.

**The grants stay six independent flags.** `IndependentGrant`,
`Grants::get`/`set` and `session_set_grant` are untouched, both arms still
exhaustive with no `_`. A host can still withdraw exactly one permission from
a running full-control session, and the actor still re-reads the live grant
before every action it gates — a session that got `secure_desktop` from its
role is refused the moment the grant is taken back, by the same check that
already refused one that never had it. What changes is only where a session
*starts*.

**The per-grant switches leave the host panel.** They existed to reach a state
that is now the default, and a checkbox that is on for every session it can be
on for is not information. `session-status.ts` keeps the permission fields —
they still decide what the panel offers, which is why the recording button is
live for a full-control guest and disabled for a view-only one — and drops the
`fieldset` that let the host toggle them.

**`session_set_grant` keeps one caller.** Answering a guest's record request
grants `recording` in the same press (§17), which is the one place a host
still moves a grant by hand.

## Consequences

- Consenting to full control is one decision with one visible consequence,
  taken in the dialog that names it, instead of one decision followed by six
  unnamed ones taken on a different screen.
- A view-only session can no longer be recorded, and a view-only guest can no
  longer be handed the clipboard or files without a new consent at a higher
  role. That is a real narrowing: previously the host could hand a watching
  guest file transfer without giving it the keyboard. The record-request path
  is the deliberate exception, because there the guest asked and the host is
  answering that specific question.
- `EncodeControl`'s `secure_desktop_allowed` is now seeded from the session's
  live grant when the media stream opens (`on_media_accepted`) rather than
  assumed `false`. It used to be correct by construction; with the grant
  arriving at consent time it would otherwise stay off until the host touched
  a switch that no longer exists.
- ADR 0029's, 0048's and 0049's reasoning about why these are *independent*
  grants still stands and is not reopened here. Only their claim that no role
  implies them is amended, and only for `FullControl`.
