// docs/bugs/03-connection-list.md, task 5: a remembered host can be removed
// from the connection list, with a confirmation and without disturbing the
// row's own reconnect control.
import { render } from 'lit-html';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const { invoke } = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock('@tauri-apps/api/core', () => ({ invoke }));

import { rememberThumbnail, thumbnailOf } from './peer-thumbnails';
import { sessionStatus, type HistoryEntry, type SessionStatus } from './session-status';

let container: HTMLElement;

beforeEach(() => {
  localStorage.clear();
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
  it('keeps the card face and its menu apart, never a button inside a button', () => {
    render_();
    const row = container.querySelector('.history-row');
    expect(row?.querySelector('button > button')).toBeNull();
    // The face is the connect control; everything else lives in the menu
    // beside it, so pressing one can never mean the other.
    expect(row?.querySelector('button.peer-card-face.history-reconnect')).not.toBeNull();
    expect(row?.querySelector('.peer-card-face .peer-menu')).toBeNull();
    expect(row?.querySelector('.peer-menu .history-remove')).not.toBeNull();
  });

  it('puts every action this row has into one menu', () => {
    render(
      sessionStatus([], 'en', () => {}, [{ ...ENTRY, has_password: true }], () => {}),
      container,
    );
    const menu = container.querySelector('.history-row .peer-menu-list');
    expect(menu?.querySelector('.peer-menu-connect')).not.toBeNull();
    expect(menu?.querySelector('[data-testid="history-connect-terminal"]')).not.toBeNull();
    expect(menu?.querySelector('[data-testid="history-auto-reconnect"]')).not.toBeNull();
    expect(menu?.querySelector('.history-forget-password')).not.toBeNull();
    expect(menu?.querySelector('.history-remove')).not.toBeNull();
  });

  it('closes the menu once an action has been taken', async () => {
    vi.spyOn(globalThis, 'confirm').mockReturnValue(true);
    render_();
    const menu = container.querySelector<HTMLDetailsElement>('.history-row .peer-menu');
    menu?.setAttribute('open', '');
    container.querySelector<HTMLButtonElement>('.history-remove')?.click();
    expect(menu?.hasAttribute('open')).toBe(false);
    await vi.waitFor(() => {
      expect(invoke).toHaveBeenCalledWith('history_remove', { args: { peer: 'host-ab12' } });
    });
  });

  it('shows the last picture of the host, and a screen glyph until there is one', () => {
    render_();
    expect(container.querySelector('.peer-thumb .peer-thumb-image')).toBeNull();
    expect(container.querySelector('.peer-thumb .peer-thumb-glyph')).not.toBeNull();

    rememberThumbnail('host-ab12', 'data:image/jpeg;base64,AAAA');
    render_();
    const image = container.querySelector<HTMLImageElement>('.peer-thumb .peer-thumb-image');
    expect(image?.getAttribute('src')).toBe('data:image/jpeg;base64,AAAA');
  });

  it('forgets the picture with the row it belonged to', async () => {
    vi.spyOn(globalThis, 'confirm').mockReturnValue(true);
    rememberThumbnail('host-ab12', 'data:image/jpeg;base64,AAAA');
    render_();
    container.querySelector<HTMLButtonElement>('.history-remove')?.click();
    await vi.waitFor(() => {
      expect(invoke).toHaveBeenCalledWith('history_remove', { args: { peer: 'host-ab12' } });
    });
    expect(thumbnailOf('host-ab12')).toBeNull();
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

  // ADR 0101. A shell is carried by the `terminal` grant, which rides
  // `Role::FullControl` alone, and the role comes from the code the host
  // handed out — the guest does not pick it. Offering the item on a row that
  // could only ever be refused would be offering work that cannot be done.
  it('offers the terminal only for a host that granted full control', () => {
    render_();
    const item = container.querySelector<HTMLButtonElement>(
      '[data-testid="history-connect-terminal"]',
    );
    expect(item).not.toBeNull();
    expect(item?.disabled).toBe(true);
    expect(item?.getAttribute('title')).not.toBe('');
  });

  it('enables the terminal item for a full-control host and asks for a shell', () => {
    const onReconnect = vi.fn();
    render(
      sessionStatus([], 'en', () => {}, [{ ...ENTRY, role: 'full_control' }], onReconnect),
      container,
    );
    const item = container.querySelector<HTMLButtonElement>(
      '[data-testid="history-connect-terminal"]',
    );
    expect(item?.disabled).toBe(false);
    item?.click();
    // The second argument is the whole difference from "Connect again": the
    // same host, the same role, dialled without the media connection.
    expect(onReconnect).toHaveBeenCalledWith('host-ab12', 'terminal');
  });

  // ADR 0124. File browsing is a grant of its own that a host can give any
  // role, so the item is not tied to full control the way the shell is; a
  // host that did not give it says so in the file manager itself.
  it('offers the file manager on every remembered host and asks for its files', () => {
    const onReconnect = vi.fn();
    render_(() => {}, onReconnect);
    const item = container.querySelector<HTMLButtonElement>('[data-testid="history-open-files"]');
    expect(item?.disabled).toBe(false);
    item?.click();
    expect(onReconnect).toHaveBeenCalledWith('host-ab12', 'files');
  });

  it('keeps the terminal item out of reach while a connect is already in flight', () => {
    render(
      sessionStatus(
        [],
        'en',
        () => {},
        [{ ...ENTRY, role: 'full_control' }],
        () => {},
        true,
      ),
      container,
    );
    expect(
      container.querySelector<HTMLButtonElement>('[data-testid="history-connect-terminal"]')
        ?.disabled,
    ).toBe(true);
  });
});

const GUEST: SessionStatus = {
  peer_label: 'guest-ab12',
  role: 'full_control',
  input: true,
  state: 'active',
  clipboard_read: true,
  clipboard_write: true,
  file_transfer: true,
  recording: true,
  display_mode: true,
  recording_active: false,
  record_request: false,
  secure_desktop: true,
  secure_desktop_input: false,
  secure_desktop_active: false,
  tunnel: true,
  terminal: true,
  terminal_active: false,
  chat_unread: false,
};

// ADR 0121: a peer is shown by what its machine is called, with the label
// every command still names it by one hover away.
describe('a peer that named its machine', () => {
  it('is shown by that name and its operating system, the label in the tooltip', () => {
    render(
      sessionStatus(
        [{ ...GUEST, device_name: 'BETA-PC', device_os: 'windows' }],
        'en',
        () => {},
        [{ ...ENTRY, device_name: 'MacBook Pro', device_os: 'macos' }],
      ),
      container,
    );

    const [session, host] = [...container.querySelectorAll('[data-testid="peer-name"]')];
    expect(session?.querySelector('.peer-label')?.textContent).toBe('BETA-PC');
    expect(session?.querySelector('.peer-os')?.getAttribute('aria-label')).toBe('Windows');
    expect(session?.getAttribute('title')).toContain('guest-ab12');
    expect(host?.querySelector('.peer-label')?.textContent).toBe('MacBook Pro');
    expect(host?.querySelector('.peer-os')?.getAttribute('aria-label')).toBe('macOS');
    expect(host?.getAttribute('title')).toContain('host-ab12');
  });

  it('falls back to the label, with a generic screen, when the peer said nothing', () => {
    render(sessionStatus([GUEST], 'en'), container);

    const name = container.querySelector('[data-testid="peer-name"]');
    expect(name?.querySelector('.peer-label')?.textContent).toBe('guest-ab12');
    expect(name?.querySelector('.peer-os')?.getAttribute('data-os')).toBe('other');
    expect(name?.querySelector('.peer-os')?.getAttribute('aria-hidden')).toBe('true');
  });

  it('is still acted on by label, whatever it is called', () => {
    const onReconnect = vi.fn();
    render(
      sessionStatus([], 'en', () => {}, [{ ...ENTRY, device_name: 'BETA-PC', device_os: 'linux' }], onReconnect),
      container,
    );
    container.querySelector<HTMLButtonElement>('.history-reconnect')?.click();
    expect(onReconnect).toHaveBeenCalledWith('host-ab12');
  });
});

// ADR 0121: what can be done to a session is in its menu; what is true about
// it, and what a person has to answer, is on the card.
describe('a session card', () => {
  function menu(): HTMLElement | null {
    return container.querySelector('[data-testid="session-card"] details.peer-menu');
  }

  it('keeps every action in the menu', () => {
    render(sessionStatus([GUEST], 'en'), container);

    const actions = menu();
    expect(actions?.querySelector('[data-testid="record-toggle"]')).not.toBeNull();
    expect(actions?.querySelector('.chat-open-btn')).not.toBeNull();
    expect(actions?.querySelector('.revoke-btn')).not.toBeNull();
    // Nothing clickable is left on the card outside the menu; the quality
    // pill is a summary that only opens onto what was measured.
    const loose = [...container.querySelectorAll('[data-testid="session-card"] button')].filter(
      (button) => !button.closest('details.peer-menu'),
    );
    expect(loose).toEqual([]);
  });

  // ADR 0120: an unread chat is a mark, not a setting — the one control that
  // stays out on the card, and only while there is something to read.
  it('keeps the unread-chat mark out on the card', () => {
    render(sessionStatus([{ ...GUEST, chat_unread: true }], 'en'), container);
    const mark = container.querySelector('[data-testid="chat-unread"]');
    expect(mark).not.toBeNull();
    expect(mark?.closest('details.peer-menu')).toBeNull();
  });

  it('shows no port forwarding, even for a session that holds the grant', () => {
    render(sessionStatus([{ ...GUEST, tunnel: true }], 'en'), container);
    expect(container.querySelector('[data-testid="tunnel-panel"], .tunnel-panel')).toBeNull();
  });

  it('is quiet when nothing is happening, and says so in one row when something is', () => {
    render(sessionStatus([GUEST], 'en'), container);
    expect(container.querySelector('[data-testid="session-status-row"]')).toBeNull();

    render(sessionStatus([{ ...GUEST, recording_active: true, terminal_active: true }], 'en'), container);
    const row = container.querySelector('[data-testid="session-status-row"]');
    expect(row?.querySelector('[data-testid="recording-indicator"]')).not.toBeNull();
    expect(row?.querySelector('[data-testid="terminal-indicator"]')).not.toBeNull();
  });

  it('keeps a guest asking to be recorded on the card, where it cannot be missed', () => {
    render(sessionStatus([{ ...GUEST, record_request: true, device_name: 'BETA-PC' }], 'en'), container);
    const request = container.querySelector('[data-testid="record-request"]');
    expect(request).not.toBeNull();
    expect(request?.closest('details.peer-menu')).toBeNull();
    expect(request?.textContent).toContain('BETA-PC');
  });
});

// ADR 0127. The dot beside a remembered host's name answers "did this host
// answer the last time we reached for it", so it takes a colour only once
// something has asked.
describe('remembered-host presence dot', () => {
  function renderWith(online: boolean | null | undefined): void {
    render(sessionStatus([], 'en', () => {}, [{ ...ENTRY, online }], () => {}), container);
  }

  function dot(): HTMLElement | null {
    return container.querySelector<HTMLElement>('[data-testid="history-card"] [data-testid="presence-dot"]');
  }

  it('is green for a host that answered', () => {
    renderWith(true);
    expect(dot()?.dataset['state']).toBe('online');
    expect(dot()?.title).toBe('Online');
    // The one dot of the card, beside the name, not a second one on the picture.
    expect(dot()?.closest('.peer-card-foot')).not.toBeNull();
    expect(container.querySelectorAll('[data-testid="history-card"] .peer-dot')).toHaveLength(1);
  });

  it('is red for a host that did not answer', () => {
    renderWith(false);
    expect(dot()?.dataset['state']).toBe('offline');
    expect(dot()?.title).toBe('Offline');
  });

  it('stays the grey of "not connected" until something has asked, and for an older answer without the field', () => {
    renderWith(null);
    expect(dot()?.dataset['state']).toBe('idle');
    expect(dot()?.title).toBe('Not connected');
    renderWith(undefined);
    expect(dot()?.dataset['state']).toBe('idle');
  });

  it('is said in words on the card face, since the dot itself is only colour', () => {
    renderWith(false);
    const face = container.querySelector<HTMLButtonElement>('button.history-reconnect');
    expect(face?.getAttribute('aria-label')).toBe('Connect again: host-ab12, Offline');
    renderWith(null);
    expect(face?.getAttribute('aria-label')).toBe('Connect again: host-ab12');
  });
});
