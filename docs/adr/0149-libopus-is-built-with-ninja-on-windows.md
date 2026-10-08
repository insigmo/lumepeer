# ADR 0149 — libopus is built with Ninja on Windows

Status: accepted
Date: 2026-10-08

Amends [ADR 0023](0023-phase7-catchup-decisions.md) §5 (what a build host
needs for `audio-opus`). Answers the question
[ADR 0146](0146-libaom-is-built-optimised-on-windows.md) left open about
`opusic-sys`.

## Context

`audio-opus` links libopus through `opus` 0.4.0, whose `opusic-sys` 0.7.5
builds the vendored libopus with the `cmake` crate 0.1.58. (ADR 0023 §5 names
`opus` 0.3 and `audiopus_sys`; the lock moved to 0.4.0 and `opusic-sys` on
2026-08-29, in d6dfc64.) ADR 0146 found that the `cmake` crate, with MSVC and
no generator named, picks Visual Studio's generator and writes
`CMAKE_<LANG>_FLAGS_RELEASE` itself, with every `/O` taken out. libaom lost
its optimisation that way. `opusic-sys` sets no flags of its own, so it is
exposed the same way, with one difference: its build script switches to the
Ninja generator whenever `ninja --version` runs, and with any generator named
the `cmake` crate leaves the release flags alone.

A release build of `opusic-sys` on the development machine, which had no
ninja, configured libopus with

    -G "Visual Studio 17 2022" ... -DCMAKE_C_FLAGS_RELEASE= -nologo -MD -Brepro -W0

and the Release configuration of `opus.vcxproj` has an empty
`<Optimization>` and no `NDEBUG`. The recorded `cl` command lines carry no
`/O` switch at all, so libopus was compiled at `cl`'s default, `/Od`. The
missing `NDEBUG` changes nothing here: libopus has no plain `assert`, its
`celt_assert` needs `ENABLE_ASSERTIONS`, and on MSVC its `silk_assert` is
`_ASSERTE`, which only `_DEBUG` turns on.

The release builds are different. The GitHub Windows images (windows-2022,
windows-2025 and the windows-11-arm partner image) have Ninja 1.13.2 on PATH,
so on the runners `opusic-sys` configures with Ninja and keeps CMake's
`/O2 /Ob2 /DNDEBUG`. The shipped binary agrees. The installed v0.0.118
client on this machine was built by the release workflow with the same
`opusic-sys` 0.7.5 and `cmake` 0.1.58, and predates libaom, so libopus is its
only CMake-built code. The code of each libopus object, with the bytes that
relocations fill in masked out, was searched for in its
`lumepeer-desktop.exe`:

| libopus built here | code sections found in v0.0.118 | in `lumepeer-service.exe` (no libopus) |
|---|---|---|
| without ninja (`/Od`) | 0 of 137 | 0 of 137 |
| with ninja (`/O2 /Ob2`) | 25 of 482 | 0 of 482 |
| Visual Studio's generator named (`/O2 /Ob2`) | 25 of 482 | — |

None of the unoptimised code is in the shipped binary, and some of the
optimised code is, byte for byte. That the rest differs is most likely a
different MSVC patch release on the runner (this machine has 14.44.35207);
it was not pinned down.

What the difference costs, timed with the product's own `OpusEncoder` and
`OpusDecoder` (48 kHz stereo, 20 ms frames, `Application::Audio`,
96 kbit/s) over 10 s of synthetic music-like stereo, 5 runs of 500 frames,
in alternating rounds on the i7-11800H:

| libopus | encode p50 / p95 per frame, µs | decode p50 / p95, µs | encode, share of real time |
|---|---|---|---|
| without ninja (`/Od`) | 388–417 / 454–496 | 106–113 / 127–137 | 1.9–2.0 % |
| with ninja | 125–127 / 159–160 | 31 / 39–46 | 0.63–0.64 % |
| Visual Studio's generator named | 129–138 / 163–170 | 32–33 / 38–43 | 0.63–0.67 % |

All three decode to the same samples. The unoptimised libopus takes three
times the processor time: about 2.5 % of a core for both directions instead
of 0.8 %. That is small next to video, but it is a build that is not the one
that ships, chosen by whether a tool happens to be on PATH, with nothing
saying which one was built.

Neither build script rebuilds libopus when that changes. The `cmake` crate
prints no `rerun-if` lines, and `opusic-sys` only reruns on
`ANDROID_NDK_HOME`. Putting ninja on PATH after a build left the
unoptimised libopus in place (`Finished` in 0.2 s); only
`cargo clean -p opusic-sys --release` brought the optimised one.

The other ways round the override were looked at:

- `CMAKE_GENERATOR_<target>` in `.cargo/config.toml` is read by the `cmake`
  crate, and naming the generator does keep `/O2 /Ob2 /DNDEBUG` (the third
  row above). But it has to name a Visual Studio version ("Visual Studio 17
  2022") and fails on a machine with only another one, it moves libaom off
  the generator the crate picks for it too, and local libopus would still be
  built differently from the release one.
- `CFLAGS` reaches CMake only through `cc`'s flags, from which the `cmake`
  crate drops every `/O` and `-O`. `opusic-sys` reads `CFLAGS` only to strip
  `-flto`.
- A `[patch]` of `opusic-sys` or of `cmake` is a fork. `cmake` 0.1.58 is the
  latest release, and the override is still on cmake-rs master.

## Decision

1. On Windows, libopus is built with Ninja, as the release runners already
   build it. `ninja` on PATH is a build requirement for `audio-opus` on MSVC
   targets, next to `cmake` (ADR 0023 §5) and, for `encode-aom`, `nasm`
   (ADR 0141). Nothing about `opusic-sys` or the `cmake` crate is changed.
2. `lumepeer-media`'s build script asks what `opusic-sys` asks: with
   `audio-opus` on an MSVC target, and no generator named in any variable
   the `cmake` crate reads one from, it runs `ninja --version`. If that
   fails, a release-profile build stops and says how to install ninja and
   that `cargo clean -p opusic-sys --release` is needed afterwards. A debug
   build only warns: its own code is unoptimised anyway, and nothing that is
   measured or shipped is built that way.
3. After the build, the release workflow runs `ci/check-cmake-optimised.py`
   on every row: the `CMakeCache.txt` of `opusic-sys`, and of
   `lumepeer-aom-sys` on the rows that build it, must hold an optimisation
   switch and `NDEBUG` in the C flags of the configuration it was built for.
   This reads what was built, so it also catches a cached libopus from
   before (the release workflow keeps `target/` between runs) and an
   `opusic-sys` update that picks its generator differently.

## Consequences

- Shipped binaries do not change: the release builds already had ninja.
- On a local Windows machine, `audio-opus` builds now come out like the
  release. A release build there needs ninja (`winget install
  Ninja-build.Ninja`), and a target directory built before needs `cargo
  clean -p opusic-sys --release` once, and `cargo clean -p opusic-sys` for
  debug builds. This machine has ninja 1.13.2 since 2026-10-08; beta and any
  other Windows machine that builds need the same.
- libopus also builds faster with Ninja: about 11 s instead of over a
  minute.
- If a runner image loses ninja, the release build stops at the build script
  instead of shipping an unoptimised libopus.
- Audio timings taken on a local Windows build before this measured the
  unoptimised libopus.
- The override in the `cmake` crate stays. libaom works around it with its
  own flags (ADR 0146), libopus by avoiding Visual Studio's generator. A fix
  upstream would make both unnecessary.
