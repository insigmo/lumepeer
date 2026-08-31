// Floating session toolbar of the remote-view window (design doc §11).
//
// A small draggable pill that follows the remote picture: drag handle,
// settings, monitor picker, chat toggle, microphone toggle, Ctrl+Alt+Del,
// and a collapse button that hides everything but the pill until it is
// expanded again. Every action goes
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
import { SETTINGS_ICON } from './icons';
import { CLIPBOARD_NOTE_MS } from './session-status';
import { HOTKEYS, hotkeyLabel } from './view-hotkeys';
import { DISPLAY_MODES, type DisplayMode } from './view-window';

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
  /**
   * The host's clipboard, if it changed since the last check.
   *
   * Sending this window's own clipboard to the host no longer runs through
   * this interface at all: a guest holds no clipboard grant of its own (ADR
   * 0029), so it is never this window's to decide, and the Rust actor now
   * reads and offers this machine's own clipboard by itself the moment it
   * changes (docs/bugs/10-clipboard-auto.md #1; ADR 0046) — the same way the
   * host side always has. What is left for this window is the *arrival*
   * half: polled so the toolbar can say a sync happened, and the text is
   * discarded the moment it is read (§15) — never rendered, logged or held.
   */
  clipboardPull(peer: string): Promise<string | null>;
  /**
   * Opens the OS file picker on the Rust side and offers what was chosen.
   *
   * No path crosses the IPC boundary in either direction: this window never
   * learns one and never supplies one (§2.3; ADR 0032).
   */
  fileOffer(peer: string): Promise<void>;
  sasRequest(peer: string): Promise<void>;
  /**
   * Asks the host to record the session (§17).
   *
   * Nothing here decides it. The host user answers, and the answer shows up
   * as the recording badge going on — or as nothing happening at all, which
   * is a refusal and an ordinary outcome rather than an error.
   */
  recordRequest(peer: string): Promise<void>;
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
  async clipboardPull(peer) {
    const { invoke } = await import('@tauri-apps/api/core');
    // A bare `peer`, like `monitorsList`: the Rust command takes it as a
    // command parameter, not a field of an `args` struct.
    return (await invoke('clipboard_pull', { peer })) as string | null;
  },
  async fileOffer(peer) {
    const { invoke } = await import('@tauri-apps/api/core');
    return invoke('file_offer', { peer });
  },
  async sasRequest(peer) {
    const { invoke } = await import('@tauri-apps/api/core');
    return invoke('sas_request', { args: { peer } });
  },
  async recordRequest(peer) {
    const { invoke } = await import('@tauri-apps/api/core');
    return invoke('record_request', { args: { peer } });
  },
  async sasAvailable() {
    const { invoke } = await import('@tauri-apps/api/core');
    return (await invoke('sas_available')) as boolean;
  },
  async monitorsList(peer) {
    const { invoke } = await import('@tauri-apps/api/core');
    // A bare `peer`, not `{ args: { peer } }`: the Rust command takes the
    // string as a command *parameter*, like `chat_transcript` and
    // `clipboard_pull` do, and an `args` key would leave `peer` missing.
    return (await invoke('monitors_list', { peer })) as MonitorDto[];
  },
  async monitorSelect(peer, monitorId) {
    const { invoke } = await import('@tauri-apps/api/core');
    // `monitor_id`, not `monitorId`: Tauri converts the *parameter* names of a
    // command, never the fields of a struct it deserializes, and
    // `MonitorSelectArgs` is plain serde. snake_case on the IPC boundary is
    // what the rest of this surface already uses (`since_us`, `code_required`).
    return invoke('monitor_select', { args: { peer, monitor_id: monitorId } });
  },
};

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
  /**
   * Whether a recording request has already been sent this session.
   *
   * Only so the button can stop inviting a second press while the first is
   * unanswered. It is not a claim about the host's decision, and it never
   * turns into one: the badge over the picture is the only thing that says
   * whether a recording is running.
   */
  recordAsked = false;
  /**
   * When the host's clipboard last arrived, in `Date.now()` milliseconds, or
   * `0` for never (docs/bugs/10-clipboard-auto.md #2; ADR 0046).
   *
   * The fact only — the text itself is never kept here or anywhere else in
   * this module (§15). Mirrors `clipboardSyncedAt` in `main.ts`'s host panel.
   */
  clipboardSyncedAt = 0;
}

