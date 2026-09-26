// Autostart, the look of the window, and updates (§21; ADR 0042, ADR 0117).
//
// Two switches that change what this machine does outside a session, so both
// live here rather than beside the per-session controls.
//
// The autostart toggle is the reason this panel exists at all: software that
// arranges to start with the session and cannot be told to stop from its own
// settings is the thing this app must not become. Off removes the entry, it
// does not disable it.
//
// Starting with the session grants nothing. The app comes up and waits for
// consent exactly as it does when a person launches it; permanent admission is
// the unattended-access panel, and it is turned on separately.

import { html, svg, type SVGTemplateResult, type TemplateResult } from "lit-html";

import type { Locale, TranslationKey } from "./i18n";
import { getStoredLocaleChoice, LOCALE_NAMES, resolveLocale, setStoredLocaleChoice, SUPPORTED_LOCALES, t } from "./i18n";
import { logoMark } from "./logo";
import { getStoredTheme, setStoredTheme, THEME_CHOICES, type ThemeChoice } from "./theme";

/** What an update check found. */
export interface UpdateInfo {
  version: string;
  current: string;
  notes: string;
}

/** How this panel reaches the core; injectable so tests need no Tauri. */
export interface SystemCommands {
  autostartStatus(): Promise<boolean>;
  autostartSet(enabled: boolean): Promise<void>;
  updateCheck(): Promise<UpdateInfo | null>;
  updateInstall(): Promise<void>;
  /** The version of this build, as the bundle states it. */
  appVersion(): Promise<string>;
}

export const tauriSystemCommands: SystemCommands = {
  async autostartStatus() {
    const { invoke } = await import("@tauri-apps/api/core");
    return (await invoke("autostart_status")) as boolean;
  },
  async autostartSet(enabled: boolean) {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("autostart_set", { args: { enabled } });
  },
  async updateCheck() {
    const { invoke } = await import("@tauri-apps/api/core");
    return (await invoke("update_check")) as UpdateInfo | null;
  },
  async updateInstall() {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("update_install");
  },
  async appVersion() {
    const { getVersion } = await import("@tauri-apps/api/app");
    return getVersion();
  },
};

interface State {
  loaded: boolean;
  /**
   * What this machine does at sign-in, or `null` while `autostartStatus` has
   * not answered yet.
   *
   * Deliberately not `false` until then. Autostart is on by default from the
   * first launch onward (ADR 0103), so a `false` here would draw a cleared
   * box on exactly the installation where the box is checked, and then tick
   * it a frame later. The row waits for the answer instead — the panel keeps
   * no default of its own, which is the same rule that makes the toggle read
   * the machine on every render rather than remember a value.
   */
  autostart: boolean | null;
  autostartError: boolean;
  checking: boolean;
  checked: boolean;
  update: UpdateInfo | null;
  installing: boolean;
  installed: boolean;
  /**
   * Why the last check or install did not finish: `off` when this build has
   * no update channel at all (`UPDATE_OFF`), which no retry will change, and
   * `failed` for everything else.
   */
  updateError: "off" | "failed" | null;
  /** This build's version, once the bundle has said; `null` until then. */
  version: string | null;
  versionRequested: boolean;
}

const state: State = {
  loaded: false,
  autostart: null,
  autostartError: false,
  checking: false,
  checked: false,
  update: null,
  installing: false,
  installed: false,
  updateError: null,
  version: null,
  versionRequested: false,
};

let onChange: (() => void) | undefined;

/** Lets main.ts re-render after an async change here. */
export function onSystemStateChange(callback: () => void): void {
  onChange = callback;
}

/** Test seam: drops the panel's state between cases. */
export function resetSystemSettings(): void {
  state.loaded = false;
  state.autostart = null;
  state.autostartError = false;
  state.checking = false;
  state.checked = false;
  state.update = null;
  state.installing = false;
  state.installed = false;
  state.updateError = null;
  state.version = null;
  state.versionRequested = false;
}

