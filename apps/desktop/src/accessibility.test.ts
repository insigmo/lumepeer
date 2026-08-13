// Design doc §19 phase 6 / §17.1: the consent screen has to pass an
// axe-core audit. This runs axe-core's rule engine against jsdom-rendered
// markup rather than a real browser — jsdom has no layout engine, so rules
// that need computed layout (color-contrast, target-size) can't run here
// and are excluded explicitly rather than silently producing false passes.
// See docs/adr/0009-phase-6-ui-accessibility-and-release-scope.md for why
// a full-browser audit isn't wired into this repo's CI.
import * as axe from 'axe-core';
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
