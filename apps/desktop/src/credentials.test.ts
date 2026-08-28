// Guest-side device-credential form (§8; ADR 0033).
//
// The two things worth pinning down: the password leaves the field the moment
// it is handed to the Rust side and is never held here, and a refusal says
// only what the host said — which factor to retype, or how long to wait.
import { render } from 'lit-html';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { SUPPORTED_LOCALES, t } from './i18n';
import { credentialsPanel, isAwaitingCredentials, isConnecting, setConnectPhase } from './invite-view';

const invoke = vi.fn();

vi.mock('@tauri-apps/api/core', () => ({
  invoke: (...args: unknown[]) => invoke(...args),
}));

let container: HTMLElement;

beforeEach(() => {
  invoke.mockReset();
  invoke.mockResolvedValue(undefined);
  setConnectPhase('idle');
  container = document.createElement('div');
  document.body.appendChild(container);
});

afterEach(() => {
  setConnectPhase('idle');
  container.remove();
});

function submit(): void {
  container.querySelector('form')?.dispatchEvent(new Event('submit', { cancelable: true }));
}

describe('device credential form', () => {
  it('is asked for only when the host actually challenged', () => {
    expect(isAwaitingCredentials()).toBe(false);
    setConnectPhase('awaiting_consent');
    expect(isAwaitingCredentials()).toBe(false);
    setConnectPhase('awaiting_credentials');
    expect(isAwaitingCredentials()).toBe(true);
  });

  it('counts as an attempt in flight, so a second Connect cannot race it', () => {
    setConnectPhase('awaiting_credentials');
    expect(isConnecting()).toBe(true);
  });

  it('shows the code field only when the host asked for a second factor', () => {
    setConnectPhase('awaiting_credentials', null, false);
    render(credentialsPanel('en'), container);
    expect(container.querySelector('#device-code')).toBeNull();

    setConnectPhase('awaiting_credentials', null, true);
    render(credentialsPanel('en'), container);
    expect(container.querySelector('#device-code')).not.toBeNull();
  });

  it('sends the password and clears both fields at once', async () => {
    setConnectPhase('awaiting_credentials', null, true);
    render(credentialsPanel('en'), container);

    const password = container.querySelector<HTMLInputElement>('#device-password');
    const code = container.querySelector<HTMLInputElement>('#device-code');
    if (password) {
      password.value = 'correct horse battery staple';
    }
    if (code) {
      code.value = '287082';
    }
    submit();

    await vi.waitFor(() =>
      expect(invoke).toHaveBeenCalledWith('unattended_submit', {
        args: { password: 'correct horse battery staple', code: '287082' },
      }),
    );
    // Nothing is left on screen to read, and nothing in the module keeps it.
    expect(password?.value).toBe('');
    expect(code?.value).toBe('');
  });

  it('sends no code at all when none was asked for', async () => {
    setConnectPhase('awaiting_credentials', null, false);
    render(credentialsPanel('en'), container);
    const password = container.querySelector<HTMLInputElement>('#device-password');
    if (password) {
      password.value = 'opensesame';
    }
    submit();

    await vi.waitFor(() =>
      expect(invoke).toHaveBeenCalledWith('unattended_submit', {
        args: { password: 'opensesame', code: null },
      }),
    );
  });

  it('says which factor to retype, and nothing more', () => {
    setConnectPhase('awaiting_credentials', 'UNATTENDED_BAD_PASSWORD', false);
    render(credentialsPanel('en'), container);
    expect(container.querySelector('[data-testid="credentials-error"]')?.textContent?.trim()).toBe(
      t('en', 'creds.badPassword'),
    );

    setConnectPhase('awaiting_credentials', 'UNATTENDED_BAD_CODE', true);
    render(credentialsPanel('en'), container);
    expect(container.querySelector('[data-testid="credentials-error"]')?.textContent?.trim()).toBe(
      t('en', 'creds.badCode'),
    );
  });

  it('passes the lockout wait through as the host stated it', () => {
    setConnectPhase('awaiting_credentials', 'UNATTENDED_LOCKED_OUT', false, 300);
    render(credentialsPanel('en'), container);
    expect(container.querySelector('[data-testid="credentials-error"]')?.textContent).toContain('300');
  });

  it('a host that cannot decide at all ends the attempt rather than asking again', () => {
    setConnectPhase('awaiting_credentials', null, false);
    setConnectPhase('failed', 'UNATTENDED_UNAVAILABLE');
    expect(isAwaitingCredentials()).toBe(false);
  });

  // docs/bugs/02-connect-form.md, task 5: the form is a modal now, and the
  // password field should already have focus rather than making the user
  // click into it — but a re-render that changed nothing must not steal
  // focus back while they are typing a second guess.
  it('focuses the password field on open and after a fresh refusal, not on every re-render', async () => {
    setConnectPhase('awaiting_credentials', null, false);
    render(credentialsPanel('en'), container);
    await Promise.resolve();
    const password = container.querySelector<HTMLInputElement>('#device-password');
    expect(document.activeElement).toBe(password);

    password?.blur();
    render(credentialsPanel('en'), container);
    await Promise.resolve();
    expect(document.activeElement).not.toBe(password);

    setConnectPhase('awaiting_credentials', 'UNATTENDED_BAD_PASSWORD');
    render(credentialsPanel('en'), container);
    await Promise.resolve();
    expect(document.activeElement).toBe(password);
  });

  // docs/bugs/02-connect-form.md, task 5: Escape gives up on the attempt the
  // same way the Cancel button on the connect form does.
  it('cancels the outstanding attempt when Escape is pressed on the backdrop', async () => {
    setConnectPhase('awaiting_credentials', null, false);
    render(credentialsPanel('en'), container);
    container
      .querySelector('.credentials-backdrop')
      ?.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
    await vi.waitFor(() => expect(invoke).toHaveBeenCalledWith('connect_cancel'));
  });

  it('is labelled in both locales, and the password field is a password field', () => {
    for (const locale of SUPPORTED_LOCALES) {
      const scoped = document.createElement('div');
      document.body.appendChild(scoped);
      try {
        setConnectPhase('awaiting_credentials', null, true);
        render(credentialsPanel(locale), scoped);
        expect(scoped.querySelector('#credentials-heading')?.textContent?.trim()).toBe(
          t(locale, 'creds.heading'),
        );
        expect(scoped.querySelector<HTMLInputElement>('#device-password')?.type).toBe('password');
        for (const control of scoped.querySelectorAll<HTMLElement>('button, input')) {
          expect(control.tabIndex).not.toBe(-1);
        }
        for (const field of scoped.querySelectorAll<HTMLInputElement>('input')) {
          const label = scoped.querySelector(`label[for="${field.id}"]`);
          expect(label?.textContent?.trim()).toBeTruthy();
        }
      } finally {
        scoped.remove();
      }
    }
  });
});
