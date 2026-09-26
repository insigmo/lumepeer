// Autostart and updates (§21; ADR 0042).
//
// What these tests pin down is the half that makes the two switches honest:
// that the autostart toggle reflects the machine rather than the click, that
// an update is never installed by a check, and that a failed install says so
// instead of claiming a new version is running.
import { html, render } from 'lit-html';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { getStoredLocaleChoice, setStoredLocaleChoice, t } from './i18n';
import {
  appVersion,
  availableUpdate,
  checkForUpdatesAtLaunch,
  onSystemStateChange,
  resetSystemSettings,
  systemSettings,
  updateSettings,
  type SystemCommands,
  type UpdateInfo,
} from './system-settings';
import { getStoredTheme } from './theme';

const update: UpdateInfo = { version: '0.0.24', current: '0.0.23', notes: '' };

let container: HTMLElement;
let commands: SystemCommands;
let setMock: ReturnType<typeof vi.fn>;
let checkMock: ReturnType<typeof vi.fn>;
let installMock: ReturnType<typeof vi.fn>;

// General and About together, the way the two tabs split this module.
function mount(): void {
  const paint = (): void => {
    render(html`${systemSettings('en', commands)}${updateSettings('en', commands)}`, container);
  };
  onSystemStateChange(paint);
  paint();
}

async function settle(): Promise<void> {
  for (let i = 0; i < 10; i += 1) {
    await Promise.resolve();
  }
}

beforeEach(() => {
  resetSystemSettings();
  localStorage.clear();
  container = document.createElement('div');
  document.body.appendChild(container);
  setMock = vi.fn().mockResolvedValue(undefined);
  checkMock = vi.fn().mockResolvedValue(null);
  installMock = vi.fn().mockResolvedValue(undefined);
  commands = {
    autostartStatus: vi.fn().mockResolvedValue(false),
    autostartSet: setMock as unknown as SystemCommands['autostartSet'],
    updateCheck: checkMock as unknown as SystemCommands['updateCheck'],
    updateInstall: installMock as unknown as SystemCommands['updateInstall'],
    appVersion: vi.fn().mockResolvedValue('0.0.23'),
  };
});

afterEach(() => {
  container.remove();
  resetSystemSettings();
  localStorage.clear();
  delete document.documentElement.dataset.theme;
});

describe('system settings', () => {
  it('reads the machine, not a remembered value, on first render', async () => {
    commands.autostartStatus = vi.fn().mockResolvedValue(true);
    mount();
    await settle();
    expect(
      (container.querySelector('[data-testid="autostart-toggle"]') as HTMLInputElement).checked,
    ).toBe(true);
  });

  // Autostart is on from the first launch of an installed copy (ADR 0103),
  // so a panel with a default of its own would draw a cleared box on exactly
  // the machine where it is ticked. There is no default: the row is not there
  // until the machine has said.
  it('draws no autostart row until the machine has answered', async () => {
    let answer = (_enabled: boolean): void => {};
    commands.autostartStatus = vi.fn(
      () =>
        new Promise<boolean>((resolve) => {
          answer = resolve;
        }),
    );
    mount();
    await settle();
    expect(container.querySelector('[data-testid="autostart-toggle"]')).toBeNull();

    answer(true);
    await settle();
    expect(
      (container.querySelector('[data-testid="autostart-toggle"]') as HTMLInputElement).checked,
    ).toBe(true);
  });

  it('turns autostart on and off through the core', async () => {
    mount();
    await settle();
    const toggle = container.querySelector('[data-testid="autostart-toggle"]') as HTMLInputElement;
    toggle.checked = true;
    toggle.dispatchEvent(new Event('change'));
    await settle();
    expect(setMock).toHaveBeenCalledWith(true);

    const after = container.querySelector('[data-testid="autostart-toggle"]') as HTMLInputElement;
    after.checked = false;
    after.dispatchEvent(new Event('change'));
    await settle();
    expect(setMock).toHaveBeenLastCalledWith(false);
  });

  it('does not claim autostart is on when the machine refused', async () => {
    setMock.mockRejectedValue(new Error('registry is read-only'));
    vi.spyOn(console, 'error').mockImplementation(() => {});
    mount();
    await settle();
    const toggle = container.querySelector('[data-testid="autostart-toggle"]') as HTMLInputElement;
    toggle.checked = true;
    toggle.dispatchEvent(new Event('change'));
    await settle();

    expect(container.querySelector('[data-testid="autostart-error"]')).not.toBeNull();
    expect(
      (container.querySelector('[data-testid="autostart-toggle"]') as HTMLInputElement).checked,
    ).toBe(false);
  });

  it('says so when there is nothing newer', async () => {
    mount();
    await settle();
    (container.querySelector('[data-testid="update-check"]') as HTMLButtonElement).click();
    await settle();
    expect(container.querySelector('[data-testid="update-none"]')?.textContent).toBe(
      t('en', 'system.upToDate'),
    );
    expect(installMock).not.toHaveBeenCalled();
  });

  it('never installs from a check alone', async () => {
    checkMock.mockResolvedValue(update);
    mount();
    await settle();
    (container.querySelector('[data-testid="update-check"]') as HTMLButtonElement).click();
    await settle();

    expect(container.querySelector('[data-testid="update-found"]')?.textContent).toContain('0.0.24');
    expect(installMock).not.toHaveBeenCalled();

    (container.querySelector('[data-testid="update-install"]') as HTMLButtonElement).click();
    await settle();
    expect(installMock).toHaveBeenCalledTimes(1);
    expect(container.querySelector('[data-testid="update-installed"]')).not.toBeNull();
  });

  // The Ctrl+Alt+Del helper is registered at start-up and removed by the
  // uninstaller (`service_control.rs::ensure_installed`). It is not a
  // permission and it admits nobody, so this panel offers nothing about it.
  it('offers nothing about the helper service', () => {
    mount();
    expect(container.querySelector('[data-testid="service-row"]')).toBeNull();
    expect(container.querySelector('[data-testid="service-toggle"]')).toBeNull();
  });

  it('reports a refused install rather than a new version', async () => {
    checkMock.mockResolvedValue(update);
    installMock.mockRejectedValue(new Error('signature verification failed'));
    vi.spyOn(console, 'error').mockImplementation(() => {});
    mount();
    await settle();
    (container.querySelector('[data-testid="update-check"]') as HTMLButtonElement).click();
    await settle();
    (container.querySelector('[data-testid="update-install"]') as HTMLButtonElement).click();
    await settle();

    expect(container.querySelector('[data-testid="update-error"]')).not.toBeNull();
    expect(container.querySelector('[data-testid="update-installed"]')).toBeNull();
  });
});

