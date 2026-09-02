// The independent grants of §8.2 as the host panel now shows them (ADR 0029;
// ADR 0048; ADR 0049).
//
// There are no per-grant switches any more: a session starts holding exactly
// what its role brings (`Grants::from_role` in `lumepeer-core`), so the panel
// reports permissions and never offers to change them. The point of these
// tests is that no path back to a switch has crept in, and that the one
// remaining `session_set_grant` caller — answering a guest's record request —
// still goes through the core rather than deciding here.
import { render } from 'lit-html';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import type { SessionStatus } from './session-status';
import { sessionStatus } from './session-status';

const invoke = vi.fn().mockResolvedValue(undefined);

vi.mock('@tauri-apps/api/core', () => ({
  invoke: (...args: unknown[]) => invoke(...args),
}));

/** A full-control session, holding everything that role brings. */
const fullControl: SessionStatus = {
  peer_label: 'guest-ab12',
  role: 'full_control',
  input: true,
  state: 'active',
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

/**
 * A view-only session, holding nothing but the picture — which now includes
 * the secure desktop, on for every role (ADR 0056).
 */
const viewOnly: SessionStatus = {
  ...fullControl,
  peer_label: 'guest-cd34',
  role: 'view_only',
  input: false,
  clipboard_read: false,
  clipboard_write: false,
  file_transfer: false,
  recording: false,
  display_mode: false,
  secure_desktop: true,
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

describe('independent grants on the host panel', () => {
  it('offers no grant switches at all: the role decided, not this panel', () => {
    render(sessionStatus([fullControl, viewOnly], 'en'), container);

    expect(container.querySelectorAll('.grant-row')).toHaveLength(0);
    expect(container.querySelectorAll('.session-grants')).toHaveLength(0);
    expect(container.querySelectorAll('input[type="checkbox"]')).toHaveLength(0);
  });

  it('lets a full-control session be recorded, because the role brought the grant', () => {
    render(sessionStatus([fullControl], 'en'), container);

    const button = container.querySelector<HTMLButtonElement>('[data-testid="record-toggle"]');
    expect(button?.disabled).toBe(false);
  });

  it('leaves a view-only session unrecordable, because its role brings nothing', () => {
    render(sessionStatus([viewOnly], 'en'), container);

    const button = container.querySelector<HTMLButtonElement>('[data-testid="record-toggle"]');
    expect(button?.disabled).toBe(true);
  });

  it('answering a record request still asks the core for the grant', async () => {
    render(sessionStatus([{ ...viewOnly, record_request: true }], 'en'), container);

    container.querySelector<HTMLButtonElement>('[data-testid="record-allow"]')?.click();

    // The IPC module is loaded on demand, so the call lands a microtask later.
    await vi.waitFor(() =>
      expect(invoke).toHaveBeenCalledWith('session_set_grant', {
        args: { peer: 'guest-cd34', grant: 'recording', allowed: true },
      }),
    );
  });
});
