// Host-side chat entry (§9.2, ADR 0023).
//
// The main window mounts the same chat.ts component the view window uses;
// these tests pin what the host side adds around it: the per-session Chat
// button in session-status and the drawer open/close/re-target behaviour.
import { render } from 'lit-html';
import { describe, expect, it, vi } from 'vitest';

import type { SessionStatus } from './session-status';
import { sessionStatus } from './session-status';

// A session the host has granted nothing beyond its role: the four
// independent grants of §8.2 all start off.
const noGrants = {
  clipboard_read: false,
  clipboard_write: false,
  file_transfer: false,
  recording: false,
  display_mode: false,
  recording_active: false,
  record_request: false,
  secure_desktop: false,
  secure_desktop_input: false,
  secure_desktop_active: false,
} as const;

const active: SessionStatus[] = [
  { peer_label: 'guest-ab12', role: 'full_control', input: true, state: 'active', ...noGrants },
];

function renderList(onOpenChat: (peer: string) => void = () => {}): HTMLElement {
  const container = document.createElement('div');
  document.body.appendChild(container);
  render(sessionStatus(active, 'en', undefined, [], undefined, false, onOpenChat), container);
  return container;
}

describe('host chat entry', () => {
  it('every active session row carries a Chat button that names its peer', () => {
    const opened = vi.fn();
    const container = renderList(opened);

    const buttons = Array.from(container.querySelectorAll<HTMLButtonElement>('.chat-open-btn'));
    expect(buttons).toHaveLength(1);
    expect(buttons[0]?.textContent?.trim()).toBe('Chat');

    buttons[0]?.click();
    expect(opened).toHaveBeenCalledWith('guest-ab12');
  });

  it('pending sessions get no Chat button — nothing is granted yet', () => {
    const pending: SessionStatus[] = [
      { peer_label: 'guest-ab12', role: 'view_only', input: false, state: 'pending', ...noGrants },
    ];
    const container = document.createElement('div');
    document.body.appendChild(container);
    try {
      render(sessionStatus(pending, 'en'), container);
      expect(container.querySelectorAll('.chat-open-btn')).toHaveLength(0);
    } finally {
      container.remove();
    }
  });
});
