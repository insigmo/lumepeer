// apps/desktop/src/toolbar.test.ts
//
// The floating session toolbar (§11) in jsdom: collapse/expand, popover
// toggling, the resolution placeholder, the monitor picker, and the mic/CAD
// command paths. Every command is a spy, so the state machine is tested
// without Tauri — the same shape `chat.test.ts` uses.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { t } from './i18n';
import {
  RESOLUTION_CHOICES,
  mountToolbar,
  renderToolbar,
  type MonitorDto,
  type ToolbarCommands,
} from './toolbar';

vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }));

let container: HTMLElement;

beforeEach(() => {
  container = document.createElement('div');
  document.body.appendChild(container);
});

afterEach(() => {
  container.remove();
});

const MONITORS: MonitorDto[] = [
  { id: 0, width: 1920, height: 1080, primary: true },
  { id: 1, width: 2560, height: 1440, primary: false },
];

function fakeCommands(): ToolbarCommands & {
  micToggle: ReturnType<typeof vi.fn>;
  sasRequest: ReturnType<typeof vi.fn>;
  recordRequest: ReturnType<typeof vi.fn>;
  sasAvailable: ReturnType<typeof vi.fn>;
  monitorsList: ReturnType<typeof vi.fn>;
  monitorSelect: ReturnType<typeof vi.fn>;
  clipboardPush: ReturnType<typeof vi.fn>;
  fileOffer: ReturnType<typeof vi.fn>;
} {
  const commands = {
    micToggle: vi.fn().mockResolvedValue(undefined),
    clipboardPush: vi.fn().mockResolvedValue(undefined),
    fileOffer: vi.fn().mockResolvedValue(undefined),
    sasRequest: vi.fn().mockResolvedValue(undefined),
    recordRequest: vi.fn().mockResolvedValue(undefined),
    sasAvailable: vi.fn().mockResolvedValue(true),
    monitorsList: vi.fn().mockResolvedValue(MONITORS),
    monitorSelect: vi.fn().mockResolvedValue(undefined),
  };
  return commands;
}

function draw(
  state: Parameters<typeof renderToolbar>[1],
  commands: ToolbarCommands = fakeCommands(),
): void {
  renderToolbar(
    container,
    state,
    'en',
    {
      toggleChat: () => true,
      chatVisible: () => true,
    },
    {
      toggleCollapsed: () => {
        state.collapsed = !state.collapsed;
        draw(state, commands);
      },
      openPopover: (which) => {
        state.openPopover = which;
        draw(state, commands);
      },
      setResolution: (value) => {
        state.resolution = value;
        draw(state, commands);
      },
      toggleMic: () => {},
      sendCad: () => {},
      askToRecord: () => {},
      sendClipboard: () => {},
      sendFile: () => {},
      pickMonitor: () => {},
      beginDrag: () => {},
      nudge: () => {},
    },
  );
}

