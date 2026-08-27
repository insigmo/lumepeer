// Host-side address book and the trust switch (§8; ADR 0034).
//
// The tests that matter here are the ones about trust: it is the only control
// on this screen that widens what a remote machine may do, so it must never
// move on one click, must never move as a side effect of anything else, and
// must show only what the core last reported.
import { render } from 'lit-html';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { addressBook, resetAddressBookPanel, saveDeviceButton, type AddressBookEntry } from './address-book';
import { SUPPORTED_LOCALES, t } from './i18n';

const invoke = vi.fn();

vi.mock('@tauri-apps/api/core', () => ({
  invoke: (...args: unknown[]) => invoke(...args),
}));

const office: AddressBookEntry = {
  peer_label: 'guest-ab12',
  name: 'Office workstation',
  tags: ['work'],
  notes: 'upstairs',
  trusted: false,
  connected: false,
};

const home: AddressBookEntry = {
  peer_label: 'guest-cd34',
  name: 'Home laptop',
  tags: ['family'],
  notes: '',
  trusted: true,
  connected: true,
};

let container: HTMLElement;

beforeEach(() => {
  invoke.mockReset();
  invoke.mockResolvedValue(undefined);
  resetAddressBookPanel();
  container = document.createElement('div');
  document.body.appendChild(container);
});

afterEach(() => {
  container.remove();
});

function trustBoxes(): HTMLInputElement[] {
  return Array.from(container.querySelectorAll<HTMLInputElement>('.book-trust input'));
}

