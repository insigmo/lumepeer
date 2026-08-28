# ADR 0042 — Updates have somewhere to come from, and autostart stops at the user

Status: accepted
Date: 2026-08-28

## Context

Two gaps, related only in that both are about what an *installed* client does
outside a session.

**Nothing to update from.** `tauri-plugin-updater` has been a dependency since
phase 6, `tauri.conf.json` has carried an Ed25519 public key since then, and
`.github/workflows/release.yml` has been handing `TAURI_SIGNING_PRIVATE_KEY` to
`tauri-action`. But `bundle.createUpdaterArtifacts` was `false`, so no manifest
was ever produced, and `plugins.updater.endpoints` was `[]`, so nothing would
have been fetched if one had been. A verifier, a key and a signature, with no
manifest and no URL: every part of the mechanism except the part that makes it
run.

**No way to start with the session.** A remote-access tool that has to be
launched by hand before it can be reached is a tool for people already at the
machine. `crates/media/src/sas.rs` records the other half of the same gap:
`SendSAS` works from a service in session 0 or from an elevated process, and
this app ships as neither by default.

That second gap is where the design has to be careful. Starting with the
session, starting before login, and admitting a guest without a human are three
separate things, and software that blurs them is how remote-access tools become
the thing security vendors detect. This ADR does the first. The second is
`docs/tasks/14-release-infrastructure.md` task 4 and is not done here. The
third is ADR 0033 and is switched on separately.

## Decisions

### 1. The manifest is a GitHub release asset, and the endpoint follows the channel

`createUpdaterArtifacts` becomes `true`, and `release.yml` passes
`includeUpdaterJson: true` so `tauri-action` emits `latest.json` and merges
each matrix row's platform entry into it on the same release.

`tauri.conf.json` keeps `endpoints: []`. That is not an oversight: a static
list in the bundle cannot follow a per-machine channel setting, so the endpoint
is resolved at check time from `config/default.toml` and passed to
`updater_builder().endpoints(...)`. The public key stays in the bundle, where
it belongs — it is the thing that must not be configurable.

### 2. Two channels, two URLs, and the stable one cannot see a beta

`[updates] channel` in `config/default.toml`, `stable` by default, next to
`[network]` and `[logging]` rather than compiled in — the whole point of a beta
channel is that a person can move one machine onto it without a special build.

- stable → `…/releases/latest/download/latest.json`
- beta → `…/releases/download/beta/beta.json`

The asymmetry is deliberate. GitHub's `/releases/latest` redirect skips
prereleases, so marking a beta release as a prerelease means a stable client is
never *shown* it, rather than being asked not to take it. There is no
equivalent "newest including prereleases" URL, so the beta manifest is copied
onto a rolling `beta` release whose single asset is always the newest
prerelease manifest.

`[updates] manifest_base_url` lets an operator who builds and distributes
lumepeer themselves point clients at their own server, the way `relay_url`
already lets them run their own relay. It must be `https`; a base URL that is
not is treated as no configuration at all. The artifact's signature is what
actually gates the install, but there is no reason to read the manifest over a
channel anyone on the path can rewrite.

### 3. Checking is a press, and installing is a second press

No background timer, no silent install. This process can be in the middle of
someone else's remote session, and an update that restarted it on its own would
end that session without anyone deciding to. `update_check` asks and reports;
`update_install` downloads and installs; the client says to restart rather than
restarting itself.

The signature check is `tauri-plugin-updater`'s, against the bundled public
key, before anything is written. There is no code path in `update_install` that
skips it and there must never be one: a network error is a failed update, never
a reason to accept an unsigned artifact.

### 4. Autostart is per-user, and off removes the entry

| Platform | Mechanism |
| --- | --- |
| Windows | `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` |
| macOS | `~/Library/LaunchAgents/io.insigmo.lumepeer.plist` (an agent, not a daemon) |
| Linux | `~/.config/autostart/io.insigmo.lumepeer.desktop` |

`HKLM`, `/Library/LaunchDaemons` and a systemd system unit are all deliberately
absent. Those start the app before anybody signs in, as a different user, and
that is not something a checkbox in a settings panel may arrange.

Two rules the implementation exists to keep:

- **Off removes the entry.** The registry value is deleted, not blanked; the
  plist and the `.desktop` file are unlinked, not left with a disabled flag.
  Autostart that cannot be undone from the app's own settings is the line
  between software and unwanted software.
- **Autostart permits nothing.** The app comes up and waits for consent exactly
  as it does when a person launches it. No session, no grant, no admission.
  Permanent admission is ADR 0033 and is turned on separately.

The toggle reads the real mechanism on every render rather than remembering
what it last wrote — the user may have removed the entry by hand — and a
refused write springs the switch back rather than showing what the user wanted.

### 5. `winreg` for the Windows arm

Added as a Windows-only dependency of `lumepeer-desktop`. Not named in §5; it
was already in the tree as a transitive build dependency of `tauri-winres`, and
the alternative — shelling out to `reg.exe` from a security-sensitive process —
is worse than a small crate. The macOS and Linux arms are plain file writes and
need nothing.

## Consequences

- Every release now carries `latest.json`. A release built without the signing
  secrets fails outright rather than shipping unsigned bundles, which is the
  correct direction for that failure.
- A wrong key pair is the expensive mistake: every build succeeds and every
  client refuses every update. `docs/release-checklist.md` names this
  explicitly, along with the deliberate corrupt-signature check.
- The beta channel exists on the client before there is a prerelease pipeline
  to feed it. A `workflow_dispatch` with `channel = beta` marks the release as
  a prerelease and republishes the manifest; beta version numbering is still an
  ordinary patch bump, so a beta and a stable release are distinguishable only
  by the prerelease flag. That is enough for the channel to be honest and not
  enough to run a real beta programme.
- Updates are checked only when someone presses the button. A host nobody looks
  at will not update itself — accepted deliberately here, and the thing to
  revisit when unattended hosts become the common case.
- The app can now arrange to start with the session, which makes what it does
  at startup matter more. It waits for consent. Every future change to startup
  behaviour has to keep that true.

## Verification

`apps/desktop/src/system-settings.test.ts` covers the panel: that the toggle
reads the machine rather than a remembered value, that a refused write does not
leave the switch claiming autostart is on, that a check never installs, and
that a failed install reports failure rather than a new version.
`autostart.rs`'s own tests cover the refusals that do not need a real registry.

The rest is manual and is written down in `docs/release-checklist.md` rather
than left in someone's head: the end-to-end previous-version → new-version
install, the deliberate corrupt-signature refusal, the stable-client-never-sees-
a-beta check, and that uninstalling removes the startup entry. None of it has
run yet — this repository has no runner that installs a client.