/** What the toolbar hands back to whoever mounted it, once, at mount. */
export interface ToolbarControls {
  /** Re-render, after state the toolbar shows but does not own has changed. */
  redraw(): void;
  /** Collapse or expand — the same thing the button does, for the hotkey. */
  toggleCollapsed(): void;
}

/** Callbacks the toolbar needs beyond IPC. */
export interface ToolbarHooks {
  /** Show or hide the chat panel; returns the new visible state. */
  toggleChat(): boolean;
  /** Whether the chat panel is visible right now. */
  chatVisible(): boolean;
  /**
   * Whether a message arrived while the panel was closed.
   *
   * The panel starts hidden, so without a mark on the button a message from
   * the host is something the guest simply never sees. A flag, not a count:
   * "there is something to read" is the whole of what the button has to say.
   */
  chatUnread(): boolean;
  /**
   * How the picture is laid out right now (§11).
   *
   * Read, never held: the window owns the layout, because the same three
   * modes are reachable from a hotkey that never goes through here.
   */
  displayMode(): DisplayMode;
  /** Asks the window to lay the picture out differently. */
  setDisplayMode(mode: DisplayMode): void;
  /** Whether the window is full screen right now. */
  fullscreen(): boolean;
  /** Asks the window to enter or leave full screen. */
  toggleFullscreen(): void;
  /**
   * Whether this host sends its cursor on its own channel (§11).
   *
   * `false` means the host is still drawing the cursor into the picture, and
   * the switch below has nothing to switch: turning a local overlay on would
   * put a second cursor next to the real one.
   */
  cursorChannel(): boolean;
  /** Whether the local cursor overlay is being drawn right now. */
  localCursor(): boolean;
  /** Turns the local cursor overlay on or off. */
  toggleLocalCursor(): void;
  /** Handed the controls above once, at mount. */
  bind(controls: ToolbarControls): void;
}

