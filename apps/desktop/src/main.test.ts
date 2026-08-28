// docs/bugs/05-settings-window.md: the settings screen, the sidebar gear
// button that opens it, and the unattended-access indicator's move into the
// sidebar are all wiring inside main.ts itself, which has no render function
// of its own to unit-test in isolation. This drives the actual entry point
// against a mocked `@tauri-apps/api/core`, the way index.html does.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const { invoke } = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock('@tauri-apps/api/core', () => ({ invoke }));

/** What `unattended_status` reports for the running test; mutated per case. */
let unattendedEnabled = false;

const RESPONSES: Record<string, unknown> = {
  session_status: [],
  connection_history: [],
  network_status: { ready: true, can_capture: true, can_encode: true },
  connect_status: {
    phase: 'idle',
    pending: false,
    code: null,
    code_required: false,
    retry_secs: null,
    credentials_auto: false,
  },
  address_book_list: [],
  connection_stats: [],
  file_transfers: { offers: [], transfers: [] },
  recordings_list: [],
  audit_status: false,
  audit_kinds: [],
  audit_list: [],
  service_status: 'unsupported',
  autostart_status: false,
};

function app(): HTMLElement {
  const el = document.querySelector('#app');
  if (!el) {
    throw new Error('#app did not render');
  }
  return el as HTMLElement;
}

/** Loads a fresh `main.ts`, the way index.html's module script does. */
async function boot(): Promise<void> {
  document.body.innerHTML = '<main id="app"></main><aside id="host-chat-panel" hidden></aside>';
  // main.ts polls every second; the assertions below don't need a second
  // round trip, and a live interval outliving `vi.resetModules()` would keep
  // calling the mock from a stale module instance.
  vi.spyOn(globalThis, 'setInterval').mockImplementation(() => 0 as unknown as ReturnType<typeof setInterval>);
  await import('./main');
  // Lets the async `refresh()` chain in main.ts settle before assertions.
  await vi.waitFor(() => {
    expect(invoke).toHaveBeenCalledWith('session_status');
  });
  await new Promise((resolve) => setTimeout(resolve, 0));
}

beforeEach(() => {
  vi.resetModules();
  invoke.mockReset();
  unattendedEnabled = false;
  invoke.mockImplementation((command: string) => {
    if (command === 'unattended_status') {
      return Promise.resolve({ enabled: unattendedEnabled, totp_enabled: false, role: 'view_only' });
    }
    if (command in RESPONSES) {
      return Promise.resolve(RESPONSES[command]);
    }
    return Promise.resolve(undefined);
  });
});

afterEach(() => {
  vi.restoreAllMocks();
  document.body.innerHTML = '';
});

describe('sidebar (task 1)', () => {
  it('labels the network line "Serverless" and offers a settings button next to it', async () => {
    await boot();
    const tag = app().querySelector('.footer-tag');
    expect(tag?.textContent).toContain('Serverless');
    expect(tag?.textContent).not.toContain('P2P');

    const button = app().querySelector<HTMLButtonElement>('.settings-btn');
    expect(button).not.toBeNull();
    expect(button?.getAttribute('aria-label')).toBe('Settings');
  });
});

describe('settings screen (task 2)', () => {
  it('opens on the gear button, closes on Escape, and returns focus to it', async () => {
    await boot();
    const button = app().querySelector<HTMLButtonElement>('.settings-btn');
    if (!button) {
      throw new Error('settings button not rendered');
    }
    button.focus();
    button.click();

    const dialog = app().querySelector('[role="dialog"].settings-panel');
    expect(dialog).not.toBeNull();
    expect(dialog?.getAttribute('aria-modal')).toBe('true');
    expect(dialog?.getAttribute('aria-labelledby')).toBe('settings-heading');

    const backdrop = app().querySelector('.settings-backdrop');
    backdrop?.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
    await vi.waitFor(() => {
      expect(app().querySelector('.settings-backdrop')).toBeNull();
    });
    expect(document.activeElement).toBe(app().querySelector('.settings-btn'));
  });
});

describe('panels moved to settings (task 3)', () => {
  it('renders the address book, unattended access, recordings, audit log and this device only inside settings', async () => {
    await boot();
    expect(app().querySelector('main.main-panel .address-book')).toBeNull();
    expect(app().querySelector('main.main-panel .unattended-panel')).toBeNull();
    expect(app().querySelector('main.main-panel .recordings')).toBeNull();
    expect(app().querySelector('main.main-panel [data-testid="system-settings"]')).toBeNull();

    app().querySelector<HTMLButtonElement>('.settings-btn')?.click();
    const dialog = app().querySelector('.settings-body');
    expect(dialog?.querySelector('.address-book')).not.toBeNull();
    expect(dialog?.querySelector('.unattended-panel')).not.toBeNull();
    expect(dialog?.querySelector('.recordings')).not.toBeNull();
    expect(dialog?.querySelector('[data-testid="system-settings"]')).not.toBeNull();
    expect(dialog?.querySelector('.invite-refresh-btn')).not.toBeNull();
  });

  it('keeps connectPanel, sessionStatus, recordingBanner and mediaWarning in the main window', async () => {
    await boot();
    const main = app().querySelector('main.main-panel');
    expect(main?.querySelector('.connect-row')).not.toBeNull();
    expect(main?.querySelector('.connections-header')).not.toBeNull();
  });
});

describe('unattended indicator (task 5)', () => {
  it('lives in the sidebar, not the main panel, and stays while access is on', async () => {
    unattendedEnabled = true;
    await boot();
    expect(
      app().querySelector('.sidebar .sidebar-bottom [data-testid="unattended-indicator"]'),
    ).not.toBeNull();
    expect(app().querySelector('main.main-panel [data-testid="unattended-indicator"]')).toBeNull();
  });

  it('is absent from the sidebar while access is off', async () => {
    unattendedEnabled = false;
    await boot();
    expect(app().querySelector('[data-testid="unattended-indicator"]')).toBeNull();
  });
});
