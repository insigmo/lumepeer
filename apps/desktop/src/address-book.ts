// Host-side address book: saved devices, tags and trust (§8; ADR 0034).
//
// Trust is the one control on this screen that widens what a stranger's
// machine may do, so it is the one control that never moves on a single
// click: turning it on opens a confirmation that says what the consequence
// is, in words, before anything is sent. Everything else here is a name, a
// tag or a note.
//
// Names, tags and notes are free text the host typed. They are rendered
// through `lit-html` bindings, which escape; nothing on this screen builds
// markup by concatenating strings.

import { html, nothing, type TemplateResult } from 'lit-html';

import type { Locale } from './i18n';
import { t } from './i18n';

/** One row of `address_book_list`. */
export interface AddressBookEntry {
  peer_label: string;
  name: string;
  tags: string[];
  notes: string;
  trusted: boolean;
  connected: boolean;
}

type Invoke = (command: string, args?: Record<string, unknown>) => Promise<unknown>;

async function invoker(): Promise<Invoke> {
  const { invoke } = await import('@tauri-apps/api/core');
  return invoke as Invoke;
}

/** Tag currently filtering the list; empty means "all". */
let tagFilter = '';
/** Peer whose trust confirmation is open, if any. */
let confirming: string | undefined;
let onChange: (() => void) | undefined;

/** Lets main.ts re-render after an async change here. */
export function onAddressBookStateChange(callback: () => void): void {
  onChange = callback;
}

function notify(): void {
  onChange?.();
}

/** Test seam: drops the transient state between cases. */
export function resetAddressBookPanel(): void {
  tagFilter = '';
  confirming = undefined;
}

async function setTrusted(peer: string, trusted: boolean): Promise<void> {
  const invoke = await invoker();
  await invoke('address_book_set_trusted', { args: { peer, trusted } });
}

async function save(peer: string, name: string, tags: string[], notes: string): Promise<void> {
  const invoke = await invoker();
  await invoke('address_book_upsert', { args: { peer, name, tags, notes } });
}

async function forget(peer: string): Promise<void> {
  const invoke = await invoker();
  await invoke('address_book_remove', { args: { peer } });
}

/** Every tag in use, for the filter. */
function allTags(entries: AddressBookEntry[]): string[] {
  return [...new Set(entries.flatMap((entry) => entry.tags))].sort();
}

/**
 * The confirmation a host has to pass through before a device becomes
 * trusted.
 *
 * Deliberately not a `confirm()` one-liner like the withdrawal below: this is
 * the direction that grants something, so it gets a real dialog with the
 * consequence spelled out, a named action button, and a cancel that is the
 * easy path.
 */
function trustConfirmation(
  entry: AddressBookEntry,
  locale: Locale,
  onRefresh: () => void,
): TemplateResult {
  return html`
    <div
      class="trust-confirm"
      role="alertdialog"
      aria-modal="true"
      aria-labelledby=${`trust-title-${entry.peer_label}`}
      aria-describedby=${`trust-body-${entry.peer_label}`}
    >
      <h4 id=${`trust-title-${entry.peer_label}`}>
        ${t(locale, 'book.trust.confirmTitle', entry.name || entry.peer_label)}
      </h4>
      <p id=${`trust-body-${entry.peer_label}`}>${t(locale, 'book.trust.confirmBody')}</p>
      <div class="trust-confirm-actions">
        <button
          type="button"
          class="trust-cancel"
          autofocus
          @click=${() => {
            confirming = undefined;
            notify();
          }}
        >
          ${t(locale, 'book.trust.cancel')}
        </button>
        <button
          type="button"
          class="trust-accept"
          @click=${() => {
            confirming = undefined;
            void setTrusted(entry.peer_label, true).then(onRefresh, (error: unknown) => {
              console.error('address_book_set_trusted failed:', error);
              onRefresh();
            });
          }}
        >
          ${t(locale, 'book.trust.confirmAction')}
        </button>
      </div>
    </div>
  `;
}

/**
 * The saved-devices panel.
 *
 * Nothing is toggled locally: a switch shows what the last `address_book_list`
 * reported, the click asks the core to change it, and `onRefresh` re-polls.
 */
