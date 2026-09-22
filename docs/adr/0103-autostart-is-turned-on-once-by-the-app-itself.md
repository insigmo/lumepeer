# ADR 0103 — Autostart is turned on once, by the app itself

Status: accepted
Date: 2026-09-22

Amends [ADR 0042](0042-updates-have-somewhere-to-come-from.md),
which built the autostart mechanism and left *who turns it on* to whatever each
platform's installer could manage. On Windows nothing could, so on Windows
nothing did.

## Context

ADR 0042 put a per-user autostart entry behind a switch in the settings panel:
`HKCU\…\Run` on Windows, a `launchd` agent on macOS, a freedesktop `.desktop`
file on Linux. The switch reads the real mechanism on every render, and turning
it off deletes the entry rather than blanking it.

What it did not settle is the state a fresh installation starts in. Three
different answers ended up in the tree:

| Platform | Who turned it on | Worked |
| --- | --- | --- |
| Linux (deb/rpm) | `packaging/deb-postinst.sh`, `rpm-post.sh` → `su -l "$target_user" -c 'lumepeer-desktop --enable-autostart'` | yes |
| macOS | `Autostart::reconcile_first_launch`, called from `setup_app` | yes |
| Windows | nobody | **no** |

`installer-hooks.nsh` installs the service and nothing else. The doc comment on
`reconcile_first_launch` claimed the opposite — "Windows and Linux both get
autostart from a hook that runs exactly once (the NSIS installer/uninstaller
…)" — and there is no such hook.

**Adding one would not work.** The bundle is `installMode: perMachine`: the
NSIS script runs elevated, and `HKCU` inside it is the administrator's hive,
not the hive of whoever later signs in and uses the app. That is the precise
trap `deb-postinst.sh` already avoids by dropping to `su -l "$target_user"`
before calling the CLI flag, and on Windows there is no equivalent to drop to —
at install time the person who will use the machine may not have an account on
it yet.

macOS has the opposite problem and the same conclusion: `.dmg` is a
drag-install with no install hook of any kind to write.

## Decision

**Autostart is turned on by the application, in its own process, the first time
an installed copy runs — on every platform — and exactly once.**

### 1. The app is the only thing that knows which user this is

`Autostart::reconcile_first_launch` runs from `setup_app` on every start. It is
the one place where "the current user" is a fact rather than a guess: the
process is already running as that person, with that person's `HKCU`, `HOME`
and `XDG_CONFIG_HOME`. Nothing an installer writes can be that.

So the macOS-only `#[cfg]` comes off it, the marker check and the enable become
platform-independent, and what stays macOS-only is the one thing that genuinely
is: sweeping away a login item pointing at an app that has been dragged to the
Trash, which deb/rpm's `prerm` and the NSIS uninstaller already do for the
other two.

### 2. A marker file is what makes this a default rather than a policy

"First launch" means: **the marker is absent**. If it is present, this does
nothing at all — it does not read the switch, does not write the entry, does
not touch anything. Somebody who turns the switch off is not argued with at the
next start.

That is the entire difference between "on by default" and "cannot be turned
off", and it is why the marker is written even when enabling failed: the
question the marker answers is "has a first launch happened", not "did it
succeed". A second attempt on the next start would be this app reaching for a
setting the user may have just declined.

ADR 0042's promise is untouched: the switch still reads the real mechanism,
turning it off still deletes the entry outright, and nothing here ever puts one
back.

### 3. The macOS marker path does not move

On macOS it stays `~/Library/LaunchAgents/io.insigmo.lumepeer.first-launch`,
beside the login item, where installed copies already have one. Everywhere else
it is `first-launch` under the per-user config directory
(`lumepeer_runtime::config::config_dir`) — where this app keeps the rest of its
files, because neither Windows nor Linux has a `LaunchAgents` directory and
neither has ever written a marker to keep.

Moving the macOS path would read as "this machine has never run this app" on
every installation out there, and turn autostart back on for everybody who
deliberately turned it off. That is a worse failure than the inconsistency of
two paths, and it is the reason for the inconsistency.

### 4. `--enable-autostart` and the deb/rpm hooks stay

They work, they are how a package manager arranges it before the app has ever
run, and they are the repair path for a machine that already has a marker: the
CLI flag turns the entry on without consulting one.

The deb/rpm hooks now duplicate what §1 does. That is harmless — both write the
same entry, and whichever runs first makes the other a no-op — and removing
them would change a mechanism that is verified on real machines to buy nothing.

## Consequences

- A fresh install on any platform starts with the machine, after the first
  time somebody opens it. On Windows that is new behaviour; on Linux the
  postinst hook already did it, and this makes the same thing true of a build
  installed some other way.
- **On Windows it takes one manual launch.** Install, reboot immediately, and
  the app does not come up: nothing has run yet to write `HKCU`. There is no
  version of this that does not, short of writing the wrong user's hive.
- A machine upgrading from an older build has no marker, so the next start
  turns autostart on once — including for somebody who had turned it off under
  a previous version, whose entry is gone and whose "off" was never recorded
  anywhere else. This is a one-time reversal for those users, and it is the
  cost of the marker being the only state there is.
- The settings panel is unchanged. It reads `Autostart::is_enabled` on every
  render, so it shows whatever this decision left behind rather than a default
  of its own.

## Alternatives considered

- **A per-user NSIS hook.** `installMode: perMachine` rules it out, and
  switching the bundle to `perUser` to get it would move the whole install into
  one account's profile and break the elevated helper service
  ([ADR 0043](0043-one-privileged-helper-with-one-capability.md)), which is a
  machine-wide thing.
- **An `HKLM\…\Run` entry, or a service.** That starts the app for every
  account on the machine and needs elevation to arrange. `autostart.rs` already
  refuses it for ADR 0042's reason: it is a different feature with different
  stakes, and not one a settings toggle may decide.
- **Turn it on at every start unless a "user turned this off" flag exists.**
  Same number of files on disk, opposite default on the failure path: a flag
  that fails to write means the app re-enables something the user switched off.
  An absent marker means the app enables something once, which is what it was
  going to do anyway.
- **A default of `true` in the settings panel's initial state.** That changes
  what the first frame *looks* like, not what the machine does, and would show
  a checked box on a machine with no entry — the exact staleness ADR 0042's
  read-every-time rule exists to prevent.
