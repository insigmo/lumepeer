# ADR 0017 — A declared glibc floor for Linux packages, enforced at bundle time

Status: accepted
Date: 2026-08-19

## Context

A `.deb` built by `task build:client:linux-amd64` on the Linux build VM
(Debian 13 trixie, glibc 2.41) installed cleanly on an Ubuntu machine and
then refused to start:

```
lumepeer-desktop: /lib/x86_64-linux-gnu/libc.so.6: version `GLIBC_2.39' not
found (required by lumepeer-desktop)
```

Inspecting that binary shows the whole story: every glibc symbol it needs
sits at `GLIBC_2.34` or below — comfortably inside Ubuntu 22.04 (2.35) and
Debian 12 (2.36) — except two, both of them *weak* undefined references:

```
446: ... FUNC WEAK DEFAULT UND pidfd_spawnp@GLIBC_2.39
475: ... FUNC WEAK DEFAULT UND pidfd_getpid@GLIBC_2.39
```

Those come from Rust's `std`, which weak-links glibc 2.39's pidfd spawn
helpers and falls back to `fork`/`exec` when they are absent. The program
is entirely prepared for them to be missing. `ld.so` is not: it validates
the whole `.gnu.version_r` requirement list against each library *before*
any symbol is bound, and a missing version there is fatal regardless of
whether anything non-weak actually needs it. So a build host one glibc
release newer than the target machine produces a package that cannot start
there, for symbols the program never calls.

This is a property of the build host, not of the code, and it is invisible
locally — the VM that produced the package runs it fine. `release.yml`
builds linux-amd64 on `ubuntu-22.04` and so never had the problem; the
local build path, the macOS/Windows-driven `task build`, and the
`ubuntu-24.04-arm` runner used for linux-arm64 all did.

Three ways out were considered:

- **Build Linux packages in an old-distro container.** The standard answer,
  and what CI effectively already does. Rejected for the local path: the
  build VM has no Docker/Podman, no passwordless sudo to install one, 3 GB
  of RAM and ~5 GB of free disk — a second full `target/` in a container is
  not affordable there.
- **Pin the glibc ABI at link time (`cargo-zigbuild`, a 2.35 sysroot).**
  Rejected as disproportionate: it changes the linker and the C toolchain
  used for `openh264-sys2` for every Linux build, to fix two weak symbols,
  and `tauri build`'s output-path handling does not survive zigbuild's
  `<triple>.<glibc>` target syntax.
- **Drop the unnecessary version requirement after linking** — what
  `patchelf --clear-symbol-version` does. Chosen.

## Decision

Lumepeer declares a supported glibc floor of **2.35** (Ubuntu 22.04; Debian
12 and RHEL 9 clear it), and enforces it as a build-time invariant rather
than a hope about which machine did the build.

`ci/glibc-floor.mjs` runs from `tauri.conf.json`'s `beforeBundleCommand` —
after cargo has linked the binaries, before the bundler copies them into
the `.deb`/`.rpm`, which is the only point at which patching them still
reaches the package. For each `GLIBC_x.y` requirement above the floor it
looks at the symbols actually carrying that version index:

- **All weak** — the requirement is dropped: those symbols' `.gnu.version`
  entries become `VER_NDX_GLOBAL` (unversioned, so a machine that *does*
  have them still binds them through the default version) and the now
  unreferenced `Vernaux` record is unlinked from the library's requirement
  chain. Nothing is moved or resized; only the chain links, the entry count
  and two version indices change.
- **Any of them non-weak** — the build fails, with the library, version and
  symbol names printed. That case is a real incompatibility: the program
  would call a function the target machine does not have, and quietly
  shipping it would reproduce exactly the failure this ADR is about.

Marking the requirement `VER_FLG_WEAK` instead of removing it also boots,
and is a two-byte edit, but `ld.so` then prints `weak version 'GLIBC_2.39'
not found` to stderr on every launch on the older machine. A released
client should not do that, so the full removal is worth the extra code.

The script is plain Node with no dependencies (Node is already required for
the webview bundle) specifically so that no build host needs `patchelf`
installed, and it is a no-op wherever the artifacts are not Linux ELF64 —
Windows and macOS builds run it and patch nothing.

## Consequences

- The floor is a single named constant (`DEFAULT_FLOOR` in
  `ci/glibc-floor.mjs`), overridable per-build with `--floor` or
  `LUMEPEER_GLIBC_FLOOR`. Raising it is a deliberate act that belongs in a
  follow-up ADR, not a side effect of upgrading a build VM.
- `node ci/glibc-floor.mjs --check` reports what would be patched without
  writing, for use outside a bundle run.
- CI is unaffected in behavior (its ubuntu-22.04 amd64 build has nothing
  above the floor) but is now covered by the same check, including the
  `ubuntu-24.04-arm` linux-arm64 job, which had the identical latent
  `GLIBC_2.39` problem for arm64 users.
- What this does **not** do is make packages portable in general: the `.deb`
  still declares the dependencies of the distro that built it, and the
  system webkit2gtk/GTK stack still has to be present. The floor covers the
  C library only, which is the part that fails before `main` and produces
  no actionable message.
- The **AppImage** `bundle.targets = "all"` also emits is out of reach of
  this check, and stays tied to its build host. It embeds the host's own
  GTK/webkit stack — 236 shared objects from Debian 13 in the build that
  prompted this ADR, with `libsystemd.so.0` needing `GLIBC_2.39` and
  `libwebkit2gtk-4.1.so.0` `GLIBC_2.38`, as genuine non-weak requirements
  inside third-party libraries that nothing may patch away. The floor
  therefore holds for the `.deb`/`.rpm`, which link the target machine's
  own system libraries; hand those out, not the AppImage, unless it was
  built on a host at or below the floor.
- Verified on the build VM by rewriting a real binary's requirement to a
  `GLIBC_2.99` that no machine has: unpatched it reproduces the reported
  failure verbatim, patched it starts and reaches GTK initialization, and
  the same patch applied to the genuine `GLIBC_2.39` requirement leaves the
  client running normally on the 2.41 host.