const ICONS = {
  handle: html`<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M6 3h4M4 6h8M4 9h8M6 12h4" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" fill="none"/></svg>`,
  settings: SETTINGS_ICON,
  monitor: (n: string) => html`<svg viewBox="0 0 16 16" aria-hidden="true"><rect x="2" y="3" width="12" height="9" rx="1" stroke="currentColor" stroke-width="1.5" fill="none"/><text x="8" y="10" text-anchor="middle" font-size="7" fill="currentColor" stroke="none" font-family="system-ui">${n}</text></svg>`,
  chat: html`<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M3 3h10a1 1 0 0 1 1 1v6a1 1 0 0 1-1 1H7l-3 3v-3H3a1 1 0 0 1-1-1V4a1 1 0 0 1 1-1Z" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round" fill="none"/></svg>`,
  chatUnread: html`<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M3 3h10a1 1 0 0 1 1 1v6a1 1 0 0 1-1 1H7l-3 3v-3H3a1 1 0 0 1-1-1V4a1 1 0 0 1 1-1Z" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round" fill="none"/><circle cx="13" cy="3" r="2.5" fill="#9fd0ff" stroke="none"/></svg>`,
  mic: html`<svg viewBox="0 0 16 16" aria-hidden="true"><rect x="6" y="2" width="4" height="7" rx="2" stroke="currentColor" stroke-width="1.5" fill="none"/><path d="M4 8a4 4 0 0 0 8 0M8 12v2" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" fill="none"/></svg>`,
  micOff: html`<svg viewBox="0 0 16 16" aria-hidden="true"><rect x="6" y="2" width="4" height="7" rx="2" stroke="currentColor" stroke-width="1.5" fill="none"/><path d="M4 8a4 4 0 0 0 8 0M8 12v2M3 3l10 10" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" fill="none"/></svg>`,
  file: html`<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M9 2H4.5A1.5 1.5 0 0 0 3 3.5v9A1.5 1.5 0 0 0 4.5 14h7a1.5 1.5 0 0 0 1.5-1.5V6L9 2Z" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round" fill="none"/><path d="M9 2v4h4" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round" fill="none"/></svg>`,
  clipboard: html`<svg viewBox="0 0 16 16" aria-hidden="true"><rect x="4" y="3" width="8" height="11" rx="1" stroke="currentColor" stroke-width="1.5" fill="none"/><path d="M6.5 3V2h3v1" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round" fill="none"/></svg>`,
  record: html`<svg viewBox="0 0 16 16" aria-hidden="true"><circle cx="8" cy="8" r="4" fill="currentColor"/><circle cx="8" cy="8" r="6.25" stroke="currentColor" stroke-width="1.3" fill="none"/></svg>`,
  cad: html`<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M3 6V4.5A1.5 1.5 0 0 1 4.5 3H6m4 0h1.5A1.5 1.5 0 0 1 13 4.5V6m0 4v1.5a1.5 1.5 0 0 1-1.5 1.5H10M6 13H4.5A1.5 1.5 0 0 1 3 11.5V10" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" fill="none"/></svg>`,
  fullscreen: html`<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M2 6V2h4M14 6V2h-4M2 10v4h4M14 10v4h-4" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" fill="none"/></svg>`,
  fullscreenExit: html`<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M6 2v4H2M10 2v4h4M6 14v-4H2M10 14v-4h4" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" fill="none"/></svg>`,
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
    setDisplayMode(mode: DisplayMode): void;
    toggleFullscreen(): void;
    toggleLocalCursor(): void;
    toggleMic(): void;
    sendCad(): void;
    askToRecord(): void;
    sendFile(): void;
    pickMonitor(id: number): void;
    beginDrag(event: PointerEvent): void;
    nudge(dx: number, dy: number): void;
  },
): void {
  const chatOn = hooks.chatVisible();
  // Only worth showing while the panel is closed: with it open the message is
  // already on screen, and a mark next to it would be a second claim about the
  // same thing.
  const chatUnread = !chatOn && hooks.chatUnread();
  const monitorLabel = state.activeMonitor === null ? '1' : String(state.activeMonitor + 1);

  const settingsPopover: TemplateResult =
    state.openPopover === 'settings'
      ? html`
          <div class="toolbar-pop" role="dialog" aria-label=${t(locale, 'toolbar.settings')}>
            <label class="toolbar-pop-row">
              <span>${t(locale, 'toolbar.settings.displayMode')}</span>
              <select
                data-testid="toolbar-display-mode"
                @change=${(event: Event) =>
                  actions.setDisplayMode((event.target as HTMLSelectElement).value as DisplayMode)}
              >
                ${DISPLAY_MODES.map(
                  (mode) =>
                    html`<option value=${mode} ?selected=${mode === hooks.displayMode()}>
                      ${t(locale, `toolbar.display.${mode}` as TranslationKeyAlias)}
                    </option>`,
                )}
              </select>
            </label>
            ${hooks.cursorChannel()
              ? html`
                  <label class="toolbar-pop-row">
                    <span>${t(locale, 'toolbar.settings.localCursor')}</span>
                    <input
                      type="checkbox"
                      data-testid="toolbar-local-cursor"
                      .checked=${hooks.localCursor()}
                      @change=${actions.toggleLocalCursor}
                    />
                  </label>
                `
              : html`<p class="toolbar-pop-note" data-testid="toolbar-cursor-embedded">
                  ${t(locale, 'toolbar.settings.cursorEmbedded')}
                </p>`}
            <p class="toolbar-pop-note">${t(locale, 'toolbar.settings.placeholder')}</p>
            <!-- A hotkey nobody can see is indistinguishable from a bug, so
                 the chords this window keeps for itself are listed here (§11). -->
            <h3 class="toolbar-pop-heading">${t(locale, 'toolbar.hotkeys')}</h3>
            <dl class="toolbar-hotkeys" data-testid="toolbar-hotkeys">
              ${HOTKEYS.map(
                (entry) => html`
                  <dt>${hotkeyLabel(entry.code)}</dt>
                  <dd>${t(locale, `toolbar.hotkey.${entry.action}` as TranslationKeyAlias)}</dd>
                `,
              )}
            </dl>
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
          aria-label=${t(locale, chatUnread ? 'toolbar.chat.unread' : 'toolbar.chat')}
          title=${t(locale, chatUnread ? 'toolbar.chat.unread' : 'toolbar.chat')}
          aria-pressed=${chatOn ? 'true' : 'false'}
          @click=${() => hooks.toggleChat()}
        >
          ${chatUnread ? ICONS.chatUnread : ICONS.chat}
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
          class="toolbar-btn"
          data-testid="toolbar-file"
          aria-label=${t(locale, 'toolbar.file')}
          title=${t(locale, 'toolbar.file')}
          @click=${actions.sendFile}
        >
          ${ICONS.file}
        </button>
        <span
          class="toolbar-btn toolbar-indicator"
          data-testid="toolbar-clipboard"
          role="img"
          aria-label=${t(locale, 'toolbar.clipboard')}
          title=${t(locale, 'toolbar.clipboard')}
        >
          ${ICONS.clipboard}
        </span>
        ${Date.now() - state.clipboardSyncedAt < CLIPBOARD_NOTE_MS
          ? html`<span class="toolbar-clipboard-note" data-testid="toolbar-clipboard-note">
              ${t(locale, 'status.clipboardSynced')}
            </span>`
          : html``}
        <button
          type="button"
          class="toolbar-btn ${state.recordAsked ? 'is-active' : ''}"
          data-testid="toolbar-record"
          aria-label=${t(locale, state.recordAsked ? 'toolbar.record.asked' : 'toolbar.record')}
          title=${t(locale, state.recordAsked ? 'toolbar.record.asked' : 'toolbar.record')}
          ?disabled=${state.recordAsked}
          @click=${actions.askToRecord}
        >
          ${ICONS.record}
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
          class="toolbar-btn ${hooks.fullscreen() ? 'is-active' : ''}"
          data-testid="toolbar-fullscreen"
          aria-label=${t(locale, hooks.fullscreen() ? 'toolbar.fullscreen.exit' : 'toolbar.fullscreen')}
          title=${t(locale, hooks.fullscreen() ? 'toolbar.fullscreen.exit' : 'toolbar.fullscreen')}
          aria-pressed=${hooks.fullscreen() ? 'true' : 'false'}
          @click=${actions.toggleFullscreen}
        >
          ${hooks.fullscreen() ? ICONS.fullscreenExit : ICONS.fullscreen}
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
 * How often this window asks whether the host's clipboard arrived
 * (docs/bugs/10-clipboard-auto.md #2).
 *
 * Not the OS-facing poll — that one is `CLIPBOARD_POLL_INTERVAL_MS` in
 * `crates/core/src/constants.rs`, on its own dedicated thread (ADR 0027).
 * This is only how often this window's own IPC check runs, on the same
 * cadence `startChatPolling`'s own default uses.
 */
const CLIPBOARD_SYNC_POLL_INTERVAL_MS = 1000;

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
  clipboardPollIntervalMs = CLIPBOARD_SYNC_POLL_INTERVAL_MS,
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
    setDisplayMode(mode: DisplayMode): void {
      // The window owns the layout; this only asks, and then redraws to show
      // whatever the window settled on.
      hooks.setDisplayMode(mode);
      draw();
    },
    toggleFullscreen(): void {
      hooks.toggleFullscreen();
      draw();
    },
    toggleLocalCursor(): void {
      hooks.toggleLocalCursor();
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
    askToRecord(): void {
      state.recordAsked = true;
      draw();
      void commands.recordRequest(peer).catch(() => {
        // The request never left (no live view, or the session ended): offer
        // the button again rather than leave it looking like someone is
        // considering a question nobody was asked.
        state.recordAsked = false;
        draw();
      });
    },
    sendFile(): void {
      void commands.fileOffer(peer).catch(() => {
        // The picker was dismissed, the host has no `file_transfer` grant, or
        // it runs a version without the message: nothing was offered, and
        // nothing here claims otherwise.
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

  // The window drives the layout and full screen from its own hotkeys too, so
  // it needs a way to bring the toolbar back in step with what it just did.
  hooks.bind({
    redraw: draw,
    toggleCollapsed: actions.toggleCollapsed,
  });

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

  // docs/bugs/10-clipboard-auto.md #2: the mandatory indicator this window
  // owes the guest now that the host's clipboard can arrive without a press
  // on either side. `clipboard_pull` hands back the text; this callback is
  // the only place that ever sees it, and it only ever checks it is not
  // `null` — the text itself is never assigned to anything (§15).
  const clipboardPollTimer = window.setInterval(() => {
    void commands
      .clipboardPull(peer)
      .then((text) => {
        if (text !== null) {
          state.clipboardSyncedAt = Date.now();
          draw();
        }
      })
      .catch(() => {
        // The session ended between polls; the next tick, or the window
        // closing, is what stops this rather than an error here.
      });
  }, clipboardPollIntervalMs);

  draw();

  return () => {
    document.removeEventListener('pointerdown', onPointerDownAnywhere, true);
    window.removeEventListener('resize', onResize);
    window.clearInterval(clipboardPollTimer);
  };
}
