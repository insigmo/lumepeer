// apps/desktop/src/toolbar.test.ts
//
// The floating session toolbar (§11) in jsdom: collapse/expand, popover
// toggling, the monitor picker, the quality preset and the host-screen
// picker. Every command is a spy, so the state machine is tested without
// Tauri — the same shape `chat.test.ts` uses.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { t } from './i18n';
import {
  hostResolutionKey,
  hostResolutionsFrom,
  mountToolbar,
  refreshHzFor,
  renderToolbar,
  scalePercentFor,
  ToolbarState,
  type MonitorDto,
  type ToolbarCommands,
  type ToolbarControls,
  type ToolbarHooks,
} from './toolbar';
import { HOTKEYS, hotkeyLabel } from './view-hotkeys';
import { DISPLAY_MODES, type DisplayMode } from './view-window';

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
  monitorsList: ReturnType<typeof vi.fn>;
  monitorSelect: ReturnType<typeof vi.fn>;
  clipboardPull: ReturnType<typeof vi.fn>;
  viewSetScale: ReturnType<typeof vi.fn>;
  hostDisplayModes: ReturnType<typeof vi.fn>;
  hostDisplaySetMode: ReturnType<typeof vi.fn>;
} {
  const commands = {
    micToggle: vi.fn().mockResolvedValue(undefined),
    clipboardPull: vi.fn().mockResolvedValue(null),
    sasRequest: vi.fn().mockResolvedValue(undefined),
    recordRequest: vi.fn().mockResolvedValue(undefined),
    monitorsList: vi.fn().mockResolvedValue(MONITORS),
    monitorSelect: vi.fn().mockResolvedValue(undefined),
    viewSetScale: vi.fn().mockResolvedValue(undefined),
    hostDisplayModes: vi.fn().mockResolvedValue({ modes: [], reason: null }),
    hostDisplaySetMode: vi.fn().mockResolvedValue(undefined),
  };
  return commands;
}

/**
 * The window state the toolbar reads but does not own: chat visibility, the
 * display mode and full screen. Held here so a test can assert on what the
 * toolbar asked for rather than on what it decided.
 */
type FakeHooks = ToolbarHooks & {
  mode: DisplayMode;
  isFullscreen: boolean;
  hasCursorChannel: boolean;
  drawsLocalCursor: boolean;
  controls: ToolbarControls | null;
};

function fakeHooks(overrides: Partial<ToolbarHooks> = {}): FakeHooks {
  const hooks: FakeHooks = {
    mode: 'fit',
    isFullscreen: false,
    hasCursorChannel: true,
    drawsLocalCursor: true,
    controls: null,
    toggleChat: () => true,
    chatVisible: () => true,
    chatUnread: () => false,
    displayMode: () => hooks.mode,
    setDisplayMode: (mode) => {
      hooks.mode = mode;
    },
    zoomPercent: () => 100,
    zoomBy: () => {},
    fullscreen: () => hooks.isFullscreen,
    toggleFullscreen: () => {
      hooks.isFullscreen = !hooks.isFullscreen;
    },
    cursorChannel: () => hooks.hasCursorChannel,
    localCursor: () => hooks.drawsLocalCursor,
    toggleLocalCursor: () => {
      hooks.drawsLocalCursor = !hooks.drawsLocalCursor;
    },
    bind: (controls) => {
      hooks.controls = controls;
    },
    ...overrides,
  };
  return hooks;
}

function draw(
  state: Parameters<typeof renderToolbar>[1],
  commands: ToolbarCommands = fakeCommands(),
  hooks: ToolbarHooks = fakeHooks(),
): void {
  renderToolbar(container, state, 'en', hooks, {
    toggleCollapsed: () => {
      state.collapsed = !state.collapsed;
      draw(state, commands, hooks);
    },
    openPopover: (which) => {
      state.openPopover = which;
      draw(state, commands, hooks);
    },
    setDisplayMode: (mode) => {
      hooks.setDisplayMode(mode);
      draw(state, commands, hooks);
    },
    toggleFullscreen: () => {
      hooks.toggleFullscreen();
      draw(state, commands, hooks);
    },
    toggleLocalCursor: () => {
      hooks.toggleLocalCursor();
      draw(state, commands, hooks);
    },
    toggleMic: () => {},
    sendCad: () => {},
    askToRecord: () => {},
    pickMonitor: () => {},
    pickQuality: (preset) => {
      state.quality = preset;
      draw(state, commands, hooks);
    },
    pickHostResolution: (key) => {
      state.hostResolution = key;
      draw(state, commands, hooks);
    },
    zoomBy: () => {},
    beginDrag: () => {},
    nudge: () => {},
  });
}

