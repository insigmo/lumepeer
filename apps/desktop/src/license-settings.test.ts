// The license panel (ADR 0163).
//
// What these tests pin down: no license reads as the free plan rather than a
// problem, a key is only offered where this build can activate one, a refused
// key says why in words, and a held license can be given back.
import { html, render } from 'lit-html';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { t } from './i18n';
import {
  licenseSettings,
  onLicenseStateChange,
  resetLicenseSettings,
  type LicenseCommands,
  type LicenseStatus,
} from './license-settings';

const free: LicenseStatus = {
  plan: 'free',
  licensed: false,
  seconds_left: null,
  offline: false,
  reason: null,
  activation_available: true,
};
const pro: LicenseStatus = { ...free, plan: 'pro', licensed: true, seconds_left: 80_000 };

let container: HTMLElement;
let commands: LicenseCommands;

function mount(): void {
  const paint = (): void => {
    render(html`${licenseSettings('en', commands)}`, container);
  };
  onLicenseStateChange(paint);
  paint();
}

async function settle(): Promise<void> {
  for (let i = 0; i < 10; i += 1) {
    await Promise.resolve();
  }
}

function byTestId(id: string): HTMLElement | null {
  return container.querySelector(`[data-testid="${id}"]`);
}

function submitKey(key: string): void {
  const field = container.querySelector<HTMLInputElement>('#license-key');
  if (!field) {
    throw new Error('no license key field');
  }
  field.value = key;
  container.querySelector('form')?.dispatchEvent(new SubmitEvent('submit', { cancelable: true }));
}

beforeEach(() => {
  resetLicenseSettings();
  container = document.createElement('div');
  document.body.appendChild(container);
  commands = {
    status: vi.fn().mockResolvedValue(free),
    activate: vi.fn().mockResolvedValue(pro),
    deactivate: vi.fn().mockResolvedValue(free),
  };
});

afterEach(() => {
  container.remove();
});

describe('license panel', () => {
  it('shows the free plan and offers a key field', async () => {
    mount();
    await settle();
    expect(byTestId('license-plan')?.textContent).toBe(t('en', 'license.plan', t('en', 'license.plan.free')));
    expect(byTestId('license-activate')).not.toBeNull();
    expect(byTestId('license-deactivate')).toBeNull();
  });

  it('offers no key field where this build cannot activate', async () => {
    commands.status = vi.fn().mockResolvedValue({ ...free, activation_available: false });
    mount();
    await settle();
    expect(byTestId('license-activate')).toBeNull();
    expect(container.textContent).toContain(t('en', 'license.unavailable'));
  });

  it('activates a key and switches to the paid plan', async () => {
    mount();
    await settle();
    submitKey('  acct-key  ');
    await settle();
    expect(commands.activate).toHaveBeenCalledWith('acct-key');
    expect(byTestId('license-plan')?.textContent).toBe(t('en', 'license.plan', t('en', 'license.plan.pro')));
    expect(byTestId('license-deactivate')).not.toBeNull();
  });

  it('says in words why a key was refused', async () => {
    commands.activate = vi.fn().mockRejectedValue({ code: 'LICENSE_REJECTED', message: 'no' });
    mount();
    await settle();
    submitKey('bad');
    await settle();
    expect(byTestId('license-error')?.textContent).toBe(t('en', 'license.error.rejected'));
    expect(byTestId('license-plan')?.textContent).toBe(t('en', 'license.plan', t('en', 'license.plan.free')));
  });

  it('gives a held license back', async () => {
    commands.status = vi.fn().mockResolvedValue(pro);
    mount();
    await settle();
    byTestId('license-deactivate')?.click();
    await settle();
    expect(commands.deactivate).toHaveBeenCalled();
    expect(byTestId('license-activate')).not.toBeNull();
  });

  it('explains a lapsed license instead of showing the server text', async () => {
    commands.status = vi.fn().mockResolvedValue({ ...free, reason: 'seat displaced' });
    mount();
    await settle();
    expect(byTestId('license-lapsed')?.textContent).toBe(t('en', 'license.lapsed'));
    expect(container.textContent).not.toContain('seat displaced');
  });
});
