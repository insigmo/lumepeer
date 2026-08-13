# ADR 0009 — Phase 6 scope: UI, accessibility and release-signing cuts

Status: accepted
Date: 2026-08-13

## Context

§19 phase 6 asks for the consent and status screens to exist as real UI,
localized in at least two languages including one RTL language, passing an
accessibility audit and a keyboard-reachability check, plus signed release
artifacts and a third-party penetration test before the project can call
itself release-ready.

This was done on the same single Linux/X11 development machine as phases 2,
4 and 5, with no browser automation harness, no paid code-signing vendor
relationship, and no independent human security tester on staff. As with ADR
0007 and ADR 0008, the rule is to verify only what this machine can actually
verify, and to record the rest as a tracked gap rather than an implicit pass.

## Decisions

**Arabic is the second locale because it is RTL, not because it is a second
translation.** `apps/desktop/src/i18n.ts`'s `Locale` type is `'en' | 'ar'`.
§19 phase 6's done-criterion is "localized in at least two languages"
alongside RTL support, and picking a second LTR language (French, German)
would satisfy the letter of "two languages" while leaving the `dir` switch
(`dirOf`, `ltr`/`rtl`) completely untested. Arabic forces every screen that
claims RTL support to actually lay out right-to-left under test, which is
the only way "supports RTL" means something more than an unused code path.

**The axe-core audit runs against jsdom, and `color-contrast`/`target-size`
are excluded because jsdom has no layout engine.** `apps/desktop/src/
accessibility.test.ts` runs `axe.run()` against markup rendered under
`jsdom`, not a real browser. This is the same cut as ADR 0007's platform
scope: jsdom parses and builds a DOM but never computes layout, so any axe
rule that needs actual pixel geometry — contrast ratio against a rendered
background, minimum tap-target size — cannot produce a meaningful result
there. Rather than let those rules silently no-op or produce a false pass,
`LAYOUT_DEPENDENT_RULES = ['color-contrast', 'target-size']` disables them
explicitly, and every other rule in axe-core's default ruleset (ARIA
correctness, label association, landmark structure, focus order semantics,
and more) runs for real against both locales, both consent-dialog states,
and both session-status states. 8/8 tests pass with zero violations found,
and no markup had to change to get there — the components were already
compliant with everything jsdom can check. A real-browser audit (Playwright
+ axe, or axe's own CI action against a built bundle) would close the
`color-contrast`/`target-size` gap; it is not wired into this repo because
that is a browser-automation dependency this environment does not carry
today, the same category of gap as the reference-hardware runner in ADR
0008.

**Keyboard reachability is checked separately from ARIA correctness, because
axe-core does not drive focus.** `apps/desktop/src/keyboard-nav.test.ts`
confirms every actionable control in the consent dialog is a real `<button>`
(not a `div` with a click handler), none is `disabled` or `tabindex="-1"`,
and — the safety-relevant part — the `Deny` action is first in DOM order and
carries `autofocus`, so a keyboard or screen-reader user who takes no action
lands on the safe default rather than on "allow full control". axe-core
checks that the markup is structurally accessible; it does not simulate
Tab and assert where focus actually goes, so this had to be a second, more
literal test.

**Updater-artifact signing closes the exact gap ADR 0008 flagged; OS-level
code signing does not, and is not attempted.** ADR 0008 recorded that
`tauri.conf.json` had no bundle signing key configured. Task 5 added the
`plugins.updater` block: an Ed25519 keypair signs update artifacts, and
`tauri-plugin-updater` verifies that signature against the `pubkey` in
`tauri.conf.json` before installing an update. `endpoints: []` because no
distribution server exists yet to serve update manifests from — the signing
mechanism is real and wired, the transport it will fetch over is not.

This is a narrower thing than OS-level code signing. Windows Authenticode
(a certificate from a CA, ~$300-600/year and an EV option that requires a
hardware token) and Apple Developer ID + notarization ($99/year plus a Mac
to run `notarytool` against, since Apple's tooling is macOS-only) are both
paid vendor relationships, and the Apple path additionally requires hardware
this repository's environment does not have access to. Neither was
attempted. This is a plain block on a real cross-platform release — an
unsigned `.exe` triggers SmartScreen friction, an unsigned `.app` is
rejected by Gatekeeper outright — and it is recorded here and in
`docs/release-checklist.md`, not silently dropped from scope.

**A third-party penetration test is out of scope for repository automation;
Task 7's security-review pass substitutes for it, imperfectly.** §19 phase
6 literally asks for a penetration test before release. Most engagement
definitions of a pentest require an independent human tester operating
outside the development process, working from a scoped rules-of-engagement
document, against a running deployed target — none of which a CI job or an
agent session inside this repository can honestly claim to be, because the
tester and the developer would be the same party. There is no vendor
relationship for one here, the same category of gap as the code-signing
certificates above.

What stands in for it: Task 7 runs a structured security-review pass over
the code and configuration in this repository (the closest thing to a
security audit achievable without an external party) and its findings are
appended below. This is explicitly *not* equivalent to a penetration test —
it reviews source and configuration, not a running attacker-reachable
deployment, and it is not independent of the team that wrote the code — but
it is the closest thing to §19's ask that this environment can produce, and
it is documented as a substitution rather than presented as satisfying the
literal requirement.

### Security review outcome

Ran 2026-08-13 against the full Tasks 1-6 diff (commits `9672602..7895a46`),
scoped to: the `plugins.updater` config, private-key handling for the
Ed25519 updater signing key, and i18n string interpolation reaching the DOM.

- **`plugins.updater` config** (`apps/desktop/src-tauri/tauri.conf.json`):
  `endpoints: []`, so there is no update-distribution host to hijack yet.
  `pubkey` is the Ed25519 *public* key (base64 minisign format) — intended
  to be committed, not a secret.
- **Private key handling**: `apps/desktop/src-tauri/updater.key` is listed
  in `.gitignore` and confirmed absent from git (`git ls-files` and
  `git check-ignore -v` both checked); not referenced by any logging
  statement in the diff.
- **i18n / DOM interpolation**: `t(locale, 'consent.request.title',
  request.peer_label)` and `session-status.ts`'s session list only ever
  pass the already-pseudonymized `peer_label` (§15) into templates, never a
  raw `NodeId`. Rendering goes through `lit-html`'s `html` tagged template
  (auto-escaping, no `unsafeHTML`/`dangerouslySetInnerHTML`-equivalent
  anywhere in the diff), so no injection path exists even if a peer label
  contained markup-like characters. `locale` itself is type-constrained to
  the `'en' | 'ar'` union by `detectLocale`, which only returns validated
  `SUPPORTED_LOCALES` members or the default — no raw `navigator.language`
  string reaches `document.documentElement.lang`/`.dir`.

**Outcome: no high/critical findings.** No medium/low findings recorded
either — the scoped areas matched their intended design with no deviation.
This review is a repo-automation substitute for the third-party penetration
test §19 phase 6 asks for, not equivalent to one: it covers only the code
introduced in Tasks 1-6, using static reading, not a tester probing the
live system.

## Consequences

Phase 6 is complete for what a single development machine without browser
automation, without paid signing vendor relationships, and without an
independent security tester can produce: two real locales with RTL actually
exercised, an axe-core audit covering every rule that does not need a layout
engine, a keyboard-reachability test with a safe default focus, and
Ed25519 updater-artifact signing wired end to end. It is not complete for
the full §19 phase 6 acceptance bar — OS-level code signing and a genuine
third-party penetration test both remain open, tracked here and in
`docs/release-checklist.md` rather than assumed done.
