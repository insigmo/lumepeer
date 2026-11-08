// A yes/no question put to the person before something that cannot be taken
// back: removing a host, forgetting a password, turning unattended access off.
//
// `window.confirm` is not the same function in every window. In the main
// process `tauri-plugin-dialog`'s init script replaces it with an *async* one
// that resolves once the native dialog is answered; the access-settings
// process (ADR 0169 §5) runs without that plugin and keeps the webview's own
// synchronous `confirm`. Read without `await`, the plugin's promise is truthy,
// so `if (!confirm(...)) return;` went ahead before anyone had answered —
// Cancel cancelled nothing. Awaiting covers both functions.

/**
 * Resolves `true` only once the person has said yes to `message`.
 *
 * A dialog that could not be shown at all (the plugin's call refused) counts
 * as no: an action that asks first never happens without the answer.
 */
export async function askFirst(message: string): Promise<boolean> {
  try {
    // Typed as the DOM's synchronous `confirm`, but a promise in the main window.
    const answer: boolean | Promise<boolean> = globalThis.confirm(message);
    return (await answer) === true;
  } catch (error) {
    console.error('the confirmation dialog could not be shown:', error);
    return false;
  }
}
