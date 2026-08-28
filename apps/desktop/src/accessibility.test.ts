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

import { addressBook, type AddressBookEntry } from './address-book';
import { auditPanel, resetAuditPanel, type AuditCommands, type AuditRow } from './audit-log';
import { resetSystemSettings, systemSettings, type SystemCommands } from './system-settings';
import { consentDialog } from './consent-dialog';
import { SUPPORTED_LOCALES } from './i18n';
import { connectPanel, credentialsPanel, inviteCodePanel, setConnectPhase } from './invite-view';
import { recordingsPanel, type RecordingEntry } from './recordings';
import { sessionStatus, type HistoryEntry, type SessionStatus } from './session-status';
import { statusPill } from './status-pill';
import { unattendedIndicator, unattendedSettings, type UnattendedStatus } from './unattended-settings';
import { titleBar } from './title-bar';

// A session the host has granted nothing beyond its role: the four
// independent grants of §8.2 all start off.
const noGrants = {
  clipboard_read: false,
  clipboard_write: false,
  file_transfer: false,
  recording: false,
  recording_active: false,
  record_request: false,
} as const;

vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn().mockResolvedValue({ code: 'test-invite-code', expires_at: 0 }),
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
      const request: SessionStatus = {
      peer_label: 'guest-ab12',
      role: 'view_only',
      input: false,
      state: 'pending',
      ...noGrants,
    };
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
        { peer_label: 'guest-ab12', role: 'full_control', input: true, state: 'active', ...noGrants },
      ];
      render(sessionStatus(sessions, locale), container);
      expect(await auditViolations(container)).toEqual([]);
    });

    it(`has no axe violations with ended connections in history (${locale})`, async () => {
      const sessions: SessionStatus[] = [
        { peer_label: 'guest-ab12', role: 'full_control', input: true, state: 'active', ...noGrants },
      ];
      const history: HistoryEntry[] = [
        { peer_label: 'guest-cd34', role: 'view_only', ended_at: Math.floor(Date.now() / 1000) - 120 },
      ];
      render(sessionStatus(sessions, locale, undefined, history), container);
      expect(await auditViolations(container)).toEqual([]);
    });
  }
});

describe('accessibility: unattended access', () => {
  const off: UnattendedStatus = { enabled: false, totp_enabled: false, role: 'view_only' };
  const on: UnattendedStatus = { enabled: true, totp_enabled: true, role: 'full_control' };

  for (const locale of SUPPORTED_LOCALES) {
    it(`has no axe violations in the settings panel, off and on (${locale})`, async () => {
      render(unattendedSettings(off, locale), container);
      expect(await auditViolations(container)).toEqual([]);

      render(unattendedSettings(on, locale), container);
      expect(await auditViolations(container)).toEqual([]);
    });

    it(`has no axe violations in the always-on indicator (${locale})`, async () => {
      render(unattendedIndicator(on, locale), container);
      expect(await auditViolations(container)).toEqual([]);
    });
  }
});

describe('accessibility: address book', () => {
  const entries: AddressBookEntry[] = [
    {
      peer_label: 'guest-ab12',
      name: 'Office workstation',
      tags: ['work'],
      notes: 'upstairs',
      trusted: false,
      connected: true,
    },
    {
      peer_label: 'guest-cd34',
      name: 'Home laptop',
      tags: ['family'],
      notes: '',
      trusted: true,
      connected: false,
    },
  ];

  for (const locale of SUPPORTED_LOCALES) {
    it(`has no axe violations when empty (${locale})`, async () => {
      render(addressBook([], locale), container);
      expect(await auditViolations(container)).toEqual([]);
    });

    it(`has no axe violations with saved devices (${locale})`, async () => {
      render(addressBook(entries, locale), container);
      expect(await auditViolations(container)).toEqual([]);
    });
  }
});

describe('accessibility: recordings panel', () => {
  const recordings: RecordingEntry[] = [
    {
      name: 'session-1700000000-ab12cd.lmrc',
      bytes: 5 * 1024 * 1024,
      modified: 1700000000,
      exported: false,
    },
  ];

  for (const locale of SUPPORTED_LOCALES) {
    it(`has no axe violations when empty (${locale})`, async () => {
      render(recordingsPanel([], locale), container);
      expect(await auditViolations(container)).toEqual([]);
    });

    it(`has no axe violations with recordings to export (${locale})`, async () => {
      render(recordingsPanel(recordings, locale), container);
      expect(await auditViolations(container)).toEqual([]);
    });
  }
});

describe('accessibility: device credential form', () => {
  for (const locale of SUPPORTED_LOCALES) {
    it(`has no axe violations, with and without the code field (${locale})`, async () => {
      setConnectPhase('awaiting_credentials', null, false);
      render(credentialsPanel(locale), container);
      expect(await auditViolations(container)).toEqual([]);

      setConnectPhase('awaiting_credentials', 'UNATTENDED_BAD_CODE', true);
      render(credentialsPanel(locale), container);
      expect(await auditViolations(container)).toEqual([]);
      setConnectPhase('idle');
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

describe('accessibility: audit log', () => {
  const rows: AuditRow[] = [
    { at_unix_secs: 1_700_000_000, peer: 'ab12cd34', kind: 'consent_granted', detail: 'full_control' },
  ];
  const commands: AuditCommands = {
    status: () => Promise.resolve(true),
    kinds: () => Promise.resolve(['consent_granted']),
    list: () => Promise.resolve(rows),
    export: () => Promise.resolve(null),
    clear: () => Promise.resolve(0),
  };

  for (const locale of SUPPORTED_LOCALES) {
    it(`has no axe violations with records listed (${locale})`, async () => {
      resetAuditPanel();
      render(auditPanel(locale, commands), container);
      await vi.waitFor(() => {
        render(auditPanel(locale, commands), container);
        expect(container.querySelector('[data-testid="audit-row"]')).not.toBeNull();
      });
      expect(await auditViolations(container)).toEqual([]);
      resetAuditPanel();
    });
  }
});

describe('accessibility: this device', () => {
  const commands: SystemCommands = {
    serviceStatus: () => Promise.resolve('running' as const),
    serviceSet: () => Promise.resolve(),
    autostartStatus: () => Promise.resolve(true),
    autostartSet: () => Promise.resolve(),
    updateCheck: () => Promise.resolve({ version: '0.0.24', current: '0.0.23', notes: '' }),
    updateInstall: () => Promise.resolve(),
  };

  for (const locale of SUPPORTED_LOCALES) {
    it(`has no axe violations, before and after an update is found (${locale})`, async () => {
      resetSystemSettings();
      render(systemSettings(locale, commands), container);
      expect(await auditViolations(container)).toEqual([]);

      container.querySelector<HTMLButtonElement>('[data-testid="update-check"]')?.click();
      await vi.waitFor(() => {
        render(systemSettings(locale, commands), container);
        expect(container.querySelector('[data-testid="update-found"]')).not.toBeNull();
      });
      expect(await auditViolations(container)).toEqual([]);
      resetSystemSettings();
    });
  }
});
