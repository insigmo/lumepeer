// Host-side unattended-access settings (§8; ADR 0033).
//
// What these tests are really about is the direction of authority and what
// this screen is allowed to know. The panel asks the core to change something
// and then shows whatever the core reports; it never flips a switch locally,
// and there is no path by which a password or its hash reaches it.
import { render } from 'lit-html';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { SUPPORTED_LOCALES, t } from './i18n';
import {
  resetUnattendedPanel,
  unattendedIndicator,
  unattendedSettings,
  type UnattendedStatus,
} from './unattended-settings';

const invoke = vi.fn();

vi.mock('@tauri-apps/api/core', () => ({
  invoke: (...args: unknown[]) => invoke(...args),
}));

const off: UnattendedStatus = { enabled: false, totp_enabled: false, role: 'view_only' };
const on: UnattendedStatus = { enabled: true, totp_enabled: false, role: 'view_only' };

let container: HTMLElement;

beforeEach(() => {
  invoke.mockReset();
  invoke.mockResolvedValue(undefined);
  resetUnattendedPanel();
  container = document.createElement('div');
  document.body.appendChild(container);
});

afterEach(() => {
  container.remove();
});

function passwordField(): HTMLInputElement {
  const field = container.querySelector<HTMLInputElement>('#unattended-password');
  expect(field).not.toBeNull();
  return field as HTMLInputElement;
}