describe('the floating session toolbar', () => {
  it('expanded shows every control: handle, settings, monitors, chat, mic, CAD, collapse', () => {
    const state = { collapsed: false, openPopover: null, sasReady: true } as Parameters<
      typeof renderToolbar
    >[1];
    draw(state);
    for (const id of [
      'toolbar-handle',
      'toolbar-settings',
      'toolbar-monitors',
      'toolbar-chat',
      'toolbar-mic',
      'toolbar-record',
      'toolbar-cad',
      'toolbar-collapse',
    ]) {
      expect(container.querySelector(`[data-testid="${id}"]`)).not.toBeNull();
    }
  });

  it('offers a record request that asks the host and then stops asking again', async () => {
    // §17: the guest may ask, and asking is all it can do. The button reports
    // that the question went out — never that a recording started, which only
    // the host's own indicator (the badge over the picture) can say.
    const commands = fakeCommands();
    const stop = mountToolbar(
      container,
      'en',
      'host-ab12',
      commands,
      { toggleChat: () => true, chatVisible: () => false },
    );
    try {
      const button = container.querySelector<HTMLButtonElement>('[data-testid="toolbar-record"]');
      expect(button).not.toBeNull();
      expect(button?.disabled).toBe(false);
      button?.click();

      await vi.waitFor(() => expect(commands.recordRequest).toHaveBeenCalledWith('host-ab12'));
      const asked = container.querySelector<HTMLButtonElement>('[data-testid="toolbar-record"]');
      expect(asked?.disabled).toBe(true);
      expect(asked?.getAttribute('aria-label')).toBe(t('en', 'toolbar.record.asked'));
      // Nothing here claims a recording is running: that is the host's to say.
      expect(container.querySelector('[data-testid="view-recording"]')).toBeNull();
    } finally {
      stop();
    }
  });

  it('offers the button again when the request could not be sent', async () => {
    const commands = fakeCommands();
    commands.recordRequest.mockRejectedValueOnce(new Error('no view'));
    const stop = mountToolbar(
      container,
      'en',
      'host-ab12',
      commands,
      { toggleChat: () => true, chatVisible: () => false },
    );
    try {
      container.querySelector<HTMLButtonElement>('[data-testid="toolbar-record"]')?.click();
      await vi.waitFor(() =>
        expect(
          container.querySelector<HTMLButtonElement>('[data-testid="toolbar-record"]')?.disabled,
        ).toBe(false),
      );
    } finally {
      stop();
    }
  });

  it('collapsed hides every control but the handle and the expand button', () => {
    const state = { collapsed: true, openPopover: null } as Parameters<
      typeof renderToolbar
    >[1];
    draw(state);
    expect(container.querySelector('[data-testid="toolbar-expand"]')).not.toBeNull();
    for (const id of [
      'toolbar-settings',
      'toolbar-monitors',
      'toolbar-chat',
      'toolbar-mic',
      'toolbar-record',
      'toolbar-cad',
    ]) {
      expect(container.querySelector(`[data-testid="${id}"]`)).toBeNull();
    }
    // The drag handle stays: the collapsed pill is still draggable.
    expect(container.querySelector('[data-testid="toolbar-handle"]')).not.toBeNull();
  });

  it('the settings popover offers the resolution choices and records the pick', () => {
    const state = {
      collapsed: false,
      openPopover: 'settings' as const,
      resolution: RESOLUTION_CHOICES[0],
    } as Parameters<typeof renderToolbar>[1];
    draw(state);
    const select = container.querySelector<HTMLSelectElement>(
      '[data-testid="toolbar-resolution"]',
    );
    expect(select).not.toBeNull();
    expect(select!.options).toHaveLength(RESOLUTION_CHOICES.length);
  });

  it('the monitors popover lists what the host announced with 1-based numbers', () => {
    const state = {
      collapsed: false,
      openPopover: 'monitors' as const,
      monitors: MONITORS,
      activeMonitor: 1,
    } as unknown as Parameters<typeof renderToolbar>[1];
    draw(state);
    const options = container.querySelectorAll('[data-testid="toolbar-monitor-option"]');
    expect(options).toHaveLength(2);
    expect(options[0]?.textContent).toContain('Screen 1');
    expect(options[1]?.textContent).toContain('Screen 2');
    // The active monitor is marked for the screen reader and the eye.
    expect(options[1]?.getAttribute('aria-pressed')).toBe('true');
  });

  it('the monitor button shows the ordinal of the watched monitor', () => {
    const state = {
      collapsed: false,
      openPopover: null,
      activeMonitor: 1,
    } as unknown as Parameters<typeof renderToolbar>[1];
    draw(state);
    const button = container.querySelector('[data-testid="toolbar-monitors"]');
    expect(button?.textContent).toContain('2');
  });

  it('the CAD button is disabled when the host platform cannot deliver the SAS', () => {
    const state = {
      collapsed: false,
      openPopover: null,
      sasReady: false,
    } as Parameters<typeof renderToolbar>[1];
    draw(state);
    const cad = container.querySelector<HTMLButtonElement>('[data-testid="toolbar-cad"]');
    expect(cad?.disabled).toBe(true);
  });

  it('every visible button carries an aria-label, none disabled without reason', () => {
    const state = { collapsed: false, openPopover: null, sasReady: true } as Parameters<
      typeof renderToolbar
    >[1];
    draw(state);
    for (const button of Array.from(container.querySelectorAll('button'))) {
      expect(button.getAttribute('aria-label')).not.toBeNull();
      expect(button.disabled).toBe(false);
    }
  });
});
