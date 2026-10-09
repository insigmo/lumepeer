# ADR 0151 — The sign-in screen is hosted on Linux and macOS too

Status: accepted
Date: 2026-10-09

The user's report: after a Linux or a Mac restarts, Lumepeer is not running at
the sign-in screen, so a guest cannot reach the machine to type the login or
the password — the thing [ADR 0126](0126-the-logon-screen-is-hosted-by-the-same-host.md)
gave Windows, asked for on the other two platforms.

Extends ADR 0126 and [ADR 0133](0133-a-locked-host-is-still-the-host.md) to
Linux and macOS. The Windows design is unchanged.

## Why it did not already work

On Windows the sign-in screen is served by the desktop binary run as
`LocalSystem` on `Winlogon` by the already-installed helper service, reading
the owner's stores from a directory only administrators can open (ADR 0126,
ADR 0123). Two things that design leans on are missing on the other platforms:

- **Nothing privileged is running before sign-in.** Autostart is per-user and
  unprivileged on all three platforms (ADR 0042, ADR 0140); on Linux and macOS
  there is no Lumepeer process at all until somebody signs in. ADR 0043
  deliberately ships no root daemon off Windows.
- **The credentials are locked away until sign-in.** On Linux they live in the
  Secret Service, on macOS in files under the owner's profile that only the
  owner can read (ADR 0128). Both are reachable only once the owner has signed
  in — which is exactly when the sign-in screen is gone.

## Decision

The same desktop binary hosts the sign-in screen, run as `root`
(`lumepeer-desktop --logon-host`, [`logon_host_unix.rs`]), the counterpart of
the Windows `logon_host`. It is the whole host — same runtime, same identity,
same device password and role — so a guest dials the same saved host it always
dials. What differs per platform is **who starts it** and **where it reads the
owner's credentials**, because the keyring is locked at the sign-in screen.

### 1. The owner enrols a copy while their keyring is open

Since the live keyring cannot be read before sign-in, the owner's client hands
a copy of the five entries a host needs — identity, device-password hash, role,
TOTP secret, audit salt — to a keeper that stores it where only `root` can
read it. The copy is made while the owner is signed in and their keyring is
open. The sign-in-screen host reads the copy, never the keyring.

Nothing but those five entries crosses, the copy is a plain fixed-shape record
([`logon_enroll.rs`]), and the keeper never interprets it — it checks the
shape and the bounds and writes it out.

### 2. Linux: a system supervisor, the equivalent of the Windows helper

`lumepeer-service --logon-supervisor` ([`logon_supervisor.rs`]) runs as `root`
from a systemd unit (`lumepeer-logon.service`, installed and enabled by the
package). It is the Linux counterpart of the Windows helper, and it does two
things and nothing else — no network, no capture, no input:

- **Keeps the owner's copy.** It listens on a Unix socket
  (`/run/lumepeer/logon-host.sock`), learns who is enrolling from the kernel
  (`SO_PEERCRED`), keeps one owner per machine (ADR 0126 §3), and writes the
  copy `root`-only under `/var/lib/lumepeer`. The client enrols it when the
  owner ticks "Also at the sign-in screen", and refreshes it whenever the
  password or role changes.
- **Decides when.** Once a second it asks `logind` which session is in front on
  `seat0`. When that session is a graphical **greeter** (`Class=greeter`), an
  owner is enrolled, and the owner has no graphical session of their own (whose
  client would be the host already), it starts `lumepeer-desktop --logon-host`
  into that screen's display, with the environment the display needs. When the
  greeter goes — somebody signed in, the seat switched — it stops it with a
  `SIGTERM`, and a `SIGKILL` if it will not leave. The host's exit code says
  whether to try again or wait for the screen to change (ADR 0126's
  "not enabled" / "role taken" codes are reused).

An SSH login is never mistaken for somebody at the machine: only an `x11` or
`wayland` session counts, never a `tty` one.

