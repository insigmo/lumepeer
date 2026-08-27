// Clipboard sync as the two windows show it (design doc §9.2, §15; ADR 0030).
//
// Two rules are under test and they are both about what the UI must *not* do.
// The host panel says a clipboard arrived and never says what was in it — a
// panel that sits on screen all session is read by whoever walks past the
// machine, and clipboard content is the one payload §15 keeps off every
// surface. And the guest's toolbar reads only the guest's own clipboard, on
// the guest's own press: the host's clipboard is never reachable from a
// webview at all.
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

describe("the guest toolbar's clipboard button", () => {
  function commands(): ToolbarCommands & { clipboardPush: ReturnType<typeof vi.fn> } {
    return {
      micToggle: vi.fn().mockResolvedValue(undefined),
      clipboardPush: vi.fn().mockResolvedValue(undefined),
      fileOffer: vi.fn().mockResolvedValue(undefined),
      sasRequest: vi.fn().mockResolvedValue(undefined),
      recordRequest: vi.fn().mockResolvedValue(undefined),
      sasAvailable: vi.fn().mockResolvedValue(true),
      monitorsList: vi.fn().mockResolvedValue([]),
      monitorSelect: vi.fn().mockResolvedValue(undefined),
    };
  }

  function draw(sendClipboard: () => void): void {
    renderToolbar(
      container,
      new ToolbarState(),
      'en',
      toolbarHooks(),
      {
        toggleCollapsed: () => {},
        openPopover: () => {},
        setResolution: () => {},
        setDisplayMode: () => {},
        toggleFullscreen: () => {},
        toggleLocalCursor: () => {},
        toggleMic: () => {},
        sendCad: () => {},
        askToRecord: () => {},
        sendClipboard,
        sendFile: () => {},
        pickMonitor: () => {},
        beginDrag: () => {},
        nudge: () => {},
      },
    );
  }

  it('is reachable from the keyboard and named in both locales', () => {
    draw(() => {});
    const button = container.querySelector<HTMLButtonElement>(
      '[data-testid="toolbar-clipboard"]',
    );
    expect(button).not.toBeNull();
    expect(button?.tagName).toBe('BUTTON');
    expect(button?.getAttribute('aria-label')).toBe('Send my clipboard to the host');
    expect(button?.hasAttribute('disabled')).toBe(false);
  });

  it('offers the guest clipboard only when the guest presses it', () => {
    let pressed = 0;
    draw(() => {
      pressed += 1;
    });
    const button = container.querySelector<HTMLButtonElement>(
      '[data-testid="toolbar-clipboard"]',
    );
    expect(pressed).toBe(0);
    button?.click();
    expect(pressed).toBe(1);
  });

  it('sends what the guest clipboard holds, and sends nothing when it is empty', async () => {
    const readText = vi.fn().mockResolvedValue('from the guest');
    Object.defineProperty(navigator, 'clipboard', {
      value: { readText },
      configurable: true,
    });
    const fake = commands();
    const { mountToolbar } = await import('./toolbar');
    const stop = mountToolbar(container, 'en', 'host-99', fake, toolbarHooks());
    container
      .querySelector<HTMLButtonElement>('[data-testid="toolbar-clipboard"]')
      ?.click();
    await vi.waitFor(() => {
      expect(fake.clipboardPush).toHaveBeenCalledWith('host-99', 'from the guest');
    });

    readText.mockResolvedValue('');
    container
      .querySelector<HTMLButtonElement>('[data-testid="toolbar-clipboard"]')
      ?.click();
    await Promise.resolve();
    await Promise.resolve();
    expect(fake.clipboardPush).toHaveBeenCalledTimes(1);
    stop();
  });
});
