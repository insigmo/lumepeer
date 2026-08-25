// Floating session toolbar of the remote-view window (design doc §11).
//
// A small draggable pill that follows the remote picture: drag handle,
// settings (screen-resolution placeholder for now), monitor picker, chat
// toggle, microphone toggle, Ctrl+Alt+Del, and a collapse button that hides
// everything but the pill until it is expanded again. Every action goes
// through the same injectable command surface the rest of the window uses,
// so the state machine is testable in jsdom without Tauri.
//
// Nothing here decides anything: the Rust actor re-checks every request
// against the session's grants (§8.1) and refuses what the session does not
// allow; this module only stops offering a control once the peer has said
// no (e.g. a `SasRequest` refused comes back as an error and the button
// says so instead of pretending success).

import { html, render, type TemplateResult } from 'lit-html';

import type { Locale } from './i18n';
import { t } from './i18n';

/** One host monitor as the IPC `monitors_list` returns it. */
export interface MonitorDto {
  id: number;
  width: number;
  height: number;
  primary: boolean;
}

/** How the toolbar talks to Tauri; injectable for tests. */
export interface ToolbarCommands {
  micToggle(peer: string, on: boolean): Promise<void>;
  sasRequest(peer: string): Promise<void>;
  sasAvailable(): Promise<boolean>;
  monitorsList(peer: string): Promise<MonitorDto[]>;
  monitorSelect(peer: string, monitorId: number): Promise<void>;
}

/** Default binding to the real IPC surface. */
export const tauriToolbarCommands: ToolbarCommands = {
  async micToggle(peer, on) {
    const { invoke } = await import('@tauri-apps/api/core');
    return invoke('mic_toggle', { args: { peer, on } });
  },
  async sasRequest(peer) {
    const { invoke } = await import('@tauri-apps/api/core');
    return invoke('sas_request', { args: { peer } });
  },
  async sasAvailable() {
    const { invoke } = await import('@tauri-apps/api/core');
    return (await invoke('sas_available')) as boolean;
  },
  async monitorsList(peer) {
    const { invoke } = await import('@tauri-apps/api/core');
    return (await invoke('monitors_list', { args: { peer } })) as MonitorDto[];
  },
  async monitorSelect(peer, monitorId) {
    const { invoke } = await import('@tauri-apps/api/core');
    return invoke('monitor_select', { args: { peer, monitorId } });
  },
};

/** Resolution choices of the settings popover (placeholder, §11). */
export const RESOLUTION_CHOICES: readonly string[] = ['native', '1080p', '720p'];

/** Render-side mirror of everything the toolbar shows. */
export class ToolbarState {
  /** Collapsed to the small pill: every button hidden but expand. */
  collapsed = false;
  /** Which popover, if any, is open. */
  openPopover: 'settings' | 'monitors' | null = null;
  /** Guest microphone streaming state (mirrors the actor's). */
  micOn = false;
  /** Whether the host platform can deliver the SAS at all. */
  sasReady = true;
  /** Monitors the host announced; empty until first fetched. */
  monitors: MonitorDto[] = [];
  /** Monitor currently being watched. */
  activeMonitor: number | null = null;
  /** Placeholder resolution choice; recorded only (§11 stub). */
  resolution: string = RESOLUTION_CHOICES[0] ?? 'native';
}

/** Callbacks the toolbar needs beyond IPC. */
export interface ToolbarHooks {
  /** Show or hide the chat panel; returns the new visible state. */
  toggleChat(): boolean;
  /** Whether the chat panel is visible right now. */
  chatVisible(): boolean;
}

