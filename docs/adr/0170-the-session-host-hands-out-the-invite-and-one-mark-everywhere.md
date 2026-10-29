# ADR 0170 — The session host hands out the invite, and one mark is drawn everywhere

Status: accepted
Date: 2026-11-02

Amends [ADR 0169](0169-the-host-is-a-system-process-in-the-session-and-the-client-runs-unelevated.md) §4
(what the client's channel to the session host can say). Replaces the
logomark recorded in `PRODUCT.md` (Brand Commitments).

## Context

### "Create invite" did nothing

On a machine hosted by the Lumepeer service (ADR 0169, v0.0.167 on this PC)
the sidebar's "Create invite" button produced no code and no message. The
window's own log says why it cannot: "this machine is hosted by the Lumepeer
service (ADR 0169); this window connects out and views other machines, and is
not a second host".

ADR 0169 moved every host-panel command onto the session host's channel —
sessions, chat, grants, recordings, updates — but not `invite_create` and
`invite_current`. Those still went to the window's own actor, which holds no
host role and refuses exactly as ADR 0085 §4 says it must (`NotTheHost`: an
invite nobody can act on is worse than none). The webview's `createInvite`
had `try … finally` and no `catch`, so the refusal was an unhandled rejection:
the button came back enabled as if it had never been pressed. `NotTheHost` is
raised by no other actor call, so the invite was the only command left behind.

The same window also showed, on every start, "This device is hosted by the
Lumepeer service, not by this window. You can still connect to other devices
from here." The owner: nobody needs it.

### Two logos

The windows drew one mark (`logo.ts`: a navy tile with a blue-gradient "L", a
violet triangle and a blue dot) and the operating system another (the
`icons/` rasters: a cyan-to-violet infinity loop on navy). Neither was drawn in
the palette ADR 0168 gave every window, and `DESIGN.md` already names "a
dark-navy dashboard with a glowing blue accent" as the look Lumepeer refuses.

## Decision

### 1. Two more requests on the channel, and neither names a role

- `CurrentInvite` reads the code the session host is living with, or `null`.
  It never issues one (ADR 0062). Effect: **Reads**.
- `Invite { renew }` hands back the live code, issued first if there is none;
  with `renew` it retires every earlier code and issues a new one. Effect: a
  new kind, **Invites**.

Issuing a code admits nobody. A guest who arrives with it meets the session
host's own consent dialog (ADR 0169 §5), a System-integrity window no program
of the person's can press; the device-password path does not need an invite at
all. The wire carries one byte — renew or not — and no role, so the code is
always for view only and a caller cannot ask for one that lets a guest request
more. `renew` takes codes away, never adds a way in.

The client's `invite_create` and `invite_current` go to the session host
whenever it hosts this machine, as every other host-panel command does. A
closed session host has no endpoint a code could name and refuses both.

### 2. A refused invite is said

The sidebar and the settings "reissue" control show a line under the button
when no code came back: "the service is not running" when nothing answers
(`HOST_SERVICE_UNREACHABLE`), otherwise "could not create an invite code; wait
for the status to turn ready".

### 3. The "hosted by the service" notice goes

The window says nothing about who hosts the machine while the service answers.
It still says, with the "Start the service" key, when the service does not
(ADR 0169 §2) — that one is a fault the person can fix.

### 4. One mark: two peers, and the light where they meet

A rounded-square tile in readout glass (`#232325`), two overlapping rings in
readout text (`#F1F1EC`), and the lens they share filled with the key yellow
(`#F2A31B`). Two machines, nothing between them — serverless — and the lit part
is where they meet: the session. It is built from the interface's own two
signature parts, the readout window and the yellow key, so it sits in both
themes without a variant.

`apps/desktop/src-tauri/icons/icon.svg` is the master. Every raster in that
folder — Windows `.ico`, macOS `.icns`, Linux PNGs, the Store tiles — is
rendered from it with `npx tauri icon icons/icon.svg` (only the files the
bundle ships are kept). `tray-template.svg` is the same glyph in one colour,
lens set in from the rings, for the macOS menu bar (ADR 0158);
`tray-template.png` is it rendered and cropped. `src/logo.ts` draws the same
geometry inline for the sidebar, the host bar and About.

## Consequences

- Any program the person runs can now ask the session host for the invite
  code, as it could already read the sessions. It cannot let anyone in with it:
  admission is still the person's click in the session host's dialog.
- The invite fix needs the session host and the client from the same release:
  an older session host does not know the two request kinds and drops the
  connection, which the client reports as "the service is not running".
- Saved shortcuts and taskbar pins pick up the new icon after the update
  replaces the executable; Windows may show the old one until its icon cache
  refreshes.

## Verification

- `cargo test -p lumepeer-service --lib host_channel_protocol`: both requests
  round-trip, `Invite` is one byte with no role, and no request widens.
- `cargo test -p lumepeer-desktop --bins host_panel`: a closed session host
  issues and shows no invite.
- `vitest`: a refused invite shows its line; the service notice is gone while
  the service answers and the unreachable notice stays.
- Not yet run live: a release build's client asking an installed session host
  for a code, and a guest dialing it.
