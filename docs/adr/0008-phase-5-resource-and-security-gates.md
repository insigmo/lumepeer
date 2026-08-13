# ADR 0008 — Phase 5 scope: resource and security gates

Status: accepted
Date: 2026-08-13

## Context

§19 phase 5 asks for CI jobs enforcing §15 (performance/resource budgets) and
the release checklist (§21), done when the release build passes every §15
threshold on the reference hardware, `cargo audit`/`cargo deny` are clean, the
codec sandbox runs as a separate process, and an SBOM is generated.

This was done on the same single Linux/X11 machine as phases 2 and 4, with no
self-hosted runner pinned to the reference hardware §16.2 describes (4 vCPU,
8 GiB RAM, no dGPU, fixed config). §16.2 is explicit that this runner is
infrastructure the project has to own; it is not something a generic
GitHub-hosted runner can stand in for, because the whole point of the gate is
a fixed baseline to measure regression against, and GitHub-hosted runner
hardware is neither fixed nor documented.

## Decisions

**`cargo audit`/`cargo deny` were already wired (phase 0/3); nothing new
here.** The `supply-chain` job predates phase 5. Phase 5 adds an SBOM step to
that same job rather than a new one, since they share the "supply chain
posture" concern and the same trigger (every push/PR).

**SBOM: `cargo-cyclonedx`, JSON, one file per workspace member.** CycloneDX
over SPDX because it has first-class Rust/Cargo tooling; JSON over XML because
every consumer of an SBOM in this project's threat model (§3) is a script, not
a person. `--describe crate` (the default) rather than per-binary: a
dependency vulnerability matters at the crate level regardless of which binary
pulls it in. The files are build artifacts (`.gitignore`), uploaded from CI,
never committed.

**The codec sandbox requirement was already met by phase 2.**
`DecoderHandle::spawn_with` (`crates/media/src/decode/mod.rs`) launches
`lumepeer-decoder-worker` with `std::process::Command`, a real OS process, not
an in-process abstraction. Phase 5 adds `DecoderHandle::pid()` so that process
can be sampled from outside without reaching into the sandbox through any new
channel.

**Resource budgets: what this repo can measure without the reference runner,
and no more.** `tests/integration/tests/resource_budget.rs` drives a real
capture → encode → sandboxed-decode loop and samples the decoder worker's own
`VmRSS` via `/proc/<pid>/status` (Linux only, same scope cut as ADR 0007)
against the `active_extra_rss_mib` gate in `ci/resource-budget.yml`. This is a
real regression test — if the worker alone blows the whole-app active-extra
budget, that is a genuine failure — but it is not the phase 5 acceptance
criterion, which is the *whole* release build against *every* §15 row on the
reference hardware. Producing that needs:

- The actual reference-hardware machine, registered as a `self-hosted,
  lumepeer-reference` runner.
- A built, running desktop app (`apps/desktop`) to measure idle RSS/CPU and
  glass-to-glass/input-RTT latency against, which in turn needs a real
  display and a real peer to connect to, not the synthetic in-process
  capturer the current test uses.

Rather than fake that with numbers from this development machine and call
phase 5 done, the `resource-budget` CI job is wired into
`.github/workflows/ci.yml` with `if: false`: the job definition, the runner
label and the command it will run all exist, so turning it on is flipping one
line once the hardware is registered, not writing new CI.

**The release checklist gets one document, not scattered assertions.**
`docs/release-checklist.md` maps every line of §21 to what enforces it today,
distinguishing "automated" from "manual, not yet automated" from "not yet
implemented" rather than letting the design doc's checklist read as more done
than it is. Signed artifact verification is the one line with genuinely
nothing behind it yet: `tauri.conf.json` has no bundle signing key configured,
which is phase 6 (signed builds) work, not phase 5.

## Consequences

Phase 5 is complete for what does not require the reference hardware:
`cargo audit`/`cargo deny` clean, SBOM generated per push, the codec sandbox
confirmed and now independently sampleable, a regression test for one
component of the active-extra RSS budget, and an honest map from §21 to its
enforcement. It is not complete for the actual acceptance criterion of §19 —
full release-build compliance with every §15 row on reference hardware — which
needs infrastructure this environment does not have. `resource-budget` in
`ci.yml` and `docs/release-checklist.md`'s last section are where that gap is
recorded, not silently.
