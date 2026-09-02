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

/**
 * One mode the host's own physical monitor can be switched to, as the IPC
 * `host_display_modes` returns it (docs/bugs/16-host-display-mode.md #4).
 *
 * Not the same thing `MonitorDto` or the picture-resolution selector below
 * name: this is the host's actual screen, never what this window receives.
 */
export interface HostDisplayModeDto {
  id: number;
  width: number;
  height: number;
  refresh_hz: number;
}

/**
 * Why the host announced no display modes at all (docs/bugs/
 * 16-host-display-mode.md #4): the selector shows this reason rather than
 * an empty, silently-useless dropdown.
 */
export type HostDisplayModeUnavailableReason =
  | 'not_granted'
  | 'platform_unsupported'
  | 'no_modes_reported';

/** What `host_display_modes` hands back: the list, or why it is empty. */
export interface HostDisplayModesDto {
  modes: HostDisplayModeDto[];
  reason: HostDisplayModeUnavailableReason | null;
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
  sasRequest(peer: string): Promise<void>;
  /**
   * Asks the host to record the session (§17).
   *
   * Nothing here decides it. The host user answers, and the answer shows up
   * as the recording badge going on — or as nothing happening at all, which
   * is a refusal and an ordinary outcome rather than an error.
   */
  recordRequest(peer: string): Promise<void>;
  monitorsList(peer: string): Promise<MonitorDto[]>;
  monitorSelect(peer: string, monitorId: number): Promise<void>;
  /**
   * Caps the picture at `scalePercent` of the host's own captured size
   * (§11; D7, docs/bugs/13-stream-resolution.md).
   *
   * A ceiling, not a target: the host's own adaptive controller stays free
   * to sit below it, and stays free to recover only up to it.
   */
  viewSetScale(peer: string, scalePercent: number): Promise<void>;
  /**
   * The host's own physical display modes, or an honest reason there are
   * none (docs/bugs/16-host-display-mode.md #4; ADR 0048).
   *
   * Distinct from {@link ToolbarCommands.viewSetScale}: that caps what this
   * window receives, this switches the actual monitor on the other side.
   */
  hostDisplayModes(peer: string): Promise<HostDisplayModesDto>;
  /**
   * Asks the host to switch its own physical monitor to `modeId` (docs/bugs/
   * 16-host-display-mode.md #4; ADR 0048).
   *
   * The host re-checks its own independent `display_mode` grant before
   * acting; this call only says whether the request could be sent at all.
   */
  hostDisplaySetMode(peer: string, modeId: number): Promise<void>;
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
  async sasRequest(peer) {
    const { invoke } = await import('@tauri-apps/api/core');
    return invoke('sas_request', { args: { peer } });
  },
  async recordRequest(peer) {
    const { invoke } = await import('@tauri-apps/api/core');
    return invoke('record_request', { args: { peer } });
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
  async viewSetScale(peer, scalePercent) {
    const { invoke } = await import('@tauri-apps/api/core');
    // `scale_percent`, not `scalePercent`: same snake_case boundary as
    // `monitor_id` above.
    return invoke('view_set_scale', { args: { peer, scale_percent: scalePercent } });
  },
  async hostDisplayModes(peer) {
    const { invoke } = await import('@tauri-apps/api/core');
    // A bare `peer`, like `monitorsList`: the Rust command takes it as a
    // command parameter, not a field of an `args` struct.
    return (await invoke('host_display_modes', { peer })) as HostDisplayModesDto;
  },
  async hostDisplaySetMode(peer, modeId) {
    const { invoke } = await import('@tauri-apps/api/core');
    // `mode_id`, not `modeId`: same snake_case boundary as `monitor_id`.
    return invoke('host_display_set_mode', { args: { peer, mode_id: modeId } });
  },
};

/**
 * The three quality presets the guest chooses between (§11; D7, docs/bugs/
 * 13-stream-resolution.md task 4).
 *
 * One control with two effects: {@link scalePercentFor} turns it into the
 * ceiling on the picture *this window* receives, and {@link refreshHzFor}
 * turns it into the refresh rate the *host's own screen* runs at. They
 * replace the free-standing "picture resolution" list, which sat next to
 * the host-screen picker saying a similar-sounding thing about a different
 * machine — two controls a person had to keep consistent by hand.
 *
 * Naming the tradeoff rather than a pixel count is what lets one choice
 * drive both: "720p at 50 Hz" is two decisions, "balance" is one.
 */
export const QUALITY_PRESETS = ['performance', 'balance', 'quality'] as const;
export type QualityPreset = (typeof QUALITY_PRESETS)[number];

/**
 * Mirrors `crates/core/src/constants.rs::ABR_MIN_SCALE_PERCENT` (§11; ADR
 * 0037). Not importable across the IPC boundary, so restated here with the
 * same reasoning: below this, text on the remote screen is not readable.
 */
const MIN_SCALE_PERCENT = 50;

/** The picture height `balance` aims the stream at, when it can reach it. */
const BALANCE_TARGET_HEIGHT = 720;

/**
 * The percentage of the host's own captured size `preset` caps the picture
 * at, for a host screen of `monitor`'s size.
 *
 * - `quality` is the host's own resolution, uncapped.
 * - `performance` is `MIN_SCALE_PERCENT`, half of whatever that is.
 * - `balance` aims at {@link BALANCE_TARGET_HEIGHT}, worked out from the
 *   monitor's height.
 *
 * Always a number, never `null`: unlike the list this replaced, a preset is
 * always on offer, so every screen has to mean *something* here. A screen
 * already at or below the target has nothing to cap and gets 100; one so
 * tall that 720p would need a ceiling under the floor (4K) gets the floor
 * itself rather than 100, because a preset picked to spend less must never
 * quietly spend more. The three stay ordered on every screen:
 * `performance` <= `balance` <= `quality`.
 */
export function scalePercentFor(preset: QualityPreset, monitor: MonitorDto | undefined): number {
  if (preset === 'quality') {
    return 100;
  }
  if (preset === 'performance') {
    return MIN_SCALE_PERCENT;
  }
  if (!monitor || monitor.height <= BALANCE_TARGET_HEIGHT) {
    return 100;
  }
  const percent = Math.round((BALANCE_TARGET_HEIGHT / monitor.height) * 100);
  return Math.max(percent, MIN_SCALE_PERCENT);
}

/**
 * The refresh rate `preset` maps to among `rates`, the distinct rates one
 * host resolution offers, or `null` when it offers none.
 *
 * Ordered by how much of the host's own hardware each preset is willing to
 * spend: `quality` takes the highest rate there is, `performance` the middle
 * one, and `balance` sits halfway between those two. Deliberately not "the
 * lowest": the bottom of a real mode list is 23–24 Hz, a cinema cadence
 * rather than a usable desktop, and nothing about "performance" should mean
 * "unusable".
 */
export function refreshHzFor(preset: QualityPreset, rates: readonly number[]): number | null {
  if (rates.length === 0) {
    return null;
  }
  const sorted = [...rates].sort((a, b) => a - b);
  const top = sorted.length - 1;
  const middle = Math.floor(top / 2);
  const index =
    preset === 'quality' ? top : preset === 'performance' ? middle : Math.floor((middle + top) / 2);
  return sorted[index] ?? null;
}

/** One resolution the host's screen offers, and every mode that reaches it. */
export interface HostResolution {
  width: number;
  height: number;
  /** The modes at this size — one per refresh rate, in announced order. */
  modes: HostDisplayModeDto[];
}

/**
 * The distinct resolutions among `modes`, each keeping the modes that reach
 * it (docs/bugs/16-host-display-mode.md #4).
 *
 * The host announces one entry per resolution *and* refresh rate, which made
 * the picker list "4096×2160" five times over — five rows to read carefully
 * apart, for a choice nobody was making. The rate is picked from the quality
 * preset by {@link refreshHzFor} instead, so the list folds down to what the
 * guest was actually choosing between.
 */
export function hostResolutionsFrom(modes: readonly HostDisplayModeDto[]): HostResolution[] {
  const bySize = new Map<string, HostResolution>();
  for (const mode of modes) {
    const existing = bySize.get(hostResolutionKey(mode));
    if (existing) {
      existing.modes.push(mode);
    } else {
      bySize.set(hostResolutionKey(mode), {
        width: mode.width,
        height: mode.height,
        modes: [mode],
      });
    }
  }
  return [...bySize.values()];
}

/** How one resolution is addressed in the select, and parsed back on change. */
export function hostResolutionKey(size: { width: number; height: number }): string {
  return `${size.width}x${size.height}`;
}

/**
 * The i18n key explaining an empty host-display-modes list (docs/bugs/
 * 16-host-display-mode.md #4): a `null` reason means "not fetched yet",
 * shown the same as a genuinely empty list rather than a third state,
 * since nothing here has failed — the popover just has not been opened.
 */
function hostResolutionEmptyKey(
  reason: HostDisplayModeUnavailableReason | null,
): 'toolbar.hostResolution.empty.notGranted' | 'toolbar.hostResolution.empty.platformUnsupported' | 'toolbar.hostResolution.empty.noModesReported' {
  switch (reason) {
    case 'not_granted':
      return 'toolbar.hostResolution.empty.notGranted';
    case 'platform_unsupported':
      return 'toolbar.hostResolution.empty.platformUnsupported';
    case 'no_modes_reported':
    case null:
      return 'toolbar.hostResolution.empty.noModesReported';
  }
}

/** Render-side mirror of everything the toolbar shows. */
export class ToolbarState {
  /** Collapsed to the small pill: every button hidden but expand. */
  collapsed = false;
  /** Which popover, if any, is open. */
  openPopover: 'settings' | 'monitors' | null = null;
  /** Guest microphone streaming state (mirrors the actor's). */
  micOn = false;
  /** Monitors the host announced; empty until first fetched. */
  monitors: MonitorDto[] = [];
  /** Monitor currently being watched. */
  activeMonitor: number | null = null;
  /**
   * The quality preset picked for this session (§11; D7, docs/bugs/
   * 13-stream-resolution.md task 4). Persists across a monitor switch; the
   * percentage it maps to is recalculated for the new screen.
   *
   * Starts at `quality`, which is the uncapped picture this window has
   * always opened with.
   */
  quality: QualityPreset = 'quality';
  /**
   * The host's own physical display modes, as last fetched, and the reason
   * when there are none (docs/bugs/16-host-display-mode.md #4; ADR 0048).
   * `null` reason with an empty list means "not fetched yet", the same
   * "empty until first fetched" convention `monitors` uses.
   */
  hostDisplayModes: HostDisplayModesDto = { modes: [], reason: null };
  /**
   * The host resolution most recently picked, as a {@link hostResolutionKey},
   * so the select can show it selected across a re-render.
   *
   * `null` until the guest picks one, which is not the same as "nothing is
   * selected": the host announces no current mode of its own, so a `null`
   * here falls back to the watched monitor's announced size — which *is* the
   * resolution the host computer is sitting at right now.
   */
  hostResolution: string | null = null;
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
  /**
   * The scale the picture is drawn at right now, as a whole percentage.
   *
   * Read, never held, for the same reason `displayMode` is: the window owns
   * the zoom, and Ctrl+wheel moves it without passing through here.
   */
  zoomPercent(): number;
  /** Asks the window to zoom by `steps` notches, positive being closer in. */
  zoomBy(steps: number): void;
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
  record: html`<svg viewBox="0 0 16 16" aria-hidden="true"><circle cx="8" cy="8" r="4" fill="currentColor"/><circle cx="8" cy="8" r="6.25" stroke="currentColor" stroke-width="1.3" fill="none"/></svg>`,
  // A bolt. The corner brackets this used to wear were the full-screen icon
  // with a gap in it, sitting one place away from the real full-screen
  // button, so the control read as a broken duplicate of its neighbour
  // rather than as the interrupt it is.
  cad: html`<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M9.5 1.5 3.5 9.5H8l-1.5 5 6-8H8l1.5-5Z" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round" fill="none"/></svg>`,
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
    pickMonitor(id: number): void;
    pickQuality(preset: QualityPreset): void;
    pickHostResolution(key: string): void;
    zoomBy(steps: number): void;
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
  // The watched monitor's own size, for filtering resolution options by it
  // (task 4.2) — `null` until it is known, which keeps only `native` on
  // offer rather than guessing. `state.monitors` defaults to `[]` on a real
  // `ToolbarState`; the `?? []` only guards a test harness that built one by
  // hand without it.
  const activeMonitorInfo = (state.monitors ?? []).find(
    (monitor) => monitor.id === (state.activeMonitor ?? 0),
  );
  // Same guard as `activeMonitorInfo` above, for the same reason: a real
  // `ToolbarState` always has this field, but a test harness building one by
  // hand as a bare object literal may not (docs/bugs/16-host-display-mode.md
  // #4).
  const hostDisplayModes = state.hostDisplayModes ?? { modes: [], reason: null };
  const hostResolutions = hostResolutionsFrom(hostDisplayModes.modes);
  // Nothing picked yet means the host is still at the size it announced for
  // the monitor being watched, so that is what the select shows selected —
  // never a blank, and never the first row of a list the guest did not choose.
  const selectedHostResolution =
    state.hostResolution ?? (activeMonitorInfo ? hostResolutionKey(activeMonitorInfo) : null);

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
            ${hooks.displayMode() === 'scaled'
              ? html`
                  <!-- "Zoom" is the one picture size with a number behind it,
                       and until now there was no way to move that number from
                       here: the entry looked identical to "Actual size" and
                       read as broken. -->
                  <div class="toolbar-pop-row" data-testid="toolbar-zoom">
                    <span>${t(locale, 'toolbar.settings.zoom')}</span>
                    <span class="toolbar-zoom">
                      <button
                        type="button"
                        data-testid="toolbar-zoom-out"
                        aria-label=${t(locale, 'toolbar.zoom.out')}
                        title=${t(locale, 'toolbar.zoom.out')}
                        @click=${() => actions.zoomBy(-1)}
                      >
                        −
                      </button>
                      <output data-testid="toolbar-zoom-value">${hooks.zoomPercent()}%</output>
                      <button
                        type="button"
                        data-testid="toolbar-zoom-in"
                        aria-label=${t(locale, 'toolbar.zoom.in')}
                        title=${t(locale, 'toolbar.zoom.in')}
                        @click=${() => actions.zoomBy(1)}
                      >
                        +
                      </button>
                    </span>
                  </div>
                `
              : html``}
            <label class="toolbar-pop-row">
              <span>${t(locale, 'toolbar.settings.quality')}</span>
              <select
                data-testid="toolbar-quality"
                @change=${(event: Event) =>
                  actions.pickQuality((event.target as HTMLSelectElement).value as QualityPreset)}
              >
                ${QUALITY_PRESETS.map(
                  (preset) =>
                    html`<option value=${preset} ?selected=${preset === state.quality}>
                      ${t(locale, `toolbar.quality.${preset}` as TranslationKeyAlias)}
                    </option>`,
                )}
              </select>
            </label>
            <label class="toolbar-pop-row">
              <span>${t(locale, 'toolbar.settings.hostResolution')}</span>
              ${hostDisplayModes.modes.length === 0
                ? html`<span data-testid="toolbar-host-resolution-empty">
                    ${t(locale, hostResolutionEmptyKey(hostDisplayModes.reason))}
                  </span>`
                : html`<select
                    data-testid="toolbar-host-resolution"
                    @change=${(event: Event) =>
                      actions.pickHostResolution((event.target as HTMLSelectElement).value)}
                  >
                    ${hostResolutions.map(
                      (resolution) =>
                        html`<option
                          value=${hostResolutionKey(resolution)}
                          ?selected=${hostResolutionKey(resolution) === selectedHostResolution}
                        >
                          ${resolution.width}×${resolution.height}
                        </option>`,
                    )}
                  </select>`}
            </label>
            <p class="toolbar-pop-note" data-testid="toolbar-host-resolution-warning">
              ${t(locale, 'toolbar.settings.hostResolutionWarning')}
            </p>
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
        <!-- No file button and no clipboard indicator: both were doors onto
             something that now happens by itself. Files travel on the
             ordinary Ctrl+C/Ctrl+V a person already uses (ADR 0047), and
             clipboard sync runs in both directions for as long as the grants
             are live (ADR 0046) — a button offering to do either was offering
             work nobody had to ask for. The transient note below stays: it is
             the fact of a sync, which is still worth saying once. -->
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
        <!-- Never disabled. It used to gray itself out on the sas_available
             command, which answered "is *this* machine Windows?" about the
             guest's own computer — a fact about the wrong end of the session,
             since the sequence is delivered on the host. The honest answer
             only exists after asking, and it comes back as SasAck. -->
        <button
          type="button"
          class="toolbar-btn"
          data-testid="toolbar-cad"
          aria-label=${t(locale, 'toolbar.cad')}
          title=${t(locale, 'toolbar.cad')}
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

