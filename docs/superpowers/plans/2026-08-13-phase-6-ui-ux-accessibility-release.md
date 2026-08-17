# Phase 6: UI/UX, accessibility, release Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close out design-doc §19 phase 6 — consent/status screens localized and accessible, updater artifacts signed, and the phase-6 items of the §21 release checklist mapped to what actually enforces them, following the same "narrow the scope to what one Linux dev machine can actually verify, document the rest in an ADR" pattern as phases 4/5.

**Architecture:** Add an `i18n.ts` dictionary module to the existing lit-html webview (`apps/desktop/src`) and thread it through `consent-dialog.ts`/`session-status.ts`/`main.ts`; add a `vitest`+`jsdom`+`axe-core` test harness under `apps/desktop` (there is none yet — phase 6 introduces the frontend's first test infra); wire Tauri's Ed25519 updater-artifact signing key into `tauri.conf.json` and CI; write ADR 0009 documenting the phase-6 scope cut (no OS-level code-signing cert, no third-party penetration test — both require infrastructure/vendor relationships this repo does not have) and updating `docs/release-checklist.md` and `README.md` accordingly.

**Tech Stack:** TypeScript, lit-html 3, vitest, jsdom, `@testing-library/dom`, `axe-core`, Tauri 2 CLI (`@tauri-apps/cli signer generate`).

**Spec:** `/home/beta/Downloads/p2p-iroh-tauri-design-v12.md` §6 (repo layout), §13 (Tauri security boundary), §17.1 (`tauri-app` row: DTO validation + `axe-core` for UI), §19 phase 6, §21 release checklist. Prior scope-narrowing precedent: `docs/adr/0007-phase-4-platform-hardening-scope.md`, `docs/adr/0008-phase-5-resource-and-security-gates.md`.

## Global Constraints

- No React/Vue/Angular; UI stays vanilla TypeScript + lit-html (§5.1). New test tooling is dev-only and must not enter the shipped bundle.
- The webview never decides anything — `main.ts`'s `refresh()` loop stays the only source of truth read from `session_status`/`license_status`; i18n only changes rendered strings and `dir`, never logic.
- CSP in `tauri.conf.json` (`default-src 'self'; script-src 'self'; ...`) must keep passing; no remote fonts/scripts for RTL support.
- `capabilities/main.json` keeps exactly its current five-command allowlist (§13) — phase 6 adds no new Tauri commands.
- Every new numeric/string constant that recurs (e.g. supported locale codes) lives in one place, not copy-pasted (§2.6 spirit).
- Secrets (the updater private signing key, its password) must never be committed; `.gitignore` and CI secrets only, matching §15's "no secrets in logs/files" rule applied to build artifacts.
- Match existing repo commit style: one focused commit per phase sub-step, `git log` shows e.g. "Phase 4 notes, README and an RGBA icon" — imperative, no conventional-commit prefix.

---

## Task 1: i18n runtime (English + Arabic/RTL)

**Files:**
- Create: `apps/desktop/src/i18n.ts`
- Create: `apps/desktop/src/i18n.test.ts`
- Test: `apps/desktop/src/i18n.test.ts` (vitest, added in Task 3's harness — this task's test runs once Task 3 lands `vitest`; write it now, run it after Task 3)

**Interfaces:**
- Produces: `type Locale = 'en' | 'ar'`; `const SUPPORTED_LOCALES: readonly Locale[]`; `function detectLocale(nav: Pick<Navigator, 'language' | 'languages'>): Locale`; `function dirOf(locale: Locale): 'ltr' | 'rtl'`; `function t(locale: Locale, key: TranslationKey): string`; `type TranslationKey` — union of every string key used by `consent-dialog.ts`/`session-status.ts`.
- Consumes: nothing (leaf module).

- [ ] **Step 1: Write `i18n.ts` with the dictionary and helpers**

```typescript
// Design doc §19 phase 6: consent screen must be localized in at least two
// languages and support RTL. Arabic is chosen for the second locale precisely
// because it is RTL, not just a second LTR translation — that is the only way
// the `dir` switch actually gets exercised.

export type Locale = 'en' | 'ar';

export const SUPPORTED_LOCALES: readonly Locale[] = ['en', 'ar'];
export const DEFAULT_LOCALE: Locale = 'en';

export type TranslationKey =
  | 'consent.none.title'
  | 'consent.none.body'
  | 'consent.request.title'
  | 'consent.request.body'
  | 'consent.action.deny'
  | 'consent.action.allowView'
  | 'consent.action.allowFull'
  | 'status.notSharing'
  | 'status.heading'
  | 'status.inputOn'
  | 'status.inputOff'
  | 'status.revoke'
  | 'status.role.viewOnly'
  | 'status.role.controlLimited'
  | 'status.role.fullControl';

type Dictionary = Record<TranslationKey, string | ((arg: string) => string)>;

const en: Dictionary = {
  'consent.none.title': 'No pending requests',
  'consent.none.body': 'Nobody is asking to connect right now.',
  'consent.request.title': (peer) => `${peer} wants to connect`,
  'consent.request.body':
    'Granting view lets them see this screen. Input, clipboard, files and recording stay off until you enable each one separately.',
  'consent.action.deny': 'Deny',
  'consent.action.allowView': 'Allow view only',
  'consent.action.allowFull': 'Allow full control',
  'status.notSharing': 'Not sharing.',
  'status.heading': 'Active sessions',
  'status.inputOn': 'input on',
  'status.inputOff': 'input off',
  'status.revoke': 'Revoke',
  'status.role.viewOnly': 'view only',
  'status.role.controlLimited': 'limited control',
  'status.role.fullControl': 'full control',
};

const ar: Dictionary = {
  'consent.none.title': 'لا توجد طلبات معلّقة',
  'consent.none.body': 'لا أحد يطلب الاتصال الآن.',
  'consent.request.title': (peer) => `${peer} يريد الاتصال`,
  'consent.request.body':
    'منح المشاهدة يتيح لهم رؤية هذه الشاشة. الإدخال والحافظة والملفات والتسجيل تبقى معطّلة حتى تفعّل كلاً منها بشكل منفصل.',
  'consent.action.deny': 'رفض',
  'consent.action.allowView': 'السماح بالمشاهدة فقط',
  'consent.action.allowFull': 'السماح بالتحكم الكامل',
  'status.notSharing': 'لا مشاركة حالياً.',
  'status.heading': 'الجلسات النشطة',
  'status.inputOn': 'الإدخال مفعّل',
  'status.inputOff': 'الإدخال معطّل',
  'status.revoke': 'إلغاء',
  'status.role.viewOnly': 'مشاهدة فقط',
  'status.role.controlLimited': 'تحكم محدود',
  'status.role.fullControl': 'تحكم كامل',
};

const dictionaries: Record<Locale, Dictionary> = { en, ar };

export function dirOf(locale: Locale): 'ltr' | 'rtl' {
  return locale === 'ar' ? 'rtl' : 'ltr';
}

export function detectLocale(nav: Pick<Navigator, 'language' | 'languages'>): Locale {
  const candidates = nav.languages && nav.languages.length > 0 ? nav.languages : [nav.language];
  for (const raw of candidates) {
    const base = raw.split('-')[0]?.toLowerCase();
    if (base && (SUPPORTED_LOCALES as readonly string[]).includes(base)) {
      return base as Locale;
    }
  }
  return DEFAULT_LOCALE;
}

export function t(locale: Locale, key: TranslationKey, arg?: string): string {
  const entry = dictionaries[locale][key];
  return typeof entry === 'function' ? entry(arg ?? '') : entry;
}
```

- [ ] **Step 2: Write the failing test**

```typescript
// apps/desktop/src/i18n.test.ts
import { describe, expect, it } from 'vitest';

import { DEFAULT_LOCALE, detectLocale, dirOf, SUPPORTED_LOCALES, t } from './i18n';

describe('i18n', () => {
  it('falls back to the default locale when nothing matches', () => {
    expect(detectLocale({ language: 'fr-FR', languages: ['fr-FR'] })).toBe(DEFAULT_LOCALE);
  });

  it('picks a supported locale from navigator.languages', () => {
    expect(detectLocale({ language: 'ar-EG', languages: ['ar-EG', 'en-US'] })).toBe('ar');
  });

  it('marks Arabic as RTL and English as LTR', () => {
    expect(dirOf('en')).toBe('ltr');
    expect(dirOf('ar')).toBe('rtl');
  });

  it('has every supported locale translate every key with no leftover placeholders', () => {
    const keys: Array<Parameters<typeof t>[1]> = [
      'consent.none.title',
      'consent.action.allowFull',
      'status.role.fullControl',
    ];
    for (const locale of SUPPORTED_LOCALES) {
      for (const key of keys) {
        expect(t(locale, key)).not.toBe('');
      }
    }
  });

  it('interpolates the peer name into the request title', () => {
    expect(t('en', 'consent.request.title', 'guest-ab12')).toBe('guest-ab12 wants to connect');
  });
});
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cd apps/desktop && npx vitest run src/i18n.test.ts`
Expected: FAIL — `vitest` is not installed yet (Task 3 adds it). Note this and continue; do not install vitest here, Task 3 owns the harness. If you're executing tasks out of order, come back and run this after Task 3 Step 3.

- [ ] **Step 4: Commit**

```bash
git add apps/desktop/src/i18n.ts apps/desktop/src/i18n.test.ts
git commit -m "Phase 6: i18n dictionary for English and Arabic (RTL)"
```

---

## Task 2: Wire i18n into the consent/status screens and add a locale switch

**Files:**
- Modify: `apps/desktop/src/consent-dialog.ts`
- Modify: `apps/desktop/src/session-status.ts`
- Modify: `apps/desktop/src/main.ts`
- Modify: `apps/desktop/index.html` (root `<html dir>` needs a default; JS overrides it on load)

**Interfaces:**
- Consumes: `Locale`, `t`, `dirOf`, `detectLocale`, `SUPPORTED_LOCALES` from Task 1's `./i18n`.
- Produces: `consentDialog(request, locale)` and `sessionStatus(sessions, locale)` — both existing exports gain a required second parameter, so this task's diff to `main.ts` and any future caller must pass `locale` at every call site.

- [ ] **Step 1: Update `consent-dialog.ts` to take a `locale` and use `t()`**

```typescript
// apps/desktop/src/consent-dialog.ts
import { html, type TemplateResult } from 'lit-html';

import type { Locale } from './i18n';
import { t } from './i18n';
import type { SessionStatus } from './session-status';

export type Role = 'view_only' | 'control_limited' | 'full_control';

async function grant(peer: string, role: Role): Promise<void> {
  const { invoke } = await import('@tauri-apps/api/core');
  await invoke('session_grant', { args: { peer, role } });
}

async function revoke(peer: string): Promise<void> {
  const { invoke } = await import('@tauri-apps/api/core');
  await invoke('session_revoke', { args: { peer } });
}

export function consentDialog(
  request: SessionStatus | undefined,
  locale: Locale,
): TemplateResult {
  if (!request) {
    return html`
      <section class="consent" aria-live="polite">
        <h1>${t(locale, 'consent.none.title')}</h1>
        <p>${t(locale, 'consent.none.body')}</p>
      </section>
    `;
  }

  return html`
    <section class="consent" role="dialog" aria-modal="true" aria-labelledby="consent-title">
      <h1 id="consent-title">${t(locale, 'consent.request.title', request.peer_label)}</h1>
      <p>${t(locale, 'consent.request.body')}</p>
      <div class="consent-actions">
        <button type="button" autofocus @click=${() => void revoke(request.peer_label)}>
          ${t(locale, 'consent.action.deny')}
        </button>
        <button type="button" @click=${() => void grant(request.peer_label, 'view_only')}>
          ${t(locale, 'consent.action.allowView')}
        </button>
        <button type="button" @click=${() => void grant(request.peer_label, 'full_control')}>
          ${t(locale, 'consent.action.allowFull')}
        </button>
      </div>
    </section>
  `;
}
```

`autofocus` on the `Deny` button is deliberate: §19 phase 6 asks for keyboard/screen-reader accessibility, and the safe default (deny) is what a keyboard/screen-reader user lands on first without having to tab past two grant buttons (Task 4 adds a focused test for this).

- [ ] **Step 2: Update `session-status.ts` to take a `locale` and use `t()`**

```typescript
// apps/desktop/src/session-status.ts
import { html, type TemplateResult } from 'lit-html';

import type { Locale } from './i18n';
import { t } from './i18n';
import type { Role } from './consent-dialog';

export interface SessionStatus {
  peer_label: string;
  role: Role;
  input: boolean;
}

async function revoke(peer: string): Promise<void> {
  const { invoke } = await import('@tauri-apps/api/core');
  await invoke('session_revoke', { args: { peer } });
}

const roleKey: Record<Role, 'status.role.viewOnly' | 'status.role.controlLimited' | 'status.role.fullControl'> = {
  view_only: 'status.role.viewOnly',
  control_limited: 'status.role.controlLimited',
  full_control: 'status.role.fullControl',
};

export function sessionStatus(sessions: SessionStatus[], locale: Locale): TemplateResult {
  if (sessions.length === 0) {
    return html`<section class="status" aria-live="polite"><p>${t(locale, 'status.notSharing')}</p></section>`;
  }

  return html`
    <section class="status" aria-live="polite">
      <h2>${t(locale, 'status.heading')}</h2>
      <ul>
        ${sessions.map(
          (session) => html`
            <li>
              <span>${session.peer_label}</span>
              <span>${t(locale, roleKey[session.role])}</span>
              <span>${session.input ? t(locale, 'status.inputOn') : t(locale, 'status.inputOff')}</span>
              <button type="button" @click=${() => void revoke(session.peer_label)}>
                ${t(locale, 'status.revoke')}
              </button>
            </li>
          `,
        )}
      </ul>
    </section>
  `;
}
```

- [ ] **Step 3: Update `main.ts` to detect/hold locale, set `dir`, and pass it through**

```typescript
// apps/desktop/src/main.ts
import { render } from 'lit-html';

import { consentDialog } from './consent-dialog';
import { detectLocale, dirOf, type Locale } from './i18n';
import { sessionStatus, type SessionStatus } from './session-status';

const root = document.querySelector('#app');
let locale: Locale = detectLocale(navigator);

function applyDir(): void {
  document.documentElement.lang = locale;
  document.documentElement.dir = dirOf(locale);
}

async function refresh(): Promise<void> {
  if (!root) {
    return;
  }
  applyDir();
  const { invoke } = await import('@tauri-apps/api/core');
  const sessions = await invoke<SessionStatus[]>('session_status');
  const pending = sessions.length === 0;

  render(
    [
      pending ? consentDialog(undefined, locale) : consentDialog(sessions[0], locale),
      sessionStatus(sessions, locale),
    ],
    root as HTMLElement,
  );
}

// Exposed for manual/e2e locale switching; the consent screen itself carries
// no locale picker (§19 phase 6 doesn't ask for one, and adding UI chrome to
// a screen that must render instantly is scope creep) — the OS/webview
// locale via `navigator.language` is what `detectLocale` reads.
export function setLocale(next: Locale): void {
  locale = next;
  void refresh();
}

void refresh();
setInterval(() => {
  void refresh();
}, 1000);
```

- [ ] **Step 4: Set a default `dir`/`lang` in `index.html` so there's no FOUC before JS runs**

Read `apps/desktop/index.html` first, then add `lang="en" dir="ltr"` to the `<html>` tag (JS overrides both on first `refresh()`).

- [ ] **Step 5: Typecheck and build**

Run: `cd apps/desktop && npm run typecheck && npm run build`
Expected: PASS, no TS errors from the new required `locale` parameter.

- [ ] **Step 6: Commit**

```bash
git add apps/desktop/src/consent-dialog.ts apps/desktop/src/session-status.ts apps/desktop/src/main.ts apps/desktop/index.html
git commit -m "Phase 6: localize consent/status screens, detect locale and RTL from navigator"
```

---

## Task 3: vitest + jsdom + axe-core harness, and the accessibility audit itself

**Files:**
- Create: `apps/desktop/vitest.config.ts`
- Create: `apps/desktop/src/accessibility.test.ts`
- Modify: `apps/desktop/package.json` (devDependencies + `test` script)
- Modify: `.github/workflows/ci.yml` (new `ui` job)

**Interfaces:**
- Consumes: `consentDialog`, `sessionStatus` from Task 2 (now requiring a `locale` argument); `SUPPORTED_LOCALES` from Task 1.
- Produces: `npm test` in `apps/desktop` runs vitest; a `ui` CI job runs it on every push/PR alongside the existing `build`/`media`/`fuzz`/`supply-chain` jobs.

- [ ] **Step 1: Add dev dependencies**

Run:
```bash
cd apps/desktop
npm install --save-dev vitest jsdom axe-core @testing-library/dom
```

- [ ] **Step 2: Write `vitest.config.ts`**

```typescript
import { defineConfig } from 'vitest/config';

export default defineConfig({
  test: {
    environment: 'jsdom',
    include: ['src/**/*.test.ts'],
  },
});
```

- [ ] **Step 3: Write the failing accessibility test**

```typescript
// apps/desktop/src/accessibility.test.ts
//
// Design doc §19 phase 6 / §17.1: the consent screen has to pass an
// axe-core audit. This runs axe-core's rule engine against jsdom-rendered
// markup rather than a real browser — jsdom has no layout engine, so rules
// that need computed layout (color-contrast, target-size) can't run here
// and are excluded explicitly rather than silently producing false passes.
// See docs/adr/0009-phase-6-ui-accessibility-and-release-scope.md for why
// a full-browser audit isn't wired into this repo's CI.
import axe from 'axe-core';
import { render } from 'lit-html';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { consentDialog } from './consent-dialog';
import { SUPPORTED_LOCALES } from './i18n';
import { sessionStatus, type SessionStatus } from './session-status';

const LAYOUT_DEPENDENT_RULES = ['color-contrast', 'target-size'];

let container: HTMLElement;

beforeEach(() => {
  container = document.createElement('div');
  document.body.appendChild(container);
});

afterEach(() => {
  container.remove();
});

async function auditViolations(node: HTMLElement): Promise<axe.Result[]> {
  const results = await axe.run(node, {
    rules: Object.fromEntries(LAYOUT_DEPENDENT_RULES.map((id) => [id, { enabled: false }])),
  });
  return results.violations;
}

describe('accessibility: consent dialog', () => {
  for (const locale of SUPPORTED_LOCALES) {
    it(`has no axe violations with no pending request (${locale})`, async () => {
      render(consentDialog(undefined, locale), container);
      expect(await auditViolations(container)).toEqual([]);
    });

    it(`has no axe violations with a pending request (${locale})`, async () => {
      const request: SessionStatus = { peer_label: 'guest-ab12', role: 'view_only', input: false };
      render(consentDialog(request, locale), container);
      expect(await auditViolations(container)).toEqual([]);
    });
  }
});

describe('accessibility: session status', () => {
  for (const locale of SUPPORTED_LOCALES) {
    it(`has no axe violations when empty (${locale})`, async () => {
      render(sessionStatus([], locale), container);
      expect(await auditViolations(container)).toEqual([]);
    });

    it(`has no axe violations with active sessions (${locale})`, async () => {
      const sessions: SessionStatus[] = [
        { peer_label: 'guest-ab12', role: 'full_control', input: true },
      ];
      render(sessionStatus(sessions, locale), container);
      expect(await auditViolations(container)).toEqual([]);
    });
  }
});
```

- [ ] **Step 4: Run the tests, fix any real violations found**

Run: `cd apps/desktop && npx vitest run`
Expected: axe-core may flag things like the `<ul>`/`<li>` list needing a landmark, or button text needing to be unique — fix `consent-dialog.ts`/`session-status.ts` markup (not the test) until this and Task 1's `i18n.test.ts` both pass. Do not disable additional rules to make it pass; only `LAYOUT_DEPENDENT_RULES` are excluded, and that list is not to grow without documenting why in the ADR (Task 6).

- [ ] **Step 5: Add the `test` script and wire CI**

```json
// apps/desktop/package.json — add under "scripts"
"test": "vitest run"
```

```yaml
# .github/workflows/ci.yml — new job, alongside `build`
  ui:
    name: ui (i18n + accessibility)
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-node@v4
        with:
          node-version: '22'
      - run: npm install
        working-directory: apps/desktop
      - run: npm run typecheck
        working-directory: apps/desktop
      - run: npm test
        working-directory: apps/desktop
```

- [ ] **Step 6: Commit**

```bash
git add apps/desktop/vitest.config.ts apps/desktop/src/accessibility.test.ts apps/desktop/package.json apps/desktop/package-lock.json .github/workflows/ci.yml
git commit -m "Phase 6: axe-core accessibility audit for the consent/status screens, wired into CI"
```

---

## Task 4: Keyboard navigation test (focus order, no keyboard traps)

**Files:**
- Create: `apps/desktop/src/keyboard-nav.test.ts`

**Interfaces:**
- Consumes: `consentDialog` (Task 2), `@testing-library/dom`'s `getByRole`/`fireEvent` (added in Task 3 Step 1).

- [ ] **Step 1: Write the failing test**

```typescript
// apps/desktop/src/keyboard-nav.test.ts
//
// axe-core (Task 3) checks markup/ARIA statically; it does not drive Tab and
// confirm focus actually lands somewhere sane. This test does that part of
// §19 phase 6's "доступность через клавиатуру" by hand.
import { render } from 'lit-html';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { consentDialog } from './consent-dialog';
import type { SessionStatus } from './session-status';

let container: HTMLElement;

beforeEach(() => {
  container = document.createElement('div');
  document.body.appendChild(container);
});

afterEach(() => {
  container.remove();
});

function focusables(root: HTMLElement): HTMLButtonElement[] {
  return Array.from(root.querySelectorAll('button'));
}

describe('keyboard navigation: consent dialog', () => {
  it('every action is a real <button>, reachable by Tab, none disabled or tabindex=-1', () => {
    const request: SessionStatus = { peer_label: 'guest-ab12', role: 'view_only', input: false };
    render(consentDialog(request, 'en'), container);

    const buttons = focusables(container);
    expect(buttons).toHaveLength(3);
    for (const button of buttons) {
      expect(button.disabled).toBe(false);
      expect(button.tabIndex).not.toBe(-1);
    }
  });

  it('the deny action is first in DOM order and carries autofocus, so a keyboard/screen-reader user lands on the safe default', () => {
    const request: SessionStatus = { peer_label: 'guest-ab12', role: 'view_only', input: false };
    render(consentDialog(request, 'en'), container);

    const buttons = focusables(container);
    expect(buttons[0]?.textContent?.trim()).toBe('Deny');
    expect(buttons[0]?.autofocus).toBe(true);
  });
});
```

- [ ] **Step 2: Run test to verify it fails or passes**

Run: `cd apps/desktop && npx vitest run src/keyboard-nav.test.ts`
Expected: PASS if Task 2 Step 1's `autofocus` on the Deny button is already in place; if this task is executed before Task 2, it will fail with "Cannot find button" until Task 2 lands. Sequence tasks 1→2→3→4 in order.

- [ ] **Step 3: Commit**

```bash
git add apps/desktop/src/keyboard-nav.test.ts
git commit -m "Phase 6: keyboard-navigation test for the consent dialog"
```

---

## Task 5: Sign updater artifacts

**Files:**
- Create: `apps/desktop/src-tauri/updater.key.pub` (public key, committed)
- Modify: `apps/desktop/src-tauri/tauri.conf.json`
- Modify: `.github/workflows/ci.yml` (release job env var, no key material committed)
- Modify: `.gitignore` (never allow the private key file to be committed by accident)

**Interfaces:**
- Consumes: `@tauri-apps/cli` (already a transitive dep via `npm install`).
- Produces: `bundle.createUpdaterArtifacts` config Tauri's build reads at package time; `TAURI_SIGNING_PRIVATE_KEY`/`TAURI_SIGNING_PRIVATE_KEY_PASSWORD` env vars a release CI job would need (not created by this task — see Task 6's ADR for why the actual release job is out of scope here).

