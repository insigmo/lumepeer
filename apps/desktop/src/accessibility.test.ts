// Design doc §19 phase 6 / §17.1: the consent screen has to pass an
// axe-core audit. This runs axe-core's rule engine against jsdom-rendered
// markup rather than a real browser — jsdom has no layout engine, so rules
// that need computed layout (color-contrast, target-size) can't run here
// and are excluded explicitly rather than silently producing false passes.
// See docs/adr/0009-phase-6-ui-accessibility-and-release-scope.md for why
// a full-browser audit isn't wired into this repo's CI.
import * as axe from 'axe-core';
import { render } from 'lit-html';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { consentDialog } from './consent-dialog';
import { SUPPORTED_LOCALES } from './i18n';
import { connectPanel, inviteCodePanel } from './invite-view';
import { sessionStatus, type HistoryEntry, type SessionStatus } from './session-status';
import { statusPill } from './status-pill';
import { titleBar } from './title-bar';

vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn().mockResolvedValue({ qr_string: 'test-ticket-string', expires_at: 0 }),
}));
vi.mock('qrcode', () => ({
  default: { toDataURL: vi.fn().mockResolvedValue('data:image/png;base64,stub') },
}));

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
      const request: SessionStatus = { peer_label: 'guest-ab12', role: 'view_only', input: false, state: 'pending' };
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
        { peer_label: 'guest-ab12', role: 'full_control', input: true, state: 'active' },
      ];
      render(sessionStatus(sessions, locale), container);
      expect(await auditViolations(container)).toEqual([]);
    });

    it(`has no axe violations with ended connections in history (${locale})`, async () => {
      const sessions: SessionStatus[] = [
        { peer_label: 'guest-ab12', role: 'full_control', input: true, state: 'active' },
      ];
      const history: HistoryEntry[] = [
        { peer_label: 'guest-cd34', role: 'view_only', ended_at: Math.floor(Date.now() / 1000) - 120 },
      ];
      render(sessionStatus(sessions, locale, undefined, history), container);
      expect(await auditViolations(container)).toEqual([]);
    });
  }
});

describe('accessibility: status pill', () => {
  for (const locale of SUPPORTED_LOCALES) {
    it(`has no axe violations when ready (${locale})`, async () => {
      render(statusPill(true, locale), container);
      expect(await auditViolations(container)).toEqual([]);
    });

    it(`has no axe violations when not ready (${locale})`, async () => {
      render(statusPill(false, locale), container);
      expect(await auditViolations(container)).toEqual([]);
    });
  }
});

describe('accessibility: title bar', () => {
  for (const locale of SUPPORTED_LOCALES) {
    it(`has no axe violations (${locale})`, async () => {
      render(titleBar(locale), container);
      expect(await auditViolations(container)).toEqual([]);
    });
  }
});

describe('accessibility: invite view', () => {
  for (const locale of SUPPORTED_LOCALES) {
    it(`has no axe violations in the connect panel (${locale})`, async () => {
      render(connectPanel(locale), container);
      expect(await auditViolations(container)).toEqual([]);
    });

    it(`has no axe violations in the invite code panel, before and after an invite is created (${locale})`, async () => {
      render(inviteCodePanel(locale), container);
      expect(await auditViolations(container)).toEqual([]);

      container.querySelector<HTMLButtonElement>('.create-btn')?.click();
      await vi.waitFor(() => {
        render(inviteCodePanel(locale), container);
        expect(container.querySelector('.code-box')).not.toBeNull();
      });
      expect(await auditViolations(container)).toEqual([]);
    });
  }
});
