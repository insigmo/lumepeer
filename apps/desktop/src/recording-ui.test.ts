// Session recording as the two people in the session see it (§17, §2.2).
//
// The rule these tests are about is "no hidden capture": recording is a
// separate grant, starting it is a separate press, and while it runs both
// sides carry an indicator neither of them can put away. Nothing here decides
// anything — the host's core does — so every test asserts on what was asked of
// the core and on what the reported state renders as, never on local guesses.
import { render } from 'lit-html';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { SUPPORTED_LOCALES, t } from './i18n';
import type { SessionStatus } from './session-status';
import { sessionStatus } from './session-status';
import { recordingBadge, decodeViewFrame, VIEW_FLAG_INPUT, VIEW_FLAG_RECORDING } from './view-window';

const invoke = vi.fn().mockResolvedValue(null);

vi.mock('@tauri-apps/api/core', () => ({
  invoke: (...args: unknown[]) => invoke(...args),
}));

const session: SessionStatus = {
  peer_label: 'guest-ab12',
  role: 'view_only',
  input: false,
  state: 'active',
  clipboard_read: false,
  clipboard_write: false,
  file_transfer: false,
  recording: false,
  recording_active: false,
  record_request: false,
  secure_desktop: false,
  secure_desktop_active: false,
};

let container: HTMLElement;

beforeEach(() => {
  invoke.mockClear();
  invoke.mockResolvedValue(null);
  container = document.createElement('div');
  document.body.appendChild(container);
});

afterEach(() => {
  container.remove();
});

function recordButton(root: HTMLElement): HTMLButtonElement | null {
  return root.querySelector<HTMLButtonElement>('[data-testid="record-toggle"]');
}

describe('the host-side recording control', () => {
  it('is unreachable without the recording grant', () => {
    render(sessionStatus([session], 'en'), container);

    const button = recordButton(container);
    expect(button).not.toBeNull();
    expect(button?.disabled).toBe(true);
    expect(button?.title).toBe(t('en', 'status.recording.needsGrant'));
  });

  it('asks the core to start and shows nothing until the core says it started', async () => {
    invoke.mockResolvedValue('C:/data/recordings/session-1-ab12.lmrc');
    const onRefresh = vi.fn();
    render(sessionStatus([{ ...session, recording: true }], 'en', onRefresh), container);

    recordButton(container)?.click();

    await vi.waitFor(() =>
      expect(invoke).toHaveBeenCalledWith('recording_toggle', {
        args: { peer: 'guest-ab12', on: true },
      }),
    );
    await vi.waitFor(() => expect(onRefresh).toHaveBeenCalled());
    // The press did not turn the badge on by itself: this render still shows
    // the state the last poll reported, which is "not recording".
    expect(container.querySelector('[data-testid="recording-indicator"]')).toBeNull();
  });

  it('shows an indicator and the file name while the core reports a recording', () => {
    render(
      sessionStatus(
        [{ ...session, recording: true, recording_active: true }],
        'en',
        () => {},
        [],
        () => {},
        false,
        () => {},
        new Map(),
        undefined,
        undefined,
        new Map([['guest-ab12', 'C:/data/recordings/session-1-ab12.lmrc']]),
      ),
      container,
    );

    const indicator = container.querySelector('[data-testid="recording-indicator"]');
    expect(indicator?.textContent?.trim()).toContain(t('en', 'status.recording.on'));
    expect(indicator?.getAttribute('role')).toBe('status');
    expect(recordButton(container)?.getAttribute('aria-pressed')).toBe('true');
    // The name, not the path: §15 keeps paths off a panel that stays on screen.
    const path = container.querySelector('[data-testid="recording-path"]');
    expect(path?.textContent?.trim()).toBe('session-1-ab12.lmrc');
    expect(path?.getAttribute('title')).toBe('C:/data/recordings/session-1-ab12.lmrc');
  });

  it('shortens a Windows path to its file name too', () => {
    render(
      sessionStatus(
        [{ ...session, recording: true, recording_active: true }],
        'en',
        () => {},
        [],
        () => {},
        false,
        () => {},
        new Map(),
        undefined,
        undefined,
        new Map([
          ['guest-ab12', 'C:\\Users\\me\\AppData\\Local\\lumepeer\\recordings\\session-1-ab12.lmrc'],
        ]),
      ),
      container,
    );

    expect(
      container.querySelector('[data-testid="recording-path"]')?.textContent?.trim(),
    ).toBe('session-1-ab12.lmrc');
  });

  it('stops through the core rather than by hiding its own indicator', async () => {
    render(
      sessionStatus([{ ...session, recording: true, recording_active: true }], 'en'),
      container,
    );

    recordButton(container)?.click();

    await vi.waitFor(() =>
      expect(invoke).toHaveBeenCalledWith('recording_toggle', {
        args: { peer: 'guest-ab12', on: false },
      }),
    );
    expect(container.querySelector('[data-testid="recording-indicator"]')).not.toBeNull();
  });

  it('re-polls after a refused start, so the button cannot lie about the core', async () => {
    invoke.mockRejectedValueOnce(new Error('not permitted'));
    const onRefresh = vi.fn();
    const errors = vi.spyOn(console, 'error').mockImplementation(() => {});
    render(sessionStatus([{ ...session, recording: true }], 'en', onRefresh), container);

    recordButton(container)?.click();

    await vi.waitFor(() => expect(onRefresh).toHaveBeenCalled());
    errors.mockRestore();
  });
});