describe('unattended settings', () => {
  it('starts off, and says so, until the core reports otherwise', () => {
    render(unattendedSettings(off, 'en'), container);
    expect(container.querySelector('.unattended-state')?.textContent?.trim()).toBe(
      t('en', 'unattended.state.off'),
    );
    // Nothing to turn off yet, so no destructive control is offered.
    expect(container.querySelector('.unattended-disable')).toBeNull();
  });

  it('sends the password to the core and clears the field immediately', async () => {
    render(unattendedSettings(off, 'en'), container);
    passwordField().value = 'correct horse battery staple';
    container.querySelector('form')?.dispatchEvent(new Event('submit', { cancelable: true }));

    await vi.waitFor(() =>
      expect(invoke).toHaveBeenCalledWith('unattended_set_password', {
        args: { password: 'correct horse battery staple' },
      }),
    );
    await vi.waitFor(() => expect(passwordField().value).toBe(''));
  });

  it('an empty password is not sent at all', () => {
    render(unattendedSettings(off, 'en'), container);
    passwordField().value = '';
    container.querySelector('form')?.dispatchEvent(new Event('submit', { cancelable: true }));
    expect(invoke).not.toHaveBeenCalled();
  });

  it('shows the core refusal instead of pretending the password was set', async () => {
    invoke.mockRejectedValueOnce({ code: 'UNATTENDED', message: 'the password must be between 8 and 1024 bytes' });
    const onRefresh = vi.fn();
    render(unattendedSettings(off, 'en', onRefresh), container);
    passwordField().value = 'short';
    container.querySelector('form')?.dispatchEvent(new Event('submit', { cancelable: true }));

    await vi.waitFor(() => expect(onRefresh).toHaveBeenCalled());
    render(unattendedSettings(off, 'en', onRefresh), container);
    expect(container.querySelector('[data-testid="unattended-error"]')?.textContent).toContain(
      'must be between 8 and 1024 bytes',
    );
  });

  it('the second factor cannot be armed before a password exists', () => {
    render(unattendedSettings(off, 'en'), container);
    const box = container.querySelector<HTMLInputElement>('.unattended-totp input');
    expect(box?.disabled).toBe(true);
    expect(container.querySelector('.unattended-hint')?.textContent?.trim()).toBe(
      t('en', 'unattended.needsTrust'),
    );
  });

  it('shows the provisioning secret once when the second factor is turned on', async () => {
    invoke.mockResolvedValueOnce({
      secret_base32: 'GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ',
      uri: 'otpauth://totp/Lumepeer:device?secret=GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ',
    });
    const onRefresh = vi.fn();
    render(unattendedSettings(on, 'en', onRefresh), container);

    container.querySelector<HTMLInputElement>('.unattended-totp input')?.click();
    await vi.waitFor(() =>
      expect(invoke).toHaveBeenCalledWith('unattended_set_totp', { args: { enabled: true } }),
    );
    await vi.waitFor(() => {
      render(unattendedSettings({ ...on, totp_enabled: true }, 'en', onRefresh), container);
      expect(container.querySelector('[data-testid="totp-secret"]')?.textContent).toBe(
        'GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ',
      );
    });

    // Dismissed, and there is no way to ask for it again: the next render of
    // the same status shows nothing.
    container.querySelector<HTMLButtonElement>('.totp-done')?.click();
    render(unattendedSettings({ ...on, totp_enabled: true }, 'en', onRefresh), container);
    expect(container.querySelector('[data-testid="totp-secret"]')).toBeNull();
  });

  it('turning it off asks first, and does nothing when the answer is no', () => {
    const confirm = vi.spyOn(globalThis, 'confirm').mockReturnValue(false);
    render(unattendedSettings(on, 'en'), container);

    container.querySelector<HTMLButtonElement>('.unattended-disable')?.click();
    expect(confirm).toHaveBeenCalled();
    expect(invoke).not.toHaveBeenCalled();
    confirm.mockRestore();
  });

  it('turning it off goes through to the core once confirmed', async () => {
    const confirm = vi.spyOn(globalThis, 'confirm').mockReturnValue(true);
    render(unattendedSettings(on, 'en'), container);

    container.querySelector<HTMLButtonElement>('.unattended-disable')?.click();
    await vi.waitFor(() => expect(invoke).toHaveBeenCalledWith('unattended_disable'));
    confirm.mockRestore();
  });

  it('the role select shows what the core holds and asks it to change', async () => {
    render(unattendedSettings({ ...on, role: 'full_control' }, 'en'), container);
    const select = container.querySelector<HTMLSelectElement>('#unattended-role');
    expect(select?.value).toBe('full_control');

    if (select) {
      select.value = 'view_only';
      select.dispatchEvent(new Event('change'));
    }
    await vi.waitFor(() =>
      expect(invoke).toHaveBeenCalledWith('unattended_set_role', { args: { role: 'view_only' } }),
    );
  });

  it('the password field is a password field, and never renders a value', () => {
    render(unattendedSettings(on, 'en'), container);
    const field = passwordField();
    expect(field.type).toBe('password');
    // Nothing in the status can carry a password, so nothing can pre-fill it.
    expect(field.value).toBe('');
    expect(container.innerHTML).not.toContain('argon2');
  });

  it('is labelled in both locales and keyboard reachable', () => {
    for (const locale of SUPPORTED_LOCALES) {
      const scoped = document.createElement('div');
      document.body.appendChild(scoped);
      try {
        render(unattendedSettings(on, locale), scoped);
        expect(scoped.querySelector('#unattended-heading')?.textContent?.trim()).toBe(
          t(locale, 'unattended.heading'),
        );
        for (const control of scoped.querySelectorAll<HTMLElement>('button, input, select')) {
          expect(control.tabIndex).not.toBe(-1);
        }
      } finally {
        scoped.remove();
      }
    }
  });
});

describe('unattended indicator', () => {
  it('is absent while unattended access is off', () => {
    render(unattendedIndicator(off, 'en'), container);
    expect(container.querySelector('[data-testid="unattended-indicator"]')).toBeNull();
  });

  /// §2: a host that can hand out a session without a person here has to show
  /// that on its own screen, and the guest side must have no way to take the
  /// sign down. There is no dismiss control to find.
  it('is present whenever it is on, and offers no way to dismiss it', () => {
    for (const locale of SUPPORTED_LOCALES) {
      const scoped = document.createElement('div');
      document.body.appendChild(scoped);
      try {
        render(unattendedIndicator(on, locale), scoped);
        const banner = scoped.querySelector('[data-testid="unattended-indicator"]');
        expect(banner).not.toBeNull();
        expect(banner?.textContent?.trim()).toBe(t(locale, 'unattended.indicator'));
        expect(banner?.querySelectorAll('button')).toHaveLength(0);
      } finally {
        scoped.remove();
      }
    }
  });
});
