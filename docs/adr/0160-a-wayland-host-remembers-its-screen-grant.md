# ADR 0160 — A Wayland host remembers its screen grant

Status: accepted
Date: 2026-10-15

Builds on [ADR 0010](0010-wayland-capture-and-input.md) (capture and input
through xdg-desktop-portal) and
[ADR 0159](0159-every-guest-of-a-wayland-host-gets-its-own-portal-connection.md)
(a portal connection per guest).

## Context

Since ADR 0159 every guest of a Wayland host reaches the portal, and every
guest raises the portal's RemoteDesktop dialog: the handshake passed
`PersistMode::DoNot` to `SelectDevices` and `SelectSources`, so GNOME asked
again each time and `Start` did not answer until somebody at the host's screen
clicked *Share*. A host nobody sits at — the `debian` VM, any machine reached
with the device password — never showed a guest anything.

The portal can remember a grant. What it takes, read from the sources at the
tags `debian` runs and checked against its installed binaries:

- **xdg-desktop-portal ≥ 1.17.0** (RemoteDesktop version 2) takes
  `persist_mode` and `restore_token` on `SelectDevices`. A combined
  remote-desktop + screen-cast session persists there and only there:
  `SelectSources` on such a session fails for any persist mode but none, or
  any restore token ("Remote desktop sessions cannot persist"). The grant is
  kept in the permission store, table `remote-desktop`, under the caller's app
  id — the empty id for every unsandboxed program. A token is single-use: the
  frontend looks it up and deletes it at `SelectDevices`, and saves it again
  (the same UUID) when `Start` brings restore data back, which `Start`'s
  results then carry as `restore_token`. A token that is not a UUID fails
  `SelectDevices` outright. Versions up to 1.16 copy only the option keys they
  know, so the two new keys are dropped and the dialog comes up as before.
- **xdg-desktop-portal-gnome ≥ 45.0** restores a session from that data
  without a dialog. Asked for `ExplicitlyRevoked`, its dialog shows
  *Remember this selection*, ticked by default; unticked, no restore data
  comes back. A restored monitor is found again by vendor, product and serial,
  not by resolution. One trap: when that monitor is gone, GNOME restores the
  input devices **with no stream** and still hands the grant back, so a kept
  token would bring every later guest input and no picture.
- **xdg-desktop-portal-kde ≥ 6.0** restores too, with no dialog and a
  "Remote control session started" notification instead. It restores when the
  same devices and screen sharing are asked for, and streams every screen
  rather than matching a monitor, so it has no GNOME-style trap. Plasma 5.27
  does not restore.
- Restore data is the backend's own (`GNOME` or `KDE` inside it), so a
  machine that switches between the two desktops gets the dialog once after
  each switch.
- `debian` runs GNOME 48.7, xdg-desktop-portal 1.20.3,
  xdg-desktop-portal-gnome 48.0 and xdg-desktop-portal-kde 6.3.5; both its
  RemoteDesktop interfaces report version 2. Ubuntu 24.04 (GNOME 46, 1.18.4)
  has both halves; Debian 12 and Ubuntu 22.04 (1.16, 1.14) do not and keep
  asking.

## Decision

1. `SelectDevices` asks for `PersistMode::ExplicitlyRevoked` and offers the
   token kept from the last handshake. `SelectSources` keeps
   `PersistMode::DoNot`, as the portal requires.
2. The token `Start` returns is written back after every handshake, because
   each one is single-use; a handshake that returns none (the person unticked
   *Remember*, or the portal is too old) forgets the old one. It lives in
   `portal-grant` in the app's local data directory, beside the relay
   measurement (ADR 0098), readable by this user only. It is not kept in the
   keystore: it is no secret — the portal treats every unsandboxed program of
   this user as one app, and any of them can list the stored grants — and a
   keyring still locked on a host that started with nobody at it must not be
   what stops its picture. A file that does not hold a UUID offers nothing.
3. The path comes from the application (`default_capture(portal_grant)` →
   `platform_backend(portal_grant)`); the sign-in-screen host, the session
   agent and `crates/host` pass `None` and ask every time, as tests do by
   building no backend at all.