/** A real state, with only the fields a render test cares about moved. */
function stateWith(overrides: Partial<ToolbarState> = {}): ToolbarState {
  return Object.assign(new ToolbarState(), overrides);
}

/** Every action stubbed out, for a render-only test that presses nothing. */
function noopActions(): Parameters<typeof renderToolbar>[4] {
  return {
    toggleCollapsed: () => {},
    openPopover: () => {},
    setDisplayMode: () => {},
    toggleFullscreen: () => {},
    toggleLocalCursor: () => {},
    toggleMic: () => {},
    sendCad: () => {},
    askToRecord: () => {},
    pickMonitor: () => {},
    pickQuality: () => {},
    pickHostResolution: () => {},
    zoomBy: () => {},
    beginDrag: () => {},
    nudge: () => {},
  };
}

describe('the floating session toolbar', () => {
  it('expanded shows every control: handle, settings, monitors, chat, mic, record, CAD, collapse', () => {
    const state = stateWith();
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
      fakeHooks({ chatVisible: () => false }),
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
      fakeHooks({ chatVisible: () => false }),
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

  it('offers full screen, and says which way the press goes', () => {
    const hooks = fakeHooks();
    const state = stateWith();
    draw(state, fakeCommands(), hooks);

    const button = (): HTMLButtonElement | null =>
      container.querySelector<HTMLButtonElement>('[data-testid="toolbar-fullscreen"]');
    expect(button()?.getAttribute('aria-pressed')).toBe('false');
    expect(button()?.getAttribute('aria-label')).toBe(t('en', 'toolbar.fullscreen'));

    button()?.click();
    expect(hooks.isFullscreen).toBe(true);
    // The label follows the state, so the way back out is named rather than
    // guessed at — full screen hides the window chrome that would say it.
    expect(button()?.getAttribute('aria-pressed')).toBe('true');
    expect(button()?.getAttribute('aria-label')).toBe(t('en', 'toolbar.fullscreen.exit'));
  });

  it('offers every display mode and asks the window rather than deciding', () => {
    const hooks = fakeHooks();
    const state = stateWith({ openPopover: 'settings' });
    draw(state, fakeCommands(), hooks);

    const select = container.querySelector<HTMLSelectElement>(
      '[data-testid="toolbar-display-mode"]',
    );
    expect(select).not.toBeNull();
    expect(Array.from(select?.options ?? []).map((option) => option.value)).toEqual([
      ...DISPLAY_MODES,
    ]);
    expect(select?.value).toBe('fit');

    if (select) {
      select.value = 'actual';
      select.dispatchEvent(new Event('change'));
    }
    expect(hooks.mode).toBe('actual');
  });

  it('offers the local-cursor switch only where the host sends a cursor', () => {
    const hooks = fakeHooks();
    const state = stateWith({ openPopover: 'settings' });
    draw(state, fakeCommands(), hooks);

    const toggle = (): HTMLInputElement | null =>
      container.querySelector<HTMLInputElement>('[data-testid="toolbar-local-cursor"]');
    expect(toggle()?.checked).toBe(true);
    toggle()?.click();
    expect(hooks.drawsLocalCursor).toBe(false);
    expect(toggle()?.checked).toBe(false);
  });

  it('says the pointer is inside the picture when the host cannot separate it', () => {
    // §11: on such a host there is nothing to switch, and offering a switch
    // that would draw a second cursor is worse than offering none.
    const hooks = fakeHooks();
    hooks.hasCursorChannel = false;
    const state = stateWith({ openPopover: 'settings' });
    draw(state, fakeCommands(), hooks);
    expect(container.querySelector('[data-testid="toolbar-local-cursor"]')).toBeNull();
    expect(
      container.querySelector('[data-testid="toolbar-cursor-embedded"]')?.textContent?.trim(),
    ).toBe(t('en', 'toolbar.settings.cursorEmbedded'));
  });

  it('lists the client hotkeys, because an invisible one is a bug', () => {
    const state = stateWith({ openPopover: 'settings' });
    draw(state);
    const list = container.querySelector('[data-testid="toolbar-hotkeys"]');
    expect(list).not.toBeNull();
    for (const entry of HOTKEYS) {
      expect(list?.textContent).toContain(hotkeyLabel(entry.code));
    }
  });

  it('hands the window controls it can drive the toolbar with', () => {
    const hooks = fakeHooks();
    const commands = fakeCommands();
    const stop = mountToolbar(container, 'en', 'host-ab12', commands, hooks);
    try {
      expect(hooks.controls).not.toBeNull();
      expect(container.querySelector('[data-testid="toolbar-collapse"]')).not.toBeNull();
      // The `toggle-toolbar` hotkey goes through exactly this.
      hooks.controls?.toggleCollapsed();
      expect(container.querySelector('[data-testid="toolbar-expand"]')).not.toBeNull();
      expect(container.querySelector('[data-testid="toolbar-collapse"]')).toBeNull();
    } finally {
      stop();
    }
  });

  it('collapsed hides every control but the handle and the expand button', () => {
    const state = stateWith({ collapsed: true });
    draw(state);
    expect(container.querySelector('[data-testid="toolbar-expand"]')).not.toBeNull();
    for (const id of [
      'toolbar-settings',
      'toolbar-fullscreen',
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

  it('marks the chat button while a message is unread, and only while the panel is closed', () => {
    const state = stateWith();
    let visible = false;
    let unread = true;
    const hooks = fakeHooks({ chatVisible: () => visible, chatUnread: () => unread });

    draw(state, fakeCommands(), hooks);
    const button = (): HTMLButtonElement =>
      container.querySelector<HTMLButtonElement>('[data-testid="toolbar-chat"]')!;
    expect(button().getAttribute('aria-label')).toBe(t('en', 'toolbar.chat.unread'));

    // Opening the panel is what reads the message; the mark goes with it.
    visible = true;
    unread = false;
    draw(state, fakeCommands(), hooks);
    expect(button().getAttribute('aria-label')).toBe(t('en', 'toolbar.chat'));
    expect(button().getAttribute('aria-pressed')).toBe('true');
  });

  it('the monitors popover lists what the host announced with 1-based numbers', () => {
    const state = stateWith({
      openPopover: 'monitors',
      monitors: MONITORS,
      activeMonitor: 1,
});
    draw(state);
    const options = container.querySelectorAll('[data-testid="toolbar-monitor-option"]');
    expect(options).toHaveLength(2);
    expect(options[0]?.textContent).toContain('Screen 1');
    expect(options[1]?.textContent).toContain('Screen 2');
    // The active monitor is marked for the screen reader and the eye.
    expect(options[1]?.getAttribute('aria-pressed')).toBe('true');
  });

  it('the monitor button shows the ordinal of the watched monitor', () => {
    const state = stateWith({ activeMonitor: 1 });
    draw(state);
    const button = container.querySelector('[data-testid="toolbar-monitors"]');
    expect(button?.textContent).toContain('2');
  });

  describe('scalePercentFor', () => {
    const monitor = (width: number, height: number): MonitorDto => ({
      id: 0,
      width,
      height,
      primary: true,
    });

    it('quality is always 100%, regardless of the monitor', () => {
      expect(scalePercentFor('quality', undefined)).toBe(100);
      expect(scalePercentFor('quality', monitor(1920, 1080))).toBe(100);
    });

    it('performance is always the ABR floor, regardless of the monitor', () => {
      expect(scalePercentFor('performance', undefined)).toBe(50);
      expect(scalePercentFor('performance', monitor(3840, 2160))).toBe(50);
    });

    it('balance is 720p worked out from the monitor height', () => {
      expect(scalePercentFor('balance', monitor(1920, 1080))).toBe(67);
      expect(scalePercentFor('balance', monitor(2560, 1440))).toBe(50);
    });

    it('balance does not cap a screen already at or below 720p', () => {
      expect(scalePercentFor('balance', monitor(1280, 720))).toBe(100);
      expect(scalePercentFor('balance', monitor(1024, 600))).toBe(100);
      // Nothing is known about the screen yet: no cap rather than a guess.
      expect(scalePercentFor('balance', undefined)).toBe(100);
    });

    it('balance clamps to the ABR floor rather than crossing it', () => {
      // A 4K screen cannot express 720p without a ceiling under 50%, so it
      // gets the floor — never 100, which would spend *more* than the preset
      // was chosen to spend.
      expect(scalePercentFor('balance', monitor(3840, 2160))).toBe(50);
    });

    it('the three presets stay ordered on every screen', () => {
      for (const screen of [
        monitor(1280, 720),
        monitor(1920, 1080),
        monitor(2560, 1440),
        monitor(3840, 2160),
      ]) {
        expect(scalePercentFor('performance', screen)).toBeLessThanOrEqual(
          scalePercentFor('balance', screen),
        );
        expect(scalePercentFor('balance', screen)).toBeLessThanOrEqual(
          scalePercentFor('quality', screen),
        );
      }
    });
  });

  describe('refreshHzFor', () => {
    // The five rates a real 4096x2160 mode list offers.
    const rates = [30, 29, 25, 24, 23];

    it('quality takes the highest rate, performance the middle one', () => {
      expect(refreshHzFor('quality', rates)).toBe(30);
      expect(refreshHzFor('performance', rates)).toBe(25);
    });

    it('balance sits between the middle and the highest', () => {
      expect(refreshHzFor('balance', rates)).toBe(29);
      // Eight rates, as a real 3840x2160 list has: middle 29, highest 60,
      // and balance halfway between those two.
      expect(refreshHzFor('balance', [23, 24, 25, 29, 30, 50, 59, 60])).toBe(50);
    });

    it('a single rate is what every preset gets', () => {
      for (const preset of ['performance', 'balance', 'quality'] as const) {
        expect(refreshHzFor(preset, [60])).toBe(60);
      }
    });

    it('no rates at all is null, not a guess', () => {
      expect(refreshHzFor('quality', [])).toBeNull();
    });
  });

  describe('hostResolutionsFrom', () => {
    it('folds one entry per refresh rate down to one entry per resolution', () => {
      const folded = hostResolutionsFrom([
        { id: 0, width: 3840, height: 2160, refresh_hz: 60 },
        { id: 1, width: 3840, height: 2160, refresh_hz: 30 },
        { id: 2, width: 1920, height: 1080, refresh_hz: 60 },
      ]);
      expect(folded.map((entry) => hostResolutionKey(entry))).toEqual([
        '3840x2160',
        '1920x1080',
      ]);
      expect(folded[0]?.modes.map((mode) => mode.refresh_hz)).toEqual([60, 30]);
    });

    it('an empty list folds to nothing', () => {
      expect(hostResolutionsFrom([])).toEqual([]);
    });
  });

  it('offers all three quality presets whatever the watched monitor is', () => {
    // Unlike the picture-resolution list this replaced, nothing is filtered
    // out by screen size: every preset means something on every screen.
    for (const monitors of [
      [{ id: 0, width: 1280, height: 720, primary: true }],
      [{ id: 0, width: 3840, height: 2160, primary: true }],
      [],
    ]) {
      const scoped = document.createElement('div');
      document.body.appendChild(scoped);
      try {
        const state = stateWith({ openPopover: 'settings', monitors, activeMonitor: 0 });
        renderToolbar(scoped, state, 'en', fakeHooks(), noopActions());
        const select = scoped.querySelector<HTMLSelectElement>('[data-testid="toolbar-quality"]');
        expect(Array.from(select?.options ?? []).map((option) => option.value)).toEqual([
          'performance',
          'balance',
          'quality',
        ]);
        expect(select?.value).toBe('quality');
      } finally {
        scoped.remove();
      }
    }
  });

  it('no longer offers a separate picture-resolution list', () => {
    const state = stateWith({ openPopover: 'settings' });
    draw(state);
    expect(container.querySelector('[data-testid="toolbar-resolution"]')).toBeNull();
  });

  it('picking a preset asks the host for the percentage worked out from the watched monitor', async () => {
    const commands = fakeCommands();
    commands.monitorsList.mockResolvedValue([{ id: 0, width: 1920, height: 1080, primary: true }]);
    const stop = mountToolbar(
      container,
      'en',
      'host-ab12',
      commands,
      fakeHooks({ chatVisible: () => false }),
    );
    try {
      container.querySelector<HTMLButtonElement>('[data-testid="toolbar-settings"]')?.click();
      await vi.waitFor(() => expect(commands.monitorsList).toHaveBeenCalled());
      const select = container.querySelector<HTMLSelectElement>('[data-testid="toolbar-quality"]');
      expect(select).not.toBeNull();
      if (select) {
        select.value = 'balance';
        select.dispatchEvent(new Event('change'));
      }
      // 720 of a 1080-tall screen.
      await vi.waitFor(() => expect(commands.viewSetScale).toHaveBeenCalledWith('host-ab12', 67));
    } finally {
      stop();
    }
  });

  it('a preset also moves the host screen to its refresh rate, at the size it is already at', async () => {
    const commands = fakeCommands();
    commands.monitorsList.mockResolvedValue([{ id: 0, width: 3840, height: 2160, primary: true }]);
    commands.hostDisplayModes.mockResolvedValue({
      modes: [
        { id: 0, width: 3840, height: 2160, refresh_hz: 60 },
        { id: 1, width: 3840, height: 2160, refresh_hz: 30 },
        { id: 2, width: 1920, height: 1080, refresh_hz: 60 },
      ],
      reason: null,
    });
    const stop = mountToolbar(
      container,
      'en',
      'host-ab12',
      commands,
      fakeHooks({ chatVisible: () => false }),
    );
    try {
      container.querySelector<HTMLButtonElement>('[data-testid="toolbar-settings"]')?.click();
      await vi.waitFor(() => expect(commands.hostDisplayModes).toHaveBeenCalled());
      const select = container.querySelector<HTMLSelectElement>('[data-testid="toolbar-quality"]');
      if (select) {
        select.value = 'performance';
        select.dispatchEvent(new Event('change'));
      }
      // Two rates at 3840x2160, so the middle one is the lower: mode 1.
      // Nobody picked a host resolution, so it is the watched monitor's own
      // size that the rate is chosen within — never the 1920x1080 entry.
      await vi.waitFor(() => expect(commands.hostDisplaySetMode).toHaveBeenCalledWith('host-ab12', 1));
    } finally {
      stop();
    }
  });

  it('leaves the host screen alone when it announced no modes of its own', async () => {
    const commands = fakeCommands();
    commands.hostDisplayModes.mockResolvedValue({ modes: [], reason: 'not_granted' });
    const stop = mountToolbar(
      container,
      'en',
      'host-ab12',
      commands,
      fakeHooks({ chatVisible: () => false }),
    );
    try {
      container.querySelector<HTMLButtonElement>('[data-testid="toolbar-settings"]')?.click();
      await vi.waitFor(() => expect(commands.hostDisplayModes).toHaveBeenCalled());
      const select = container.querySelector<HTMLSelectElement>('[data-testid="toolbar-quality"]');
      if (select) {
        select.value = 'performance';
        select.dispatchEvent(new Event('change'));
      }
      await vi.waitFor(() => expect(commands.viewSetScale).toHaveBeenCalled());
      expect(commands.hostDisplaySetMode).not.toHaveBeenCalled();
    } finally {
      stop();
    }
  });

  it('the host-resolution warning note is always shown next to the quality preset (docs/bugs/16-host-display-mode.md #4)', () => {
    const state = stateWith({ openPopover: 'settings' });
    draw(state);
    const warning = container.querySelector('[data-testid="toolbar-host-resolution-warning"]');
    expect(warning?.textContent).toContain('host computer itself');
  });

  it('an empty host-display-modes list shows the reason rather than a blank select (docs/bugs/16-host-display-mode.md #4)', () => {
    for (const [reason, expected] of [
      ['not_granted', 'has not allowed'],
      ['platform_unsupported', 'cannot change'],
      ['no_modes_reported', 'no available resolutions'],
    ] as const) {
      const scoped = document.createElement('div');
      document.body.appendChild(scoped);
      try {
        const state = stateWith({
          openPopover: 'settings',
          hostDisplayModes: { modes: [], reason },
        });
        renderToolbar(scoped, state, 'en', fakeHooks(), noopActions());
        expect(scoped.querySelector('[data-testid="toolbar-host-resolution"]')).toBeNull();
        const empty = scoped.querySelector('[data-testid="toolbar-host-resolution-empty"]');
        expect(empty?.textContent).toContain(expected);
      } finally {
        scoped.remove();
      }
    }
  });

  it('lists each host resolution once, without its refresh rates (docs/bugs/16-host-display-mode.md #4)', () => {
    const state = stateWith({
      openPopover: 'settings',
      hostDisplayModes: {
        modes: [
          { id: 0, width: 3840, height: 2160, refresh_hz: 60 },
          { id: 1, width: 3840, height: 2160, refresh_hz: 30 },
          { id: 2, width: 3840, height: 2160, refresh_hz: 24 },
          { id: 3, width: 1920, height: 1080, refresh_hz: 60 },
        ],
        reason: null,
      },
    });
    draw(state);
    const options = Array.from(
      container.querySelectorAll<HTMLOptionElement>('[data-testid="toolbar-host-resolution"] option'),
    );
    expect(options.map((option) => option.value)).toEqual(['3840x2160', '1920x1080']);
    for (const option of options) {
      expect(option.textContent).not.toContain('Hz');
    }
  });

  it('starts on the resolution the host computer is already at', () => {
    // Nothing has been picked, and the host announces no current mode of its
    // own — but the monitor it is streaming says its size, and that is it.
    const state = stateWith({
      openPopover: 'settings',
      monitors: [{ id: 0, width: 1920, height: 1080, primary: true }],
      activeMonitor: 0,
      hostDisplayModes: {
        modes: [
          { id: 0, width: 3840, height: 2160, refresh_hz: 60 },
          { id: 1, width: 1920, height: 1080, refresh_hz: 60 },
        ],
        reason: null,
      },
    });
    draw(state);
    const select = container.querySelector<HTMLSelectElement>(
      '[data-testid="toolbar-host-resolution"]',
    );
    expect(select?.value).toBe('1920x1080');
  });

  it('picking a host resolution switches the host at the preset\'s refresh rate (docs/bugs/16-host-display-mode.md #4)', async () => {
    const commands = fakeCommands();
    commands.hostDisplayModes.mockResolvedValue({
      modes: [
        { id: 0, width: 1920, height: 1080, refresh_hz: 60 },
        { id: 1, width: 2560, height: 1440, refresh_hz: 144 },
        { id: 2, width: 2560, height: 1440, refresh_hz: 60 },
        { id: 3, width: 2560, height: 1440, refresh_hz: 30 },
      ],
      reason: null,
    });
    const stop = mountToolbar(
      container,
      'en',
      'host-ab12',
      commands,
      fakeHooks({ chatVisible: () => false }),
    );
    try {
      container.querySelector<HTMLButtonElement>('[data-testid="toolbar-settings"]')?.click();
      await vi.waitFor(() =>
        expect(
          container.querySelectorAll('[data-testid="toolbar-host-resolution"] option').length,
        ).toBe(2),
      );
      const select = container.querySelector<HTMLSelectElement>(
        '[data-testid="toolbar-host-resolution"]',
      );
      expect(select).not.toBeNull();
      if (select) {
        select.value = '2560x1440';
        select.dispatchEvent(new Event('change'));
      }
      // The preset is still `quality`, which takes the highest rate on offer
      // at that size: 144 Hz, mode 1.
      await vi.waitFor(() =>
        expect(commands.hostDisplaySetMode).toHaveBeenCalledWith('host-ab12', 1),
      );
    } finally {
      stop();
    }
  });

  it('a monitor switch recalculates the ceiling the preset maps to on the new screen', async () => {
    const commands = fakeCommands();
    commands.monitorsList.mockResolvedValue([
      { id: 0, width: 1920, height: 1080, primary: true },
      { id: 1, width: 1280, height: 720, primary: false },
    ]);
    const stop = mountToolbar(
      container,
      'en',
      'host-ab12',
      commands,
      fakeHooks({ chatVisible: () => false }),
    );
    try {
      container.querySelector<HTMLButtonElement>('[data-testid="toolbar-settings"]')?.click();
      await vi.waitFor(() => expect(commands.monitorsList).toHaveBeenCalled());
      const select = container.querySelector<HTMLSelectElement>('[data-testid="toolbar-quality"]');
      if (select) {
        select.value = 'balance';
        select.dispatchEvent(new Event('change'));
      }
      await vi.waitFor(() => expect(commands.viewSetScale).toHaveBeenCalledWith('host-ab12', 67));
      commands.viewSetScale.mockClear();

      container.querySelector<HTMLButtonElement>('[data-testid="toolbar-monitors"]')?.click();
      await vi.waitFor(() => {
        const option = container.querySelectorAll('[data-testid="toolbar-monitor-option"]')[1];
        expect(option).not.toBeUndefined();
      });
      container
        .querySelectorAll<HTMLButtonElement>('[data-testid="toolbar-monitor-option"]')[1]
        ?.click();
      await vi.waitFor(() => expect(commands.monitorSelect).toHaveBeenCalledWith('host-ab12', 1));
      // A 1280x720 screen has nothing for `balance` to cap: the same preset
      // now means no cap at all, rather than a ceiling the old screen's size
      // happened to imply.
      await vi.waitFor(() => expect(commands.viewSetScale).toHaveBeenCalledWith('host-ab12', 100));
    } finally {
      stop();
    }
  });

  it('a monitor switch re-asks for the new screen\'s own display modes', async () => {
    const commands = fakeCommands();
    commands.hostDisplayModes.mockResolvedValue({
      modes: [{ id: 0, width: 1920, height: 1080, refresh_hz: 60 }],
      reason: null,
    });
    const stop = mountToolbar(
      container,
      'en',
      'host-ab12',
      commands,
      fakeHooks({ chatVisible: () => false }),
    );
    try {
      container.querySelector<HTMLButtonElement>('[data-testid="toolbar-settings"]')?.click();
      await vi.waitFor(() => expect(commands.hostDisplayModes).toHaveBeenCalledTimes(1));

      container.querySelector<HTMLButtonElement>('[data-testid="toolbar-monitors"]')?.click();
      await vi.waitFor(() =>
        expect(
          container.querySelectorAll('[data-testid="toolbar-monitor-option"]')[1],
        ).not.toBeUndefined(),
      );
      container
        .querySelectorAll<HTMLButtonElement>('[data-testid="toolbar-monitor-option"]')[1]
        ?.click();
      await vi.waitFor(() => expect(commands.monitorSelect).toHaveBeenCalledWith('host-ab12', 1));

      // The list belonged to the monitor that was being watched, so opening
      // the popover again asks for this one's.
      container.querySelector<HTMLButtonElement>('[data-testid="toolbar-settings"]')?.click();
      await vi.waitFor(() => expect(commands.hostDisplayModes).toHaveBeenCalledTimes(2));
    } finally {
      stop();
    }
  });

  it('no longer offers a file button or a clipboard indicator', () => {
    // Files travel on the ordinary Ctrl+C/Ctrl+V (ADR 0047) and clipboard
    // sync runs by itself while the grants are live (ADR 0046).
    draw(stateWith());
    for (const id of ['toolbar-file', 'toolbar-clipboard']) {
      expect(container.querySelector(`[data-testid="${id}"]`)).toBeNull();
    }
  });

  it('the Ctrl+Alt+Del button is never disabled ahead of an answer', async () => {
    // The old `sas_available` gate asked whether the *guest's* machine was
    // Windows, which says nothing about the host that has to deliver the
    // sequence. Asking and reporting is the only honest shape.
    const commands = fakeCommands();
    const stop = mountToolbar(
      container,
      'en',
      'host-ab12',
      commands,
      fakeHooks({ chatVisible: () => false }),
    );
    try {
      const cad = container.querySelector<HTMLButtonElement>('[data-testid="toolbar-cad"]');
      expect(cad?.disabled).toBe(false);
      cad?.click();
      await vi.waitFor(() => expect(commands.sasRequest).toHaveBeenCalledWith('host-ab12'));
    } finally {
      stop();
    }
  });

  it('offers a zoom stepper only while the picture size is Zoom', () => {
    const hooks = fakeHooks();
    const state = stateWith({ openPopover: 'settings' });
    draw(state, fakeCommands(), hooks);
    expect(container.querySelector('[data-testid="toolbar-zoom"]')).toBeNull();

    hooks.mode = 'scaled';
    draw(state, fakeCommands(), hooks);
    expect(container.querySelector('[data-testid="toolbar-zoom"]')).not.toBeNull();
    expect(
      container.querySelector('[data-testid="toolbar-zoom-value"]')?.textContent,
    ).toContain('100%');
  });

  it('every visible button carries an aria-label, none disabled without reason', () => {
    const state = stateWith();
    draw(state);
    for (const button of Array.from(container.querySelectorAll('button'))) {
      expect(button.getAttribute('aria-label')).not.toBeNull();
      expect(button.disabled).toBe(false);
    }
  });
});
