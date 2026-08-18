# ADR 0002 — Dependency version deviations from §5

Status: accepted
Date: 2026-08-12

## Context

§5 of the design document pins exact versions. Two of them cannot be used as
written, and one crate name in §5/§6 collides with the Rust namespace.

## Decision

| §5 says | We use | Why |
|---|---|---|
| `scap = "0.1"` | `scap = "=0.1.0-beta.1"` | Only pre-releases of `scap` exist on crates.io; `"0.1"` does not resolve. The pre-release is pinned exactly, and §5.1 already plans to replace `scap` with native bindings after the MVP. |
| `openh264 = "0.6"` | `openh264 = "0.9"` | 0.6 is long unmaintained; 0.9 is the current line. This is a software fallback decoder on an untrusted input path, so running an outdated version is the larger risk. |
| `iroh = "=1.0.2"` | unchanged | 1.0.3 exists, but §5 pins 1.0.2 and updating is a separate compatibility PR. |

Two further mechanical adaptations, neither of which changes the architecture:

- **Crate names are prefixed.** §6 names the directories `crates/core`,
  `crates/net`, `crates/media`; the directories keep those names, but the
  packages are `lumepeer-core`, `lumepeer-net`, `lumepeer-media`. A crate
  literally named `core` shadows the Rust `core` in every path and error
  message.
- **iroh 1.0 renamed `NodeId` to `EndpointId`** (`EndpointId = PublicKey`) and
  `NodeAddr` to `EndpointAddr`. `lumepeer_core::NodeId` is an alias for that
  same `PublicKey`, so the design document's vocabulary stays intact in our
  own signatures.

Additionally, `serde-big-array` was added: `postcard`/`serde` cannot derive for
`[u8; 64]` signature fields without it.

## Consequences

`scap` and `openh264` are behind the optional features `capture-scap` and
`encode-openh264` of `lumepeer-media`, off by default, so a phase 0/1 build
needs no platform SDK. Phase 2 turns them on and re-evaluates both pins.
