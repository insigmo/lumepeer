// The port-forwarding panel (design doc §4.1, §8.2, §18; ADR 0078).
//
// The panel's job is to make a tunnel two visible decisions rather than one
// invisible capability. So the tests are about that: nothing is reachable
// until the host names an address, naming one is a deliberate press, taking
// one back is another, and while something is actually being forwarded the
// row says so.
import * as axe from 'axe-core';
import { render } from 'lit-html';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { SUPPORTED_LOCALES, t } from './i18n';
import {
  isForwarding,
  parseAddress,
  tunnelPanel,
  type TunnelCommands,
  type TunnelRow,
} from './tunnels';

const PEER = 'guest-ab12';

function commands(): TunnelCommands & {
  setTarget: ReturnType<typeof vi.fn>;
  closeAll: ReturnType<typeof vi.fn>;
} {
  return {
    setTarget: vi.fn().mockResolvedValue(undefined),
    closeAll: vi.fn().mockResolvedValue(undefined),
  };
}

function row(over: Partial<TunnelRow> = {}): TunnelRow {
  return {
    peer_label: PEER,
    host: '127.0.0.1',
    port: 8080,
    streams: 0,
    bytes: 0,
    ...over,
  };
}

let container: HTMLElement;

beforeEach(() => {
  container = document.createElement('div');
  document.body.appendChild(container);
});

afterEach(() => {
  container.remove();
});

function draw(rows: TunnelRow[], cmds: TunnelCommands): void {
  render(tunnelPanel(PEER, rows, 'en', cmds), container);
}

function press(testid: string): void {
  container.querySelector<HTMLButtonElement>(`[data-testid="${testid}"]`)?.click();
}

describe('a session with the grant and no address', () => {
  it('says it reaches nothing, rather than showing an empty list', () => {
    draw([], commands());
    expect(container.querySelector('[data-testid="tunnel-empty"]')?.textContent?.trim()).toBe(
      t('en', 'tunnel.none'),
    );
    expect(container.querySelectorAll('[data-testid="tunnel-target"]')).toHaveLength(0);
  });

  it('has nothing to close, so the close button is not pressable', () => {
    draw([], commands());
    expect(
      container.querySelector<HTMLButtonElement>('[data-testid="tunnel-close-all"]')?.disabled,
    ).toBe(true);
  });
});

describe('naming an address', () => {
  it('takes a deliberate submit, and sends the two halves apart', () => {
    const cmds = commands();
    draw([], cmds);
    const input = container.querySelector<HTMLInputElement>('[data-testid="tunnel-address"]');
    const form = container.querySelector<HTMLFormElement>('[data-testid="tunnel-add"]');
    expect(input).not.toBeNull();
    // Typing alone allows nothing.
    if (input) {
      input.value = '127.0.0.1:8080';
    }
    expect(cmds.setTarget).not.toHaveBeenCalled();

    form?.dispatchEvent(new Event('submit', { cancelable: true, bubbles: true }));
    expect(cmds.setTarget).toHaveBeenCalledWith(PEER, '127.0.0.1', 8080, true);
  });

  it('refuses something that is not an address without asking the actor', () => {
    const cmds = commands();
    draw([], cmds);
    const input = container.querySelector<HTMLInputElement>('[data-testid="tunnel-address"]');
    const form = container.querySelector<HTMLFormElement>('[data-testid="tunnel-add"]');
    for (const text of ['', 'localhost', '127.0.0.1:', ':8080', '127.0.0.1:99999', 'a b:80']) {
      if (input) {
        input.value = text;
      }
      form?.dispatchEvent(new Event('submit', { cancelable: true, bubbles: true }));
    }
    expect(cmds.setTarget).not.toHaveBeenCalled();
  });

  it('splits what an address is, in both spellings', () => {
    expect(parseAddress('127.0.0.1:8080')).toEqual({ host: '127.0.0.1', port: 8080 });
    expect(parseAddress('  localhost:443 ')).toEqual({ host: 'localhost', port: 443 });
    expect(parseAddress('[::1]:5432')).toEqual({ host: '::1', port: 5432 });
    expect(parseAddress('::1:5432')).toBeNull();
    expect(parseAddress('127.0.0.1:0')).toBeNull();
    expect(parseAddress('127.0.0.1')).toBeNull();
  });
});

describe('an address that is already named', () => {
  it('can be taken back, which is the same command in reverse', () => {
    const cmds = commands();
    draw([row()], cmds);
    expect(container.querySelector('[data-testid="tunnel-target"]')?.textContent).toContain(
      '127.0.0.1:8080',
    );
    press('tunnel-deny');
    expect(cmds.setTarget).toHaveBeenCalledWith(PEER, '127.0.0.1', 8080, false);
  });

  it('says so on the row while something is actually going through it', () => {
    draw([row({ streams: 2, bytes: 4096 })], commands());
    const active = container.querySelector('[data-testid="tunnel-active"]');
    expect(active?.textContent).toContain(t('en', 'tunnel.active'));
    expect(active?.textContent).toContain('2');
    expect(isForwarding([row({ streams: 2 })], PEER)).toBe(true);
    expect(isForwarding([row({ streams: 0 })], PEER)).toBe(false);
  });

  it('closes every forwarded connection at once', () => {
    const cmds = commands();
    draw([row({ streams: 3 })], cmds);
    press('tunnel-close-all');
    expect(cmds.closeAll).toHaveBeenCalledWith(PEER);
  });

  it('belongs to its own session and to no other', () => {
    draw([row({ peer_label: 'guest-cd34' })], commands());
    expect(container.querySelectorAll('[data-testid="tunnel-target"]')).toHaveLength(0);
    expect(container.querySelector('[data-testid="tunnel-empty"]')).not.toBeNull();
  });
});

describe('accessibility', () => {
  const LAYOUT_DEPENDENT_RULES = ['color-contrast', 'target-size'];

  for (const locale of SUPPORTED_LOCALES) {
    it(`has no axe violations, and every control is reachable (${locale})`, async () => {
      render(
        tunnelPanel(PEER, [row({ streams: 1, bytes: 128 })], locale, commands()),
        container,
      );
      const results = await axe.run(container, {
        rules: Object.fromEntries(LAYOUT_DEPENDENT_RULES.map((id) => [id, { enabled: false }])),
      });
      expect(results.violations).toEqual([]);
      expect(container.querySelectorAll('[tabindex]')).toHaveLength(0);
      for (const testid of ['tunnel-deny', 'tunnel-allow', 'tunnel-close-all']) {
        const button = container.querySelector<HTMLButtonElement>(`[data-testid="${testid}"]`);
        expect(button, `${testid} missing in ${locale}`).not.toBeNull();
        expect(button?.tagName).toBe('BUTTON');
      }
    });
  }
});