  /**
   * The resolution the host's screen is at right now: the one the guest
   * picked, or — until it picks one — the size the watched monitor itself
   * announced, which is the same thing said by the other side.
   */
  function hostResolutionNow(): string | null {
    if (state.hostResolution !== null) {
      return state.hostResolution;
    }
    const monitor = state.monitors.find((entry) => entry.id === (state.activeMonitor ?? 0));
    return monitor ? hostResolutionKey(monitor) : null;
  }

  /**
   * Switches the host's own screen to `preset`'s refresh rate at the
   * resolution it is already at.
   *
   * Does nothing at all when there is nothing to do it with: no announced
   * modes (no `display_mode` grant, or a platform that cannot switch), or a
   * resolution the announced list does not contain. Silence here is the
   * right answer — the host's screen is not this window's to guess at.
   */
  function applyHostMode(preset: QualityPreset): void {
    const key = hostResolutionNow();
    const resolution = hostResolutionsFrom(state.hostDisplayModes.modes).find(
      (entry) => hostResolutionKey(entry) === key,
    );
    if (!resolution) {
      return;
    }
    const hz = refreshHzFor(
      preset,
      resolution.modes.map((mode) => mode.refresh_hz),
    );
    const mode = resolution.modes.find((entry) => entry.refresh_hz === hz);
    if (!mode) {
      return;
    }
    void commands.hostDisplaySetMode(peer, mode.id).catch(() => {
      // The host's `display_mode` grant is gone, the id is stale, or the
      // session ended: the select keeps showing what was asked for, and the
      // host's own monitor simply does not change — the same "refused is not
      // shown as an error" contract `pickQuality` follows.
    });
  }

