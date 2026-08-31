// File transfer panel (design doc §9.2; ADR 0032).
//
// Two lists and three buttons. The first list is offers waiting for an
// answer, because an incoming file is a decision and not a notification: it
// arrives with a name and a size and goes nowhere until someone says yes. The
// second is what is actually moving, with a cancel that works at any point.
//
// Nothing here decides anything, and nothing here touches a filesystem. Both
// pickers — the file to send, the directory to receive into — run in Rust,
// because `capabilities/view.json` gives a view window no filesystem rights
// and a picker in the webview would be that right under another name (§2.3).
// What crosses the IPC boundary is a peer label, a basename and a byte count.

import { html, type TemplateResult } from 'lit-html';

import type { Locale } from './i18n';
import { t } from './i18n';

/** How a transfer ended, mirroring `TransferState` in the actor. */
export type TransferState = 'running' | 'completed' | 'cancelled' | 'failed';

/** One offer waiting for this user to answer. */
export interface OfferRow {
  peer_label: string;
  name: string;
  size: number;
  /** Whether this offer came from the peer's clipboard rather than its file
   *  picker (docs/bugs/14-clipboard-files.md #1, #3). */
  from_clipboard: boolean;
}

/** One transfer running or recently ended. */
export interface TransferRow {
  peer_label: string;
  transfer_id: number;
  name: string;
  size: number;
  moved: number;
  incoming: boolean;
  state: TransferState;
  /** Whether this transfer started from the peer's clipboard (docs/bugs/
   *  14-clipboard-files.md #3). */
  from_clipboard: boolean;
}

/** What `file_transfers` returns in one poll. */
export interface FileTransfers {
  offers: OfferRow[];
  transfers: TransferRow[];
}

/** How this panel talks to Tauri; injectable so the logic is testable. */
export interface FileCommands {
  offer(peer: string): Promise<void>;
  offerClipboard(peer: string): Promise<void>;
  accept(peer: string, accept: boolean, fromClipboard: boolean): Promise<void>;
  abort(peer: string, transferId: number): Promise<void>;
  list(): Promise<FileTransfers>;
}

/** Default binding to the real IPC surface. */
export const tauriFileCommands: FileCommands = {
  async offer(peer) {
    const { invoke } = await import('@tauri-apps/api/core');
    return invoke('file_offer', { peer });
  },
  async offerClipboard(peer) {
    const { invoke } = await import('@tauri-apps/api/core');
    return invoke('file_offer_clipboard', { peer });
  },
  async accept(peer, accept, fromClipboard) {
    const { invoke } = await import('@tauri-apps/api/core');
    return invoke('file_accept', {
      args: { peer, accept, from_clipboard: fromClipboard },
    });
  },
  async abort(peer, transferId) {
    const { invoke } = await import('@tauri-apps/api/core');
    // `transfer_id`: `FileAbortArgs` is deserialized by plain serde, which
    // does not rename anything, so `transferId` never reached it and the
    // cancel button silently did nothing.
    return invoke('file_abort', { args: { peer, transfer_id: transferId } });
  },
  async list() {
    const { invoke } = await import('@tauri-apps/api/core');
    return (await invoke('file_transfers')) as FileTransfers;
  },
};

const UNITS = ['B', 'KB', 'MB', 'GB'] as const;

/**
 * A byte count a person can read at a glance.
 *
 * Deliberately locale-independent: these are unit symbols, not prose, and an
 * RTL layout puts the number and the symbol in the right order on its own.
 */
export function formatSize(bytes: number): string {
  let value = Math.max(0, bytes);
  let unit = 0;
  while (value >= 1024 && unit < UNITS.length - 1) {
    value /= 1024;
    unit += 1;
  }
  const rounded = unit === 0 ? Math.round(value) : Math.round(value * 10) / 10;
  return `${rounded} ${UNITS[unit]}`;
}

/** Whole-percent progress, clamped, with a zero-size transfer counting as done. */
export function percent(row: TransferRow): number {
  if (row.size === 0) {
    return 100;
  }
  return Math.min(100, Math.max(0, Math.round((row.moved / row.size) * 100)));
}

const STATE_KEY = {
  completed: 'files.state.completed',
  cancelled: 'files.state.cancelled',
  failed: 'files.state.failed',
} as const;

/**
 * Renders the panel for one peer.
 *
 * Scoped to a peer rather than global because a grant is per session: the
 * "send a file" button belongs next to the session it would use, and an offer
 * belongs to the session it arrived on.
 */
