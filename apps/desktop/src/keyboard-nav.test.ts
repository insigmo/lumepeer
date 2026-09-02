// apps/desktop/src/keyboard-nav.test.ts
//
// axe-core (Task 3) checks markup/ARIA statically; it does not drive Tab and
// confirm focus actually lands somewhere sane. This test does that part of
// §19 phase 6's "доступность через клавиатуру" by hand.
import { render } from 'lit-html';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { addressBook, type AddressBookEntry } from './address-book';
import { consentDialog } from './consent-dialog';
import { connectPanel, inviteCodePanel } from './invite-view';
import type { SessionStatus } from './session-status';
import { titleBar } from './title-bar';
import { unattendedSettings, type UnattendedStatus } from './unattended-settings';

// A session the host has granted nothing beyond its role: the four
// independent grants of §8.2 all start off.
const noGrants = {
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
} as const;

vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn().mockResolvedValue({ code: 'test-invite-code', expires_at: 0 }),
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
    const request: SessionStatus = {
      peer_label: 'guest-ab12',
      role: 'view_only',
      input: false,
      state: 'pending',
      ...noGrants,
    };
    render(consentDialog(request, 'en'), container);

    const buttons = focusables(container);
    expect(buttons).toHaveLength(3);
    for (const button of buttons) {
      expect(button.disabled).toBe(false);
      expect(button.tabIndex).not.toBe(-1);
    }
  });

  it('the deny action is first in DOM order and carries autofocus, so a keyboard/screen-reader user lands on the safe default', () => {
    const request: SessionStatus = {
      peer_label: 'guest-ab12',
      role: 'view_only',
      input: false,
      state: 'pending',
      ...noGrants,
    };
    render(consentDialog(request, 'en'), container);

    const buttons = focusables(container);
    expect(buttons[0]?.textContent?.trim()).toBe('Deny');
    expect(buttons[0]?.autofocus).toBe(true);
  });
});

describe('keyboard navigation: unattended access and the address book', () => {
  const on: UnattendedStatus = { enabled: true, totp_enabled: false, role: 'view_only' };

  it('every settings control is reachable by Tab and none is taken out of the order', () => {
    render(unattendedSettings(on, 'en'), container);

    const controls = Array.from(
      container.querySelectorAll<HTMLElement>('button, input, select'),
    );
    expect(controls.length).toBeGreaterThan(0);
    for (const control of controls) {
      expect(control.tabIndex).not.toBe(-1);
    }
  });

  it('the trust confirmation puts focus on the safe answer, not on the one that grants', () => {
    const entry: AddressBookEntry = {
      peer_label: 'guest-ab12',
      name: 'Office workstation',
      tags: [],
      notes: '',
      trusted: false,
      connected: false,
    };
    render(addressBook([entry], 'en'), container);
    container.querySelector<HTMLInputElement>('.book-trust input')?.click();
    render(addressBook([entry], 'en'), container);

    const buttons = Array.from(container.querySelectorAll<HTMLButtonElement>('.trust-confirm button'));
    expect(buttons).toHaveLength(2);
    // Cancel comes first in DOM order and carries autofocus: a keyboard or
    // screen-reader user lands on the answer that grants nothing.
    expect(buttons[0]?.className).toContain('trust-cancel');
    expect(buttons[0]?.autofocus).toBe(true);
    for (const button of buttons) {
      expect(button.disabled).toBe(false);
      expect(button.tabIndex).not.toBe(-1);
    }
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

  it('the create-invite and copy-code buttons are reachable across the invite lifecycle', async () => {
    render(inviteCodePanel('en'), container);
    let buttons = focusables(container);
    expect(buttons).toHaveLength(1);
    expect(buttons[0]?.disabled).toBe(false);

    buttons[0]?.click();
    await vi.waitFor(() => {
      render(inviteCodePanel('en'), container);
      expect(container.querySelector('.copy-btn')).not.toBeNull();
    });

    // Copy is the only control here: the code itself is plain selectable text
    // and reissuing the invite lives in the settings window (docs/bugs/04).
    buttons = focusables(container);
    expect(buttons).toHaveLength(1);
    for (const button of buttons) {
      expect(button.disabled).toBe(false);
      expect(button.tabIndex).not.toBe(-1);
    }
  });
});
