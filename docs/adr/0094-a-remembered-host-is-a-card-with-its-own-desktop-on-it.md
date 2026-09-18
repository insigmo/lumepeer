# ADR 0094 — A remembered host is a card with its own desktop on it

Status: accepted
Date: 2026-09-19

Replaces the connection list's rows with cards, and gives each one the picture
of the machine it leads to. Builds on
[ADR 0016](0016-a-guest-remembers-the-hosts-it-connected-to.md), which created
the list, and [ADR 0085](0085-the-view-surface.md), which owns the window the
picture comes from.

## Context

The connection list was a row of controls per host: a label, a role, a "last
seen", then Connect again, a Reconnect-on-its-own checkbox, Forget password
and Remove, all of them side by side and all of them the same size. Four
controls, one of which ends the row and one of which deletes a password, laid
out so that the one people actually press — connect — was neither larger nor
more central than the two that destroy something.

Every remote-desktop client people compare this one to (RustDesk, AnyDesk,
TeamViewer) settles this the same way: a card per machine, the connect action
*is* the card, and everything else is behind one overflow menu. The comparison
is in `docs/comparison-rustdesk-teamviewer-anydesk.md` and it is not a matter
of taste — a list of machines is scanned by recognition, not read.

What those clients put on the card is an operating-system logo, which is the
one fact every row has in common and so tells nobody anything. This client can
do better: it has watched the machine. The picture of a desktop — which
wallpaper, which windows were open — is how a person actually recognizes which
of four Windows boxes they want.

## Decision

**The list is a grid of cards.** One card per remembered host and per live
session: a picture, the role and how long ago it was, then a footer with a
state dot, the host's pseudonymized id, and a three-dot menu. The card face is
the connect control for a remembered host; for a live session it is a panel,
because there is nothing to connect to.

**The menu holds what the row already did, and nothing new.** Connect,
Reconnect on its own, Forget password, Remove for a remembered host; Chat,
Save this device, Revoke for a live session. Every one of them is the same
call to the same command as before — this ADR moves controls, it does not add
capabilities. The live panels a session carries (connection quality, recording,
the file-transfer and tunnel panels, the secure-desktop and terminal
indicators) stay exactly where they were, under the card's footer.

**The picture is kept by the webview, for the webview.** The view window
downscales its canvas to 256px wide every fifteen seconds and writes a JPEG
data URL to `localStorage`, keyed by the host's *stable* pseudonym; the
connection list reads it back. Twelve hosts keep a picture, newest first.

Three properties decide this:

- The actor never holds it. The frames are already in the webview (ADR 0058
  moved decoding there), the picture is only ever shown back to the person who
  watched it, and handing it to the trusted side would make that side store
  screen content it has no other reason to hold (§2.3, §15).
- It is keyed by `host_tag`, not the per-session label. `peer_tag` is re-salted
  every run precisely so a guest cannot be correlated across runs, which would
  lose every picture at every restart; `host_tag` is the name the
  remembered-hosts list is already keyed by. The actor therefore puts both in
  the view window's URL, and `ViewWindows::open` gained the second argument.
- Removing a host removes its picture. Nothing outlives the row that explained
  it.

## Consequences

- A picture of a remote desktop persists in this webview's storage between
  sessions. It is per-viewer, never leaves the machine, is capped at twelve
  hosts and a few kilobytes each, and goes when the row goes — but it is
  screen content at rest that was not at rest before, and anyone for whom that
  is the wrong trade clears site data or removes the row.
- A host that has never been watched long enough to be photographed shows a
  screen glyph. So does every live session card: this node is the host there,
  and the guest's screen is not its to show.
- The menu is a `<details>` element. The panel re-renders every second, and
  anything whose openness lived in a template variable would shut itself while
  somebody was reading it; the browser owns the `open` attribute and lit-html
  leaves attributes it does not bind alone. Closing it on an outside click and
  on Escape is two document listeners in `main.ts`.