The supervisor is the one root process ADR 0043 said the other platforms would
not ship. It earns it the way the Windows helper does: it is the sign-in-screen
keeper, it reads no configuration a user can write, its socket admits a bounded
request from any local account and decides by the kernel's `SO_PEERCRED` who
owns the screen, and it does nothing at all until an account opts in.

### 3. macOS: a LoginWindow launchd agent

`launchd` runs the host as `root` in the `LoginWindow` session —
`LimitLoadToSessionType = LoginWindow` — and unloads it the moment somebody
signs in, which is the lifetime the host wants (`packaging/macos-loginwindow-
agent.plist`). The agent passes the owner's keystore directory in
`LUMEPEER_LOGON_KEYSTORE`; the host opens it with the same `FileKeystore`
the client uses (ADR 0128) and reads the five entries. There is no
always-running keeper to enrol through, because the files are already where a
`root` agent can read them — the macOS "enrolment" is installing the agent,
which needs an administrator and so cannot be a drag-install step.

### 4. What the host is, on either platform

As on Windows (ADR 0126 §4): nobody is present, so attendance is `Unattended`
and the device password is the only way in; the screen is treated as the
secure desktop, so a keystroke needs `secure_desktop_input` and the picture
needs `secure_desktop`, carried by a full-control role; there is no banner;
and it keeps nothing — no address book, no invite file, no audit database —
everything is in memory and dies with the screen. Signing in stops the host
and the ordinary client takes over, a reconnect of a few seconds (ADR 0106).

## Not done

- **The GNOME/Wayland greeter's picture.** A GDM sign-in screen draws with
  GNOME Shell on Wayland as GDM's own account, where there is no portal to ask
  and the existing portal capturer would raise a dialog nobody can answer. Its
  frames come instead from `org.gnome.Mutter.ScreenCast`/`RemoteDesktop` on
  that account's bus, reached by a helper running **as that account** — the
  same interface `gnome-remote-desktop` uses to serve a GDM screen. That
  backend is **not wired in** this change: an **X11 greeter** (LightDM, SDDM on
  X11, GDM's Xorg fallback) works now through the existing X11 capturer and
  XTEST injector against the greeter's `DISPLAY`/`XAUTHORITY`, which the
  supervisor resolves; a **Wayland greeter** gets no picture yet and the guest
  is told there is no capture backend (§18), rather than shown a frozen screen.
- **macOS pre-login capture is unproven here.** ScreenCaptureKit is reported
  working pre-login from a `LoginWindow` agent on macOS 14.4+ (Apple DTS,
  rdar://121253782), but was unreliable on macOS 15 for several developers and
  is untested on this project's 26.x target. Apple also advises against running
  a whole GUI app as `root` pre-login and suggests an agent + privileged-helper
  split; if the single-process host will not capture or inject at the
  `LoginWindow`, that split is the fallback.
- **macOS input at the sign-in screen.** `CGEventPost` needs Accessibility,
  which cannot be granted to a pre-login agent the ordinary way
  ([ADR 0150](0150-the-mac-cursor-leaves-the-picture.md), the TCC story). To be
  established on a real machine.
- **A one-owner query on macOS.** The toggle's state on Linux comes from the
  supervisor; macOS has no runtime query for the agent yet, so the settings
  panel does not show the toggle there — the agent is installed by hand.

## Verification

Tests that run anywhere: the enrolment codec and its bounds
(`logon_enroll`), the `loginctl`/`/proc` parsing and the greeter/SSH/Wayland
distinctions (`logon_seat`), and the supervisor's owner policy
(`logon_supervisor`). Built and unit-tested on Linux (WSL Debian 13) and
Windows; the macOS arm compiles into the same binary but its pre-login path is
unrun.

**None of the real hardware steps have run** — no reboot, no sign-out, no live
greeter on either platform. See `docs/release-checklist.md`, "Hosting the
sign-in screen on Linux and macOS".