export function fileTransferPanel(
  peer: string,
  data: FileTransfers,
  locale: Locale,
  commands: FileCommands,
  onChange: () => void = () => {},
): TemplateResult {
  const offers = data.offers.filter((offer) => offer.peer_label === peer);
  const transfers = data.transfers.filter((row) => row.peer_label === peer);

  const answer = (accept: boolean, fromClipboard: boolean): void => {
    void commands.accept(peer, accept, fromClipboard).then(onChange, (error: unknown) => {
      console.error('file_accept failed:', error);
      onChange();
    });
  };

  return html`
    <section class="file-panel" data-testid="file-panel">
      <div class="file-panel-head">
        <h3>${t(locale, 'files.heading')}</h3>
        <button
          type="button"
          class="file-send-btn"
          data-testid="file-send"
          aria-label=${`${t(locale, 'files.send')}: ${peer}`}
          @click=${() => {
            void commands.offer(peer).then(onChange, (error: unknown) => {
              console.error('file_offer failed:', error);
              onChange();
            });
          }}
        >
          ${t(locale, 'files.send')}
        </button>
        <button
          type="button"
          class="file-send-clipboard-btn"
          data-testid="file-send-clipboard"
          aria-label=${`${t(locale, 'files.sendClipboard')}: ${peer}`}
          @click=${() => {
            void commands.offerClipboard(peer).then(onChange, (error: unknown) => {
              console.error('file_offer_clipboard failed:', error);
              onChange();
            });
          }}
        >
          ${t(locale, 'files.sendClipboard')}
        </button>
      </div>
      ${offers.length === 0
        ? ''
        : html`
            <ul class="file-offers" aria-live="polite">
              ${offers.map(
                (offer) => html`
                  <li data-testid="file-offer">
                    <span class="file-name">${offer.name}</span>
                    <span class="file-size">${formatSize(offer.size)}</span>
                    ${offer.from_clipboard
                      ? html`<span class="file-clipboard-tag" data-testid="file-clipboard-tag"
                          >${t(locale, 'files.fromClipboard')}</span
                        >`
                      : ''}
                    <button
                      type="button"
                      data-testid="file-accept"
                      aria-label=${`${t(locale, 'files.accept')}: ${offer.name}`}
                      @click=${() => answer(true, offer.from_clipboard)}
                    >
                      ${t(locale, 'files.accept')}
                    </button>
                    <button
                      type="button"
                      data-testid="file-decline"
                      aria-label=${`${t(locale, 'files.decline')}: ${offer.name}`}
                      @click=${() => answer(false, offer.from_clipboard)}
                    >
                      ${t(locale, 'files.decline')}
                    </button>
                  </li>
                `,
              )}
            </ul>
          `}
      ${transfers.length === 0
        ? ''
        : html`
            <ul class="file-transfers" aria-live="polite">
              ${transfers.map(
                (row) => html`
                  <li data-testid="file-transfer">
                    <span class="file-direction"
                      >${t(locale, row.incoming ? 'files.incoming' : 'files.outgoing')}</span
                    >
                    <span class="file-name">${row.name}</span>
                    <span class="file-size">${formatSize(row.size)}</span>
                    ${row.from_clipboard
                      ? html`<span class="file-clipboard-tag" data-testid="file-clipboard-tag"
                          >${t(locale, 'files.fromClipboard')}</span
                        >`
                      : ''}
                    ${row.state === 'running'
                      ? html`
                          <progress
                            data-testid="file-progress"
                            max="100"
                            value=${percent(row)}
                            aria-label=${`${row.name}: ${percent(row)}%`}
                          ></progress>
                          <button
                            type="button"
                            data-testid="file-cancel"
                            aria-label=${`${t(locale, 'files.cancel')}: ${row.name}`}
                            @click=${() => {
                              void commands
                                .abort(peer, row.transfer_id)
                                .then(onChange, (error: unknown) => {
                                  console.error('file_abort failed:', error);
                                  onChange();
                                });
                            }}
                          >
                            ${t(locale, 'files.cancel')}
                          </button>
                        `
                      : html`<span class="file-state" data-testid="file-state"
                          >${t(locale, STATE_KEY[row.state])}</span
                        >`}
                  </li>
                `,
              )}
            </ul>
          `}
    </section>
  `;
}