export function addressBook(
  entries: AddressBookEntry[],
  locale: Locale,
  onRefresh: () => void = () => {},
): TemplateResult {
  const tags = allTags(entries);
  const shown = tagFilter ? entries.filter((entry) => entry.tags.includes(tagFilter)) : entries;
  return html`
    <section class="address-book" aria-labelledby="address-book-heading">
      <h3 id="address-book-heading">${t(locale, 'book.heading')}</h3>
      <p class="address-book-explain">${t(locale, 'book.explain')}</p>
      ${tags.length > 0
        ? html`
            <label class="book-filter-label" for="book-filter">${t(locale, 'book.filter.label')}</label>
            <select
              id="book-filter"
              @change=${(event: Event) => {
                tagFilter = (event.target as HTMLSelectElement).value;
                notify();
              }}
            >
              <option value="" ?selected=${tagFilter === ''}>${t(locale, 'book.filter.all')}</option>
              ${tags.map(
                (tag) => html`<option value=${tag} ?selected=${tagFilter === tag}>${tag}</option>`,
              )}
            </select>
          `
        : nothing}
      ${shown.length === 0
        ? html`<p class="address-book-empty">${t(locale, 'book.empty')}</p>`
        : html`
            <ul class="address-book-list">
              ${shown.map(
                (entry) => html`
                  <li class="book-row" data-testid="book-row">
                    <span class="book-name">${entry.name || entry.peer_label}</span>
                    <span class="peer-meta">${entry.peer_label}</span>
                    ${entry.connected
                      ? html`<span class="peer-meta book-connected">${t(locale, 'book.connected')}</span>`
                      : ''}
                    ${entry.tags.map((tag) => html`<span class="book-tag">${tag}</span>`)}
                    ${entry.notes ? html`<span class="book-note">${entry.notes}</span>` : ''}
                    <span class="book-trust-state" data-testid="book-trust-state"
                      >${t(locale, entry.trusted ? 'book.trusted' : 'book.untrusted')}</span
                    >
                    <label class="book-trust">
                      <input
                        type="checkbox"
                        .checked=${entry.trusted}
                        aria-label=${`${t(locale, 'book.trusted')}: ${entry.name || entry.peer_label}`}
                        @change=${(event: Event) => {
                          const wanted = (event.target as HTMLInputElement).checked;
                          // The box is put back where the core last had it:
                          // until the core answers, what it shows would be a
                          // claim this panel is not entitled to make.
                          (event.target as HTMLInputElement).checked = entry.trusted;
                          if (wanted) {
                            confirming = entry.peer_label;
                            notify();
                            return;
                          }
                          if (
                            !globalThis.confirm(
                              t(locale, 'book.untrust.confirm', entry.name || entry.peer_label),
                            )
                          ) {
                            return;
                          }
                          void setTrusted(entry.peer_label, false).then(onRefresh, (error: unknown) => {
                            console.error('address_book_set_trusted failed:', error);
                            onRefresh();
                          });
                        }}
                      />
                      <span>${t(locale, 'book.trusted')}</span>
                    </label>
                    <button
                      type="button"
                      class="book-forget"
                      @click=${() => {
                        if (
                          !globalThis.confirm(
                            t(locale, 'book.remove.confirm', entry.name || entry.peer_label),
                          )
                        ) {
                          return;
                        }
                        void forget(entry.peer_label).then(onRefresh, (error: unknown) => {
                          console.error('address_book_remove failed:', error);
                          onRefresh();
                        });
                      }}
                    >
                      ${t(locale, 'book.remove')}
                    </button>
                    ${confirming === entry.peer_label ? trustConfirmation(entry, locale, onRefresh) : ''}
                  </li>
                `,
              )}
            </ul>
          `}
    </section>
  `;
}

/**
 * The "save this device" control that turns a live session into a book entry.
 *
 * Saving is all it does. The new entry is untrusted, and it stays that way
 * until the host goes to the panel above and passes the confirmation: a device
 * must never become trusted as a side effect of having connected once (§2.1).
 */
export function saveDeviceButton(
  peerLabel: string,
  locale: Locale,
  onRefresh: () => void = () => {},
): TemplateResult {
  return html`
    <button
      type="button"
      class="book-save-btn"
      aria-label=${`${t(locale, 'book.addFromSession')}: ${peerLabel}`}
      @click=${() => {
        void save(peerLabel, peerLabel, [], '').then(onRefresh, (error: unknown) => {
          console.error('address_book_upsert failed:', error);
          onRefresh();
        });
      }}
    >
      ${t(locale, 'book.addFromSession')}
    </button>
  `;
}
