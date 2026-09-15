// The host's warning that somebody else is about to take this machine down
// (§4.1; ADR 0084).
//
// The one surface in this app with a deadline attached to it. Everything else
// the host is shown can wait for the person to look; this cannot, because the
// window it renders is `REBOOT_WARNING_SECS` long and then the machine goes
// away. The actor raises the app window the moment it starts (`main.rs`),
// exactly as it does for a consent request, and this is what is waiting there.
//
// It decides nothing. `reboot_cancel` is the decision, and it is made in Rust.

import { html, nothing, type TemplateResult } from 'lit-html';

import { t, type Locale } from './i18n';

/**
 * A running warning, as `reboot_pending` hands it over.
 *
 * `peer_label` is the pseudonymized label (§15), never a name or an address —
 * the same string the session list shows, so the person can tell which of
 * several guests is asking.
 */
export interface RebootPending {
  peer_label: string;
  mode: 'reboot' | 'shutdown';
  seconds_left: number;
}

/**
 * The banner, or nothing at all when this machine has not been asked.
 *
 * Two things it deliberately does:
 *
 * - **Names the act.** "Restart" and "shut down" are different decisions —
 *   one of them ends remote access until somebody walks to the machine — so
 *   they get different sentences rather than one with a word swapped into it.
 * - **Says it out loud.** `role="alert"` rather than the `role="status"` the
 *   other banners here use: a screen reader must interrupt for this one.
 *   `aria-live` is left to the implicit assertive of `alert`, and the
 *   countdown lives in the same element so it is not announced again every
 *   second on its own.
 */
export function rebootBanner(
  pending: RebootPending | null | undefined,
  locale: Locale,
  onCancel: () => void,
): TemplateResult | typeof nothing {
  // Falsy rather than `=== null`: "nothing came back" and "nobody asked" are
  // the same picture, and of the two readings only this one is safe when a
  // poll answers with something unexpected.
  if (!pending) {
    return nothing;
  }
  const title = pending.mode === 'shutdown' ? 'reboot.banner.shutdown' : 'reboot.banner.reboot';
  return html`<div class="reboot-banner" role="alert" data-testid="reboot-banner">
    <div class="reboot-banner-text">
      <strong>${t(locale, title, pending.peer_label)}</strong>
      <span>${t(locale, 'reboot.banner.countdown', String(pending.seconds_left))}</span>
    </div>
    <button type="button" class="reboot-banner-cancel" data-testid="reboot-cancel" @click=${onCancel}>
      ${t(locale, 'reboot.banner.cancel')}
    </button>
  </div>`;
}
