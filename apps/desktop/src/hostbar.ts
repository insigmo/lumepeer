// The host's always-on-top session bar (ADR 0055).
//
// Its own Tauri window, so that closing or minimizing the main one does not
// take the host's controls away with it: §2.2's "the person at this machine
// can see what is happening, and can stop it" has to hold while they are
// working in something else, and a taskbar button does not hold it.
//
// Deliberately small. Everything a session needs mid-flight that is not "who
// is connected" and "stop this" lives in the main window, which the one
// button at the bottom raises — a second full UI floating over every other
// application would be the opposite of staying out of the way.

import { html, render } from 'lit-html';

import { detectLocale, dirOf, t, type Locale } from './i18n';
import { logoMark } from './logo';
import type { SessionStatus } from './session-status';

const root = document.querySelector<HTMLElement>('#hostbar');
const locale: Locale = detectLocale(navigator);

/** How often the bar re-asks the actor who is connected. */
const POLL_MS = 1000;

/**
 * Whether the card is open, or collapsed to the edge tab.
 *
 * Kept here and mirrored onto the window by `host_bar_expand`: the window has
 * no chrome to resize itself by, so the page is what knows which of the two
 * shapes it is currently drawn at.
 */
let expanded = true;
let sessions: SessionStatus[] = [];

const ROLE_KEY = {
  view_only: 'status.role.viewOnly',
  control_limited: 'status.role.controlLimited',
  full_control: 'status.role.fullControl',
} as const;

/** Chevron pointing at the inline end — collapse. */
const CHEVRON_END = html`<svg viewBox="0 0 16 16" aria-hidden="true">
  <path d="M6 3l5 5-5 5" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" />
</svg>`;

/** Chevron pointing at the inline start — expand. */
const CHEVRON_START = html`<svg viewBox="0 0 16 16" aria-hidden="true">
  <path d="M10 3l-5 5 5 5" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" />
</svg>`;

async function invoke<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  const { invoke: call } = await import('@tauri-apps/api/core');
  return call<T>(command, args);
}

/**
 * Opens or collapses the bar.
 *
 * The window is resized first and the page re-rendered after, so the new
 * contents never paint into the old shape.
 */
function setExpanded(next: boolean): void {
  expanded = next;
  void invoke('host_bar_expand', { args: { expanded: next } }).then(draw, (error: unknown) => {
    // The resize was refused or the bar is on its way out with the session.
    // Draw what we asked for anyway: a page that stays in the old state while
    // the window moved is worse than one that is briefly the wrong size.
    console.error('host_bar_expand failed:', error);
    draw();
  });
}

function draw(): void {
  if (!root) {
    return;
  }
  document.documentElement.lang = locale;
  document.documentElement.dir = dirOf(locale);
  root.classList.toggle('is-collapsed', !expanded);

  if (!expanded) {
    render(
      html`
        <button
          type="button"
          class="tab-btn"
          data-testid="hostbar-expand"
          aria-label=${t(locale, 'hostbar.expand')}
          title=${t(locale, 'hostbar.expand')}
          @click=${() => setExpanded(true)}
        >
          ${CHEVRON_START}
        </button>
      `,
      root,
    );
    return;
  }

  render(
    html`
      <header class="bar-head" data-tauri-drag-region>
        <span class="bar-mark" data-tauri-drag-region>${logoMark()}</span>
        <span class="bar-name" data-tauri-drag-region>Lumepeer</span>
        <button
          type="button"
          class="tab-btn"
          data-testid="hostbar-collapse"
          aria-label=${t(locale, 'hostbar.collapse')}
          title=${t(locale, 'hostbar.collapse')}
          @click=${() => setExpanded(false)}
        >
          ${CHEVRON_END}
        </button>
      </header>
      <h1 class="bar-heading">${t(locale, 'connections.header')}</h1>
      <ul class="bar-list" data-testid="hostbar-list" aria-live="polite">
        ${sessions.map(
          (session) => html`
            <li class="bar-row">
              <span class="bar-dot" aria-hidden="true"></span>
              <span class="bar-peer">${session.peer_label}</span>
              <span class="bar-role">${t(locale, ROLE_KEY[session.role])}</span>
              <button
                type="button"
                class="bar-revoke"
                aria-label=${`${t(locale, 'status.revoke')}: ${session.peer_label}`}
                @click=${() => {
                  void invoke('session_revoke', { args: { peer: session.peer_label } }).then(
                    () => void refresh(),
                    (error: unknown) => {
                      console.error('session_revoke failed:', error);
                    },
                  );
                }}
              >
                ${t(locale, 'status.revoke')}
              </button>
            </li>
          `,
        )}
      </ul>
      <button
        type="button"
        class="bar-open"
        data-testid="hostbar-open"
        @click=${() => {
          void invoke('host_bar_focus_main').catch((error: unknown) => {
            console.error('host_bar_focus_main failed:', error);
          });
        }}
      >
        ${t(locale, 'hostbar.openApp')}
      </button>
    `,
    root,
  );
}

/**
 * Re-reads the session list.
 *
 * A failed poll leaves the last list up rather than blanking the bar: the
 * actor being busy for a second is not the same as nobody being connected,
 * and the window itself goes away when that becomes true (`set_host_bar`).
 */
async function refresh(): Promise<void> {
  try {
    sessions = (await invoke<SessionStatus[]>('session_status')).filter(
      (session) => session.state === 'active',
    );
  } catch (error) {
    console.error('session_status failed:', error);
  }
  draw();
}

draw();
void refresh();
setInterval(() => {
  void refresh();
}, POLL_MS);