describe('address book', () => {
  it('says so when nothing is saved', () => {
    render(addressBook([], 'en'), container);
    expect(container.querySelector('.address-book-empty')?.textContent?.trim()).toBe(
      t('en', 'book.empty'),
    );
  });

  it('lists each device with its name, tags, note and trust state', () => {
    render(addressBook([office, home], 'en'), container);

    const rows = container.querySelectorAll('[data-testid="book-row"]');
    expect(rows).toHaveLength(2);
    expect(rows[0]?.querySelector('.book-name')?.textContent).toBe('Office workstation');
    expect(rows[0]?.querySelector('.book-tag')?.textContent).toBe('work');
    expect(rows[0]?.querySelector('.book-note')?.textContent).toBe('upstairs');
    const states = container.querySelectorAll('[data-testid="book-trust-state"]');
    expect(states[0]?.textContent?.trim()).toBe(t('en', 'book.untrusted'));
    expect(states[1]?.textContent?.trim()).toBe(t('en', 'book.trusted'));
  });

  it('shows a device as trusted only because the core reported it', () => {
    render(addressBook([office, home], 'en'), container);
    const boxes = trustBoxes();
    expect(boxes[0]?.checked).toBe(false);
    expect(boxes[1]?.checked).toBe(true);
  });

  /// The confirmation is the point: trusting a device lets it start a session
  /// with nobody here, so a single click must not be enough to do it.
  it('trusting opens a confirmation and sends nothing until it is accepted', async () => {
    const onRefresh = vi.fn();
    render(addressBook([office], 'en', onRefresh), container);

    trustBoxes()[0]?.click();
    expect(invoke).not.toHaveBeenCalled();

    render(addressBook([office], 'en', onRefresh), container);
    const dialog = container.querySelector('.trust-confirm');
    expect(dialog).not.toBeNull();
    expect(dialog?.getAttribute('role')).toBe('alertdialog');
    expect(dialog?.querySelector('h4')?.textContent).toContain('Office workstation');

    container.querySelector<HTMLButtonElement>('.trust-accept')?.click();
    await vi.waitFor(() =>
      expect(invoke).toHaveBeenCalledWith('address_book_set_trusted', {
        args: { peer: 'guest-ab12', trusted: true },
      }),
    );
  });

  it('cancelling the confirmation leaves the device untrusted and sends nothing', () => {
    render(addressBook([office], 'en'), container);
    trustBoxes()[0]?.click();
    render(addressBook([office], 'en'), container);

    container.querySelector<HTMLButtonElement>('.trust-cancel')?.click();
    render(addressBook([office], 'en'), container);

    expect(container.querySelector('.trust-confirm')).toBeNull();
    expect(invoke).not.toHaveBeenCalled();
    expect(trustBoxes()[0]?.checked).toBe(false);
  });

  it('the checkbox does not stay on while the core has not agreed', () => {
    render(addressBook([office], 'en'), container);
    const box = trustBoxes()[0];
    box?.click();
    // The click ticked it; the handler puts it back, because "trusted" is the
    // core's statement to make and it has not made it.
    expect(box?.checked).toBe(false);
  });

  it('withdrawing trust asks first and goes straight through once confirmed', async () => {
    const confirm = vi.spyOn(globalThis, 'confirm').mockReturnValue(true);
    render(addressBook([home], 'en'), container);

    trustBoxes()[0]?.click();
    expect(confirm).toHaveBeenCalled();
    await vi.waitFor(() =>
      expect(invoke).toHaveBeenCalledWith('address_book_set_trusted', {
        args: { peer: 'guest-cd34', trusted: false },
      }),
    );
    confirm.mockRestore();
  });

  it('declining the withdrawal changes nothing', () => {
    const confirm = vi.spyOn(globalThis, 'confirm').mockReturnValue(false);
    render(addressBook([home], 'en'), container);

    trustBoxes()[0]?.click();
    expect(invoke).not.toHaveBeenCalled();
    confirm.mockRestore();
  });

  it('filters by tag without touching what is stored', () => {
    render(addressBook([office, home], 'en'), container);
    const filter = container.querySelector<HTMLSelectElement>('#book-filter');
    expect(filter).not.toBeNull();

    if (filter) {
      filter.value = 'family';
      filter.dispatchEvent(new Event('change'));
    }
    render(addressBook([office, home], 'en'), container);

    const rows = container.querySelectorAll('[data-testid="book-row"]');
    expect(rows).toHaveLength(1);
    expect(rows[0]?.querySelector('.book-name')?.textContent).toBe('Home laptop');
    expect(invoke).not.toHaveBeenCalled();
  });

  it('forgetting a device asks first', async () => {
    const confirm = vi.spyOn(globalThis, 'confirm').mockReturnValue(true);
    render(addressBook([office], 'en'), container);

    container.querySelector<HTMLButtonElement>('.book-forget')?.click();
    await vi.waitFor(() =>
      expect(invoke).toHaveBeenCalledWith('address_book_remove', { args: { peer: 'guest-ab12' } }),
    );
    confirm.mockRestore();
  });

  /// Free text from a human: `lit-html` escapes bindings, and nothing on this
  /// screen builds markup by hand. A device named with a tag must render as
  /// text, not as an element.
  it('renders a hostile device name as text', () => {
    render(
      addressBook([{ ...office, name: '<img src=x onerror=alert(1)>' }], 'en'),
      container,
    );
    expect(container.querySelector('img')).toBeNull();
    expect(container.querySelector('.book-name')?.textContent).toBe('<img src=x onerror=alert(1)>');
  });

  it('is labelled in both locales and keyboard reachable', () => {
    for (const locale of SUPPORTED_LOCALES) {
      const scoped = document.createElement('div');
      document.body.appendChild(scoped);
      try {
        render(addressBook([office, home], locale), scoped);
        expect(scoped.querySelector('#address-book-heading')?.textContent?.trim()).toBe(
          t(locale, 'book.heading'),
        );
        for (const control of scoped.querySelectorAll<HTMLElement>('button, input, select')) {
          expect(control.tabIndex).not.toBe(-1);
        }
        for (const box of scoped.querySelectorAll<HTMLInputElement>('.book-trust input')) {
          expect(box.getAttribute('aria-label')).toBeTruthy();
        }
      } finally {
        scoped.remove();
      }
    }
  });
});

describe('saving a device from a session', () => {
  it('saves it untrusted, and asks for no permission on the way', async () => {
    const onRefresh = vi.fn();
    render(saveDeviceButton('guest-ab12', 'en', onRefresh), container);

    container.querySelector<HTMLButtonElement>('.book-save-btn')?.click();
    await vi.waitFor(() =>
      expect(invoke).toHaveBeenCalledWith('address_book_upsert', {
        args: { peer: 'guest-ab12', name: 'guest-ab12', tags: [], notes: '' },
      }),
    );
    // Nothing else was called: connecting once is not a path to trust.
    expect(invoke).toHaveBeenCalledTimes(1);
    await vi.waitFor(() => expect(onRefresh).toHaveBeenCalled());
  });
});
