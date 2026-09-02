# Release checklist (design doc §21)

Each line of §21 mapped to what actually enforces it, so "the checklist
passed" means something more specific than someone reading the design doc and
agreeing. Update this table when a line moves from manual to automated, not
the other way around.

| §21 line                                                                                                                             | Enforced by                                                                                                                                                                                                                                                                                                  | Status                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  |
|--------------------------------------------------------------------------------------------------------------------------------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| Protocol golden vectors, fuzz corpus and interop tests pass                                                                          | `fuzz` job (`cargo check --manifest-path tests/fuzz/Cargo.toml`), `tests/integration/tests/protocol_golden.rs` replaying the corpus, `tests/interop/golden_vectors.txt`                                                                                                                                      | Automated, runs on every push/PR                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                        |
| `cargo audit`, `cargo deny`, SBOM and signed artifact verification pass in release CI                                                | `supply-chain` job: `cargo audit`, `cargo deny check`, `cargo cyclonedx` uploaded as the `sbom` artifact; `apps/desktop/src-tauri/tauri.conf.json`'s `plugins.updater` block (Ed25519 keypair, `pubkey` set)                                                                                                 | Audit/deny/SBOM automated. Updater-artifact signing is configured (bundle-time): `.github/workflows/release.yml`'s `build` job passes `TAURI_SIGNING_PRIVATE_KEY`/`TAURI_SIGNING_PRIVATE_KEY_PASSWORD` from repo secrets to `tauri-action`, which signs artifacts with the Ed25519 key — **the secrets themselves still have to be created and added under repo Settings → Secrets before the first release run, or every platform build fails.** Runtime verification is now wired: `tauri-plugin-updater` is a dependency of `apps/desktop/src-tauri` and registered with `.plugin(...)` in `main.rs`, with `updater:default` granted in `capabilities/main.json`, so downloaded artifacts get checked against `pubkey` before install. There **is** now something to verify against: `bundle.createUpdaterArtifacts` is `true`, `.github/workflows/release.yml` passes `includeUpdaterJson: true` so each matrix row merges its platform entry into the release's `latest.json`, and the client resolves its endpoint per channel at check time from `config/default.toml`'s `[updates]` section (ADR 0042). `tauri.conf.json` keeps `endpoints: []` on purpose — a static list could not follow the channel. OS-level installer code signing (Windows Authenticode, Apple Developer ID + notarization) is **not attempted** — paid vendor relationships and, for Apple, hardware this repo lacks. Blocks an actual cross-platform release; see ADR 0009. |
| No `unsafe` without `SAFETY:` rationale, test and owner review; no `unwrap`/`expect` on network, parse, keystore or permission paths | `unsafe_code = "deny"` at the workspace level (`Cargo.toml`), with narrow, commented `#![allow(unsafe_code)]` only in `crates/media` (the shared-memory ring of §11.3, the platform backends, `sas.rs`) and `crates/service` (the service dispatcher and its DACL'd pipe, ADR 0043); `clippy::unwrap_used`/`clippy::expect_used` at `"warn"` workspace-wide, promoted to a hard failure by `RUSTFLAGS: -D warnings` in `build` | Automated (clippy + lint config), still relies on `#[allow(...)]` review discipline for the intentional exceptions inside test files                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| User-visible consent, active-session indicator and immediate revoke work on every claimed host OS                                    | `crates/core`'s `SessionManager` (§8) plus the UI screens of phase 6: `apps/desktop/src/consent-dialog.ts`/`session-status.ts`, localized in `en`/`ar` (`apps/desktop/src/i18n.ts`), audited by `apps/desktop/src/accessibility.test.ts` (axe-core, 8/8 passing) and `apps/desktop/src/keyboard-nav.test.ts` | Implemented and tested for Linux/X11 (see ADR 0007/0008 for platform scope). Consent/revoke logic tested by `tests/integration/tests/consent_cycle.rs`, `error_matrix.rs`; the UI layer's accessibility and keyboard reachability are tested under jsdom, not a real browser — see ADR 0009 for what that does and does not cover                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| Privacy review confirms logs/telemetry carry no secret or content data                                                               | `crates/core`/`crates/net` structured logging follows §15's field allowlist by construction (no `NodeId`, ticket, IP, clipboard, filename, token or keystroke fields exist on the log call sites)                                                                                                            | Manual review, no automated grep-for-secrets gate yet                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
| Security review confirms no unattended/hidden control                                                                                | `SessionManager` requires an explicit `ConsentGrant` before any capture or input path opens (§8, enforced in `crates/core` and exercised by `tests/integration/tests/consent_cycle.rs`)                                                                                                                      | Manual review before each release; the invariant itself is tested                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| Concurrent guest limits (§8.2, §14) tested for Trial/Pro/Team, including the boundary                                                | `tests/integration/tests/limits.rs`                                                                                                                                                                                                                                                                          | Automated                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| `MAX_PENDING_CONSENTS` queue overflow tested (§8.1, §14)                                                                             | `tests/integration/tests/limits.rs`, `tests/integration/tests/error_matrix.rs`                                                                                                                                                                                                                               | Automated                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| Third-party penetration test (§19 phase 6)                                                                                           | Not run — no independent human tester or vendor engagement exists for this repository. Task 7's security-review pass over the codebase substitutes for it, imperfectly; see ADR 0009's penetration-test decision and "Security review outcome" section                                                       | **Not equivalent to a pentest, tracked as an open gap.** Findings recorded in ADR 0009's "Security review outcome" section (Task 7 complete, no high/critical findings)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |

## Performance and resource budgets (§15, referenced by §21 via §19 phase 5)

`ci/resource-budget.yml` holds the thresholds. `tests/integration/tests/resource_budget.rs`
measures the sandboxed decoder worker's own RSS against the `active_extra_rss_mib`
gate and is wired into the `resource-budget` CI job — currently disabled
(`if: false`) because no `[self-hosted, lumepeer-reference]` runner is
registered against this repository yet. See
`docs/adr/0008-phase-5-resource-and-security-gates.md` for why, and what it
would take to turn it on. Until then, this line of §21/§19 is **not** a real
release gate: it is tooling that runs the moment the hardware exists.

## Release pipeline

`.github/workflows/release.yml` builds and publishes on every push to
`master`: `guard` skips the workflow's own version-bump commit, `version`
computes the next patch tag from the highest existing `v*` tag (or takes the
bump type from `workflow_dispatch`, or the tag as-is on a manual `v*` push),
writes it into `Cargo.toml` / `apps/desktop/package.json` /
`apps/desktop/src-tauri/tauri.conf.json`, commits as `github-actions[bot]` and
pushes the commit + tag with the default `GITHUB_TOKEN` (no PAT needed —
GitHub does not let that token's own pushes retrigger workflows, so this
can't loop). `build` then runs the platform matrix — Linux amd64/arm64
(`.deb` + `.rpm`, `tauri.conf.json`'s `bundle.targets = "all"`), Windows
amd64/arm64 (NSIS + MSI) and macOS arm64 (`.dmg`) — each publishing straight
to that tag's GitHub Release via `tauri-action`. `install.sh` / `install.ps1`
at the repo root pull the matching asset from the latest (or a pinned)
release and install it with the platform's native package manager.

Windows/Linux ARM64 build **natively** on GitHub's `windows-11-arm` /
`ubuntu-24.04-arm` hosted runners, not cross-compiled, so no extra linker
setup is needed for those legs.


## Updates and autostart (ADR 0042)

Neither has an automated gate: both are properties of an *installed* client,
and this repository has no runner that installs one. These are the manual
steps, and what "checked" means for each.

### Before the first signed release

- [ ] `TAURI_SIGNING_PRIVATE_KEY` and `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`
      exist under repo Settings → Secrets, and the private key is the pair of
      the `pubkey` in `apps/desktop/src-tauri/tauri.conf.json`. Without them
      every platform build fails; with the *wrong* pair every build succeeds
      and every client refuses the update, which is the more expensive
      mistake. The private key lives only in the secret store — never in the
      repository, never in a config file.

### Every release

- [ ] The release carries `latest.json` alongside the bundles, and its
      `platforms` map holds an entry per built target with a `signature`.
- [ ] Install the **previous** version from its own release, then run
      "Check for updates" in the client: it offers the new version, installs
      it, and the restarted client reports the new version.
- [ ] Deliberately break it once per key rotation: edit the `signature` of one
      platform entry in a copy of `latest.json`, point a test client at it
      (`[updates] manifest_base_url`), and confirm the install is **refused**.
      A client that installs an artifact whose signature does not verify is a
      release blocker, not a bug report. There must be no fallback to an
      unsigned artifact on any network error.
- [ ] Update **over** an installed copy on each platform, not only a clean
      install: the previous version's settings, keystore entries and audit log
      survive it.

### Beta channel

- [ ] A run dispatched with `channel = beta` marks the GitHub release as a
      prerelease and republishes the manifest to the rolling `beta` release.
- [ ] A client left on `channel = "stable"` is **not** offered that build.
      This is the check that matters: GitHub's `/releases/latest` redirect
      skips prereleases, and if it ever stops doing so, stable clients start
      taking betas silently.

### Autostart

- [ ] Turning the switch on creates the entry, on each platform:
      `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` (Windows),
      `~/Library/LaunchAgents/io.insigmo.lumepeer.plist` (macOS),
      `~/.config/autostart/io.insigmo.lumepeer.desktop` (Linux).
- [ ] Signing out and back in starts the client — and it starts **waiting for
      consent**, with no session and no grant. An autostarted client that
      admits anyone is the failure this feature must not have.
- [ ] Turning the switch off **removes** the entry. Not disabled, not blanked:
      absent.
- [ ] Uninstalling removes the entry too, on each platform.

### Installers

- [ ] Install and uninstall on all three platforms.
- [ ] Uninstall leaves no startup entry behind (above), and says what it does
      with the per-user data directory rather than silently keeping or
      silently deleting the audit log and recordings.

### The Ctrl+Alt+Del helper service (ADR 0043)

No automated gate, and deliberately none: installing it registers a
`LocalSystem` service on whatever machine runs the check.

- [ ] `lumepeer-service.exe` is in the installed directory, next to
      `lumepeer-desktop.exe`. Without it the panel shows nothing at all, which
      looks the same as "not supported here".
- [ ] Install from the panel: Windows raises its administrator prompt, and
      afterwards `sc query LumepeerHelper` reports `RUNNING`.
- [ ] With the helper running and the client started **unelevated**, the guest
      toolbar's Ctrl+Alt+Del reaches the host's secure desktop. This is the
      whole reason the service exists; if it does not work here it is not a bug
      report, it is the feature missing.
- [ ] Stop the service (`sc stop LumepeerHelper`) and press it again: the
      client falls back to its in-process path and answers honestly — a
      `SasAck(false)` on an unelevated client, not a silent success.
- [ ] Remove from the same panel: the prompt appears, the service is gone from
      `services.msc`, and the app keeps working.
- [ ] Uninstalling the app removes the service too, or says it did not.
- [ ] The pipe is not reachable from a non-interactive logon. Check it once per
      release with a scheduled task running as a different user: opening
      `\\.\pipe\lumepeer-service` must fail with access denied, not connect.

### Full local control: elevated client and secure-desktop input (ADR 0057)

No automated gate: these need a real elevated app, a live UAC prompt, and a
second machine. All manual, once per release.

- [ ] The client now requests administrator: launching `lumepeer-desktop.exe`
      raises Windows' own UAC prompt every time. With a guest holding `input`,
      open `services.msc` (or `regedit`) on the host and have the guest click a
      control inside it — the click lands. Before ADR 0057 the same click was
      dropped by UIPI, so this is the elevated-window half of the feature.
- [ ] `secure_desktop_input` is off for every role: connect a guest at
      **full control** and confirm the "Allow controlling the admin prompt"
      switch starts off. No role turns it on.
- [ ] With the helper installed, the switch on, and a UAC prompt up on the
      host: the guest clicks `Yes`/`No` on the prompt and it is answered. This
      is the whole secure-desktop-input feature; the worker runs as
      `LocalSystem` on `Winlogon` for the one event, then exits.
- [ ] Turn the switch off mid-prompt: the guest's next click on the secure
      desktop is a no-op (the actor re-reads the grant per event). Turn it back
      on: clicks land again.
- [ ] With the helper **not** installed, or the switch off, the guest sees ADR
      0056's honest "respond to it there" message and the picture, and nothing
      the guest does reaches the prompt — the ADR 0011/0056 fallback is intact.