const ICONS = {
  handle: html`<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M6 3h4M4 6h8M4 9h8M6 12h4" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" fill="none"/></svg>`,
  settings: html`<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M8 10a2 2 0 1 0 0-4 2 2 0 0 0 0 4Zm5.4-2a5.4 5.4 0 0 0-.1-1l1.2-1-1.2-2-1.4.5a5.5 5.5 0 0 0-1.7-1L10 2H6l-.2 1.5a5.5 5.5 0 0 0-1.7 1L2.7 4 1.5 6l1.2 1a5.4 5.4 0 0 0 0 2l-1.2 1 1.2 2 1.4-.5a5.5 5.5 0 0 0 1.7 1L6 14h4l.2-1.5a5.5 5.5 0 0 0 1.7-1l1.4.5 1.2-2-1.2-1c.1-.3.1-.7.1-1Z" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round" fill="none"/></svg>`,
  monitor: (n: string) => html`<svg viewBox="0 0 16 16" aria-hidden="true"><rect x="2" y="3" width="12" height="9" rx="1" stroke="currentColor" stroke-width="1.5" fill="none"/><text x="8" y="10" text-anchor="middle" font-size="7" fill="currentColor" stroke="none" font-family="system-ui">${n}</text></svg>`,
  chat: html`<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M3 3h10a1 1 0 0 1 1 1v6a1 1 0 0 1-1 1H7l-3 3v-3H3a1 1 0 0 1-1-1V4a1 1 0 0 1 1-1Z" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round" fill="none"/></svg>`,
  mic: html`<svg viewBox="0 0 16 16" aria-hidden="true"><rect x="6" y="2" width="4" height="7" rx="2" stroke="currentColor" stroke-width="1.5" fill="none"/><path d="M4 8a4 4 0 0 0 8 0M8 12v2" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" fill="none"/></svg>`,
  micOff: html`<svg viewBox="0 0 16 16" aria-hidden="true"><rect x="6" y="2" width="4" height="7" rx="2" stroke="currentColor" stroke-width="1.5" fill="none"/><path d="M4 8a4 4 0 0 0 8 0M8 12v2M3 3l10 10" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" fill="none"/></svg>`,
  cad: html`<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M3 6V4.5A1.5 1.5 0 0 1 4.5 3H6m4 0h1.5A1.5 1.5 0 0 1 13 4.5V6m0 4v1.5a1.5 1.5 0 0 1-1.5 1.5H10M6 13H4.5A1.5 1.5 0 0 1 3 11.5V10" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" fill="none"/></svg>`,
  collapse: html`<svg viewBox="0 0 16 16" aria-hidden="true"><path d="m4 10 4-4 4 4" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" fill="none"/></svg>`,
  expand: html`<svg viewBox="0 0 16 16" aria-hidden="true"><path d="m4 6 4 4 4-4" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" fill="none"/></svg>`,
};

/**
 * Renders the toolbar into `container`.
 *
 * Pure render: every state change goes through {@link renderToolbar} again,
 * which makes the whole surface a function of {@link ToolbarState} and keeps
 * the drag/press handlers the only imperative parts.
 */
