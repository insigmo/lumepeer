// Video statistics overlay of one view window (Ctrl+Alt+Shift+S).
//
// For a person working out why a session is slow or soft. Everything shown
// was measured, on this machine or on the media connection, and a figure
// nothing has measured yet reads as a dash rather than as a zero pretending
// to be a reading — the same rule the connection-quality panel follows
// (ADR 0026).
//
// The number that needs reading carefully is the queue delay. The two
// machines share no clock, so it is not the age of a frame: it is how much
// later than the best recent case the frame arrived. It is near zero on a
// link that keeps up and grows, frame by frame, on one that is asked to carry
// more than it can — which is exactly the delay a person feels between a
// click and the picture answering it.

import { html, type TemplateResult } from 'lit-html';

import type { Locale, TranslationKey } from './i18n';
import { t } from './i18n';
import { WireCodec } from './view-decoder';

/** One answer of the `view_media_stats` IPC call; see `media_stats.rs`. */
export interface MediaStats {
  fps: number;
  kbps: number;
  frame_bytes_avg: number;
  frame_bytes_max: number;
  keyframes_recent: number;
  frames: number;
  keyframes: number;
  queue_ms: number | null;
  queue_max_ms: number | null;
  rtt_ms: number | null;
  cwnd_bytes: number | null;
  lost_packets: number | null;
  congestion_events: number | null;
  relay: boolean | null;
}

/** How long decoding took over the period since the previous reading. */
export interface DecodeReading {
  /** Pictures that came out of the decoder. */
  count: number;
  avgMs: number | null;
  maxMs: number | null;
}

/**
 * Frames handed to the decoder whose picture has not come out yet, beyond
 * which the meter assumes the decoder dropped some and starts over.
 */
const MAX_PENDING = 256;

/**
 * Time from `VideoDecoder.decode` to the picture arriving on the output
 * callback, matched by the frame's timestamp.
 */
export class DecodeMeter {
  readonly #pending = new Map<number, number>();
  #count = 0;
  #totalMs = 0;
  #maxMs = 0;

  /** A frame with this timestamp was handed to the decoder at `now`. */
  submitted(timestampUs: number, now: number): void {
    if (this.#pending.size >= MAX_PENDING) {
      this.#pending.clear();
    }
    this.#pending.set(timestampUs, now);
  }

  /** The picture of the frame with this timestamp came out at `now`. */
  decoded(timestampUs: number, now: number): void {
    const at = this.#pending.get(timestampUs);
    if (at === undefined) {
      return;
    }
    this.#pending.delete(timestampUs);
    const took = Math.max(now - at, 0);
    this.#count += 1;
    this.#totalMs += took;
    this.#maxMs = Math.max(this.#maxMs, took);
  }

  /** The reading since the previous call; starts the next period. */
  take(): DecodeReading {
    const reading: DecodeReading =
      this.#count === 0
        ? { count: 0, avgMs: null, maxMs: null }
        : { count: this.#count, avgMs: this.#totalMs / this.#count, maxMs: this.#maxMs };
    this.#count = 0;
    this.#totalMs = 0;
    this.#maxMs = 0;
    return reading;
  }

  /** Forgets frames in flight: the decoder they were handed to is gone. */
  reset(): void {
    this.#pending.clear();
  }
}

/** The picture as this window last drew it. */
export interface PictureReading {
  width: number;
  height: number;
  codec: WireCodec | null;
}

/** One line of the overlay. */
export interface StatsRow {
  label: TranslationKey;
  value: string;
}

const UNKNOWN = '—';
const BYTES_PER_KB = 1024;

const CODEC_NAME: Readonly<Record<WireCodec, string>> = {
  [WireCodec.H264]: 'H.264',
  [WireCodec.Av1]: 'AV1',
  [WireCodec.Vp9]: 'VP9',
};

function kilobytes(bytes: number): string {
  const kb = bytes / BYTES_PER_KB;
  return kb < 10 ? kb.toFixed(1) : Math.round(kb).toString();
}

function pair(first: string, second: string, unit: string): string {
  return `${first} / ${second} ${unit}`;
}

function orUnknown(value: number | null, format: (value: number) => string): string {
  return value === null ? UNKNOWN : format(value);
}

/** Every line of the overlay, in the order it is drawn. */
export function statsRows(
  locale: Locale,
  media: MediaStats | null,
  decode: DecodeReading,
  picture: PictureReading,
): StatsRow[] {
  const ms = t(locale, 'stats.unit.ms');
  const kb = t(locale, 'stats.unit.kb');
  const pictureValue =
    picture.width > 0 && picture.height > 0
      ? [
          `${picture.width}×${picture.height}`,
          picture.codec === null ? null : CODEC_NAME[picture.codec],
        ]
          .filter((part) => part !== null)
          .join(' · ')
      : UNKNOWN;
  const decodeValue =
    decode.avgMs === null || decode.maxMs === null
      ? UNKNOWN
      : pair(decode.avgMs.toFixed(1), decode.maxMs.toFixed(1), ms);
  const rows: StatsRow[] = [{ label: 'stats.picture', value: pictureValue }];
  if (!media) {
    rows.push({ label: 'stats.decode', value: decodeValue });
    return rows;
  }
  const queue =
    media.queue_ms === null || media.queue_max_ms === null
      ? UNKNOWN
      : pair(media.queue_ms.toString(), media.queue_max_ms.toString(), ms);
  const route =
    media.relay === null
      ? UNKNOWN
      : t(locale, media.relay ? 'quality.path.relay' : 'quality.path.direct');
  const loss =
    media.lost_packets === null || media.congestion_events === null
      ? UNKNOWN
      : `${media.lost_packets} / ${media.congestion_events}`;
  rows.push(
    { label: 'stats.fps', value: media.fps.toString() },
    { label: 'stats.bitrate', value: `${media.kbps} ${t(locale, 'stats.unit.kbps')}` },
    {
      label: 'stats.frameSize',
      value:
        media.fps === 0
          ? UNKNOWN
          : pair(kilobytes(media.frame_bytes_avg), kilobytes(media.frame_bytes_max), kb),
    },
    { label: 'stats.keyframes', value: `${media.keyframes_recent} / ${media.keyframes}` },
    { label: 'stats.queue', value: queue },
    { label: 'stats.decode', value: decodeValue },
    { label: 'stats.rtt', value: orUnknown(media.rtt_ms, (rtt) => `${rtt} ${ms}`) },
    { label: 'stats.path', value: route },
    { label: 'stats.loss', value: loss },
    {
      label: 'stats.cwnd',
      value: orUnknown(media.cwnd_bytes, (cwnd) => `${kilobytes(cwnd)} ${kb}`),
    },
  );
  return rows;
}

/** The overlay itself. Drawn only while it is switched on. */
export function statsOverlay(locale: Locale, rows: readonly StatsRow[]): TemplateResult {
  return html`<section class="view-stats" aria-label=${t(locale, 'stats.title')}>
    <h2 class="view-stats-title">${t(locale, 'stats.title')}</h2>
    <dl class="view-stats-rows">
      ${rows.map((row) => html`<dt>${t(locale, row.label)}</dt><dd>${row.value}</dd>`)}
    </dl>
  </section>`;
}
