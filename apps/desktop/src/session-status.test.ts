// docs/bugs/03-connection-list.md, task 5: a remembered host can be removed
// from the connection list, with a confirmation and without disturbing the
// row's own reconnect control.
import { render } from 'lit-html';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const { invoke } = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock('@tauri-apps/api/core', () => ({ invoke }));

import { sessionStatus, type HistoryEntry } from './session-status';

let container: HTMLElement;

beforeEach(() => {
  invoke.mockReset();
  invoke.mockResolvedValue(undefined);
  container = document.createElement('div');
  document.body.appendChild(container);
});

afterEach(() => {
  container.remove();
  vi.restoreAllMocks();
});

const ENTRY: HistoryEntry = {
  peer_label: 'host-ab12',
  role: 'view_only',
  last_seen_at: Math.floor(Date.now() / 1000) - 60,
};

function render_(onRefresh: () => void = () => {}, onReconnect: (peer: string) => void = () => {}): void {
  render(sessionStatus([], 'en', onRefresh, [ENTRY], onReconnect), container);
}

describe('remembered-host row', () => {
  it('offers two separate controls, not a button nested in a button', () => {
    render_();
    const row = container.querySelector('.history-row');
    const buttons = row?.querySelectorAll('button') ?? [];
    expect(buttons).toHaveLength(2);
    expect(row?.querySelector('button > button')).toBeNull();
  });

  it('clicking the row still reconnects by label', () => {
    const onReconnect = vi.fn();
    render_(() => {}, onReconnect);
    container.querySelector<HTMLButtonElement>('.history-reconnect')?.click();
    expect(onReconnect).toHaveBeenCalledWith('host-ab12');
  });

  it('asks for confirmation, then removes the row and refreshes', async () => {
    vi.spyOn(globalThis, 'confirm').mockReturnValue(true);
    const onRefresh = vi.fn();
    render_(onRefresh);

    container.querySelector<HTMLButtonElement>('.history-remove')?.click();
    expect(globalThis.confirm).toHaveBeenCalledWith(expect.stringContaining('host-ab12'));

    await vi.waitFor(() => {
      expect(invoke).toHaveBeenCalledWith('history_remove', { args: { peer: 'host-ab12' } });
      expect(onRefresh).toHaveBeenCalled();
    });
  });

  // ADR 0084 §6. The flag nothing else sets: a row that has merely been
  // connected to, granted a role, or had its password saved is still off.
  it('starts off for a remembered host and reads the flag rather than assuming it', () => {
    render_();
    const box = container.querySelector<HTMLInputElement>(
      '[data-testid="history-auto-reconnect"] input',
    );
    expect(box).not.toBeNull();
    expect(box?.checked).toBe(false);

    render(
      sessionStatus([], 'en', () => {}, [{ ...ENTRY, trusted: true }], () => {}),
      container,
    );
    expect(
      container.querySelector<HTMLInputElement>('[data-testid="history-auto-reconnect"] input')
        ?.checked,
    ).toBe(true);
  });

  it('switching it sends the host label and the new state, and refreshes', async () => {
    const onRefresh = vi.fn();
    render_(onRefresh);
    const box = container.querySelector<HTMLInputElement>(
      '[data-testid="history-auto-reconnect"] input',
    );
    box!.checked = true;
    box?.dispatchEvent(new Event('change'));

    await vi.waitFor(() => {
      expect(invoke).toHaveBeenCalledWith('history_set_trusted', {
        args: { peer: 'host-ab12', trusted: true },
      });
      expect(onRefresh).toHaveBeenCalled();
    });
    // Turning it back off is the same call with the other answer, and never
    // a removal: this is a standing decision, not the row.
    invoke.mockClear();
    render(
      sessionStatus([], 'en', onRefresh, [{ ...ENTRY, trusted: true }], () => {}),
      container,
    );
    const on = container.querySelector<HTMLInputElement>(
      '[data-testid="history-auto-reconnect"] input',
    );
    on!.checked = false;
    on?.dispatchEvent(new Event('change'));
    await vi.waitFor(() => {
      expect(invoke).toHaveBeenCalledWith('history_set_trusted', {
        args: { peer: 'host-ab12', trusted: false },
      });
    });
    expect(invoke).not.toHaveBeenCalledWith('history_remove', expect.anything());
  });

  it('offers no forget-password control for a host with no saved password', () => {
    render_();
    expect(container.querySelector('.history-forget-password')).toBeNull();
  });

  it('offers a forget-password control for a host that signs in by itself', async () => {
    vi.spyOn(globalThis, 'confirm').mockReturnValue(true);
    const onRefresh = vi.fn();
    render(
      sessionStatus([], 'en', onRefresh, [{ ...ENTRY, has_password: true }], () => {}),
      container,
    );

    const forget = container.querySelector<HTMLButtonElement>('.history-forget-password');
    expect(forget).not.toBeNull();
    forget?.click();
    expect(globalThis.confirm).toHaveBeenCalledWith(expect.stringContaining('host-ab12'));

    await vi.waitFor(() => {
      expect(invoke).toHaveBeenCalledWith('history_forget_password', {
        args: { peer: 'host-ab12' },
      });
      expect(onRefresh).toHaveBeenCalled();
    });
    // The row itself stays: forgetting the password is not forgetting the host.
    expect(invoke).not.toHaveBeenCalledWith('history_remove', expect.anything());
  });

  it('does nothing if the confirmation is declined', () => {
    vi.spyOn(globalThis, 'confirm').mockReturnValue(false);
    const onRefresh = vi.fn();
    render_(onRefresh);

    container.querySelector<HTMLButtonElement>('.history-remove')?.click();
    expect(invoke).not.toHaveBeenCalled();
    expect(onRefresh).not.toHaveBeenCalled();
  });

  it('labels the remove button with the host it removes, for a list with more than one row', () => {
    render(
      sessionStatus(
        [],
        'en',
        () => {},
        [ENTRY, { peer_label: 'host-cd34', role: 'view_only', last_seen_at: ENTRY.last_seen_at }],
      ),
      container,
    );
    const labels = [...container.querySelectorAll('.history-remove')].map((el) =>
      el.getAttribute('aria-label'),
    );
    expect(labels).toEqual([
      expect.stringContaining('host-ab12'),
      expect.stringContaining('host-cd34'),
    ]);
    expect(new Set(labels).size).toBe(2);
  });
});
