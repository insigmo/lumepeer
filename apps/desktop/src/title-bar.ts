// Custom title bar chrome (PRODUCT.md: "custom, non-OS title-bar chrome").
//
// The main window is created with decorations: false (tauri.conf.json), so
// this is the only way left to minimize/maximize/close it — the window
// controls here are a functional replacement for the OS ones, not decoration.
//
// data-tauri-drag-region is the other half of that replacement: a mousedown on
// an element carrying it starts a window drag, and a double click toggles
// maximize. It needs core:window:allow-start-dragging in the main window
// capability, otherwise the drag is denied and the bar feels dead.

import { html, type TemplateResult } from 'lit-html';

import { logoMark } from './logo';
import type { Locale } from './i18n';
import { t } from './i18n';

async function minimize(): Promise<void> {
  const { getCurrentWindow } = await import('@tauri-apps/api/window');
  await getCurrentWindow().minimize();
}

async function toggleMaximize(): Promise<void> {
  const { getCurrentWindow } = await import('@tauri-apps/api/window');
  await getCurrentWindow().toggleMaximize();
}

async function closeWindow(): Promise<void> {
  const { getCurrentWindow } = await import('@tauri-apps/api/window');
  await getCurrentWindow().close();
}

export function titleBar(locale: Locale): TemplateResult {
  return html`
    <div class="title-bar" data-tauri-drag-region>
      <div class="title-bar-left" data-tauri-drag-region>${logoMark()}<span>Lumepeer</span></div>
      <div class="title-bar-controls">
        <button type="button" aria-label=${t(locale, 'titlebar.minimize')} @click=${() => void minimize()}>
          <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
            <line x1="1" y1="5" x2="9" y2="5" stroke="currentColor" stroke-width="1" />
          </svg>
        </button>
        <button type="button" aria-label=${t(locale, 'titlebar.maximize')} @click=${() => void toggleMaximize()}>
          <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
            <rect x="1.5" y="1.5" width="7" height="7" stroke="currentColor" stroke-width="1" fill="none" />
          </svg>
        </button>
        <button
          type="button"
          class="close-btn"
          aria-label=${t(locale, 'titlebar.close')}
          @click=${() => void closeWindow()}
        >
          <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
            <line x1="1" y1="1" x2="9" y2="9" stroke="currentColor" stroke-width="1" />
            <line x1="9" y1="1" x2="1" y2="9" stroke="currentColor" stroke-width="1" />
          </svg>
        </button>
      </div>
    </div>
  `;
}
