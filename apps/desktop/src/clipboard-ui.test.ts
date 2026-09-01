// Clipboard sync as the two windows show it (design doc §9.2, §15; ADR 0030,
// ADR 0046).
//
// Both windows now sync automatically, and both are under the same rule:
// they may say a clipboard arrived, never what was in it — a panel or a
// toolbar that sits on screen all session is read by whoever walks past the
// machine, and clipboard content is the one payload §15 keeps off every
// surface. The guest's toolbar no longer pushes anything on a press
// (docs/bugs/10-clipboard-auto.md #1, #3): the clipboard icon is a status,
// and the only thing it does is notice the host's clipboard arriving.
import { render } from 'lit-html';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { CLIPBOARD_NOTE_MS, sessionStatus, type SessionStatus } from './session-status';
import {
  renderToolbar,
  ToolbarState,
  type ToolbarCommands,
  type ToolbarHooks,
} from './toolbar';

/**
 * The window state the toolbar reads but does not own. None of it matters to
 * what this file tests — only that the toolbar has somewhere to read it from.
 */
function toolbarHooks(): ToolbarHooks {
  return {
    toggleChat: () => true,
    chatVisible: () => false,
    chatUnread: () => false,
    displayMode: () => 'fit',
    setDisplayMode: () => {},
    fullscreen: () => false,
    toggleFullscreen: () => {},
    cursorChannel: () => false,
    localCursor: () => true,
    toggleLocalCursor: () => {},
    bind: () => {},
  };
}

const invoke = vi.fn().mockResolvedValue(undefined);

vi.mock('@tauri-apps/api/core', () => ({
  invoke: (...args: unknown[]) => invoke(...args),
}));

const activeSession: SessionStatus = {
  peer_label: 'guest-ab12',
  role: 'view_only',
  input: false,
  state: 'active',
  clipboard_read: true,
  clipboard_write: true,
  file_transfer: false,
  recording: false,
  display_mode: false,
  recording_active: false,
  record_request: false,
};

let container: HTMLElement;

beforeEach(() => {
  invoke.mockClear();
  container = document.createElement('div');
  document.body.appendChild(container);
});

afterEach(() => {
  container.remove();
});

describe('the host panel', () => {
  it('says a clipboard arrived without saying what it was', () => {
    const secret = 'correct horse battery staple';
    render(
      sessionStatus(
        [activeSession],
        'en',
        () => {},
        [],
        () => {},
        false,
        () => {},
        new Map([[activeSession.peer_label, Date.now()]]),
      ),
      container,
    );

    const note = container.querySelector('[data-testid="clipboard-note"]');
    expect(note).not.toBeNull();
    expect(note?.textContent?.trim()).toBe('Clipboard synced');
    expect(container.textContent).not.toContain(secret);
  });

  it('stops claiming a sync once the note has aged out', () => {
    render(
      sessionStatus(
        [activeSession],
        'en',
        () => {},
        [],
        () => {},
        false,
        () => {},
        new Map([[activeSession.peer_label, Date.now() - CLIPBOARD_NOTE_MS - 1]]),
      ),
      container,
    );
    expect(container.querySelector('[data-testid="clipboard-note"]')).toBeNull();
  });

  it('shows nothing for a session that has never synced', () => {
    render(sessionStatus([activeSession], 'en'), container);
    expect(container.querySelector('[data-testid="clipboard-note"]')).toBeNull();
  });
});

describe("the guest toolbar's clipboard indicator", () => {
  function commands(): ToolbarCommands & { clipboardPull: ReturnType<typeof vi.fn> } {
    return {
      micToggle: vi.fn().mockResolvedValue(undefined),
      clipboardPull: vi.fn().mockResolvedValue(null),
      fileOffer: vi.fn().mockResolvedValue(undefined),
      sasRequest: vi.fn().mockResolvedValue(undefined),
      recordRequest: vi.fn().mockResolvedValue(undefined),
      sasAvailable: vi.fn().mockResolvedValue(true),
      monitorsList: vi.fn().mockResolvedValue([]),
      monitorSelect: vi.fn().mockResolvedValue(undefined),
      viewSetScale: vi.fn().mockResolvedValue(undefined),
      hostDisplayModes: vi.fn().mockResolvedValue({ modes: [], reason: null }),
      hostDisplaySetMode: vi.fn().mockResolvedValue(undefined),
    };
  }

  function draw(state: ToolbarState): void {
    renderToolbar(container, state, 'en', toolbarHooks(), {
      toggleCollapsed: () => {},
      openPopover: () => {},
      setDisplayMode: () => {},
      toggleFullscreen: () => {},
      toggleLocalCursor: () => {},
      toggleMic: () => {},
      sendCad: () => {},
      askToRecord: () => {},
      sendFile: () => {},
      pickMonitor: () => {},
      pickResolution: () => {},
      pickHostDisplayMode: () => {},
      beginDrag: () => {},
      nudge: () => {},
    });
  }

  it('is a status, not a control, and named in both locales', () => {
    draw(new ToolbarState());
    const indicator = container.querySelector('[data-testid="toolbar-clipboard"]');
    expect(indicator).not.toBeNull();
    // Not a button: nothing responds to a click any more (docs/bugs/10-
    // clipboard-auto.md #3).
    expect(indicator?.tagName).not.toBe('BUTTON');
    expect(indicator?.getAttribute('aria-label')).toBe(
      'Clipboard sync is automatic in both directions',
    );
  });

  it('shows a note right after a sync and hides it once the note ages out', () => {
    const state = new ToolbarState();
    state.clipboardSyncedAt = Date.now();
    draw(state);
    const note = container.querySelector('[data-testid="toolbar-clipboard-note"]');
    expect(note).not.toBeNull();
    expect(note?.textContent?.trim()).toBe('Clipboard synced');

    state.clipboardSyncedAt = Date.now() - CLIPBOARD_NOTE_MS - 1;
    draw(state);
    expect(container.querySelector('[data-testid="toolbar-clipboard-note"]')).toBeNull();
  });

  it('shows nothing before the first sync', () => {
    draw(new ToolbarState());
    expect(container.querySelector('[data-testid="toolbar-clipboard-note"]')).toBeNull();
  });

  it('notices the host clipboard arriving without ever putting its text in the DOM', async () => {
    const secret = 'correct horse battery staple';
    const fake = commands();
    fake.clipboardPull.mockResolvedValue(secret);
    const { mountToolbar } = await import('./toolbar');
    // A fast poll so the test does not wait a full second for the real
    // default (docs/bugs/10-clipboard-auto.md #2).
    const stop = mountToolbar(container, 'en', 'host-99', fake, toolbarHooks(), 5);
    try {
      await vi.waitFor(() => {
        expect(
          container.querySelector('[data-testid="toolbar-clipboard-note"]'),
        ).not.toBeNull();
      });
      expect(container.textContent).not.toContain(secret);
    } finally {
      stop();
    }
  });
});
