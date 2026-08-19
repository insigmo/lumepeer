// Persistent connectivity status indicator (PRODUCT.md: green dot "Ready to
// connect" / red dot "Not ready to connect"). Variant A of
// status-pill-preview.html — light, tinted to the sidebar's own palette.

import { html, type TemplateResult } from 'lit-html';

import type { Locale } from './i18n';
import { t } from './i18n';

export function statusPill(ready: boolean, locale: Locale): TemplateResult {
  const state = ready ? 'ready' : 'not-ready';
  const label = ready ? t(locale, 'status.ready') : t(locale, 'status.notReady');
  return html`
    <div class="status-pill" data-state=${state} role="status" aria-live="polite">
      <span class="status-dot" aria-hidden="true"></span>
      <span class="status-text">${label}</span>
    </div>
  `;
}
