// `window.confirm` is synchronous in a plain webview and async once
// tauri-plugin-dialog's init script has replaced it (main window). Both must
// end in the person's answer, and nothing else.
import { afterEach, describe, expect, it, vi } from 'vitest';

import { askFirst } from './ask-first';

/** What tauri-plugin-dialog 2.x installs as `window.confirm`: an async function. */
function pluginConfirm(answer: Promise<boolean>): ReturnType<typeof vi.spyOn> {
  return vi
    .spyOn(globalThis, 'confirm')
    .mockImplementation((() => answer) as unknown as typeof globalThis.confirm);
}

afterEach(() => {
  vi.restoreAllMocks();
});

describe('askFirst', () => {
  it("passes the webview's own synchronous answer through", async () => {
    vi.spyOn(globalThis, 'confirm').mockReturnValue(true);
    await expect(askFirst('Remove it?')).resolves.toBe(true);
    vi.spyOn(globalThis, 'confirm').mockReturnValue(false);
    await expect(askFirst('Remove it?')).resolves.toBe(false);
  });

  it("waits for the dialog plugin's answer instead of reading its promise as yes", async () => {
    pluginConfirm(Promise.resolve(false));
    await expect(askFirst('Remove it?')).resolves.toBe(false);
    pluginConfirm(Promise.resolve(true));
    await expect(askFirst('Remove it?')).resolves.toBe(true);
  });

  it('shows the message it was given', async () => {
    const confirm = pluginConfirm(Promise.resolve(true));
    await askFirst('Forget the password saved for host-ab12?');
    expect(confirm).toHaveBeenCalledWith('Forget the password saved for host-ab12?');
  });

  it('counts a dialog that could not be shown as no', async () => {
    vi.spyOn(console, 'error').mockImplementation(() => {});
    pluginConfirm(Promise.reject(new Error('dialog.message not allowed')));
    await expect(askFirst('Remove it?')).resolves.toBe(false);
  });
});
