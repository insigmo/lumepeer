// Connection-quality pill (§18; ADR 0026, ADR 0037).
//
// What is checked here is the one claim the panel exists to make: everything
// it shows was measured, and anything that was not measured says so rather
// than showing a zero.

import { render } from 'lit-html';
import { beforeEach, describe, expect, it } from 'vitest';

import { connectionQuality, type ConnectionStats } from './connection-quality';
import { t } from './i18n';

function stats(overrides: Partial<ConnectionStats> = {}): ConnectionStats {
  return {
    peer_label: 'guest-ab12',
    rtt_ms: 42,
    loss_permille: 25,
    goodput_kbps: 3_200,
    path: 'direct',
    transport: 'iroh',
    fallbacks: [],
    relay_region: null,
    bitrate_kbps: 4_000,
    fps: 30,
    codec: 'h264',
    ...overrides,
  };
}

let container: HTMLElement;

beforeEach(() => {
  document.body.innerHTML = '';
  container = document.createElement('div');
  document.body.append(container);
});

describe('connection quality pill', () => {
  it('shows nothing at all for a session with no measured link', () => {
    render(connectionQuality(undefined, 'en'), container);
    expect(container.querySelector('[data-testid="quality"]')).toBeNull();
  });

  it('summarizes the path and the round trip in the pill itself', () => {
    render(connectionQuality(stats(), 'en'), container);
    const pill = container.querySelector('[data-testid="quality-pill"]');
    expect(pill?.textContent).toContain(t('en', 'quality.path.direct'));
    expect(pill?.textContent).toContain('42 ms');
  });

  it('reports the path iroh observed, not one the settings asked for', () => {
    for (const path of ['direct', 'relay', 'mixed', 'unknown'] as const) {
      render(connectionQuality(stats({ path }), 'en'), container);
      expect(container.querySelector('[data-testid="quality"]')?.getAttribute('data-path')).toBe(
        path,
      );
    }
  });

  it('opens onto loss, throughput and what is being sent', () => {
    render(connectionQuality(stats(), 'en'), container);
    const details = container.querySelector('[data-testid="quality-details"]');
    // 25 permille is 2.5 per cent, shown as a figure a person reads.
    expect(details?.textContent).toContain('2.5%');
    expect(details?.textContent).toContain('3200 kbit/s');
    expect(details?.textContent).toContain('4000 kbit/s');
    expect(details?.textContent).toContain('30 fps');
  });

  it('says a value was not measured rather than showing a zero', () => {
    render(
      connectionQuality(
        stats({ rtt_ms: null, loss_permille: null, goodput_kbps: null, bitrate_kbps: null, fps: null }),
        'en',
      ),
      container,
    );
    const unknown = t('en', 'quality.unknown');
    expect(container.querySelector('[data-testid="quality-pill"]')?.textContent).toContain(unknown);
    expect(container.querySelector('[data-testid="quality-details"]')?.textContent).not.toContain(
      '0 ms',
    );
  });

  // gap-tasks/06: which codec was negotiated belongs here and nowhere else,
  // under the codec's own name, and only while a picture travels in it.
  it('names the codec the picture travels in', () => {
    for (const [codec, name] of [
      ['h264', 'H.264'],
      ['av1', 'AV1'],
      ['vp9', 'VP9'],
    ] as const) {
      render(connectionQuality(stats({ codec }), 'en'), container);
      const details = container.querySelector('[data-testid="quality-details"]');
      expect(details?.textContent).toContain(t('en', 'quality.codecLabel'));
      expect(details?.textContent).toContain(name);
    }
  });

  it('names no codec while no picture is travelling', () => {
    render(connectionQuality(stats({ codec: null }), 'en'), container);
    expect(
      container.querySelector('[data-testid="quality-details"]')?.textContent,
    ).not.toContain(t('en', 'quality.codecLabel'));
  });

  it('names the relay by region only, and only when one is in use', () => {
    render(connectionQuality(stats({ path: 'relay', relay_region: 'euw1-1' }), 'en'), container);
    const details = container.querySelector('[data-testid="quality-details"]');
    expect(details?.textContent).toContain('euw1-1');
    expect(details?.textContent).toContain(t('en', 'quality.relayLabel'));

    render(connectionQuality(stats({ path: 'direct', relay_region: null }), 'en'), container);
    expect(
      container.querySelector('[data-testid="quality-details"]')?.textContent,
    ).not.toContain(t('en', 'quality.relayLabel'));
  });

  // gap-tasks/23 task 3: the panel has to say which transport is carrying the
  // session, in words, and the obfuscated one is always a single direct UDP
  // path — so it is named as what it is rather than as "direct" twice over.
  it('names the obfuscated transport in the pill instead of the iroh path', () => {
    render(connectionQuality(stats({ transport: 'obfuscated' }), 'en'), container);
    const pill = container.querySelector('[data-testid="quality-pill"]');
    expect(pill?.textContent).toContain(t('en', 'quality.path.obfuscated'));
    expect(pill?.textContent).not.toContain(t('en', 'quality.path.direct'));
    expect(
      container.querySelector('[data-testid="quality"]')?.getAttribute('data-transport'),
    ).toBe('obfuscated');
  });

  // gap-tasks/23 task 3: which attempt was given up on, and what this machine
  // observed when it was — never a guess at why the network did it.
  it('says which transport was given up on and what happened to it', () => {
    render(
      connectionQuality(
        stats({
          transport: 'iroh',
          fallbacks: [{ transport: 'obfuscated', failure: 'DIAL_FAILED' }],
        }),
        'en',
      ),
      container,
    );
    const details = container.querySelector('[data-testid="quality-details"]');
    expect(details?.textContent).toContain(t('en', 'quality.fallbackLabel'));
    expect(details?.textContent).toContain(
      t('en', 'quality.fallback.noAnswer', t('en', 'quality.transport.obfuscated')),
    );
  });

  it('falls back to a neutral phrase for a failure code it does not know', () => {
    render(
      connectionQuality(
        stats({ fallbacks: [{ transport: 'obfuscated', failure: 'SOMETHING_NEW' }] }),
        'en',
      ),
      container,
    );
    const details = container.querySelector('[data-testid="quality-details"]');
    expect(details?.textContent).toContain(
      t('en', 'quality.fallback.failed', t('en', 'quality.transport.obfuscated')),
    );
    expect(details?.textContent).not.toContain('SOMETHING_NEW');
  });

  it('shows no fallback row at all when the first transport worked', () => {
    render(connectionQuality(stats(), 'en'), container);
    expect(
      container.querySelector('[data-testid="quality-details"]')?.textContent,
    ).not.toContain(t('en', 'quality.fallbackLabel'));
  });

  it('is a native disclosure, so it is reachable without a pointer', () => {
    render(connectionQuality(stats(), 'en'), container);
    const details = container.querySelector<HTMLDetailsElement>('[data-testid="quality"]');
    expect(details?.tagName).toBe('DETAILS');
    expect(details?.open).toBe(false);
    expect(details?.querySelector('summary')).not.toBeNull();
  });
});
