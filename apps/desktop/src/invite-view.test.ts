// §21 punch-list item 6: sending the same request twice used to be possible
// and produced a run of errors. The rule the UI has to hold up is that one
// request goes out and the Connect button stays disabled until the far side
// has accepted or refused — which is *not* when `invite_connect` resolves.
import { render } from 'lit-html';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const { invoke } = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock('@tauri-apps/api/core', () => ({ invoke }));

let container: HTMLElement;

beforeEach(() => {
  vi.resetModules();
  invoke.mockReset();
  invoke.mockResolvedValue(undefined);
  container = document.createElement('div');
  document.body.appendChild(container);
});

afterEach(() => {
  container.remove();
});

type InviteView = typeof import('./invite-view');

async function load(): Promise<InviteView> {
  return import('./invite-view');
}

function connectButton(): HTMLButtonElement {
  const button = container.querySelector<HTMLButtonElement>('button.connect-btn');
  if (!button) {
    throw new Error('the connect form has no submit button');
  }
  return button;
}

/** Fills the ticket field and submits, the way a paste-and-click does. */
function submit(ticket: string): void {
  const input = container.querySelector<HTMLInputElement>('#ticket-input');
  const form = container.querySelector('form');
  if (!input || !form) {
    throw new Error('the connect form is not rendered');
  }
  input.value = ticket;
  form.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }));
}

/**
 * Lets the dial's own promise chain finish.
 *
 * The submit handler is deliberately not awaited by the DOM, and the sink goes
 * through a dynamic `import()` — all microtasks, so one macrotask turn is
 * enough and does not depend on counting `await`s inside the module.
 */
function settle(): Promise<void> {
  return new Promise((resolve) => {
    setTimeout(resolve, 0);
  });
}

describe('connect form: one request at a time', () => {
  it('stays disabled after the dial resolves, because the host has not decided yet', async () => {
    const view = await load();
    render(view.connectPanel('en'), container);
    expect(connectButton().disabled).toBe(false);

    submit('lumepeer1:abc');
    await settle();
    expect(invoke).toHaveBeenCalledWith('invite_connect', { args: { ticket: 'lumepeer1:abc' } });
    // `invite_connect` has already resolved here: the handshake landed. The
    // wait that matters — the host user's decision — is still outstanding.
    expect(view.isConnecting()).toBe(true);

    render(view.connectPanel('en'), container);
    expect(connectButton().disabled).toBe(true);
    expect(connectButton().textContent?.trim()).toMatch(/^Connecting\.{1,3}$/);

    view.setConnectPhase('connected');
    render(view.connectPanel('en'), container);
    expect(connectButton().disabled).toBe(false);
  });

  it('ignores a second submit while the first is still outstanding', async () => {
    const view = await load();
    render(view.connectPanel('en'), container);

    submit('lumepeer1:abc');
    await settle();
    expect(invoke).toHaveBeenCalledTimes(1);

    submit('lumepeer1:abc');
    submit('lumepeer1:abc');
    await settle();
    expect(invoke).toHaveBeenCalledTimes(1);

    view.setConnectPhase('connected');
  });

  it('reports a refusal in words rather than leaving the form silent', async () => {
    const view = await load();
    render(view.connectPanel('en'), container);

    submit('lumepeer1:abc');
    await settle();

    view.setConnectPhase('denied');
    render(view.connectPanel('en'), container);
    expect(connectButton().disabled).toBe(false);
    expect(container.querySelector('.connect-error')?.textContent).toContain('declined');
  });

  /// The `[object Object]` bug: Tauri rejects with a plain `{code, message}`
  /// object, not an `Error`, so stringifying it naively loses the message.
  it('shows the message from a rejected command, not [object Object]', async () => {
    invoke.mockRejectedValue({ code: 'BAD_TICKET', message: 'the invite is not valid or has expired' });
    const view = await load();
    render(view.connectPanel('en'), container);

    submit('nonsense');
    await settle();
    render(view.connectPanel('en'), container);
    expect(container.querySelector('.connect-error')).not.toBeNull();
    expect(container.querySelector('.connect-error')?.textContent).toContain(
      'the invite is not valid or has expired',
    );
    expect(container.querySelector('.connect-error')?.textContent).not.toContain('[object Object]');
  });
});

describe('remembered hosts', () => {
  it('reconnects by label, never by handing the invite code back to the webview', async () => {
    const view = await load();
    await view.reconnect('host-ab12');
    expect(invoke).toHaveBeenCalledWith('history_connect', { args: { peer: 'host-ab12' } });
    view.setConnectPhase('connected');
  });
});
