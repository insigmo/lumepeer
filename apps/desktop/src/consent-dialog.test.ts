// apps/desktop/src/consent-dialog.test.ts
//
// What the host is asked when a guest came for a shell alone (ADR 0131):
// the dialog says so, and its one "yes" is the role that carries a shell —
// the host then grants that session the shell and nothing else.
import { render } from 'lit-html';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { consentDialog } from './consent-dialog';
import type { SessionStatus } from './session-status';

const { invoke } = vi.hoisted(() => ({ invoke: vi.fn().mockResolvedValue(undefined) }));
vi.mock('@tauri-apps/api/core', () => ({ invoke }));

const REQUEST: SessionStatus = {
  peer_label: 'guest-ab12',
  state: 'pending',
  role: 'full_control',
  input: false,
  clipboard_read: false,
  clipboard_write: false,
  file_transfer: false,
  recording: false,
  display_mode: false,
  recording_active: false,
  record_request: false,
  secure_desktop: false,
  secure_desktop_input: false,
  secure_desktop_active: false,
  tunnel: false,
  terminal: false,
  terminal_active: false,
  terminal_only: false,
  chat_unread: false,
};

let container: HTMLElement;

beforeEach(() => {
  container = document.createElement('div');
  document.body.appendChild(container);
  invoke.mockClear();
});

afterEach(() => {
  container.remove();
});

function actions(): string[] {
  return Array.from(container.querySelectorAll('.consent-actions button')).map(
    (button) => button.className,
  );
}

describe('consent dialog', () => {
  it('offers a screen request the view and full-control answers', () => {
    render(consentDialog(REQUEST, 'en'), container);

    expect(actions()).toEqual(['consent-action-deny', 'consent-action-view', 'consent-action-full']);
    expect(container.textContent).toContain('Granting view');
  });

  it('says a terminal request is a shell and nothing else, and has one yes', () => {
    render(consentDialog({ ...REQUEST, terminal_only: true }, 'en'), container);

    expect(actions()).toEqual(['consent-action-deny', 'consent-action-terminal']);
    expect(container.textContent).toContain('terminal only');
    expect(container.textContent).not.toContain('Granting view');
  });

  it('lets a terminal request in with the role that carries a shell', async () => {
    render(consentDialog({ ...REQUEST, terminal_only: true }, 'en'), container);

    container.querySelector<HTMLButtonElement>('.consent-action-terminal')?.click();
    await vi.waitFor(() => {
      expect(invoke).toHaveBeenCalledWith('session_grant', {
        args: { peer: 'guest-ab12', role: 'full_control' },
      });
    });
  });
});