  const actions = {
    toggleCollapsed(): void {
      state.collapsed = !state.collapsed;
      state.openPopover = null;
      draw();
    },
    openPopover(which: 'settings' | 'monitors' | null): void {
      state.openPopover = which;
      // Settings needs the list too, to filter resolution options by the
      // watched monitor's size (task 4.2) — not only the monitors popover.
      if ((which === 'monitors' || which === 'settings') && state.monitors.length === 0) {
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
      // The host's own display modes live only in the settings popover
      // (docs/bugs/16-host-display-mode.md #4); "not fetched yet" and
      // "genuinely empty" both start as an empty list with no reason, so
      // this only re-fetches while neither has arrived.
      if (
        which === 'settings' &&
        state.hostDisplayModes.modes.length === 0 &&
        state.hostDisplayModes.reason === null
      ) {
        void commands
          .hostDisplayModes(peer)
          .then((modes) => {
            state.hostDisplayModes = modes;
            draw();
          })
          .catch(() => {
            // The host refused, speaks an older protocol, or the session is
            // gone: the same honest-empty note a genuine `NoModesReported`
            // shows, never a select that silently does nothing.
            state.hostDisplayModes = { modes: [], reason: 'no_modes_reported' };
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
        // The session ended, or this guest holds no `input` grant: the host's
        // own screen simply does not change. Whether the host managed to
        // synthesize the sequence is a separate answer that arrives as
        // `SasAck` — an unelevated host with no helper service installed is
        // refused by Windows itself (ADR 0043).
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
    pickMonitor(id: number): void {
      void commands
        .monitorSelect(peer, id)
        .then(() => {
          state.activeMonitor = id;
          state.openPopover = null;
          // The preset persists across the switch, but the percentage it maps
          // to does not: it was worked out from the old monitor's size (task
          // 4.4), so it is recomputed for the new one.
          const monitor = state.monitors.find((entry) => entry.id === id);
          void commands.viewSetScale(peer, scalePercentFor(state.quality, monitor)).catch(() => {});
          // The host announces the modes of the monitor it is *targeting*, so
          // the list and the pick both belonged to the old screen. Dropping
          // them back to "not fetched yet" is what makes the next opening of
          // the popover ask again, for this screen.
          state.hostDisplayModes = { modes: [], reason: null };
          state.hostResolution = null;
          draw();
        })
        .catch(() => {
          draw();
        });
    },
    pickQuality(preset: QualityPreset): void {
      state.quality = preset;
      const monitor = state.monitors.find((entry) => entry.id === (state.activeMonitor ?? 0));
      void commands.viewSetScale(peer, scalePercentFor(preset, monitor)).catch(() => {
        // The host refused (grant gone, an old peer) or the session ended:
        // the selector keeps showing what was asked for, and the picture
        // simply does not change.
      });
      // The preset names a refresh rate as well as a picture ceiling, so the
      // host's own screen follows it — at whatever resolution it is already
      // sitting at. A host that never announced any modes (no `display_mode`
      // grant, or a platform that cannot switch) resolves to `null` here and
      // is left alone.
      applyHostMode(preset);
      draw();
    },
    pickHostResolution(key: string): void {
      state.hostResolution = key;
      applyHostMode(state.quality);
      draw();
    },
    zoomBy(steps: number): void {
      hooks.zoomBy(steps);
      draw();
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