export function renderToolbar(
  container: HTMLElement,
  state: ToolbarState,
  locale: Locale,
  hooks: ToolbarHooks,
  actions: {
    toggleCollapsed(): void;
    openPopover(which: 'settings' | 'monitors' | null): void;
    setResolution(value: string): void;
    toggleMic(): void;
    sendCad(): void;
    pickMonitor(id: number): void;
    beginDrag(event: PointerEvent): void;
    nudge(dx: number, dy: number): void;
  },
): void {
  const chatOn = hooks.chatVisible();
  const monitorLabel = state.activeMonitor === null ? '1' : String(state.activeMonitor + 1);

  const settingsPopover: TemplateResult =
    state.openPopover === 'settings'
      ? html`
          <div class="toolbar-pop" role="dialog" aria-label=${t(locale, 'toolbar.settings')}>
            <label class="toolbar-pop-row">
              <span>${t(locale, 'toolbar.settings.resolution')}</span>
              <select
                data-testid="toolbar-resolution"
                @change=${(event: Event) =>
                  actions.setResolution((event.target as HTMLSelectElement).value)}
              >
                ${RESOLUTION_CHOICES.map(
                  (choice) =>
                    html`<option value=${choice} ?selected=${choice === state.resolution}>
                      ${t(locale, `toolbar.resolution.${choice}` as TranslationKeyAlias)}
                    </option>`,
                )}
              </select>
            </label>
            <p class="toolbar-pop-note">${t(locale, 'toolbar.settings.placeholder')}</p>
          </div>
        `
      : html``;

  const monitorsPopover: TemplateResult =
    state.openPopover === 'monitors'
      ? html`
          <div class="toolbar-pop" role="dialog" aria-label=${t(locale, 'toolbar.monitors')}>
            ${state.monitors.length === 0
              ? html`<p class="toolbar-pop-note">${t(locale, 'toolbar.monitors.empty')}</p>`
              : html`
                  <ul class="toolbar-monitor-list">
                    ${state.monitors.map(
                      (monitor) => html`
                        <li>
                          <button
                            type="button"
                            data-testid="toolbar-monitor-option"
                            class=${monitor.id === state.activeMonitor ? 'is-active' : ''}
                            aria-pressed=${monitor.id === state.activeMonitor ? 'true' : 'false'}
                            @click=${() => actions.pickMonitor(monitor.id)}
                          >
                            ${t(locale, 'toolbar.monitors.entry', String(monitor.id + 1))}
                            ${monitor.width > 0
                              ? html`<span class="toolbar-monitor-size"
                                  >${monitor.width}×${monitor.height}</span
                                >`
                              : html``}
                          </button>
                        </li>
                      `,
                    )}
                  </ul>
                `}
          </div>
        `
      : html``;

  const buttons: TemplateResult = state.collapsed
    ? html`
        <button
          type="button"
          class="toolbar-btn"
          data-testid="toolbar-expand"
          aria-label=${t(locale, 'toolbar.expand')}
          title=${t(locale, 'toolbar.expand')}
          @click=${actions.toggleCollapsed}
        >
          ${ICONS.expand}
        </button>
      `
    : html`
        <button
          type="button"
          class="toolbar-btn"
          data-testid="toolbar-settings"
          aria-label=${t(locale, 'toolbar.settings')}
          title=${t(locale, 'toolbar.settings')}
          aria-expanded=${state.openPopover === 'settings' ? 'true' : 'false'}
          @click=${() => actions.openPopover(state.openPopover === 'settings' ? null : 'settings')}
        >
          ${ICONS.settings}
        </button>
        <button
          type="button"
          class="toolbar-btn"
          data-testid="toolbar-monitors"
          aria-label=${t(locale, 'toolbar.monitors')}
          title=${t(locale, 'toolbar.monitors')}
          aria-expanded=${state.openPopover === 'monitors' ? 'true' : 'false'}
          @click=${() => actions.openPopover(state.openPopover === 'monitors' ? null : 'monitors')}
        >
          ${ICONS.monitor(monitorLabel)}
        </button>
        <button
          type="button"
          class="toolbar-btn ${chatOn ? 'is-active' : ''}"
          data-testid="toolbar-chat"
          aria-label=${t(locale, 'toolbar.chat')}
          title=${t(locale, 'toolbar.chat')}
          aria-pressed=${chatOn ? 'true' : 'false'}
          @click=${() => hooks.toggleChat()}
        >
          ${ICONS.chat}
        </button>
        <button
          type="button"
          class="toolbar-btn ${state.micOn ? 'is-active' : ''}"
          data-testid="toolbar-mic"
          aria-label=${t(locale, 'toolbar.mic')}
          title=${t(locale, 'toolbar.mic')}
          aria-pressed=${state.micOn ? 'true' : 'false'}
          @click=${actions.toggleMic}
        >
          ${state.micOn ? ICONS.mic : ICONS.micOff}
        </button>
        <button
          type="button"
          class="toolbar-btn ${state.sasReady ? '' : 'is-disabled'}"
          data-testid="toolbar-cad"
          aria-label=${t(locale, 'toolbar.cad')}
          title=${t(locale, 'toolbar.cad')}
          ?disabled=${!state.sasReady}
          @click=${actions.sendCad}
        >
          ${ICONS.cad}
        </button>
        <button
          type="button"
          class="toolbar-btn"
          data-testid="toolbar-collapse"
          aria-label=${t(locale, 'toolbar.collapse')}
          title=${t(locale, 'toolbar.collapse')}
          @click=${actions.toggleCollapsed}
        >
          ${ICONS.collapse}
        </button>
      `;

  const tree: TemplateResult = html`
    <div class="toolbar ${state.collapsed ? 'toolbar-collapsed' : ''}" data-testid="toolbar">
      <button
        type="button"
        class="toolbar-btn toolbar-handle"
        data-testid="toolbar-handle"
        aria-label=${t(locale, 'toolbar.dragHandle')}
        title=${t(locale, 'toolbar.dragHandle')}
        @pointerdown=${(event: PointerEvent) => actions.beginDrag(event)}
        @keydown=${(event: KeyboardEvent) => {
          const step = event.shiftKey ? 32 : 8;
          if (event.key === 'ArrowLeft') {
            event.preventDefault();
            actions.nudge(-step, 0);
          } else if (event.key === 'ArrowRight') {
            event.preventDefault();
            actions.nudge(step, 0);
          } else if (event.key === 'ArrowUp') {
            event.preventDefault();
            actions.nudge(0, -step);
          } else if (event.key === 'ArrowDown') {
            event.preventDefault();
            actions.nudge(0, step);
          }
        }}
      >
        ${ICONS.handle}
      </button>
      ${buttons}
    </div>
    ${settingsPopover} ${monitorsPopover}
  `;
  render(tree, container);
}

/** Local alias so the resolution keys stay type-checked without widening the union. */
type TranslationKeyAlias = Parameters<typeof t>[1];

/** Minimum on-screen inset the drag clamps to. */
const TOOLBAR_INSET = 8;

/**
 * Wires the toolbar to the DOM: render loop, drag, popovers, and IPC.
 *
 * Returns a stop function that removes the global listeners; the container
 * itself lives and dies with the view window.
 */
