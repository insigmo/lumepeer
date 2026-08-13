# Release checklist (design doc §21)

Each line of §21 mapped to what actually enforces it, so "the checklist
passed" means something more specific than someone reading the design doc and
agreeing. Update this table when a line moves from manual to automated, not
the other way around.

| §21 line | Enforced by | Status |
|---|---|---|
| Protocol golden vectors, fuzz corpus and interop tests pass | `fuzz` job (`cargo check --manifest-path tests/fuzz/Cargo.toml`), `tests/integration/tests/protocol_golden.rs` replaying the corpus, `tests/interop/golden_vectors.txt` | Automated, runs on every push/PR |
| `cargo audit`, `cargo deny`, SBOM and signed artifact verification pass in release CI | `supply-chain` job: `cargo audit`, `cargo deny check`, `cargo cyclonedx` uploaded as the `sbom` artifact | Audit/deny/SBOM automated. Signed artifact verification is **not wired up**: `tauri.conf.json` has no bundle signing key yet, so there is nothing to verify. Blocks an actual release, tracked for phase 6. |
| No `unsafe` without `SAFETY:` rationale, test and owner review; no `unwrap`/`expect` on network, parse, keystore or permission paths | `unsafe_code = "deny"` at the workspace level (`Cargo.toml`), with narrow, commented `#![allow(unsafe_code)]` only in `crates/media`'s shared-memory ring (§11.3); `clippy::unwrap_used`/`clippy::expect_used` at `"warn"` workspace-wide, promoted to a hard failure by `RUSTFLAGS: -D warnings` in `build` | Automated (clippy + lint config), still relies on `#[allow(...)]` review discipline for the intentional exceptions inside test files |
| User-visible consent, active-session indicator and immediate revoke work on every claimed host OS | `crates/core`'s `SessionManager` (§8) plus the UI screens of phase 6 | **Not automated, not fully implemented.** Consent/revoke logic exists and is tested (`tests/integration/tests/consent_cycle.rs`, `error_matrix.rs`); the user-visible screen is phase 6 work, see ADR 0007/0008 for platform scope |
| Privacy review confirms logs/telemetry carry no secret or content data | `crates/core`/`crates/net` structured logging follows §15's field allowlist by construction (no `NodeId`, ticket, IP, clipboard, filename, token or keystroke fields exist on the log call sites) | Manual review, no automated grep-for-secrets gate yet |
| Security review confirms no unattended/hidden control | `SessionManager` requires an explicit `ConsentGrant` before any capture or input path opens (§8, enforced in `crates/core` and exercised by `tests/integration/tests/consent_cycle.rs`) | Manual review before each release; the invariant itself is tested |
| Concurrent guest limits (§8.2, §14) tested for Trial/Pro/Team, including the boundary | `tests/integration/tests/limits.rs` | Automated |
| `MAX_PENDING_CONSENTS` queue overflow tested (§8.1, §14) | `tests/integration/tests/limits.rs`, `tests/integration/tests/error_matrix.rs` | Automated |

## Performance and resource budgets (§15, referenced by §21 via §19 phase 5)

`ci/resource-budget.yml` holds the thresholds. `tests/integration/tests/resource_budget.rs`
measures the sandboxed decoder worker's own RSS against the `active_extra_rss_mib`
gate and is wired into the `resource-budget` CI job — currently disabled
(`if: false`) because no `[self-hosted, lumepeer-reference]` runner is
registered against this repository yet. See
`docs/adr/0008-phase-5-resource-and-security-gates.md` for why, and what it
would take to turn it on. Until then, this line of §21/§19 is **not** a real
release gate: it is tooling that runs the moment the hardware exists.
