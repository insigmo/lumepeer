// Port forwarding, as the host sees it (design doc §4.1, §8.2; ADR 0078).
//
// A tunnel is the one capability that reaches past the host's machine and
// into the network it sits in, so this panel is about two things and not one:
// which addresses this guest may reach, and what is going through them right
// now. The `tunnel` grant says a tunnel may exist at all and comes with full
// control; every address on it is a separate, deliberate act by the host, and
// the list starts empty.
//
// Nothing here decides anything. Adding an address is a command the actor
// authorizes, the host re-reads both halves for every connection that goes
// through, and taking an address back closes the connections that were using
// it — so the list on screen is always a description of what is happening
// rather than of what was once agreed.

import { html, type TemplateResult } from 'lit-html';

import { formatSize } from './file-transfers';
import type { Locale } from './i18n';
import { t } from './i18n';

/** One address a session's tunnel may reach, as `tunnel_status` reports it. */
export interface TunnelRow {
  peer_label: string;
  host: string;
  port: number;
  /** How many TCP connections are open through it right now. */
  streams: number;
  /** Bytes carried through this session's tunnel, both directions. */
  bytes: number;
}

/** How this panel talks to Tauri; injectable so the logic is testable. */
export interface TunnelCommands {
  setTarget(peer: string, host: string, port: number, allowed: boolean): Promise<void>;
  closeAll(peer: string): Promise<void>;
}

/** Default binding to the real IPC surface. */
export const tauriTunnelCommands: TunnelCommands = {
  async setTarget(peer, host, port, allowed) {
    const { invoke } = await import('@tauri-apps/api/core');
    return invoke('tunnel_set_target', { args: { peer, host, port, allowed } });
  },
  async closeAll(peer) {
    const { invoke } = await import('@tauri-apps/api/core');
    return invoke('tunnel_close_all', { args: { peer } });
  },
};

/**
 * Splits `host:port` into its two halves, or refuses it.
 *
 * Deliberately strict and deliberately not a validator of host *names*: what
 * an address is, is decided in Rust by `TunnelTarget::named`, and this only
 * has to get the two halves apart well enough to hand them over. An IPv6
 * literal is written in brackets, because otherwise its own colons are
 * indistinguishable from the one before the port.
 */
export function parseAddress(text: string): { host: string; port: number } | null {
  const trimmed = text.trim();
  const bracketed = /^\[(?<host>[^\]]+)\]:(?<port>\d{1,5})$/u.exec(trimmed);
  const plain = /^(?<host>[^:\s]+):(?<port>\d{1,5})$/u.exec(trimmed);
  const groups = bracketed?.groups ?? plain?.groups;
  if (!groups?.['host'] || !groups['port']) {
    return null;
  }
  const port = Number(groups['port']);
  if (!Number.isInteger(port) || port < 1 || port > 65535) {
    return null;
  }
  return { host: groups['host'], port };
}

/** Whether anything is actually being carried through this session's tunnel. */
export function isForwarding(rows: readonly TunnelRow[], peer: string): boolean {
  return rows.some((row) => row.peer_label === peer && row.streams > 0);
}

/**
 * The port-forwarding panel of one active session.
 *
 * Rendered only for a session that holds the grant: a list of addresses that
 * cannot be reached is not information, it is a control that does nothing.
 */
export function tunnelPanel(
  peer: string,
  rows: readonly TunnelRow[],
  locale: Locale,
  commands: TunnelCommands,
  onChange: () => void = () => {},
): TemplateResult {
  const mine = rows.filter((row) => row.peer_label === peer);
  const open = mine.reduce((total, row) => total + row.streams, 0);
  const bytes = mine.reduce((total, row) => Math.max(total, row.bytes), 0);

  const run = (promise: Promise<void>): void => {
    void promise.then(onChange, (error: unknown) => {
      console.error('tunnel command failed:', error);
      onChange();
    });
  };

  const add = (event: Event): void => {
    event.preventDefault();
    const form = event.currentTarget as HTMLFormElement;
    const input = form.querySelector<HTMLInputElement>('input[name="address"]');
    const parsed = input ? parseAddress(input.value) : null;
    if (!parsed || !input) {
      input?.setCustomValidity(t(locale, 'tunnel.badAddress'));
      input?.reportValidity();
      return;
    }
    input.setCustomValidity('');
    input.value = '';
    run(commands.setTarget(peer, parsed.host, parsed.port, true));
  };

  return html`
    <section class="tunnel-panel" data-testid="tunnel-panel">
      <div class="tunnel-head">
        <h4>${t(locale, 'tunnel.heading')}</h4>
        ${open > 0
          ? html`<span class="tunnel-active" data-testid="tunnel-active"
              >${t(locale, 'tunnel.active')} · ${t(locale, 'tunnel.streams', String(open))} ·
              ${formatSize(bytes)}</span
            >`
          : ''}
      </div>
      ${mine.length === 0
        ? html`<p data-testid="tunnel-empty">${t(locale, 'tunnel.none')}</p>`
        : html`
            <ul class="tunnel-targets">
              ${mine.map(
                (row) => html`
                  <li data-testid="tunnel-target">
                    <span class="tunnel-address">${row.host}:${row.port}</span>
                    <span class="tunnel-streams">${t(locale, 'tunnel.streams', String(row.streams))}</span>
                    <button
                      type="button"
                      data-testid="tunnel-deny"
                      aria-label=${`${t(locale, 'tunnel.deny')}: ${row.host}:${row.port}`}
                      @click=${() => run(commands.setTarget(peer, row.host, row.port, false))}
                    >
                      ${t(locale, 'tunnel.deny')}
                    </button>
                  </li>
                `,
              )}
            </ul>
          `}
      <form class="tunnel-add" data-testid="tunnel-add" @submit=${add}>
        <label>
          ${t(locale, 'tunnel.addressLabel')}
          <input
            type="text"
            name="address"
            data-testid="tunnel-address"
            placeholder=${t(locale, 'tunnel.addressPlaceholder')}
            @input=${(event: Event) => {
              (event.currentTarget as HTMLInputElement).setCustomValidity('');
            }}
          />
        </label>
        <button type="submit" data-testid="tunnel-allow">${t(locale, 'tunnel.allow')}</button>
      </form>
      <button
        type="button"
        data-testid="tunnel-close-all"
        ?disabled=${open === 0}
        @click=${() => run(commands.closeAll(peer))}
      >
        ${t(locale, 'tunnel.closeAll')}
      </button>
    </section>
  `;
}