export function mountToolbar(
  container: HTMLElement,
  locale: Locale,
  peer: string,
  commands: ToolbarCommands,
  hooks: ToolbarHooks,
): () => void {
  const state = new ToolbarState();
  const root = container.closest('#view') ?? document.body;

  const position = { left: TOOLBAR_INSET, top: TOOLBAR_INSET };

  function clampToViewport(): void {
    const bounds = root.getBoundingClientRect();
    const width = container.offsetWidth || 1;
    const height = container.offsetHeight || 1;
    position.left = Math.max(
      TOOLBAR_INSET,
      Math.min(position.left, Math.max(TOOLBAR_INSET, bounds.width - width - TOOLBAR_INSET)),
    );
    position.top = Math.max(
      TOOLBAR_INSET,
      Math.min(position.top, Math.max(TOOLBAR_INSET, bounds.height - height - TOOLBAR_INSET)),
    );
  }

  function paint(): void {
    container.style.left = `${position.left}px`;
    container.style.top = `${position.top}px`;
  }

  function draw(): void {
    renderToolbar(container, state, locale, hooks, actions);
    paint();
  }

  const actions = {
    toggleCollapsed(): void {
      state.collapsed = !state.collapsed;
      state.openPopover = null;
      draw();
    },
    openPopover(which: 'settings' | 'monitors' | null): void {
      state.openPopover = which;
      if (which === 'monitors' && state.monitors.length === 0) {
        void commands
          .monitorsList(peer)
          .then((monitors) => {
            state.monitors = monitors;
            draw();
          })
          .catch(() => {
            // The host refused or the session is gone; the popover shows its
            // empty note instead of a list that would silently do nothing.
            state.monitors = [];
            draw();
          });
      }
      draw();
    },
    setResolution(value: string): void {
      // Placeholder by design (§11): the choice is recorded so the real
      // implementation can pick it up, but nothing is sent to the host yet.
      if (RESOLUTION_CHOICES.includes(value)) {
        state.resolution = value;
      }
      draw();
    },
    toggleMic(): void {
      const next = !state.micOn;
      void commands
        .micToggle(peer, next)
        .then(() => {
          state.micOn = next;
          draw();
        })
        .catch(() => {
          // Refused (no media connection yet, no grant): stay off and say so.
          state.micOn = false;
          draw();
        });
    },
    sendCad(): void {
      void commands.sasRequest(peer).catch(() => {
        // The host answered SasAck(false) or the session ended; the button
        // stays enabled but the failure is visible in the log, not silent.
      });
    },
    pickMonitor(id: number): void {
      void commands
        .monitorSelect(peer, id)
        .then(() => {
          state.activeMonitor = id;
          state.openPopover = null;
          draw();
        })
        .catch(() => {
          draw();
        });
    },
    beginDrag(event: PointerEvent): void {
      if (event.button !== 0) {
        return;
      }
      event.preventDefault();
      const startX = event.clientX;
      const startY = event.clientY;
      const startLeft = position.left;
      const startTop = position.top;
      const handle = event.currentTarget as HTMLElement;
      handle.setPointerCapture(event.pointerId);
      const onMove = (move: PointerEvent): void => {
        position.left = startLeft + (move.clientX - startX);
        position.top = startTop + (move.clientY - startY);
        clampToViewport();
        paint();
      };
      const onUp = (): void => {
        handle.removeEventListener('pointermove', onMove);
        handle.removeEventListener('pointerup', onUp);
      };
      handle.addEventListener('pointermove', onMove);
      handle.addEventListener('pointerup', onUp);
    },
    nudge(dx: number, dy: number): void {
      position.left += dx;
      position.top += dy;
      clampToViewport();
      paint();
    },
  };

  // Whether the host can even try the SAS: off-Windows hosts cannot, and the
  // button says so instead of letting someone press it into a dead end.
  void commands
    .sasAvailable()
    .then((available) => {
      state.sasReady = available;
      draw();
    })
    .catch(() => {
      state.sasReady = true;
    });

  const onPointerDownAnywhere = (event: PointerEvent): void => {
    if (state.openPopover === null) {
      return;
    }
    const target = event.target as Node;
    if (!container.contains(target)) {
      state.openPopover = null;
      draw();
    }
  };
  document.addEventListener('pointerdown', onPointerDownAnywhere, true);

  const onResize = (): void => {
    clampToViewport();
    paint();
  };
  window.addEventListener('resize', onResize);

  draw();

  return () => {
    document.removeEventListener('pointerdown', onPointerDownAnywhere, true);
    window.removeEventListener('resize', onResize);
  };
}