- [ ] **Step 1: Generate the keypair locally, keep only the public half**

```bash
cd apps/desktop
npx @tauri-apps/cli signer generate -w src-tauri/updater.key
```

This writes `src-tauri/updater.key` (private, password-protected) and prints the public key. Copy the printed public key into `src-tauri/updater.key.pub` by hand; do not commit `updater.key` itself.

- [ ] **Step 2: Ignore the private key file**

```gitignore
# apps/desktop/src-tauri/updater.key — Ed25519 updater signing key (Task 5,
# ADR 0009). Never commit; lives only in the release operator's secret store.
apps/desktop/src-tauri/updater.key
```

- [ ] **Step 3: Wire the public key and updater-artifact flag into `tauri.conf.json`**

```json
{
  "$schema": "https://schema.tauri.app/config/2",
  "productName": "Lumepeer",
  "version": "0.1.0",
  "identifier": "io.insigmo.lumepeer",
  "build": {
    "frontendDist": "../dist",
    "devUrl": "http://localhost:5173",
    "beforeDevCommand": "npm run dev",
    "beforeBuildCommand": "npm run build"
  },
  "app": {
    "windows": [
      {
        "label": "main",
        "title": "Lumepeer",
        "width": 960,
        "height": 640,
        "resizable": true
      }
    ],
    "security": {
      "csp": "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self' ipc: http://ipc.localhost; frame-src 'none'; object-src 'none'; base-uri 'none'",
      "dangerousDisableAssetCspModification": false,
      "freezePrototype": true
    }
  },
  "bundle": {
    "active": true,
    "targets": "all",
    "icon": ["icons/icon.png"],
    "createUpdaterArtifacts": true
  },
  "plugins": {
    "updater": {
      "pubkey": "<contents of updater.key.pub, one line, pasted here>",
      "endpoints": []
    }
  }
}
```