describe("a guest's request to be recorded", () => {
  it('is shown to the host as a question, with nothing recording yet', () => {
    render(sessionStatus([{ ...session, record_request: true }], 'en'), container);

    const prompt = container.querySelector('[data-testid="record-request"]');
    expect(prompt?.textContent).toContain('guest-ab12');
    expect(container.querySelector('[data-testid="recording-indicator"]')).toBeNull();
  });

  it('grants the permission first and only then records, on the host user\u2019s press', async () => {
    render(sessionStatus([{ ...session, record_request: true }], 'en'), container);

    container.querySelector<HTMLButtonElement>('[data-testid="record-allow"]')?.click();

    await vi.waitFor(() =>
      expect(invoke).toHaveBeenCalledWith('session_set_grant', {
        args: { peer: 'guest-ab12', grant: 'recording', allowed: true },
      }),
    );
    await vi.waitFor(() =>
      expect(invoke).toHaveBeenCalledWith('recording_toggle', {
        args: { peer: 'guest-ab12', on: true },
      }),
    );
    const order = invoke.mock.calls.map((call) => call[0]);
    expect(order.indexOf('session_set_grant')).toBeLessThan(order.indexOf('recording_toggle'));
  });

  it('declining answers the guest instead of leaving it waiting', async () => {
    render(sessionStatus([{ ...session, record_request: true }], 'en'), container);

    container.querySelector<HTMLButtonElement>('[data-testid="record-deny"]')?.click();

    await vi.waitFor(() =>
      expect(invoke).toHaveBeenCalledWith('recording_toggle', {
        args: { peer: 'guest-ab12', on: false },
      }),
    );
    expect(invoke).not.toHaveBeenCalledWith('session_set_grant', expect.anything());
  });

  it('is named in both locales', () => {
    for (const locale of SUPPORTED_LOCALES) {
      const scoped = document.createElement('div');
      document.body.appendChild(scoped);
      try {
        render(sessionStatus([{ ...session, record_request: true }], locale), scoped);
        expect(recordButton(scoped)?.textContent?.trim()).toBe(
          t(locale, 'status.recording.start'),
        );
        expect(
          scoped.querySelector('[data-testid="record-allow"]')?.textContent?.trim(),
        ).toBe(t(locale, 'status.recording.allow'));
      } finally {
        scoped.remove();
      }
    }
  });
});

describe('the guest-side indicator', () => {
  /** A `view_next_frame` response with the given flags byte and no picture. */
  function frame(flags: number): ArrayBuffer {
    const buffer = new ArrayBuffer(18);
    const view = new DataView(buffer);
    view.setUint8(0, 1); // live
    view.setUint8(1, flags);
    return buffer;
  }

  it('reads recording and input as independent flags of every frame', () => {
    expect(decodeViewFrame(frame(0)).recording).toBe(false);
    expect(decodeViewFrame(frame(VIEW_FLAG_RECORDING)).recording).toBe(true);
    // A view-only session being recorded must not read as an input grant.
    expect(decodeViewFrame(frame(VIEW_FLAG_RECORDING)).input).toBe(false);
    const both = decodeViewFrame(frame(VIEW_FLAG_INPUT | VIEW_FLAG_RECORDING));
    expect(both.input).toBe(true);
    expect(both.recording).toBe(true);
  });

  it('renders a badge only while the host says it is recording', () => {
    render(recordingBadge(false, 'en'), container);
    expect(container.querySelector('[data-testid="view-recording"]')).toBeNull();

    render(recordingBadge(true, 'en'), container);
    const badge = container.querySelector('[data-testid="view-recording"]');
    expect(badge?.textContent?.trim()).toContain(t('en', 'view.recording'));
    expect(badge?.getAttribute('role')).toBe('status');
  });

  it('carries no way to dismiss it', () => {
    render(recordingBadge(true, 'en'), container);
    const badge = container.querySelector('[data-testid="view-recording"]');
    expect(badge?.querySelectorAll('button')).toHaveLength(0);
    expect(badge?.querySelectorAll('a')).toHaveLength(0);
  });
});