// Whether to ask the channel once at launch (ADR 0117). On unless turned off,
// and kept beside the language choice: it is this window's habit, not a fact
// about the machine.
const AUTO_CHECK_STORAGE_KEY = "lumepeer.updates.autoCheck";

export function getAutoCheck(): boolean {
  try {
    return localStorage.getItem(AUTO_CHECK_STORAGE_KEY) !== "off";
  } catch {
    return true;
  }
}

function setAutoCheck(on: boolean): void {
  try {
    if (on) {
      localStorage.removeItem(AUTO_CHECK_STORAGE_KEY);
    } else {
      localStorage.setItem(AUTO_CHECK_STORAGE_KEY, "off");
    }
  } catch {
    // The switch still holds for this running window.
  }
}

/** This build's version, or `null` before the bundle has answered. */
export function appVersion(): string | null {
  return state.version;
}

/** The release waiting to be installed, if a check found one. */
export function availableUpdate(): UpdateInfo | null {
  return state.installed ? null : state.update;
}

/** Reads this build's version once; the sidebar and the About tab both show it. */
export function loadAppVersion(commands: SystemCommands = tauriSystemCommands): void {
  if (state.versionRequested) {
    return;
  }
  state.versionRequested = true;
  void commands.appVersion().then(
    (version) => {
      state.version = version;
      onChange?.();
    },
    (error: unknown) => {
      console.error("app version unavailable:", error);
    },
  );
}

function errorKind(error: unknown): "off" | "failed" {
  return (error as { code?: unknown } | null)?.code === "UPDATE_OFF" ? "off" : "failed";
}

/**
 * Asks the channel whether a newer release exists. Only asks: installing is
 * always its own press (ADR 0042).
 *
 * `quiet` is the launch-time check, whose failure is nobody's business until
 * they come looking — a machine offline at startup is not an error to show.
 */
function runCheck(commands: SystemCommands, quiet: boolean): void {
  state.checking = true;
  state.updateError = null;
  state.installed = false;
  onChange?.();
  void commands.updateCheck().then(
    (found) => {
      state.checking = false;
      state.checked = true;
      state.update = found ?? null;
      onChange?.();
    },
    (error: unknown) => {
      state.checking = false;
      if (quiet) {
        console.warn("launch-time update check failed:", error);
      } else {
        console.error("update_check failed:", error);
        state.updateError = errorKind(error);
      }
      onChange?.();
    },
  );
}

/**
 * The one check this app makes without being asked, once per launch (ADR
 * 0117): it reports, and never downloads or installs.
 */
export function checkForUpdatesAtLaunch(commands: SystemCommands = tauriSystemCommands): void {
  if (getAutoCheck()) {
    runCheck(commands, true);
  }
}

function install(commands: SystemCommands): void {
  state.installing = true;
  state.updateError = null;
  onChange?.();
  void commands.updateInstall().then(
    () => {
      state.installing = false;
      state.installed = true;
      onChange?.();
    },
    (error: unknown) => {
      console.error("update_install failed:", error);
      state.installing = false;
      state.updateError = errorKind(error);
      onChange?.();
    },
  );
}

const THEME_LABELS: Record<ThemeChoice, TranslationKey> = {
  system: "system.theme.system",
  light: "system.theme.light",
  dark: "system.theme.dark",
};

const ICON = (body: SVGTemplateResult): TemplateResult => html`<svg
  viewBox="0 0 16 16"
  width="14"
  height="14"
  aria-hidden="true"
  fill="none"
  stroke="currentColor"
  stroke-width="1.4"
  stroke-linecap="round"
  stroke-linejoin="round"
>${body}</svg>`;

