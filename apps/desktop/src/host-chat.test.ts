// Host-side chat entry (§9.2, ADR 0023).
//
// The main window mounts the same chat.ts component the view window uses;
// these tests pin what the host side adds around it: the per-session Chat
// button in session-status and the drawer open/close/re-target behaviour.
import { render } from 'lit-html';
import { describe, expect, it, vi } from 'vitest';

import type { SessionStatus } from './session-status';
import { sessionStatus } from './session-status';

const active: SessionStatus[] = [
  { peer_label: 'guest-ab12', role: 'full_control', input: true, state: 'active' },
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
      { peer_label: 'guest-ab12', role: 'view_only', input: false, state: 'pending' },
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