describe('language picker', () => {
  it('shows "system default" selected when nothing is stored', () => {
    render(systemSettings('en', commands), container);
    const select = container.querySelector('[data-testid="language-select"]') as HTMLSelectElement;
    expect(select.value).toBe('system');
    expect(getStoredLocaleChoice()).toBeNull();
  });

  it('persists a manual choice and reports the resolved locale', () => {
    const onLocaleChange = vi.fn();
    render(systemSettings('en', commands, onLocaleChange), container);
    const select = container.querySelector('[data-testid="language-select"]') as HTMLSelectElement;
    select.value = 'ar';
    select.dispatchEvent(new Event('change'));

    expect(getStoredLocaleChoice()).toBe('ar');
    expect(onLocaleChange).toHaveBeenCalledWith('ar');
  });

  it('clears the stored choice when "system default" is picked again', () => {
    setStoredLocaleChoice('ar');
    const onLocaleChange = vi.fn();
    render(systemSettings('en', commands, onLocaleChange), container);
    const select = container.querySelector('[data-testid="language-select"]') as HTMLSelectElement;
    expect(select.value).toBe('ar');

    select.value = 'system';
    select.dispatchEvent(new Event('change'));

    expect(getStoredLocaleChoice()).toBeNull();
    expect(onLocaleChange).toHaveBeenCalledTimes(1);
  });
});

describe('theme picker (ADR 0117)', () => {
  it('starts on "system" and leaves the page to the system scheme', () => {
    mount();
    expect((container.querySelector('[data-testid="theme-system"]') as HTMLInputElement).checked).toBe(true);
    expect(document.documentElement.dataset.theme).toBeUndefined();
  });

  it('applies and remembers an explicit choice, and "system" clears it again', () => {
    mount();
    const dark = container.querySelector('[data-testid="theme-dark"]') as HTMLInputElement;
    dark.checked = true;
    dark.dispatchEvent(new Event('change'));
    expect(document.documentElement.dataset.theme).toBe('dark');
    expect(getStoredTheme()).toBe('dark');

    const system = container.querySelector('[data-testid="theme-system"]') as HTMLInputElement;
    system.checked = true;
    system.dispatchEvent(new Event('change'));
    expect(document.documentElement.dataset.theme).toBeUndefined();
    expect(getStoredTheme()).toBe('system');
  });
});

describe('version and updates (ADR 0117)', () => {
  it('shows the version the bundle reports', async () => {
    mount();
    await settle();
    expect(appVersion()).toBe('0.0.23');
    expect(container.querySelector('[data-testid="app-version"]')?.textContent).toBe(
      t('en', 'system.version', '0.0.23'),
    );
  });

  it('asks once at launch and offers what it found, without installing it', async () => {
    checkMock.mockResolvedValue(update);
    checkForUpdatesAtLaunch(commands);
    await settle();
    expect(checkMock).toHaveBeenCalledTimes(1);
    expect(availableUpdate()?.version).toBe('0.0.24');
    expect(installMock).not.toHaveBeenCalled();
  });

  it('does not ask at launch once that is switched off', async () => {
    mount();
    await settle();
    const toggle = container.querySelector('[data-testid="update-autocheck"]') as HTMLInputElement;
    expect(toggle.checked).toBe(true);
    toggle.checked = false;
    toggle.dispatchEvent(new Event('change'));

    checkForUpdatesAtLaunch(commands);
    await settle();
    expect(checkMock).not.toHaveBeenCalled();
  });

  it('keeps a failed launch-time check to itself', async () => {
    checkMock.mockRejectedValue(new Error('offline'));
    vi.spyOn(console, 'warn').mockImplementation(() => {});
    checkForUpdatesAtLaunch(commands);
    mount();
    await settle();
    expect(container.querySelector('[data-testid="update-error"]')).toBeNull();
  });

  it('says a build without an update channel has none, rather than that it failed', async () => {
    checkMock.mockRejectedValue({ code: 'UPDATE_OFF', message: 'no endpoint' });
    vi.spyOn(console, 'error').mockImplementation(() => {});
    mount();
    await settle();
    (container.querySelector('[data-testid="update-check"]') as HTMLButtonElement).click();
    await settle();
    expect(container.querySelector('[data-testid="update-error"]')?.textContent).toBe(
      t('en', 'system.updateOff'),
    );
  });

  it('stops offering an update once it is installed', async () => {
    checkMock.mockResolvedValue(update);
    mount();
    await settle();
    (container.querySelector('[data-testid="update-check"]') as HTMLButtonElement).click();
    await settle();
    expect(availableUpdate()).not.toBeNull();
    (container.querySelector('[data-testid="update-install"]') as HTMLButtonElement).click();
    await settle();
    expect(availableUpdate()).toBeNull();
  });
});