const THEME_ICONS: Record<ThemeChoice, TemplateResult> = {
  system: ICON(svg`<rect x="1.75" y="2.5" width="12.5" height="8.5" rx="1.5" /><path d="M5.5 13.5h5M8 11v2.5" />`),
  light: ICON(
    svg`<circle cx="8" cy="8" r="2.75" /><path d="M8 1.5v1.25M8 13.25v1.25M1.5 8h1.25M13.25 8h1.25M3.4 3.4l.9.9M11.7 11.7l.9.9M3.4 12.6l.9-.9M11.7 4.3l.9-.9" />`,
  ),
  dark: ICON(svg`<path d="M13.25 9.6A5.5 5.5 0 0 1 6.4 2.75a5.5 5.5 0 1 0 6.85 6.85Z" />`),
};

/**
 * General settings: how the window looks, which language it speaks, and
 * whether this machine starts it at sign-in (ADR 0042, ADR 0117).
 */
export function systemSettings(
  locale: Locale,
  commands: SystemCommands = tauriSystemCommands,
  onLocaleChange: (locale: Locale) => void = () => {},
): TemplateResult {
  if (!state.loaded) {
    state.loaded = true;
    void commands.autostartStatus().then(
      (enabled) => {
        state.autostart = enabled;
        onChange?.();
      },
      (error: unknown) => {
        console.error("autostart_status failed:", error);
        state.autostartError = true;
        onChange?.();
      },
    );
  }

  const theme = getStoredTheme();

  return html`
    <div class="system-settings" data-testid="system-settings">
      <section class="settings-group" aria-labelledby="system-appearance-heading">
        <h3 id="system-appearance-heading">${t(locale, "system.appearance")}</h3>
        <div class="setting-row">
          <span class="setting-text">
            <span class="setting-label" id="system-theme-label">${t(locale, "system.theme")}</span>
          </span>
          <div class="segmented" role="radiogroup" aria-labelledby="system-theme-label" data-testid="theme-picker">
            ${THEME_CHOICES.map(
              (choice) => html`
                <label class="segment">
                  <input
                    type="radio"
                    name="system-theme"
                    value=${choice}
                    data-testid=${`theme-${choice}`}
                    .checked=${theme === choice}
                    @change=${() => {
                      setStoredTheme(choice);
                      onChange?.();
                    }}
                  />
                  <span>${THEME_ICONS[choice]}${t(locale, THEME_LABELS[choice])}</span>
                </label>
              `,
            )}
          </div>
        </div>
        <div class="setting-row">
          <span class="setting-text">
            <label class="setting-label" for="system-language">${t(locale, "system.language")}</label>
          </span>
          <select
            id="system-language"
            data-testid="language-select"
            @change=${(event: Event) => {
              const value = (event.target as HTMLSelectElement).value;
              const choice: Locale | null = value === "system" ? null : (value as Locale);
              setStoredLocaleChoice(choice);
              onLocaleChange(resolveLocale(navigator));
            }}
          >
            <option value="system" ?selected=${getStoredLocaleChoice() === null}>
              ${t(locale, "system.language.systemDefault")}
            </option>
            ${SUPPORTED_LOCALES.map(
              (code) => html`
                <option value=${code} ?selected=${getStoredLocaleChoice() === code}>
                  ${LOCALE_NAMES[code]}
                </option>
              `,
            )}
          </select>
        </div>
      </section>

      ${state.autostart === null && !state.autostartError ? "" : html`
      <section class="settings-group" aria-labelledby="system-device-heading">
        <h3 id="system-device-heading">${t(locale, "system.heading")}</h3>
        <label class="setting-row">
          <span class="setting-text">
            <span class="setting-label">${t(locale, "system.autostart")}</span>
            <span class="setting-note">${t(locale, "system.autostartNote")}</span>
          </span>
          <input
            type="checkbox"
            role="switch"
            class="switch"
            data-testid="autostart-toggle"
            .checked=${state.autostart ?? false}
            @change=${(event: Event) => {
              const input = event.target as HTMLInputElement;
              const wanted = input.checked;
              state.autostartError = false;
              void commands.autostartSet(wanted).then(
                () => {
                  state.autostart = wanted;
                  onChange?.();
                },
                (error: unknown) => {
                  console.error("autostart_set failed:", error);
                  // The switch springs back, and is put back by hand rather than
                  // left to the re-render: the bound value never changed, so lit
                  // has nothing to write, and the box would keep showing what the
                  // user wanted instead of what this machine actually does.
                  state.autostartError = true;
                  input.checked = state.autostart ?? false;
                  onChange?.();
                },
              );
            }}
          />
        </label>
        ${
          state.autostartError
            ? html`<p
                class="system-error"
                role="status"
                data-testid="autostart-error"
              >
                ${t(locale, "system.autostartFailed")}
              </p>`
            : ""
        }
      </section>
      `}
    </div>
  `;
}

