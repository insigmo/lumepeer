import { render } from 'lit-html';
import { describe, expect, it } from 'vitest';

import { t } from './i18n';
import { WireCodec } from './view-decoder';
import { DecodeMeter, statsOverlay, statsRows, type MediaStats } from './view-stats';

const MEDIA: MediaStats = {
  fps: 30,
  kbps: 2400,
  frame_bytes_avg: 10_240,
  frame_bytes_max: 51_200,
  keyframes_recent: 1,
  frames: 900,
  keyframes: 3,
  queue_ms: 15,
  queue_max_ms: 40,
  rtt_ms: 87,
  cwnd_bytes: 65_536,
  lost_packets: 12,
  congestion_events: 3,
  relay: false,
};

const PICTURE = { width: 1920, height: 1080, codec: WireCodec.H264 };
const NO_DECODE = { count: 0, avgMs: null, maxMs: null };

function valueOf(rows: ReturnType<typeof statsRows>, label: string): string | undefined {
  return rows.find((row) => row.label === label)?.value;
}

describe('DecodeMeter', () => {
  it('times each picture from decode to output, matched by timestamp', () => {
    const meter = new DecodeMeter();
    meter.submitted(1_000, 10);
    meter.submitted(34_000, 40);
    meter.decoded(1_000, 13);
    meter.decoded(34_000, 45);
    expect(meter.take()).toEqual({ count: 2, avgMs: 4, maxMs: 5 });
  });

  it('starts every period empty, and an empty period reads as unmeasured', () => {
    const meter = new DecodeMeter();
    meter.submitted(1, 0);
    meter.decoded(1, 2);
    meter.take();
    expect(meter.take()).toEqual(NO_DECODE);
  });

  it('ignores a picture it never saw submitted, and forgets frames on reset', () => {
    const meter = new DecodeMeter();
    meter.decoded(7, 100);
    meter.submitted(8, 0);
    meter.reset();
    meter.decoded(8, 100);
    expect(meter.take()).toEqual(NO_DECODE);
  });
});

describe('statsRows', () => {
  it('shows every measured figure with its unit', () => {
    const rows = statsRows('en', MEDIA, { count: 30, avgMs: 3.14, maxMs: 7 }, PICTURE);
    expect(valueOf(rows, 'stats.picture')).toBe('1920×1080 · H.264');
    expect(valueOf(rows, 'stats.fps')).toBe('30');
    expect(valueOf(rows, 'stats.bitrate')).toBe('2400 kbit/s');
    expect(valueOf(rows, 'stats.frameSize')).toBe('10 / 50 KB');
    expect(valueOf(rows, 'stats.keyframes')).toBe('1 / 3');
    expect(valueOf(rows, 'stats.queue')).toBe('15 / 40 ms');
    expect(valueOf(rows, 'stats.decode')).toBe('3.1 / 7.0 ms');
    expect(valueOf(rows, 'stats.rtt')).toBe('87 ms');
    expect(valueOf(rows, 'stats.path')).toBe(t('en', 'quality.path.direct'));
    expect(valueOf(rows, 'stats.loss')).toBe('12 / 3');
    expect(valueOf(rows, 'stats.cwnd')).toBe('64 KB');
  });

  it('reads unmeasured figures as a dash, never as a zero', () => {
    const rows = statsRows(
      'en',
      {
        ...MEDIA,
        fps: 0,
        queue_ms: null,
        queue_max_ms: null,
        rtt_ms: null,
        cwnd_bytes: null,
        lost_packets: null,
        congestion_events: null,
        relay: null,
      },
      NO_DECODE,
      { width: 0, height: 0, codec: null },
    );
    for (const label of [
      'stats.picture',
      'stats.frameSize',
      'stats.queue',
      'stats.decode',
      'stats.rtt',
      'stats.path',
      'stats.loss',
      'stats.cwnd',
    ]) {
      expect(valueOf(rows, label)).toBe('—');
    }
  });

  it('names a relayed route as one', () => {
    const rows = statsRows('ru', { ...MEDIA, relay: true }, NO_DECODE, PICTURE);
    expect(valueOf(rows, 'stats.path')).toBe(t('ru', 'quality.path.relay'));
    expect(valueOf(rows, 'stats.bitrate')).toBe('2400 кбит/с');
  });

  it('still shows what this window measured when the host numbers are missing', () => {
    const rows = statsRows('en', null, { count: 5, avgMs: 2, maxMs: 4 }, PICTURE);
    expect(rows.map((row) => row.label)).toEqual(['stats.picture', 'stats.decode']);
  });
});

describe('statsOverlay', () => {
  it('draws one labelled line per row', () => {
    const host = document.createElement('div');
    render(statsOverlay('en', statsRows('en', MEDIA, NO_DECODE, PICTURE)), host);
    expect(host.querySelector('h2')?.textContent).toBe('Video statistics');
    expect(host.querySelectorAll('dt')).toHaveLength(11);
    expect(host.querySelector('section')?.getAttribute('aria-label')).toBe('Video statistics');
  });
});
