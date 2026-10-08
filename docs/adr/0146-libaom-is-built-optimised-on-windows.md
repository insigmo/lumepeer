# ADR 0146 — libaom is built optimised on Windows

Status: accepted
Date: 2026-10-08

Amends [ADR 0141](0141-a-host-with-no-hardware-encoder-encodes-av1-in-software.md)
§1 (how `lumepeer-aom-sys` builds libaom). Bears on the figures behind
[ADR 0142](0142-the-software-av1-budget-is-the-frame-interval.md) and
[ADR 0143](0143-the-guest-can-pick-the-hosts-encoder.md).

## Context

The stage-1 comparison that ADR 0141 left open was run on 2026-10-08 on the
development machine (i7-11800H): the product's encoder (`codec-bench
--encoder lumepeer-aom`, the vendored libaom `lumepeer-aom-sys` builds)
against the stage-1 `aom-s10` on the recorded 1080p desktop, 900 frames paced
at 30 a second, at 2, 4, 8 and 16 Mbit/s for libaom.

The bitstreams are byte for byte the stage-1 ones at all four bitrates (md5
`eccbe76c…`, `2ede5598…`, `35cf164e…`, `0ca6dded…`). The time is not:

| | p50, ms | p95, ms | p99, ms | cores | late frames of 900 |
|---|---|---|---|---|---|
| stage-1 `aom-s10` (2026-10-01) | 5.8–6.2 | 10.2–14.5 | 15.3–18.4 | 0.56–0.71 | 0 |
| `aom-s10`, same session | 6.5–6.7 | 12.6–15.1 | 17.2–21.2 | 0.59–0.69 | 0–3 |
| product `lumepeer-aom` | **13.2–14.3** | **33.3–43.9** | 48.5–61.9 | **1.75–1.97** | **24–207** |

Same output, three times the processor time: the same code, compiled
differently. The generated build says how. `lumepeer-aom-sys` builds libaom
with the `cmake` crate (0.1.58). With MSVC and the Visual Studio generator,
which is what a Windows build and the release workflow use, that crate sets
`CMAKE_C_FLAGS_RELEASE` and `CMAKE_CXX_FLAGS_RELEASE` itself, to the flags it
gets from `cc` at opt-level 0 with every `/O` taken out. Its own source says
so: "this *overrides* things like the optimization flags, which is bad". So
CMake's `/O2 /Ob2 /DNDEBUG` were gone. In every libaom project the Release
configuration has an empty `<Optimization>`, which is MSVC's `/Od`, and no
`NDEBUG`, so libaom's asserts are compiled in. The stage-1 build of the same
release has `MaxSpeed`, `AnySuitable` and `NDEBUG`.

The assembly was assembled and the AVX2 intrinsics were compiled with
`/arch:AVX2`, so `check_configuration` (ADR 0141) passed. It reads
`aom_config.h`, which says which kernels exist, not how the C around them was
compiled.

On Linux the crate sets only `CMAKE_C_FLAGS`, and CMake's `-O3 -DNDEBUG`
stay. That is why stage 2, which was done on a Linux VM, found the product
and the stage-1 shim equal in time.

Every Windows release that carries `encode-aom`, from v0.0.129 to v0.0.135,
ships this libaom. Every software AV1 figure taken on Windows since then
measured the unoptimised encoder: beta's own measurement (26.3, 27.2 and
28.5 ms at p95 against `openh264`'s 26.3–27.8 ms, ADR 0142 and 0143), and the
18–21 ms of the development machine. ADR 0142 put beta's 23–25 ms on the
synthetic screen being harder than the stage-1 recording, and ADR 0143 built
on "on beta the two take about the same time". Both were reading this.

## Decision

1. On MSVC targets `build.rs` passes `/O2 /Ob2 /DNDEBUG` to libaom's C and
   C++ through the `cmake` crate's `cflag` and `cxxflag`. Those lead the list
   it writes into `CMAKE_<LANG>_FLAGS_RELEASE`, so they survive the
   override. The CRT switch the crate adds is kept.
2. After configure, `check_optimised` reads `CMakeCache.txt` and stops the
   build unless `CMAKE_C_FLAGS_RELEASE` holds an optimisation flag (`/O2`,
   `-O2` or `-O3`) and `NDEBUG`. This is the same kind of stop as the
   missing nasm and the missing AVX2: a software encoder that ships only
   because it is fast must not come out slow without anyone noticing.

The bitstream does not change. Checked on the same clip and bitrates as
above, the fixed build gives the same four md5s and:

| | p50, ms | p95, ms | p99, ms | cores | late frames of 900 |
|---|---|---|---|---|---|
| product `lumepeer-aom`, fixed | 6.8–6.9 | 11.4–16.0 | 17.5–21.3 | 0.59–0.73 | 0 |

The fixed Release projects show `MaxSpeed`, `AnySuitable`, `NDEBUG` and AVX2.

## Consequences

- On Windows, software AV1 costs what stage 1 measured: about half the
  frame time and a third of the processor time of the shipped encoder. That
  holds for a host's own choice (ADR 0141) and for a guest's pick (ADR 0143)
  alike.
- The start measurement of ADR 0143 compares libaom against `openh264` as
  it was measured in stage 1. On a host like beta, AV1 is no longer the
  coin flip ADR 0143 describes. beta itself now has a hardware H.264 encoder
  (ADR 0144) and does not measure; the figure for a beta-class host without
  one is in docs/research/software-av1.md.
- Releases from v0.0.129 to v0.0.135 keep the slow encoder. The next release
  replaces it.
- `opusic-sys` builds libopus with the same crate and the same generator.
  Whether its Windows build lost its optimisation too is not part of this
  decision.