`endpoints: []` is deliberate: this task signs artifacts so verification has something to check; it does not stand up an update-distribution server, which is a separate, unscoped piece of infrastructure (documented in Task 6's ADR).

- [ ] **Step 4: Build once locally to confirm the config is accepted**

Run: `cd apps/desktop/src-tauri && cargo check`
Expected: PASS — this only validates the config parses and the crate compiles; it does not require running a full bundle build (no Linux bundler deps assumed present).

- [ ] **Step 5: Commit**

```bash
git add apps/desktop/src-tauri/updater.key.pub apps/desktop/src-tauri/tauri.conf.json .gitignore
git commit -m "Phase 6: sign updater artifacts with an Ed25519 key (§21 signed-artifact line)"
```

---

## Task 6: ADR 0009 (phase-6 scope) and documentation updates

**Files:**
- Create: `docs/adr/0009-phase-6-ui-accessibility-and-release-scope.md`
- Modify: `docs/release-checklist.md`
- Modify: `README.md`

**Interfaces:**
- Consumes: nothing new; references facts established in Tasks 1–5 and 7.

- [ ] **Step 1: Write the ADR**

Cover, following the structure of ADR 0007/0008 (Context / Decisions):
- Why Arabic was picked as the second locale (real RTL, not a second LTR translation) — §19 phase 6, done-criterion "локализован минимум на 2 языка".
- Why the axe-core audit runs against jsdom rather than a real browser, and which two rules (`color-contrast`, `target-size`) are excluded and why (no layout engine in jsdom) — this is the same "verify only what this machine can actually verify" cut as ADR 0007's platform scope.
- Why updater-artifact signing (Task 5) closes the specific gap ADR 0008 flagged ("`tauri.conf.json` has no bundle signing key") but OS-level code signing (Windows Authenticode certificate, Apple Developer ID + notarization) is not attempted: both require a paid vendor relationship and, for Apple, hardware this repo does not have access to. State plainly this blocks a real cross-platform release and is tracked, not silently dropped.
- Why a third-party penetration test (§19 phase 6 literal ask) is out of scope for repository automation: it requires an independent human tester per most engagement definitions, which no CI job or agent session can substitute for. Document what stands in for it instead — Task 7's security-review pass — and that it is *not* equivalent, only the closest thing achievable here.

- [ ] **Step 2: Update `docs/release-checklist.md`**

Change the "Signed artifact verification is **not wired up**" row (currently pointing at ADR 0008) to reflect Task 5: updater-artifact signing is wired; OS-level installer code signing is not, with a forward reference to ADR 0009. Change the "user-visible consent... phase 6 work" row to point at the now-implemented, tested, localized, accessible screens (Tasks 1–4) instead of "not implemented". Add a row for the penetration-test line of §19 phase 6, pointing at ADR 0009 and Task 7's findings.

- [ ] **Step 3: Update `README.md`**

Add a "Phase 6" paragraph in the same style as the existing Phase 0–5 paragraphs (see lines 58–139 for the pattern), stating what's done (i18n/RTL, axe-core-audited consent/status screens, keyboard-nav test, updater-artifact signing) and what's explicitly deferred with an ADR reference (OS code signing, third-party pentest).

- [ ] **Step 4: Commit**

```bash
git add docs/adr/0009-phase-6-ui-accessibility-and-release-scope.md docs/release-checklist.md README.md
git commit -m "Phase 6: ADR 0009 and docs for the UI/accessibility/release scope cut"
```

---

## Task 7: Security review pass (penetration-test substitute)

**Files:**
- No source files — this task runs the repo's `/security-review` process against the full set of phase-6 changes (Tasks 1–6) and records the outcome.
- Modify: `docs/adr/0009-phase-6-ui-accessibility-and-release-scope.md` (append findings summary once available)

**Interfaces:**
- Consumes: the complete diff produced by Tasks 1–6.

- [ ] **Step 1: Run the security review**

Invoke the `security-review` skill against the branch/diff covering Tasks 1–6. Pay particular attention to: the new `plugins.updater` config (no `endpoints` pointing anywhere that could be hijacked — must stay `[]` until a real distribution server exists), the private key handling in Task 5 (never logged, never in a committed file, `.gitignore` entry actually matches the path), and that i18n string interpolation (`t(locale, 'consent.request.title', request.peer_label)`) never round-trips anything other than the already-pseudonymized `peer_label` — no raw `NodeId` reaching the DOM (§15).

- [ ] **Step 2: Triage findings**

Any high/critical finding blocks phase 6 completion per §19's done-criterion ("no high/critical unresolved findings") — fix before proceeding. Medium/low findings get recorded in the ADR with a decision (fix now / accept and document why).

- [ ] **Step 3: Append the outcome to ADR 0009 and commit**

```bash
git add docs/adr/0009-phase-6-ui-accessibility-and-release-scope.md
git commit -m "Phase 6: record security review outcome (penetration-test substitute)"
```

---

## Self-Review Notes

- **Spec coverage:** consent screen (Task 2), session status (Task 2), i18n/RTL (Tasks 1–2), keyboard/screen-reader accessibility (Tasks 3–4), signed builds (Task 5), penetration test (Task 7 + ADR scope cut in Task 6) — every §19 phase-6 bullet has a task. Done-criteria: axe-core audit (Task 3), ≥2 locales (Task 1, en+ar), no high/critical findings (Task 7), release gates (Task 6 updates `docs/release-checklist.md`).
- **Type consistency:** `consentDialog`/`sessionStatus` signatures gain `locale: Locale` consistently across Tasks 1–4; `roleKey` map in Task 2 Step 2 covers all three `Role` variants exhaustively (TS will error on a missing key since it's a `Record<Role, ...>`).
- **Sequencing:** Tasks 1→2→3→4 must run in that order (each depends on the previous); Task 5 is independent and can run in parallel; Task 6 depends on 1–5 being done (it documents them); Task 7 depends on 1–6 (reviews the full diff).
