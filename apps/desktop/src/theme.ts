// Light and dark (ADR 0117).
//
// The stylesheet already follows the system on its own: `app.css` draws the
// light palette, switches to the dark one under `prefers-color-scheme: dark`,
// and lets `data-theme` on <html> override either way. So "system" is the
// absence of the attribute rather than a value this module keeps in sync with
// the OS — a system switch at dusk needs no listener to take effect.
//
// A window preference, not machine state, so it lives in localStorage next to
// the language choice (i18n.ts) rather than behind an IPC command.

export type ThemeChoice = 'system' | 'light' | 'dark';

export const THEME_CHOICES: readonly ThemeChoice[] = ['system', 'light', 'dark'];

const THEME_STORAGE_KEY = 'lumepeer.theme';

/** The theme a person picked by hand, or "system" when they have not. */
export function getStoredTheme(): ThemeChoice {
  try {
    const raw = localStorage.getItem(THEME_STORAGE_KEY);
    return raw === 'light' || raw === 'dark' ? raw : 'system';
  } catch {
    // Storage can be unavailable; the system's own scheme is the safe reading.
    return 'system';
  }
}

/** Saves a choice ("system" clears it) and applies it to this window at once. */
export function setStoredTheme(choice: ThemeChoice): void {
  try {
    if (choice === 'system') {
      localStorage.removeItem(THEME_STORAGE_KEY);
    } else {
      localStorage.setItem(THEME_STORAGE_KEY, choice);
    }
  } catch {
    // Nothing to recover: the choice still applies to this running window.
  }
  applyTheme(choice);
}

/** Puts `choice` on <html>, where the stylesheet reads it. */
export function applyTheme(choice: ThemeChoice = getStoredTheme()): void {
  if (choice === 'system') {
    delete document.documentElement.dataset.theme;
  } else {
    document.documentElement.dataset.theme = choice;
  }
}