/** The status line of the update row: what the last check or install said. */
function updateStatus(locale: Locale): TemplateResult {
  const error =
    state.updateError === null
      ? ""
      : html`<span class="setting-note system-error" data-testid="update-error"
          >${t(locale, state.updateError === "off" ? "system.updateOff" : "system.updateFailed")}</span
        >`;
  if (state.installed) {
    return html`<span class="setting-label" data-testid="update-installed"
      >${t(locale, "system.installedRestart")}</span
    >`;
  }
  if (state.update !== null) {
    return html`
      <span class="setting-label" data-testid="update-found">${t(locale, "system.available", state.update.version)}</span>
      ${error || html`<span class="setting-note">${t(locale, "system.installNote")}</span>`}
    `;
  }
  if (state.checking) {
    return html`<span class="setting-label">${t(locale, "system.checking")}</span>`;
  }
  if (state.checked && state.updateError === null) {
    return html`<span class="setting-label" data-testid="update-none">${t(locale, "system.upToDate")}</span>`;
  }
  return html`<span class="setting-label">${t(locale, "system.updates")}</span>${error}`;
}

/**
 * About: which build this is, and the update check (§21; ADR 0042, ADR 0117).
 *
 * Checking and installing stay two presses. This app can be in the middle of
 * somebody else's remote session, and an update that restarted the process on
 * its own would end that session without anyone deciding to.
 */
export function updateSettings(
  locale: Locale,
  commands: SystemCommands = tauriSystemCommands,
): TemplateResult {
  loadAppVersion(commands);
  const offer = state.update !== null && !state.installed;

  return html`
    <section class="settings-group about" data-testid="update-settings">
      <div class="about-head">
        ${logoMark()}
        <div class="about-title">
          <p class="about-name">Lumepeer</p>
          ${state.version
            ? html`<p class="about-version" data-testid="app-version">${t(locale, "system.version", state.version)}</p>`
            : ""}
        </div>
      </div>
      <div class="setting-row">
        <span class="setting-text" role="status" aria-live="polite">${updateStatus(locale)}</span>
        ${offer
          ? html`<button
              type="button"
              class="btn btn-primary"
              data-testid="update-install"
              ?disabled=${state.installing}
              @click=${() => install(commands)}
            >
              ${state.installing ? t(locale, "system.installing") : t(locale, "system.installUpdate")}
            </button>`
          : html`<button
              type="button"
              class="btn"
              data-testid="update-check"
              ?disabled=${state.checking || state.installing}
              @click=${() => runCheck(commands, false)}
            >
              ${t(locale, "system.checkUpdates")}
            </button>`}
      </div>
      <label class="setting-row">
        <span class="setting-text">
          <span class="setting-label">${t(locale, "system.autoCheck")}</span>
          <span class="setting-note">${t(locale, "system.autoCheckNote")}</span>
        </span>
        <input
          type="checkbox"
          role="switch"
          class="switch"
          data-testid="update-autocheck"
          .checked=${getAutoCheck()}
          @change=${(event: Event) => {
            setAutoCheck((event.target as HTMLInputElement).checked);
            onChange?.();
          }}
        />
      </label>
    </section>
  `;
}
