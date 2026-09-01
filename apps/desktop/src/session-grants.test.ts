// Host-side switches for the five independent grants of §8.2 (ADR 0029,
// ADR 0046).
//
// The point of these tests is the direction of authority: this panel asks the
// core to change a grant and then shows whatever the core says it holds. It
// never decides, never toggles optimistically, and has no path to `view` or
// `input` at all.
import { render } from 'lit-html';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { SUPPORTED_LOCALES, t } from './i18n';
import type { SessionStatus } from './session-status';
import { sessionStatus } from './session-status';

const invoke = vi.fn().mockResolvedValue(undefined);

vi.mock('@tauri-apps/api/core', () => ({
  invoke: (...args: unknown[]) => invoke(...args),
}));

const noGrants = {
  clipboard_read: false,
  clipboard_write: false,
  file_transfer: false,
  recording: false,
  recording_active: false,
  record_request: false,
  secure_desktop: false,
  secure_desktop_active: false,
} as const;

const activeSession: SessionStatus = {
  peer_label: 'guest-ab12',
  role: 'full_control',
  input: true,
  state: 'active',
  ...noGrants,
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

function switches(root: HTMLElement): HTMLInputElement[] {
  return Array.from(root.querySelectorAll<HTMLInputElement>('.grant-row input'));
}

describe('independent grant switches', () => {
  it('offers exactly the five independent grants, all off on a fresh session', () => {
    render(sessionStatus([activeSession], 'en'), container);

    const boxes = switches(container);
    expect(boxes).toHaveLength(5);
    expect(boxes.every((box) => box.type === 'checkbox')).toBe(true);
    expect(boxes.some((box) => box.checked)).toBe(false);
  });

  it('shows a grant as on only because the core reported it', () => {
    render(
      sessionStatus([{ ...activeSession, file_transfer: true }], 'en'),
      container,
    );

    const checked = switches(container).filter((box) => box.checked);
    expect(checked).toHaveLength(1);
    expect(checked[0]?.getAttribute('aria-label')).toContain('Send and receive files');
  });

  it('asks the core to change one grant and re-polls rather than toggling locally', async () => {
    const onRefresh = vi.fn();
    render(sessionStatus([activeSession], 'en', onRefresh), container);

    const box = switches(container)[0];
    expect(box).toBeDefined();
    box?.click();

    // The IPC module is loaded on demand, so the call lands a microtask later.
    await vi.waitFor(() =>
      expect(invoke).toHaveBeenCalledWith('session_set_grant', {
        args: { peer: 'guest-ab12', grant: 'clipboard_read', allowed: true },
      }),
    );
    await vi.waitFor(() => expect(onRefresh).toHaveBeenCalled());
  });

  it('turning a held grant off sends allowed: false', async () => {
    render(sessionStatus([{ ...activeSession, recording: true }], 'en'), container);

    const box = switches(container).find((candidate) =>
      candidate.getAttribute('aria-label')?.includes('recorded'),
    );
    box?.click();

    await vi.waitFor(() =>
      expect(invoke).toHaveBeenCalledWith('session_set_grant', {
        args: { peer: 'guest-ab12', grant: 'recording', allowed: false },
      }),
    );
  });

  it('the secure-desktop switch is labeled honestly and independent of the rest (ADR 0046)', async () => {
    render(sessionStatus([activeSession], 'en'), container);

    const box = switches(container).find((candidate) =>
      candidate.getAttribute('aria-label')?.includes('administrator prompt'),
    );
    expect(box).toBeDefined();
    // Not "secure desktop" — the label says the consequence, not the
    // mechanism (ADR 0046).
    expect(box?.getAttribute('aria-label')).not.toContain('secure desktop');
    // `activeSession` holds `full_control` and nothing else: the row is
    // unchecked because the grant is independent of the role, never derived
    // from it (ADR 0046's central requirement).
    expect(box?.checked).toBe(false);

    box?.click();
    await vi.waitFor(() =>
      expect(invoke).toHaveBeenCalledWith('session_set_grant', {
        args: { peer: 'guest-ab12', grant: 'secure_desktop', allowed: true },
      }),
    );
  });

  it('re-polls after a refused change, so a switch cannot stay on against the core', async () => {
    invoke.mockRejectedValueOnce(new Error('denied'));
    const onRefresh = vi.fn();
    const errors = vi.spyOn(console, 'error').mockImplementation(() => {});
    render(sessionStatus([activeSession], 'en', onRefresh), container);

    switches(container)[0]?.click();

    await vi.waitFor(() => expect(onRefresh).toHaveBeenCalled());
    errors.mockRestore();
  });

  it('a pending session gets no switches: nothing is granted before consent', () => {
    render(
      sessionStatus([{ ...activeSession, state: 'pending', input: false }], 'en'),
      container,
    );

    expect(switches(container)).toHaveLength(0);
  });

  it('every switch is keyboard reachable and named in both locales', () => {
    for (const locale of SUPPORTED_LOCALES) {
      const scoped = document.createElement('div');
      document.body.appendChild(scoped);
      try {
        render(sessionStatus([activeSession], locale), scoped);
        const boxes = switches(scoped);
        expect(boxes).toHaveLength(5);
        for (const box of boxes) {
          // Native checkboxes are in the tab order unless something takes
          // them out of it; nothing here may.
          expect(box.tabIndex).toBe(0);
          expect(box.disabled).toBe(false);
          expect(box.getAttribute('aria-label')).toBeTruthy();
        }
        const legend = scoped.querySelector('.session-grants legend');
        expect(legend?.textContent?.trim()).toBe(t(locale, 'status.grants.heading'));
      } finally {
        scoped.remove();
      }
    }
  });
});
