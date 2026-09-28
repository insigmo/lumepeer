# ADR 0128 — The Mac release is signed by one certificate and keeps its secrets in files

Status: accepted
Date: 2026-09-28

The user's reports, in that order: connecting to the installed Lumepeer on a
Mac gives no picture while the Mac asks for Screen & System Audio Recording,
although System Settings already shows Lumepeer switched on there; and later,
Lumepeer on the Mac does not start at all.

## Context

`release.yml` never signed the Mac rows. Checked on the published v0.0.109
bundles on GitHub's macOS runners:

- `x86_64`: `code object is not signed at all`.
- `aarch64`: the linker's ad-hoc signature only, whose requirement is
  `cdhash H"…"`, the hash of that one build.

Two parts of macOS tie what they allow to the code that asked, and both lost
Lumepeer on every update.

**Privacy grants.** Screen Recording and Accessibility are kept as a row naming
the bundle identifier and the code's *designated requirement*. Neither kind of
requirement above survives a rebuild, so after an update the row stayed
switched on in System Settings while macOS no longer applied it: the next
`SCShareableContent` request raised the system prompt again and failed, and the
guest got ADR 0110's "did not allow Lumepeer to record" or nothing. Switching
the row on does not help — it already *is* on.

**The login keychain.** The identity key, the unattended credentials, the audit
salt and the remembered passwords were items of the login keychain. An item
belongs to the build that wrote it unless the app carries an Apple team id.
Reproduced on the runners: v0.0.108 starts in 3 s; after updating to v0.0.109
the start stops before the first window at *"Lumepeer wants to use your
confidential information stored in 'io.insigmo.lumepeer' in your keychain. The
authenticity of 'Lumepeer' cannot be verified"*, which wants the login
password. A Deny, or a prompt behind another window, is an app that "does not
start": `spawn_actor` failed and `main` exited with a line on stderr, which a
GUI start never shows, and nothing in the log.

A self-signed certificate fixes the first and not the second: signed build A
wrote the item, signed build B (same certificate, same requirement, another
code hash) got the same prompt. The keychain wants a team id, which only an
Apple-issued certificate carries. The e2e matrix had shown on 2026-09-24 that a
grant *does* follow a self-signed requirement across rebuilds;
`e2e/matrix/deploy.sh` signs its bundle that way since then.

## Decision

1. Every Mac release row is signed by one self-signed code-signing certificate
   ("Lumepeer Code Signing", RSA-3072, valid to 2056). Its PKCS#12 and password
   are the repository secrets `MACOS_SIGNING_P12` / `MACOS_SIGNING_P12_PASSWORD`.
   The designated requirement becomes
   `identifier "io.insigmo.lumepeer" and certificate root = H"20247ba4…"` (the
   certificate's SHA-1; self-signed, it is its own root), identical for every
   release, so a grant is given once and kept across updates.
2. `ci/import-macos-signing-cert.sh` imports it into a temporary keychain and
   hands its hash to the Tauri build as `APPLE_SIGNING_IDENTITY`.
   `tauri-macos-sign` imports `APPLE_CERTIFICATE` only when the certificate's
   name starts like one of Apple's, so its own import path cannot be used.
   Tauri signs the sidecars, then the bundle, with the hardened runtime.
   The row fails when the secrets are missing, and
   `ci/check-macos-signature.sh` fails it when the finished bundle, or any
   executable in `Contents/MacOS`, is not signed by that certificate.
3. On macOS the secrets live in files: `FileKeystore` under
   `~/Library/Application Support/io.insigmo.lumepeer/keystore` (directory
   `0700`, files `0600`), keyed by a random `keystore.secret` beside them, as
   the Windows protected store of ADR 0123 is. The first start moves whatever
   the login keychain holds there, entry by entry, and writes
   `moved-from-keychain`; after that the keychain is never read. Nothing is
   deleted from the keychain, since a delete may ask as well.
4. A keychain that will not give up the identity ends the start with an error
   in the log (`main` now logs its fatal start error, not only to stderr) and
   mints nothing: a new identity would orphan every paired device. The next
   start asks again. Any other entry that cannot be moved leaves the marker
   unwritten and is asked for again next start, alone.

The certificate is not a Developer ID and the build is not notarized, so
Gatekeeper treats a downloaded `.dmg` exactly as before: the first launch goes
through System Settings > Privacy & Security > Open Anyway. Updates arrive
through the in-app updater, which carries no quarantine flag.

The private key must stay secret: anything signed with it and naming
`io.insigmo.lumepeer` would inherit every grant a user gave Lumepeer.

## Consequences

- The secrets on a Mac are now as open to the user's own programs as they are
  on Windows (Credential Manager) and Linux (Secret Service): readable by
  anything running as that user, by nobody else. The keychain's per-app list
  only ever held for a build that never changed, and it cost a password prompt
  and a start that could fail at every update.
- Once, on the first update to a release with this ADR, the keychain asks for
  each item it holds (**Allow** is enough), and Screen Recording and
  Accessibility must be allowed again — remove Lumepeer from each list with
  **−** and allow it when asked — because those rows were written for the
  unsigned build. After that no update asks for anything.
- The hardened runtime is on. Nothing Lumepeer does on macOS needs an
  exception: no Apple Events, no microphone, no `dlopen` of non-system
  libraries; `sandbox_init` in the decoder worker is unaffected.
- Replacing the certificate costs every Mac user one more re-grant. It is
  valid for thirty years for that reason.
