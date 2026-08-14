// apps/desktop/src/keyboard-nav.test.ts
//
// axe-core (Task 3) checks markup/ARIA statically; it does not drive Tab and
// confirm focus actually lands somewhere sane. This test does that part of
// §19 phase 6's "доступность через клавиатуру" by hand.
import { render } from 'lit-html';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { consentDialog } from './consent-dialog';
import type { SessionStatus } from './session-status';

let container: HTMLElement;

beforeEach(() => {
  container = document.createElement('div');
  document.body.appendChild(container);
});

afterEach(() => {
  container.remove();
});

function focusables(root: HTMLElement): HTMLButtonElement[] {
  return Array.from(root.querySelectorAll('button'));
}

describe('keyboard navigation: consent dialog', () => {
  it('every action is a real <button>, reachable by Tab, none disabled or tabindex=-1', () => {
    const request: SessionStatus = { peer_label: 'guest-ab12', role: 'view_only', input: false, state: 'pending' };
    render(consentDialog(request, 'en'), container);

    const buttons = focusables(container);
    expect(buttons).toHaveLength(3);
    for (const button of buttons) {
      expect(button.disabled).toBe(false);
      expect(button.tabIndex).not.toBe(-1);
    }
  });

  it('the deny action is first in DOM order and carries autofocus, so a keyboard/screen-reader user lands on the safe default', () => {
    const request: SessionStatus = { peer_label: 'guest-ab12', role: 'view_only', input: false, state: 'pending' };
    render(consentDialog(request, 'en'), container);

    const buttons = focusables(container);
    expect(buttons[0]?.textContent?.trim()).toBe('Deny');
    expect(buttons[0]?.autofocus).toBe(true);
  });
});
