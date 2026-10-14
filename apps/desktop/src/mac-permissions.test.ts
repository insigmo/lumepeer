// The macOS permissions screen: when it opens by itself, what each row
// offers, and what closing it remembers.

import { render } from 'lit-html';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import {
  closeMacPermissions,
  hasMacPermissions,
  macPermissionsSettingsRow,
  macPermissionsView,
  onMacPermissionsStateChange,
  resetMacPermissions,
  shouldOpenAtStart,
  startMacPermissions,
  type MacPermissionsCommands,
  type MacPermissionsDto,
} from './mac-permissions';

const NONE: MacPermissionsDto = {
  screen_recording: 'not_granted',
  accessibility: 'not_granted',
  microphone: 'not_granted',
};
const ALL: MacPermissionsDto = {
  screen_recording: 'granted',
  accessibility: 'granted',
  microphone: 'granted',
};

let container: HTMLElement;

function fakeCommands(answer: MacPermissionsDto | null): MacPermissionsCommands & {
  read: ReturnType<typeof vi.fn>;
  request: ReturnType<typeof vi.fn>;
} {
  return {
    read: vi.fn().mockResolvedValue(answer),
    request: vi.fn().mockResolvedValue(undefined),
  };
}

function draw(): void {
  render(macPermissionsView('en'), container);
}

beforeEach(() => {
  container = document.createElement('div');
  document.body.appendChild(container);
  localStorage.clear();
  resetMacPermissions();
  onMacPermissionsStateChange(draw);
});

afterEach(() => {
  resetMacPermissions();
  container.remove();
  localStorage.clear();
});

describe('mac permissions: when the screen opens by itself', () => {
  it('opens on the first start with anything off', () => {
    expect(shouldOpenAtStart(NONE, null)).toBe(true);
  });

  it('stays shut when everything is on', () => {
    expect(shouldOpenAtStart(ALL, null)).toBe(false);
  });

  it('stays shut for what the person left off last time', () => {
    expect(shouldOpenAtStart({ ...ALL, microphone: 'denied' }, ['screen_recording', 'accessibility'])).toBe(
      false,
    );
  });

  it('opens again when something it saw on has gone, as after an update', () => {
    expect(shouldOpenAtStart({ ...ALL, accessibility: 'not_granted' }, ['screen_recording', 'accessibility'])).toBe(
      true,
    );
  });
});

describe('mac permissions: the screen', () => {
  it('shows nothing and offers nothing off macOS', async () => {
    await startMacPermissions(fakeCommands(null));
    expect(hasMacPermissions()).toBe(false);
    expect(container.querySelector('[data-testid="mac-permissions"]')).toBeNull();
    render(macPermissionsSettingsRow('en'), container);
    expect(container.querySelector('[data-testid="mac-permissions-open"]')).toBeNull();
  });

  it('lists the three, with an Allow button on each that is off', async () => {
    await startMacPermissions(fakeCommands({ ...NONE, accessibility: 'granted' }));
    const rows = [...container.querySelectorAll<HTMLElement>('.mac-perm')];
    expect(rows.map((row) => row.dataset.state)).toEqual(['not_granted', 'granted', 'not_granted']);
    expect(container.querySelector('[data-testid="mac-permission-ask-accessibility"]')).toBeNull();
    expect(
      container.querySelector('[data-testid="mac-permission-ask-screen_recording"]')?.textContent?.trim(),
    ).toBe('Allow');
    expect(container.querySelector('[data-testid="mac-permissions-done"]')?.textContent?.trim()).toBe('Later');
  });

  it('sends a refused microphone to System Settings', async () => {
    // A first start: nothing is remembered, and the microphone is off.
    await startMacPermissions(fakeCommands({ ...ALL, microphone: 'denied' }));
    expect(
      container.querySelector('[data-testid="mac-permission-ask-microphone"]')?.textContent?.trim(),
    ).toBe('Open System Settings');
  });

  it('asks for the one whose button was pressed and reads again', async () => {
    const commands = fakeCommands(NONE);
    await startMacPermissions(commands);
    commands.read.mockResolvedValue({ ...NONE, screen_recording: 'granted' });
    container.querySelector<HTMLButtonElement>('[data-testid="mac-permission-ask-screen_recording"]')?.click();
    expect(commands.request).toHaveBeenCalledWith('screen_recording');
    await vi.waitFor(() =>
      expect(
        container.querySelector<HTMLElement>('[data-testid="mac-permission-screen_recording"]')?.dataset.state,
      ).toBe('granted'),
    );
  });

  it('remembers what was on when closed, so a later start does not nag', async () => {
    await startMacPermissions(fakeCommands({ ...NONE, screen_recording: 'granted' }));
    closeMacPermissions();
    expect(container.querySelector('[data-testid="mac-permissions"]')).toBeNull();
    expect(localStorage.getItem('lumepeer.macPermissions.granted')).toBe('screen_recording');

    resetMacPermissions();
    onMacPermissionsStateChange(draw);
    await startMacPermissions(fakeCommands({ ...NONE, screen_recording: 'granted' }));
    expect(container.querySelector('[data-testid="mac-permissions"]')).toBeNull();
  });

  it('says Done once everything is on, reopened from the settings', async () => {
    await startMacPermissions(fakeCommands(ALL));
    expect(container.querySelector('[data-testid="mac-permissions"]')).toBeNull();
    render(macPermissionsSettingsRow('en'), container);
    container.querySelector<HTMLButtonElement>('[data-testid="mac-permissions-open"]')?.click();
    expect(container.querySelector('[data-testid="mac-permissions-done"]')?.textContent?.trim()).toBe('Done');
  });
});
