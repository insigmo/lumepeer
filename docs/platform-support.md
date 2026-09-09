# Platform support: Raspberry Pi (arm64) and ChromeOS (Crostini)

Written for gap-tasks/05 (`docs/gap-tasks/05-packaging-arm-chromeos-freebsd.md`,
backlog item 10). FreeBSD is covered separately in
[ADR 0068](adr/0068-freebsd-is-not-a-supported-platform.md) — that is a
research-only call with no hardware behind it, kept out of this file because
this file is otherwise all real, on-device verification.

Read this section by what was actually run versus researched:

- **Raspberry Pi** — real hardware, `ssh beta@pi.local`, a Raspberry Pi 4
  Model B Rev 1.4. Everything under it was executed on that device unless
  marked otherwise.
- **ChromeOS/Crostini** — no Chromebook was available to this session.
  Everything under it is drawn from Google/Chromium's own published
  architecture docs, clearly cited, and is **not verified on a device**. Do
  not read anything there as tested.

## Raspberry Pi 4/5 (arm64, Raspberry Pi OS 64-bit)

### Device and what was on it

`ssh beta@pi.local` reached a real Raspberry Pi 4 Model B Rev 1.4, 7.6 GiB
RAM, 4 cores, running **Raspberry Pi OS "trixie" (Debian 13) — the Lite
(headless) image**: `Linux pi 6.12.47+rpt-rpi-v8 #1 SMP PREEMPT Debian
1:6.12.47-1+rpt1`, glibc 2.41. No display was attached (`/dev/fb*` absent,
both DRM connectors report `status: disconnected`), and no desktop session
was installed at all — no `xserver-xorg`, no display manager, no wlroots
compositor, nothing under `dpkg -l` beyond X11/Wayland *client* libraries
(`libx11-6`, `libwayland-client0`) and the portal backends
(`xdg-desktop-portal`, `xdg-desktop-portal-gtk`, `xdg-desktop-portal-wlr`).
`XDG_SESSION_TYPE=tty`, `loginctl list-sessions` showed only the SSH login,
no seat. Everything needed to actually exercise a graphical session — Xvfb,
`sway`, `seatd`, `gnome-keyring`, `xterm` — was installed during this
session and is recorded below as installed, not assumed present on a stock
image.

### Installing the package

