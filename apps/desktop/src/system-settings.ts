// Autostart and updates (§21; ADR 0042).
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

import { html, type TemplateResult } from "lit-html";

import type { Locale } from "./i18n";
import { t } from "./i18n";

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
};

interface State {
  loaded: boolean;
  autostart: boolean;
  autostartError: boolean;
  checking: boolean;
  checked: boolean;
  update: UpdateInfo | null;
  installing: boolean;
  installed: boolean;
  updateError: boolean;
}

const state: State = {
  loaded: false,
  autostart: false,
  autostartError: false,
  checking: false,
  checked: false,
  update: null,
  installing: false,
  installed: false,
  updateError: false,
};

let onChange: (() => void) | undefined;

/** Lets main.ts re-render after an async change here. */
export function onSystemStateChange(callback: () => void): void {
  onChange = callback;
}

/** Test seam: drops the panel's state between cases. */
export function resetSystemSettings(): void {
  state.loaded = false;
  state.autostart = false;
  state.autostartError = false;
  state.checking = false;
  state.checked = false;
  state.update = null;
  state.installing = false;
  state.installed = false;
  state.updateError = false;
}

/**
 * The autostart switch and the update check (ADR 0042).
 *
 * Updates are checked on a press, not on a timer, and installed on a second
 * press. This app can be in the middle of somebody else's remote session, and
 * an update that restarted the process on its own would end that session
 * without anyone deciding to.
 */
export function systemSettings(
  locale: Locale,
  commands: SystemCommands = tauriSystemCommands,
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

  return html`
    <section class="system-settings" data-testid="system-settings">
      <h3>${t(locale, "system.heading")}</h3>

      <label class="system-row">
        <input
          type="checkbox"
          data-testid="autostart-toggle"
          .checked=${state.autostart}
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
                input.checked = state.autostart;
                onChange?.();
              },
            );
          }}
        />
        <span>${t(locale, "system.autostart")}</span>
      </label>
      <p class="system-note">${t(locale, "system.autostartNote")}</p>
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
      <div class="system-row">
        <button
          type="button"
          data-testid="update-check"
          ?disabled=${state.checking || state.installing}
          @click=${() => {
            state.checking = true;
            state.updateError = false;
            state.installed = false;
            onChange?.();
            void commands.updateCheck().then(
              (found) => {
                state.checking = false;
                state.checked = true;
                state.update = found;
                onChange?.();
              },
              (error: unknown) => {
                console.error("update_check failed:", error);
                state.checking = false;
                state.updateError = true;
                onChange?.();
              },
            );
          }}
        >
          ${state.checking ? t(locale, "system.checking") : t(locale, "system.checkUpdates")}
        </button>
        ${
          state.update === null
            ? state.checked && !state.updateError
              ? html`<span
                  class="system-note"
                  role="status"
                  data-testid="update-none"
                  >${t(locale, "system.upToDate")}</span
                >`
              : ""
            : html`
                <span class="system-note" data-testid="update-found"
                  >${t(locale, "system.available", state.update.version)}</span
                >
                <button
                  type="button"
                  data-testid="update-install"
                  ?disabled=${state.installing}
                  @click=${() => {
                  state.installing = true;
                  state.updateError = false;
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
                      state.updateError = true;
                      onChange?.();
                    },
                  );
                }}
                >
                  ${
                  state.installing
                    ? t(locale, "system.installing")
                    : t(locale, "system.installUpdate")
                }
                </button>
              `
        }
      </div>
      ${
        state.installed
          ? html`<p
              class="system-note"
              role="status"
              data-testid="update-installed"
            >
              ${t(locale, "system.installedRestart")}
            </p>`
          : ""
      }
      ${
        state.updateError
          ? html`<p
              class="system-error"
              role="status"
              data-testid="update-error"
            >
              ${t(locale, "system.updateFailed")}
            </p>`
          : ""
      }
    </section>
  `;
}
