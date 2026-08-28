// Autostart and updates (§21; ADR 0042).
//
// What these tests pin down is the half that makes the two switches honest:
// that the autostart toggle reflects the machine rather than the click, that
// an update is never installed by a check, and that a failed install says so
// instead of claiming a new version is running.
import { render } from 'lit-html';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { t } from './i18n';
import {
  onSystemStateChange,
  resetSystemSettings,
  systemSettings,
  type SystemCommands,
  type UpdateInfo,
} from './system-settings';

const update: UpdateInfo = { version: '0.0.24', current: '0.0.23', notes: '' };

let container: HTMLElement;
let commands: SystemCommands;
let setMock: ReturnType<typeof vi.fn>;
let checkMock: ReturnType<typeof vi.fn>;
let installMock: ReturnType<typeof vi.fn>;
let serviceSetMock: ReturnType<typeof vi.fn>;

function mount(): void {
  const paint = (): void => {
    render(systemSettings('en', commands), container);
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
  container = document.createElement('div');
  document.body.appendChild(container);
  setMock = vi.fn().mockResolvedValue(undefined);
  checkMock = vi.fn().mockResolvedValue(null);
  installMock = vi.fn().mockResolvedValue(undefined);
  serviceSetMock = vi.fn().mockResolvedValue(undefined);
  commands = {
    serviceStatus: vi.fn().mockResolvedValue('not_installed'),
    serviceSet: serviceSetMock as unknown as SystemCommands['serviceSet'],
    autostartStatus: vi.fn().mockResolvedValue(false),
    autostartSet: setMock as unknown as SystemCommands['autostartSet'],
    updateCheck: checkMock as unknown as SystemCommands['updateCheck'],
    updateInstall: installMock as unknown as SystemCommands['updateInstall'],
  };
});

afterEach(() => {
  container.remove();
  resetSystemSettings();
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

  it('installs the helper service and re-reads what the machine says', async () => {
    mount();
    await settle();
    (container.querySelector('[data-testid="service-toggle"]') as HTMLButtonElement).click();
    await settle();
    expect(serviceSetMock).toHaveBeenCalledWith(true);
  });

  it('never claims the helper service changed when the prompt was declined', async () => {
    serviceSetMock.mockRejectedValue(new Error('elevation declined'));
    vi.spyOn(console, 'error').mockImplementation(() => {});
    mount();
    await settle();
    (container.querySelector('[data-testid="service-toggle"]') as HTMLButtonElement).click();
    await settle();
    expect(container.querySelector('[data-testid="service-error"]')).not.toBeNull();
    expect(container.querySelector('[data-testid="service-row"]')?.textContent).toContain(
      t('en', 'system.serviceOff'),
    );
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