The workspace's own release pipeline (`.github/workflows/release.yml`)
already publishes a `linux-arm64 (deb/rpm)` row built on `ubuntu-24.04-arm`
with `capture-x11,capture-portal,encode-openh264`. Rather than cross-compile
locally under this session's own disk constraints, the **prebuilt release
artifact** was used: `Lumepeer_0.0.62_arm64.deb` from
[release v0.0.62](https://github.com/insigmo/lumepeer/releases/tag/v0.0.62),
downloaded directly onto the Pi with `curl`. That release was built from
commit `b879491`, which is the direct parent of this batch's base commit
`8200dd0` — the one commit between them (`8200dd0`) is docs-only
("docs: add gap backlog and second-wave gap-tasks batches"), so the binary
is functionally identical to what this branch would produce.

```
Package: lumepeer
Version: 0.0.62
Architecture: arm64
Depends: libpipewire-0.3-0, libwebkit2gtk-4.1-0, libgtk-3-0
Recommends: xdg-desktop-portal, pipewire
```

`sudo apt-get install -y ./Lumepeer_0.0.62_arm64.deb` resolved cleanly with
**zero missing dependencies** and no extra downloads (0 B/14.6 MB — nothing
new to fetch). That is worth calling out precisely because Raspberry Pi OS
trixie has renamed the underlying libraries for the 64-bit `time_t`
transition (`libpipewire-0.3-0t64`, `libgtk-3-0t64` are what `apt-cache
policy` actually finds; the un-suffixed names the `.deb` depends on resolve
via each package's transitional `Provides`). `apt` handled this
transparently; a raw `dpkg -i` without `apt`'s dependency resolution would
not have.

### glibc floor (ADR 0017)

ADR 0017 declares a floor of glibc 2.35 and patches away weak `GLIBC_2.39`
requirements at bundle time. On the installed binary:

```
$ objdump -T /usr/bin/lumepeer-desktop | grep -oE 'GLIBC_[0-9.]+' | sort -Vu | tail -5
GLIBC_2.29
GLIBC_2.30
GLIBC_2.32
GLIBC_2.33
GLIBC_2.34
```

Highest requirement is 2.34 — inside the 2.35 floor, and comfortably below
the Pi's own 2.41. No patched-weak `GLIBC_2.39` symbol shows up either,
consistent with `ci/glibc-floor.mjs` having done its job on the
`ubuntu-24.04-arm` build host. This is a real check against a real binary,
not a re-read of the ADR.

### Runtime dependency versions actually present

| Dependency | Declared | Installed on this Pi |
|---|---|---|
| `libpipewire-0.3-0` | Depends | `libpipewire-0.3-0t64` 1.4.2-1+rpt2 (satisfies via `Provides`) |
| `libwebkit2gtk-4.1-0` | Depends | 2.50.4-1~deb13u1 |
| `libgtk-3-0` | Depends | `libgtk-3-0t64` 1:3.24.49-3+rpt8 (satisfies via `Provides`) |
| `xdg-desktop-portal` | Recommends | 1.20.3+ds-1 |
| `pipewire` | Recommends | 1.4.2-1+rpt2, with `wireplumber` 0.5.8-2 and `pipewire-pulse` |

All ADR 0039-listed runtime deps are present on stock Raspberry Pi OS
trixie, Lite image included — this was not something the Lite image
guaranteed by design, it happens to already carry these because trixie's
own base install pulls in PipeWire-adjacent packages for other reasons.

### A real, undeclared requirement this device exposed: the Linux Secret Service

The Pi is where this surfaced, but it is not Pi-specific — it is true of
**any** Debian/Raspberry-Pi-OS-Lite-shaped install, including a headless
server or a container. It belongs in this doc because "install the .deb and
run it" is exactly the step that hit it.

`crates/net/src/keystore.rs`'s `open()` deliberately refuses to fall back to
an encrypted file on Linux — the doc comment is explicit that doing so
"would weaken the storage of §11.2 without the user being told." On Linux
that means the D-Bus Secret Service (`org.freedesktop.secrets`,
conventionally served by `gnome-keyring` or a distro's equivalent) is a hard
requirement to bind the network endpoint at all, host or guest role, view
window or none. Neither the `.deb`'s `Depends` nor its `Recommends` mention
one, and Raspberry Pi OS Lite ships none.

Running `lumepeer-desktop` on a fresh Lite install (real D-Bus session bus
at `/run/user/1000/bus`, real Xvfb display, no keyring installed) shows a
GTK window appear — capture/GTK init happens first and does not need the
keystore — then a few seconds later the process exits:

```
fatal: failed to bind the network endpoint: keystore unavailable: zbus error:
org.freedesktop.DBus.Error.ServiceUnknown: The name org.freedesktop.secrets
was not provided by any .service files
```

Installing `gnome-keyring` (`sudo apt-get install -y gnome-keyring`,
available in trixie's own repos, 48.0-1) is not sufficient by itself either.
`gnome-keyring-daemon`'s documented `--unlock` behavior ("read a password
from stdin ... or create it if the login keyring does not exist") did not
create a usable default collection in this session over SSH — `secret-tool
store`/`lookup` and a direct D-Bus `ReadAlias("default")` both kept
returning no collection. Creating one explicitly via
`Service.CreateCollection` does start a `Prompt`, but completing that prompt
requires an interactive confirmation dialog (`gcr-prompter`) that never
rendered a window in this headless setup and the prompt auto-completed as
dismissed. This is the standard PAM (`pam_gnome_keyring`) integration path
that a real graphical login unlocks automatically with the user's login
password — not reproducible non-interactively over SSH in the time this
session had, and Raspberry Pi OS Lite has no such login flow to begin with
(it has no display manager).

**Net finding:** a stock Raspberry Pi OS Lite install cannot run
`lumepeer-desktop` at all — not degraded, not view-only, an unconditional
fatal exit — until a Secret Service provider is installed *and* its default
collection is unlocked, and the latter is not straightforward without a
graphical login session. This is a real gap between what the `.deb`
declares and what it actually needs, independent of ARM/aarch64 specifics;
it would reproduce identically on an amd64 Debian server install. Fixing it
(declaring a `gnome-keyring` dependency, or a documented headless-unlock
recipe, or a different keystore fallback policy for genuinely headless
targets) is a `crates/net` design question outside this packaging batch's
authorized files (this batch's task list is `docs/` and, conditionally, the
release matrix) and is called out here as a finding, not fixed here.

### Host role: what could and could not be exercised

No desktop session (X11 or Wayland) exists on Raspberry Pi OS Lite by
default. Both were stood up manually for this test, and both are reported
honestly for what they reached given the keystore gate above:

**X11.** `Xvfb :99 -screen 0 1920x1080x24` gives a real (virtual) X server;
`xrandr` reports one 1920x1080 monitor. `DISPLAY=:99
/usr/bin/lumepeer-desktop`, run with a genuinely unlocked keystore path
would be needed to reach a live capture session — this session could not
get a keyring unlocked (see above), so the **X11 capture/encode path itself**
was verified independently, directly through `lumepeer-media`
(`capture-x11,encode-openh264` features), not through the full desktop app —
see "Software `openh264` numbers" below. This is a real, on-hardware
measurement of the same `X11Capturer`/`select_encoder` code path the app
uses; it is just not wrapped in the app's own network/session layer.

**Wayland.** A real wlroots compositor (`sway` 1.10.1, `libwlroots-0.18`
0.18.2) was brought up via `seatd` (installed and started; the Pi's `beta`
user is already in the `video` group `seatd -g video` grants access to) —
`sway`'s own Wayland socket (`wayland-1`) came up, `swaybar`/`swaybg`
attached to it. That is a real, working Wayland session. The
`xdg-desktop-portal-wlr` screen-cast backend, however, failed immediately
with `Could not find render node` when `sway` ran under
`WLR_BACKEND=headless` (needed because no monitor is attached), and reverting
to `sway`'s normal DRM auto-detection found zero connected outputs
(`swaymsg -t get_outputs` → `[]`) since there is genuinely no display
plugged into this Pi's HDMI port. **The ADR 0039 portal capture path was not
exercised end to end on this device.** What was verified: the compositor
and both portal daemons (`xdg-desktop-portal`, `xdg-desktop-portal-gtk`,
`xdg-desktop-portal-wlr`) install and start cleanly on this OS/kernel
combination; what stalled is DMA-BUF export from a compositor with no real
GPU-backed output, which needs either a physical monitor on the Pi's HDMI
port or a software-rendering configuration this session did not find a
working combination for in the time available.

### Guest role

Not reached as a full session for the same keystore reason as the host role
above. The window/GTK/webview stack initializes normally under Xvfb (a
window titled `lumepeer-desktop` was observed via `xwininfo` before the
delayed keystore panic), so nothing here suggests a guest-side rendering
problem specific to arm64 — the blocker is the same Secret Service gate,
not the guest UI.

### Host+guest pairing

The task's own rule (two Lumepeer processes cannot share one desktop) calls
for a second machine. `192.168.40.128` (`LINUX_HOST` in `Taskfile.yml`) and
`192.168.40.130` (`WAYLANDLINUX_HOST`) were both unreachable — `ssh: connect
... Connection timed out` — on repeated attempts over the course of this
session; `betas-iMac.local` failed mDNS resolution outright. The main
Windows desktop this session runs on was deliberately not used as the
second machine, per this batch's own instructions. **No live host+guest
pairing test was completed on real hardware in this session** — see the
"could not verify" list at the end of the parent report for what exactly
was missing.

### Software `openh264` numbers on Pi 4

Because the full app could not get past the keystore gate, real numbers for
the capture→encode path were measured directly against
`lumepeer-media`'s own public API (`X11Capturer` + `select_encoder`,
`capture-x11,encode-openh264` features), built **natively on the Pi**
(`rustup` stable 1.98.1/aarch64, `cmake`/`build-essential` installed for
`openh264-sys2`'s vendored C++ build, which compiled with
`HAVE_NEON_AARCH64` — the real NEON-optimized path, not a generic fallback).
This exercises the identical `ScreenCapturer`/`VideoEncoder` code the
desktop app calls; it is just not wrapped in the app's network/session
layer, which is exactly the part the keystore gate blocked.

Test content: an `xterm` window (604x524) on the `Xvfb :99` 1920x1080
desktop, continuously scrolling one line of text every 50 ms — a real,
modest, "someone is working in a terminal" workload, not a synthetic
worst-case full-screen flash and not an idle desktop either (an idle screen
would mostly report `next_frame() -> None`, since capture only returns a
frame when the hash changes, and would say nothing about encode cost).

Encoder config used: the workspace defaults (`EncoderConfig::default()`) —
30 fps target, 8000 kbps target bitrate, H.264, exactly what a fresh session
starts at before ABR adjusts it.

```
PLACEHOLDER_PI_BENCH_RESULT
```

**Recommended profile for Pi-class hardware:** PLACEHOLDER_PROFILE_RECOMMENDATION

### What is explicitly refused

- **32-bit Raspberry Pi OS (armv7).** Not supported. No new release-matrix
  row is added for it; this is the explicit record of that refusal the
  batch asks for.
- **The Raspberry Pi hardware encoder (V4L2 M2M).** Out of scope for this
  batch by the batch's own instruction — a different backend, a different
  pass.
- **ABR constants.** Not touched. They are shared across every platform by
  design; a Pi-specific tuning pass would violate that.

## ChromeOS (Crostini) — researched, not verified on hardware

No Chromebook was available to this session. Nothing below was run; all of
it is drawn from Google/Chromium's own published documentation, cited
inline. Read every claim in this section as "this is what Crostini's own
architecture says should happen," not as "this was observed."

### What Crostini actually is

A Linux (Termina VM, Debian container) environment ChromeOS runs
alongside itself, not inside it. Chromium's own container/VM documentation
is explicit about the isolation model:
["by design, one client cannot get access to any other client on the
system"](https://chromium.googlesource.com/chromiumos/docs/+/master/containers_and_vms.md) —
stated as a deliberate security boundary, not an incidental gap. Graphical
output goes through **Sommelier**, "a Wayland proxy compositor that runs
inside the container," which forwards "contents, input events, clipboard
data" between container apps and Chrome, and starts **Xwayland** (rootless)
for X11 clients, acting as their window manager.

### Host role: expected impossible, and this is why

A Wayland client — which is what `lumepeer-desktop` is via `wry`/GTK on
Linux, X11 or portal path either way — only ever gets the one Wayland
surface Sommelier hands it. There is no protocol path from inside the
container to "the whole ChromeOS desktop," and Chromium's own text above
says that gap is deliberate, not a missing feature: a compromised or
malicious container app is specifically meant to be unable to see or attack
anything outside its own window. Neither `capture-x11` (would only ever see
Xwayland's own rootless composited surface, not the ChromeOS desktop behind
it) nor `capture-portal` (the portal negotiates a *stream Sommelier itself
would have to offer*, and nothing in Sommelier's documented feature set
offers "the ChromeOS compositor's own screen") has a route to real host-role
capture here. **Expected: host role does not work, by the platform's own
design, not by a Lumepeer limitation that could be coded around.**

The task file asks that this be closed in the UI with an honest message
(§18) rather than a blank screen. `MediaUnavailable` /
`MediaUnavailableReason::{NoCaptureBackend,NoEncoder}` (ADR 0024) already
exists for exactly this shape of problem — a host that cannot produce a
picture says so on both screens instead of leaving the guest waiting. It is
**not a clean fit for Crostini specifically**, though: ADR 0024's mechanism
fires when the capture backend fails to *start* (no display, no portal
grant). On Crostini, `capture-x11`/`capture-portal` would very plausibly
*start successfully* against Xwayland's own rootless surface and report a
picture — just the wrong one, the container's own composited output rather
than the ChromeOS desktop the operator thinks they are sharing. Recognizing
that specific case (running inside a Crostini container at all, versus a
real Linux desktop) is new detection work, not a rewire of an existing
switch, and this batch's file list for Task 2 is `docs/` only — no UI code
is included in what was authorized here. That is flagged as a follow-up,
not attempted in this session.

### Guest role: expected to work, unverified

Nothing in Sommelier's documented model prevents an ordinary windowed Linux
GUI app from running as a normal client — this is exactly the case Crostini
is built for (running Linux desktop apps in a window). The guest role only
ever needs its own window, normal input events landing on it, and clipboard
text — all three are explicitly listed as things Sommelier forwards.
**Expected: works**, in the same sense every other GTK/Wayland Linux app
works under Crostini. Not run on a device.

### Audio: genuinely open

ChromeOS has bridged Crostini audio to its own CRAS server via PulseAudio
since ChromeOS 74 (playback) and 79 (capture) — well documented. Lumepeer's
Linux audio backend (ADR 0039) is **PipeWire**, not PulseAudio, talking
directly to a PipeWire graph (`STREAM_CAPTURE_SINK` on the monitor of the
default sink). Whether a Crostini container's guest tools expose a native
PipeWire graph backed by that CRAS bridge, or only a PulseAudio-compatible
socket, was not something this session could pin down to a primary source
with confidence — general reporting on `cros-container-guest-tools`
mentions PipeWire packaging work for the container side, but not
specifically whether it presents as a real PipeWire server Lumepeer's
`audio-capture-pipewire`/`playout::linux_pipewire` code would find and
attach to. **This is left as an open question, not a guess either way.**

### Input: expected to work, unverified

Ordinary keyboard/pointer events reaching a container's own window are the
core, oldest-supported Crostini use case; nothing found suggests a
restriction here beyond what Sommelier's normal forwarding already covers.
Not run on a device.

## FreeBSD

See [ADR 0068](adr/0068-freebsd-is-not-a-supported-platform.md) — decision:
not a supported platform, with the reasoning (upstream Tauri/wry has no
committed FreeBSD support, and the `webkit2gtk-4.1` port has a recent,
real, unresolved build failure in FreeBSD's own package infrastructure)
recorded there in full. No hardware was available to attempt a build either
way.
