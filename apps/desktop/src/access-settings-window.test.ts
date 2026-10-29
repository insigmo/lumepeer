// The elevated access-settings window (ADR 0169 §5): the two panels that
// decide who gets in, editable, and nothing else of the main screen.
import { render } from 'lit-html';
import { describe, expect, it, vi } from 'vitest';

import { accessSettingsView } from './access-settings-window';
import { t } from './i18n';

vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn().mockResolvedValue(undefined),
}));

describe('access settings window', () => {
  it('shows the editable unattended panel and the saved devices', () => {
    const container = document.createElement('div');
    render(
      accessSettingsView(
        {
          unattended: { enabled: true, totp_enabled: false, role: 'view_only' },
          devices: [
            {
              peer_label: 'peer-1',
              name: 'office',
              tags: [],
              notes: '',
              trusted: true,
              connected: false,
            },
          ],
        },
        'en',
        () => {},
      ),
      container,
    );
    expect(container.querySelector('h2')?.textContent).toBe(t('en', 'access.heading'));
    expect(container.querySelector('#unattended-password')).not.toBeNull();
    expect(container.querySelector('[data-testid="unattended-open-access-settings"]')).toBeNull();
    expect(container.textContent).toContain('office');
  });
});
