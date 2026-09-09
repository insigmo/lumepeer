# ADR 0068 — FreeBSD is not a supported platform

Status: accepted
Date: 2026-09-09

Written for gap-tasks/05 (`docs/gap-tasks/05-packaging-arm-chromeos-freebsd.md`,
Task 3). No FreeBSD box or VM was available to this session; every finding
below is research against upstream sources, not a build attempted here. Where
that matters it is called out explicitly.

## Context

The release matrix (`.github/workflows/release.yml`) builds six lines today:
Linux amd64/arm64, Windows amd64/arm64, macOS arm64/amd64. FreeBSD is not
among them, and GitHub Actions has no FreeBSD-hosted runner at all — a
FreeBSD line could only ever be a self-hosted runner or a manual build, never
the same CI path the other six get. Before deciding which of those (if
either) is worth doing, three questions needed real answers rather than
assumption:

- Does `wry` (the webview crate `tauri` builds on) support FreeBSD at all?
- Does the FreeBSD ports tree actually produce a working `webkit2gtk-4.1`
  today?
- Are the other two Linux-path dependencies — `x11rb` and PipeWire — a
  problem?

**`wry`/Tauri.** Tauri's own documentation lists its supported platforms as
Linux, Windows, macOS, Android and iOS — FreeBSD is not one of them.
[tauri-apps/tauri#12735](https://github.com/tauri-apps/tauri/issues/12735)
("support development and distribution on FreeBSD") has been open since
February 2025, is unassigned, and carries no linked PR or maintainer response.
A 2022 discussion asking the same question
([tauri-apps/discussions#5117](https://github.com/orgs/tauri-apps/discussions/5117))
went unanswered beyond "we'd like this too". `wry`'s own `Cargo.toml` does gate
its `webkit2gtk`/`gtk` dependency table on `target_os` in `linux`, `dragonfly`,
`freebsd`, `openbsd`, `netbsd` together — so the crate is not hostile to
FreeBSD at the `cfg` level — but a dependency table entry is not the same
claim as "this builds and runs there, and someone checks that it keeps
doing so". Nothing upstream currently makes that claim, tests it in CI, or
tracks regressions against it.

**`webkit2gtk-4.1` on FreeBSD ports.** The port exists
(`www/webkit2-gtk`, flavor `41` = `libwebkit2gtk-4.1`, currently 2.46.6_8 per
FreshPorts) and is actively maintained by the FreeBSD GNOME team. But its
build has a real, recent, non-environmental failure on record: a December
2025 pkg-fallout report
([freebsd-gnome list](https://lists.freebsd.org/archives/freebsd-gnome/2025-December/010409.html))
for `webkit2-gtk_41-2.46.6_4` on `main-amd64-default` shows JavaScriptCore's
DFG JIT failing to compile — "no matching member function" errors passing a
`JSC::DFG::SpeculativeJIT` where a `JSC::MacroAssembler` is expected — a
genuine source-level incompatibility, not a disk-full or timeout flake (a
jail/kernel version mismatch was also flagged in the same log as a
contributing warning, but the JIT errors are a separate, real compile
failure). This is not a one-off either: WebKitGTK-on-FreeBSD has a multi-year
history of pkg-fallout reports across versions (2019's webkit-gtk2 SIGSEGV,
2025's webkit2-gtk_40 failures blocking the xfce4-desktop quarterly build,
the December 2025 report above). At the time of this research,
`www/webkit2-gtk`'s FreshPorts page shows no package information in the
official binary repository, consistent with the build not currently
succeeding there reliably.

**`x11rb`.** Pure Rust, talks the X11 protocol over a socket; nothing in it
is Linux-specific, and it needs no FreeBSD-specific bindings unless the
`allow-unsafe-code`/`libxcb`-sharing feature is turned on (Lumepeer does not
turn it on). No evidence of a FreeBSD-specific problem here.

**PipeWire.** `multimedia/pipewire` exists in the FreeBSD ports tree,
installable via `pkg install pipewire`, and is maintained
(`arrowd@FreeBSD.org`, 15 dependencies, required by 41 packages per
FreshPorts). No obvious blocker.

So two of the three questions come back clean, and the third — the one
`wry` actually depends on to put a window on screen at all — comes back
"exists, but its most recent public build attempt failed on a real compiler
incompatibility, and no working binary package is currently published for
it." A build that cannot get its webview dependency past `pkg install` (or
past building it from ports) never reaches the point of finding out whether
Lumepeer's own code has a problem.

## Decision

**FreeBSD is not a supported platform.** No release line is added for it, no
self-hosted-runner plan is adopted, and no "manual build, no release
artifacts" middle ground is claimed either — that would still be asserting
the build works, just unreleased, and nothing in this research demonstrates
that. The honest claim is narrower: Tauri upstream does not commit to
FreeBSD support, and the one native dependency in the chain that upstream
support would most need to lean on has a recent, real, unresolved build
failure in FreeBSD's own infrastructure. Nothing was attempted locally —
there is no FreeBSD box or VM available to this project — so this is a
documented refusal grounded in what was found, not a build that was tried
and abandoned partway.

This is revisitable. The two conditions that would change it: Tauri/wry
publishing an actual supported-platform claim for FreeBSD (closing or
resolving #12735 with real CI coverage), and `www/webkit2-gtk`'s `41` flavor
building cleanly and staying green in FreeBSD's own package-fallout tracking
for a sustained period. Either alone narrows the gap; both together would
make a manual-build ADR the next honest step.

## Consequences

- `.github/workflows/release.yml`'s six-row matrix is unchanged.
- Nobody is told FreeBSD works when it has not been shown to. A user running
  Lumepeer's installer scripts on FreeBSD gets no matching release asset and
  no misleading claim in `README.md` either.
- If someone attempts a manual FreeBSD build anyway, the place they will
  most likely stall first is exactly identified above: `webkit2-gtk_41`
  failing to compile against a current FreeBSD/Clang toolchain. That is
  useful to know before spending the effort, which is the point of writing
  this down instead of leaving the question unanswered.

## Alternatives considered

- **"Supported as a manual build, no release artifacts."** Rejected: this
  session verified no such build to completion (no hardware to do it on),
  and the ports evidence points at the webview dependency itself currently
  failing to build in FreeBSD's own official infrastructure. Claiming
  "manual build works" without having built it is exactly the "непроверенный
  артефакт хуже отсутствующего" mistake the batch this ADR belongs to warns
  against, just moved from the release matrix into prose.
- **Add a FreeBSD row with `continue-on-error` as a canary.** Rejected outright:
  GitHub Actions has no FreeBSD-hosted runner, so this would need a
  self-hosted one — a standing infrastructure commitment out of scope for a
  packaging documentation pass, and not something this session could stand
  up or verify.
- **Wait and say nothing.** Rejected: the batch this ADR closes out asks for
  an explicit decision, not silence; a future contributor asking "does
  FreeBSD work" deserves this research rather than having to redo it.
