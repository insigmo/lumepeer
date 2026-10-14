// Which desktop this webview runs on, for the few places the UI follows the
// platform's own conventions instead of its own.

/** Whether this webview runs on macOS. */
export const IS_MAC =
  typeof navigator !== 'undefined' && /Macintosh|Mac OS X/.test(navigator.userAgent);
