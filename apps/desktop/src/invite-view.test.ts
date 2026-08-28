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

  // ADR 0027: the dial runs off the actor loop, so `invite_connect` resolving
  // now means "the attempt started", not "the handshake landed". The button
  // has to stay disabled across both waits or the user fires a second attempt
  // into the first one.
  it('stays disabled through the dial as well as the decision', async () => {
    const view = await load();
    render(view.connectPanel('en'), container);

    submit('lumepeer1:abc');
    await settle();
    view.setConnectPhase('dialing', null);
    expect(view.isConnecting()).toBe(true);
    render(view.connectPanel('en'), container);
    expect(connectButton().disabled).toBe(true);

    view.setConnectPhase('awaiting_consent', null);
    expect(view.isConnecting()).toBe(true);

    view.setConnectPhase('connected', null);
    render(view.connectPanel('en'), container);
    expect(connectButton().disabled).toBe(false);
  });

  // Before ADR 0027 every transport failure reached the user as one sentence,
  // which is what sent the field report of ADR 0026 to the wrong machine.
  it('names the failure it was, not just that there was one', async () => {
    const view = await load();
    render(view.connectPanel('en'), container);

    submit('lumepeer1:abc');
    await settle();
    view.setConnectPhase('failed', 'DIAL_FAILED');
    render(view.connectPanel('en'), container);
    const message = container.querySelector('.connect-error')?.textContent ?? '';
    expect(message).toContain('Could not reach');
    expect(message).not.toContain('DIAL_FAILED');
  });

  it('falls back to the generic wording for a code it does not know', async () => {
    const view = await load();
    render(view.connectPanel('en'), container);

    submit('lumepeer1:abc');
    await settle();
    view.setConnectPhase('failed', 'SOMETHING_NEW');
    render(view.connectPanel('en'), container);
    expect(container.querySelector('.connect-error')?.textContent).toContain(
      'ended before it was accepted',
    );
  });

  // A fresh attempt must clear what the last one said, or the form shows a
  // stale failure underneath a live "Connecting..." button.
  it('clears the previous failure when a new attempt starts', async () => {
    const view = await load();
    render(view.connectPanel('en'), container);

    submit('lumepeer1:abc');
    await settle();
    view.setConnectPhase('failed', 'DIAL_FAILED');
    render(view.connectPanel('en'), container);
    expect(container.querySelector('.connect-error')).not.toBeNull();

    view.setConnectPhase('dialing', null);
    render(view.connectPanel('en'), container);
    expect(container.querySelector('.connect-error')).toBeNull();

    view.setConnectPhase('connected', null);
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

describe('invite code panel', () => {
  /** Creates an invite and renders the panel that shows it. */
  async function withCode(view: InviteView, code: string): Promise<void> {
    invoke.mockResolvedValue({ code });
    render(view.inviteCodePanel('en'), container);
    container.querySelector<HTMLButtonElement>('.create-btn')?.click();
    await vi.waitFor(() => {
      render(view.inviteCodePanel('en'), container);
      expect(container.querySelector('.code-box')).not.toBeNull();
    });
  }

  const CODE = `lumepeer1:${'x'.repeat(200)}`;

  // Two controls used to copy the code: the code box was a button as well as
  // the Copy button next to it. One affordance, on the button that says so.
  it('offers exactly one control that copies, and it is not the code itself', async () => {
    const view = await load();
    await withCode(view, CODE);

    const codeBox = container.querySelector('.code-box');
    expect(codeBox).not.toBeNull();
    expect(codeBox?.tagName).not.toBe('BUTTON');

    const buttons = container.querySelectorAll('button');
    expect(buttons).toHaveLength(1);
    expect(buttons[0]?.className).toBe('copy-btn');
    expect(buttons[0]?.textContent?.trim()).toBe('Copy invite code');
  });

  it('copies the whole code, not the truncated line on screen', async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, 'clipboard', {
      configurable: true,
      value: { writeText },
    });
    const view = await load();
    await withCode(view, CODE);

    container.querySelector<HTMLButtonElement>('.copy-btn')?.click();
    await vi.waitFor(() => {
      expect(writeText).toHaveBeenCalledWith(CODE);
    });
  });

  // The line is clipped by CSS, which jsdom has no layout engine to apply, so
  // what is pinned here is the part that survives without one: the whole code
  // is on `title` and can be read by hovering.
  it('keeps the whole code on title even though the line is truncated', async () => {
    const view = await load();
    await withCode(view, CODE);
    expect(container.querySelector('.code-box')?.getAttribute('title')).toBe(CODE);
  });

  // Reissuing retires every code handed out before it (ADR 0016). That is a
  // revoke, and it belongs in settings under a name that says so — not one
  // button below Copy.
  it('does not offer to reissue the code from the sidebar', async () => {
    const view = await load();
    await withCode(view, CODE);
    expect(container.querySelector('.create-btn')).toBeNull();
    expect(container.textContent).not.toContain('Refresh');
  });
});

describe('credentials form: a refused password can be corrected', () => {
  function credentialsSubmitButton(): HTMLButtonElement {
    const button = container.querySelector<HTMLButtonElement>('button.credentials-submit');
    if (!button) {
      throw new Error('the credentials form has no submit button');
    }
    return button;
  }

  /** Fills the password field and submits, the way typing and Enter does. */
  function submitCredentials(password: string): void {
    const input = container.querySelector<HTMLInputElement>('#device-password');
    const form = container.querySelector('form.credentials-form');
    if (!input || !form) {
      throw new Error('the credentials form is not rendered');
    }
    input.value = password;
    form.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }));
  }

  // docs/bugs/02-connect-form.md, task 4: `submitting` used to be reset only
  // on a phase change, and a bad password leaves the phase at
  // `awaiting_credentials` — so the submit button, and with it implicit
  // submission on Enter, stayed disabled forever after one wrong guess.
  it('lets a second, corrected password reach unattended_submit', async () => {
    const view = await load();
    view.setConnectPhase('awaiting_credentials');
    render(view.credentialsPanel('en'), container);

    submitCredentials('wrong-password');
    await settle();
    expect(invoke).toHaveBeenCalledWith('unattended_submit', {
      args: { password: 'wrong-password', code: null },
    });

    view.setConnectPhase('awaiting_credentials', 'UNATTENDED_BAD_PASSWORD');
    render(view.credentialsPanel('en'), container);
    expect(credentialsSubmitButton().disabled).toBe(false);

    submitCredentials('right-password');
    await settle();
    expect(invoke).toHaveBeenCalledWith('unattended_submit', {
      args: { password: 'right-password', code: null },
    });

    view.setConnectPhase('connected');
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