4. **A refused grant asks again, it never waits.** A kept token whose session
   comes back with no stream, or that the portal refuses at any step, is
   forgotten, its session closed, and the handshake runs once more without it
   — which raises the dialog. A token the portal no longer knows raises the
   dialog by itself. A *Cancel* on that dialog is a person's answer: it is
   reported as denied, the token is forgotten, and the question is not put to
   them twice.
5. **Every guest uses the kept grant**, the owner decided on 2026-10-15:
   a guest admitted by the device password and one somebody at the host let
   in through lumepeer's own consent window alike. One click in lumepeer is
   enough; after the first *Share* the desktop's dialog does not come back.
   Weighed against keeping the dialog for consent guests (two clicks per
   guest, the second asking the same person the same thing) and against a
   setting of its own. What keeps this safe is unchanged: lumepeer's admission
   — consent, or the device password a trusted device must prove — still
   decides every guest, and the dialog never named a guest anyway, so it only
   ever said that somebody was at the screen. GNOME's top-bar indicator (KDE:
   its notification) and lumepeer's banner still show every session.

## Consequences

- After one *Share* with *Remember this selection* ticked, every later guest
  of a GNOME ≥ 45 host gets a picture with nobody at the host, across app
  restarts too.
- On older portals and desktops nothing changes: the options are dropped and
  the dialog comes up for every guest.
- Revoking: GNOME 48 lists no grant of an unsandboxed app anywhere in
  Settings. Deleting `portal-grant` makes lumepeer ask again; the grant itself
  stays in the permission store (table `remote-desktop`) until it is deleted
  there.
- The host's log tells a restored grant (`the portal restored the remembered
  screen grant`) from a dialog somebody answered: xdg-desktop-portal hands a
  restored grant its own token back, and a new one otherwise.
- Not covered: the sign-in-screen host still asks for every guest.

## Verified

Unit tests (every Linux CI row with `capture-portal`): only a UUID is ever
offered; a kept token is offered back until a handshake returns none, in a
file of mode 0600; a damaged file offers nothing; the default grant asks the
portal to keep nothing and writes nothing.

Live on `debian` (GNOME 48.7 on Wayland), 2026-10-15, with the owner at the
screen for the first round only:

- `a_remembered_grant_brings_the_picture_back_without_a_dialog` (opt-in,
  `LUMEPEER_TEST_PORTAL_RESTORE=1`, grant file set with
  `LUMEPEER_TEST_PORTAL_GRANT`): three guests through the real
  `WaylandPortalCapturer`, one process. Guest 1 offered a token the portal had
  never seen: the dialog came up, the owner clicked *Share* after 35 s, a new
  token was kept. Guests 2 and 3 offered it back and had their first frame
  (1280×800) after 0.38 s and 0.43 s, the same token returned both times — no
  dialog.
- Then the e2e build of the app (`e2e/matrix/deploy.sh linux`), a new process
  reading the token that test left in its grant file: three guests in a row
  from the Windows machine through the matrix harness (invite, grant, view).
  Picture after 1.5 s, 0.3 s and 0.3 s; the host logged `the portal restored
  the remembered screen grant` for each, and the permission store's
  `remote-desktop` table held that one grant and nothing else.
- Again with nobody at the screen, 09:57–10:01 UTC: five guests in one host
  process (picture 0.3–0.6 s after the grant), then a restarted host with two
  more (1.0 s each), all restored, the token unchanged throughout.
  xdg-desktop-portal-gnome warns `Failed to associate portal window with
  parent window` for every dialog it opens for a caller with no parent window,
  as ours; its journal has none in that window.

## Not verified

- GNOME restoring a grant whose monitor is gone (devices, no stream): read
  from xdg-desktop-portal-gnome 48.0's `restore_screen_cast_streams` and
  `restore_from_data`, not produced on a machine.
- KDE: read from xdg-desktop-portal-kde 6.3.5's `remotedesktop.cpp`, not tried
  live (`debian`'s seat0 was GNOME).
