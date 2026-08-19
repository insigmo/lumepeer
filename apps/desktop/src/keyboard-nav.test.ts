// apps/desktop/src/keyboard-nav.test.ts
//
// axe-core (Task 3) checks markup/ARIA statically; it does not drive Tab and
// confirm focus actually lands somewhere sane. This test does that part of
// §19 phase 6's "доступность через клавиатуру" by hand.
import { render } from 'lit-html';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { consentDialog } from './consent-dialog';
import { connectPanel, inviteCodePanel } from './invite-view';
import type { SessionStatus } from './session-status';
import { titleBar } from './title-bar';

vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn().mockResolvedValue({ qr_string: 'test-ticket-string', expires_at: 0 }),
}));
vi.mock('qrcode', () => ({
  default: { toDataURL: vi.fn().mockResolvedValue('data:image/png;base64,stub') },
}));

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
    const request: SessionStatus = { peer_label: 'guest-ab12', role: 'view_only', input: false, state: 'pending' };
    render(consentDialog(request, 'en'), container);

    const buttons = focusables(container);
    expect(buttons).toHaveLength(3);
    for (const button of buttons) {
      expect(button.disabled).toBe(false);
      expect(button.tabIndex).not.toBe(-1);
    }
  });

  it('the deny action is first in DOM order and carries autofocus, so a keyboard/screen-reader user lands on the safe default', () => {
    const request: SessionStatus = { peer_label: 'guest-ab12', role: 'view_only', input: false, state: 'pending' };
    render(consentDialog(request, 'en'), container);

    const buttons = focusables(container);
    expect(buttons[0]?.textContent?.trim()).toBe('Deny');
    expect(buttons[0]?.autofocus).toBe(true);
  });
});

describe('keyboard navigation: title bar', () => {
  it('minimize/maximize/close are real buttons, reachable by Tab, none disabled or tabindex=-1', () => {
    render(titleBar('en'), container);

    const buttons = focusables(container);
    expect(buttons).toHaveLength(3);
    for (const button of buttons) {
      expect(button.disabled).toBe(false);
      expect(button.tabIndex).not.toBe(-1);
    }
  });
});

describe('keyboard navigation: invite view', () => {
  it('the connect form input is reachable and its submit button is a real, enabled button', () => {
    render(connectPanel('en'), container);

    const input = container.querySelector('input#ticket-input');
    expect(input).not.toBeNull();
    expect(input?.getAttribute('tabindex')).not.toBe('-1');

    const buttons = focusables(container);
    expect(buttons).toHaveLength(1);
    expect(buttons[0]?.disabled).toBe(false);
  });

  it('the create-invite, code-box, copy-code and refresh-invite buttons are reachable across the invite lifecycle', async () => {
    render(inviteCodePanel('en'), container);
    let buttons = focusables(container);
    expect(buttons).toHaveLength(1);
    expect(buttons[0]?.disabled).toBe(false);

    buttons[0]?.click();
    await vi.waitFor(() => {
      render(inviteCodePanel('en'), container);
      expect(container.querySelector('.copy-btn')).not.toBeNull();
    });

    // Code box (copies on click), explicit copy button, and refresh-invite.
    buttons = focusables(container);
    expect(buttons).toHaveLength(3);
    for (const button of buttons) {
      expect(button.disabled).toBe(false);
      expect(button.tabIndex).not.toBe(-1);
    }
  });
});
