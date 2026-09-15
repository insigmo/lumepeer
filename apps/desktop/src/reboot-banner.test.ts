import { render } from 'lit-html';
import { beforeEach, describe, expect, it, vi } from 'vitest';

import { rebootBanner } from './reboot-banner';

let root: HTMLElement;

beforeEach(() => {
  root = document.createElement('div');
  document.body.replaceChildren(root);
});

describe('the host reboot warning (ADR 0084)', () => {
  it('shows nothing at all when nobody has asked', () => {
    render(rebootBanner(null, 'en', () => {}), root);
    expect(root.querySelector('[data-testid="reboot-banner"]')).toBeNull();
  });

  it('names the guest, the act, and the seconds left', () => {
    render(
      rebootBanner({ peer_label: 'guest-ab12', mode: 'reboot', seconds_left: 7 }, 'en', () => {}),
      root,
    );
    const banner = root.querySelector('[data-testid="reboot-banner"]');
    expect(banner).not.toBeNull();
    // Which guest: the person may have several, and "somebody" is not a
    // warning they can act on (§15 — the pseudonymized label, never a name).
    expect(banner?.textContent).toContain('guest-ab12');
    expect(banner?.textContent).toContain('7');
  });

  it('says which of the two acts was asked for, because they are not the same', () => {
    render(
      rebootBanner({ peer_label: 'guest-ab12', mode: 'reboot', seconds_left: 9 }, 'en', () => {}),
      root,
    );
    const restart = root.querySelector('[data-testid="reboot-banner"]')?.textContent ?? '';
    render(
      rebootBanner({ peer_label: 'guest-ab12', mode: 'shutdown', seconds_left: 9 }, 'en', () => {}),
      root,
    );
    const off = root.querySelector('[data-testid="reboot-banner"]')?.textContent ?? '';
    expect(restart).not.toBe('');
    expect(off).not.toBe('');
    expect(restart).not.toBe(off);
  });

  it('interrupts rather than waiting to be read', () => {
    render(
      rebootBanner({ peer_label: 'guest-ab12', mode: 'shutdown', seconds_left: 3 }, 'en', () => {}),
      root,
    );
    // `alert`, not the `status` the recording and media banners use: this one
    // has a deadline, and a polite live region is announced after whatever
    // the screen reader is already saying.
    expect(root.querySelector('[data-testid="reboot-banner"]')?.getAttribute('role')).toBe('alert');
  });

  it('carries the button that stops it', () => {
    const cancel = vi.fn();
    render(
      rebootBanner({ peer_label: 'guest-ab12', mode: 'reboot', seconds_left: 5 }, 'en', cancel),
      root,
    );
    root.querySelector<HTMLButtonElement>('[data-testid="reboot-cancel"]')?.click();
    expect(cancel).toHaveBeenCalledOnce();
  });

  it('renders in every locale without falling back to a key', () => {
    for (const locale of ['ru', 'ar', 'ja', 'de'] as const) {
      render(
        rebootBanner({ peer_label: 'guest-ab12', mode: 'shutdown', seconds_left: 4 }, locale, () => {}),
        root,
      );
      const text = root.querySelector('[data-testid="reboot-banner"]')?.textContent ?? '';
      expect(text).toContain('guest-ab12');
      expect(text).not.toContain('reboot.banner');
    }
  });
});
