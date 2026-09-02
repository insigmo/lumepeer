// The host's always-on-top session bar (ADR 0055).
//
// Drives the real entry point against a mocked `@tauri-apps/api/core`, the
// way `hostbar.html` does. The properties worth holding are that the bar
// reports the actor's session list rather than any state of its own, that
// collapsing is a window resize and not a CSS trick, and that it exposes no
// control beyond ending a session and reaching the main window.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const { invoke } = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock('@tauri-apps/api/core', () => ({ invoke }));

const ACTIVE_GUEST = {
  peer_label: 'guest-ab12',
  state: 'active',
  role: 'full_control',
  input: true,
  clipboard_read: true,
  clipboard_write: true,
  file_transfer: true,
  recording: true,
  display_mode: true,
  secure_desktop: true,
  recording_active: false,
  record_request: false,
  secure_desktop_input: false,
  secure_desktop_active: false,
};

/** What `session_status` answers for the running test; mutated per case. */
let statusRows: unknown[] = [];

function bar(): HTMLElement {
  const el = document.querySelector('#hostbar');
  if (!el) {
    throw new Error('#hostbar did not render');
  }
  return el as HTMLElement;
}

/** Loads a fresh `hostbar.ts`, the way hostbar.html's module script does. */
async function boot(): Promise<void> {
  document.body.innerHTML = '<div id="hostbar"></div>';
  // The bar polls every second; a live interval outliving `vi.resetModules()`
  // would keep calling the mock from a stale module instance.
  vi.spyOn(globalThis, 'setInterval').mockImplementation(
    () => 0 as unknown as ReturnType<typeof setInterval>,
  );
  await import('./hostbar');
  await vi.waitFor(() => {
    expect(invoke).toHaveBeenCalledWith('session_status', undefined);
  });
  await new Promise((resolve) => setTimeout(resolve, 0));
}

beforeEach(() => {
  vi.resetModules();
  invoke.mockReset();
  statusRows = [ACTIVE_GUEST];
  invoke.mockImplementation((command: string) => {
    if (command === 'session_status') {
      return Promise.resolve(statusRows);
    }
    return Promise.resolve(undefined);
  });
});

afterEach(() => {
  vi.restoreAllMocks();
  document.body.innerHTML = '';
});

describe('the host session bar', () => {
  it('lists the guests the actor reported, by pseudonymized label', async () => {
    await boot();

    const rows = bar().querySelectorAll('.bar-row');
    expect(rows).toHaveLength(1);
    expect(rows[0]?.querySelector('.bar-peer')?.textContent).toBe('guest-ab12');
  });

  it('shows only live sessions: one still waiting for consent is not connected', async () => {
    statusRows = [{ ...ACTIVE_GUEST, state: 'pending' }];
    await boot();

    expect(bar().querySelectorAll('.bar-row')).toHaveLength(0);
  });

  it('collapsing asks the window to shrink, not just the page to hide things', async () => {
    await boot();

    bar().querySelector<HTMLButtonElement>('[data-testid="hostbar-collapse"]')?.click();

    await vi.waitFor(() =>
      expect(invoke).toHaveBeenCalledWith('host_bar_expand', { args: { expanded: false } }),
    );
    await vi.waitFor(() => expect(bar().classList.contains('is-collapsed')).toBe(true));
    // Collapsed, the only thing left is the way back.
    expect(bar().querySelectorAll('button')).toHaveLength(1);
    expect(bar().querySelector('[data-testid="hostbar-expand"]')).not.toBeNull();
  });

  it('expanding again asks for the full size', async () => {
    await boot();
    bar().querySelector<HTMLButtonElement>('[data-testid="hostbar-collapse"]')?.click();
    await vi.waitFor(() => expect(bar().classList.contains('is-collapsed')).toBe(true));

    bar().querySelector<HTMLButtonElement>('[data-testid="hostbar-expand"]')?.click();

    await vi.waitFor(() =>
      expect(invoke).toHaveBeenCalledWith('host_bar_expand', { args: { expanded: true } }),
    );
    await vi.waitFor(() => expect(bar().classList.contains('is-collapsed')).toBe(false));
  });

  it('ends a session through the actor, by label', async () => {
    await boot();

    bar().querySelector<HTMLButtonElement>('.bar-revoke')?.click();

    await vi.waitFor(() =>
      expect(invoke).toHaveBeenCalledWith('session_revoke', { args: { peer: 'guest-ab12' } }),
    );
  });

  it('reaches the rest of the app by raising the main window, not by carrying it', async () => {
    await boot();

    bar().querySelector<HTMLButtonElement>('[data-testid="hostbar-open"]')?.click();

    await vi.waitFor(() =>
      expect(invoke).toHaveBeenCalledWith('host_bar_focus_main', undefined),
    );
  });

  it('offers no permission control of its own: the role decided that', async () => {
    await boot();

    expect(bar().querySelectorAll('input')).toHaveLength(0);
    const labels = [...bar().querySelectorAll('button')].map(
      (button) => button.getAttribute('data-testid') ?? button.className,
    );
    expect(labels).toEqual(['hostbar-collapse', 'bar-revoke', 'hostbar-open']);
  });
});
