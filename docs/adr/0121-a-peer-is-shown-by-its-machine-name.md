# ADR 0121 — A peer is shown by its machine name, and a card holds facts, not settings

Status: accepted
Date: 2026-09-28

Two reports about the connections list, from a host with one guest connected.
Adds `PROTOCOL_MINOR` 21. Takes the tunnel panel of
[ADR 0078](0078-a-tunnel-is-a-grant-plus-an-address-the-host-named.md) off
the session card, and narrows what
[ADR 0094](0094-a-remembered-host-is-a-card-with-its-own-desktop-on-it.md)'s
card carries outside its menu.

## 1. A peer is shown by what its machine is called

**Report.** Every card was named `b0ffe8b3288991d5`. Nobody can tell two of
those apart, and the per-run label a host hands its guests changes on every
start, so it cannot even be learnt.

**Decision.** Each side tells the other what its machine is called and which
operating system it runs, and the list shows that — the OS icon and the
name — with the label as the name's tooltip.

- **Wire.** `MessageKind::DeviceInfo { name, os }`, appended after
  `FileOpResult`. `name` is `whoami::devicename()` (the pretty computer name
  on macOS, the DNS host name on Windows, `PRETTY_HOSTNAME` or the host name on
  Linux); `os` is `std::env::consts::OS`, a string rather than an enum so an
  operating system this build has not heard of is a generic icon, not a
  malformed frame. `check_limits` bounds them at 256 and 16 bytes. Either end
  may send it, so `MessageKind::direction` (ADR 0122) answers `None`.
- **Gate.** The minor alone, in both directions: both sides announce one and
  the message is the same either way, so there is no feature string. A peer
  below 21 is sent nothing and is shown by its label, exactly as before.
- **When.** A guest sends it right after the handshake, so the host sees who
  is asking while it still decides. A host sends it only in
  `start_granted_session`, ahead of the `ConsentGrant` on the same ordered
  stream: holding an invite is not enough to learn what the machine behind it
  is called, and the guest still knows the name by the time it writes the
  history row the grant triggers.
- **Cleaning.** `lumepeer_core::device` drops control characters and the
  invisible formatting ones (bidi overrides and isolates, zero-width
  characters), collapses whitespace and cuts the name to 64 characters; the
  OS tag is reduced to lowercase ASCII. A name must not be able to reorder or
  hide the row it sits in.
- **Keeping.** The host holds a guest's name for the connection that said it
  and forgets it with that connection; it builds no record of who visited
  (ADR 0016). The guest writes the host's name onto that host's history row,
  and a later visit leaves it alone the way it leaves `trusted` alone.

**What it is not.** The name is the peer's own claim, unverified. Nothing is
keyed on it: every command still names a peer by its label, the history is
still keyed by `host_tag`, and the label is one hover away wherever the name
is shown. The consent dialog is unchanged and still names the guest by label —
a self-chosen name is the one thing a stranger with an invite would like to
put there.

## 2. A card holds what is true; the menu holds what can be done

**Report.** The session card stacked every control it had under the name —
the record button, the file hint, the port-forwarding form — and the
forwarding form's button ran out of the card.

**Decision.**

- **Menu.** Chat, *Record session* / *Stop recording*, *Save this device* and
  *Revoke*. The record item is disabled without the `recording` grant, as the
  button was.
- **Card.** The machine's name, role and input state, and one wrapping row of
  status marks that is left out entirely when there is nothing to say: the
  quality pill, the recording indicator and file name, the secure-desktop and
  terminal indicators, the clipboard note. The indicators no switch may hide
  (§17, ADR 0049, ADR 0079) stay on the card and out of the menu.
- **Questions and marks stay out.** A guest asking to be recorded and a file
  waiting to be accepted are questions for the person at this machine, not
  settings, and stay on the card under the status row; so does the unread
  chat mark of [ADR 0120](0120-saving-a-device-trusts-it-and-an-unread-chat-is-marked.md),
  beside the name. The file panel appears only while this guest has an offer
  or a transfer; sending needs no control of its own (copying is sending), so
  an idle panel was a hint and a heading.
- **Port forwarding leaves the card.** At the owner's request the tunnel
  panel is no longer drawn there, and the main window stops polling
  `tunnel_status`. The feature itself stays: the `tunnel` grant, the
  commands, the actor's tunnels and `tunnels.ts` with its tests are untouched,
  waiting for a place of their own. Until then no address can be allowed from
  the interface, so a guest holding the grant reaches nothing — the reason the
  panel sat on the card (a forwarded port next to the switch that ends it)
  has nothing to show.

## Consequences

- `PROTOCOL_MINOR` 21; two fields (`device_name`, `device_os`) on
  `session_status` and on `connection_history`; `device` on the history file,
  `#[serde(default)]`, so older files load with no name.
- `tests/interop/golden_vectors.txt` freezes five `DeviceInfo` vectors for
  minor 21.
- The session bar lists a guest by the same name, label in the tooltip.
- One new string, `connections.peerId`, the tooltip ("ID: …").
- The file-hint rule for cards is now scoped under the list; at equal
  specificity the general rule came later and right-aligned it.
