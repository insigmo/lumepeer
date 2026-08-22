// In-session chat panel (design doc §9.2; ADR 0023).
//
// Split from the `view.ts` entry point so the state machine — transcript
// bookkeeping, send gating, the poll loop — is testable in jsdom, exactly
// like `view-window.ts`. The panel never decides anything: the Rust actor
// validates every message against §9.2, keeps the authoritative transcript
// and refuses sends without a live session.

import { html, render, type TemplateResult } from 'lit-html';

import type { Locale } from './i18n';
import { t } from './i18n';

/// One transcript row as the IPC `chat_transcript` returns it.
export interface ChatRow {
  outgoing: boolean;
  text: string;
  atUnix: number;
}

/** How the panel talks to Tauri; injectable for tests. */
export interface ChatCommands {
  chatTranscript(peer: string): Promise<ChatRow[]>;
  chatSend(peer: string, text: string): Promise<unknown>;
}

/** Default binding to the real IPC surface. */
export const tauriChatCommands: ChatCommands = {
  async chatTranscript(peer) {
    const { invoke } = await import('@tauri-apps/api/core');
    return (await invoke('chat_transcript', { peer })) as ChatRow[];
  },
  async chatSend(peer, text) {
    const { invoke } = await import('@tauri-apps/api/core');
    return invoke('chat_send', { args: { peer, text } });
  },
};

/** Local, render-side mirror of the actor's transcript. */
export class ChatState {
  private rows: ChatRow[] = [];

  /** Replaces the local mirror with the authoritative one. */
  replace(rows: ChatRow[]): void {
    this.rows = rows;
  }

  /** Current rows, oldest first. */
  get transcript(): readonly ChatRow[] {
    return this.rows;
  }
}

/**
 * Renders the chat panel into `container`.
 *
 * The input is a plain `<input>` on purpose: chat is one-line messages, and
 * Enter-to-send must not fight the remote-control keyboard forwarding —
 * typing into the panel never reaches the host, because focus sits in the
 * panel's input, not on the canvas surface.
 */
export function renderChat(
  container: HTMLElement,
  state: ChatState,
  locale: Locale,
  peer: string,
  commands: ChatCommands,
): void {
  const rows = state.transcript;
  const list = html`
    <ul class="chat-log" aria-label=${t(locale, 'chat.logLabel')}>
      ${rows.map(
        (row) => html`
          <li class=${row.outgoing ? 'chat-row chat-out' : 'chat-row chat-in'}>
            <span class="chat-text">${row.text}</span>
          </li>
        `,
      )}
    </ul>
  `;
  const form = html`
    <form
      class="chat-compose"
      @submit=${(event: Event) => {
        event.preventDefault();
        const input = container.querySelector<HTMLInputElement>('.chat-input');
        const text = input?.value.trim() ?? '';
        if (text.length === 0) {
          return;
        }
        if (input) {
          input.value = '';
        }
        void commands.chatSend(peer, text);
      }}
    >
      <input
        class="chat-input"
        type="text"
        maxlength="4096"
        .ariaLabel=${t(locale, 'chat.inputLabel')}
        .placeholder=${t(locale, 'chat.inputPlaceholder')}
      />
      <button type="submit" class="chat-send">${t(locale, 'chat.send')}</button>
    </form>
  `;
  render(html`${list}${form}`, container);
}

/** Template used by the entry point to place the panel inside its drawer. */
export function chatDrawer(): TemplateResult {
  return html`<aside id="chat-panel" class="chat-panel" hidden></aside>`;
}

/**
 * Wires a periodic transcript poll. The actor is the source of truth; the
 * panel re-reads on every interval and after each send, so a message the
 * peer sent shows up within one tick.
 */
export function startChatPolling(
  container: HTMLElement,
  state: ChatState,
  locale: Locale,
  peer: string,
  commands: ChatCommands,
  intervalMs = 1000,
): () => void {
  let stopped = false;
  const refresh = async (): Promise<void> => {
    try {
      state.replace(await commands.chatTranscript(peer));
      renderChat(container, state, locale, peer, commands);
    } catch {
      // Session ended; the window closes through the normal path.
      stopped = true;
    }
  };
  void refresh();
  const timer = window.setInterval(() => {
    if (!stopped) {
      void refresh();
    }
  }, intervalMs);
  return () => {
    stopped = true;
    window.clearInterval(timer);
  };
}
