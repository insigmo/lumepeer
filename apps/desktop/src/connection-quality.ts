// Connection-quality pill of one live session (design doc §18; ADR 0026,
// ADR 0037).
//
// ADR 0026 is about a product that could not tell a user why a session was
// bad — it could only report what it had intended, never what was happening.
// This is the other half of that: everything here was measured on this
// machine, and a value nothing has measured yet reads as unknown rather than
// as a zero pretending to be a reading.
//
// A `<details>` element rather than a hand-rolled toggle: the summary is the
// pill, the disclosure is the detail, and both are keyboard-reachable and
// screen-reader-correct without this module holding any state. The panel
// re-renders every second, and lit-html reuses the element, so an open
// disclosure stays open across polls by itself.

import { html, type TemplateResult } from 'lit-html';

import type { Locale, TranslationKey } from './i18n';
import { t } from './i18n';

/** How a peer is actually reached, as the Rust side classifies iroh's paths. */
export type PathKind = 'direct' | 'relay' | 'mixed' | 'unknown';

/**
 * Which of the two transports of ADR 0080 carries the session (gap-tasks/23
 * task 3).
 *
 * Not the same question as the path, and the panel answers both: a path is
 * how iroh is reaching the peer *within* the iroh transport, and the
 * obfuscated transport has none to report because it is one direct UDP path
 * and never a relay.
 */
export type TransportKind = 'iroh' | 'obfuscated';

/** A video codec, as the Rust side names it (§11; ADR 0067). */
export type VideoCodec = 'h264' | 'av1' | 'vp9';

/** One transport the connect tried and gave up on before this session. */
export interface TransportFallback {
  transport: TransportKind;
  /** §18 code of the last error that transport returned. */
  failure: string;
}

/** One row of the `connection_stats` IPC call. */
export interface ConnectionStats {
  peer_label: string;
  /** Smoothed control-channel round trip, milliseconds. */
  rtt_ms: number | null;
  /** Frames the receiver could not turn into a picture, permille. */
  loss_permille: number | null;
  /** Media throughput the receiver observed, kilobits per second. */
  goodput_kbps: number | null;
  path: PathKind;
  /** Which transport carries this session (gap-tasks/23 task 3). */
  transport: TransportKind;
  /**
   * Transports the connect gave up on before this session existed, oldest
   * first; empty when the first one tried worked.
   */
  fallbacks: TransportFallback[];
  /** Region of the relay in use; never its address (§15). */
  relay_region: string | null;
  /** Encoder bitrate this machine is sending at, or null when watching. */
  bitrate_kbps: number | null;
  /** Frame rate this machine is sending at, or null when watching. */
  fps: number | null;
  /**
   * Codec the picture travels in, on either side of the session, or null
   * while no picture does (gap-tasks/06; ADR 0067).
   */
  codec: VideoCodec | null;
}

const PATH_KEY: Readonly<Record<PathKind, TranslationKey>> = {
  direct: 'quality.path.direct',
  relay: 'quality.path.relay',
  mixed: 'quality.path.mixed',
  unknown: 'quality.path.unknown',
};

/**
 * What each codec is called. Not localized: H.264, AV1 and VP9 are the
 * codecs' own names in every language, the way a relay region is shown as
 * the region's own code.
 */
const CODEC_NAME: Readonly<Record<VideoCodec, string>> = {
  h264: 'H.264',
  av1: 'AV1',
  vp9: 'VP9',
};

/** What each transport is called, for the row that says one was given up on. */
const TRANSPORT_KEY: Readonly<Record<TransportKind, TranslationKey>> = {
  iroh: 'quality.transport.iroh',
  obfuscated: 'quality.transport.obfuscated',
};

/**
 * What a transport that did not connect is said to have done, by the §18 code
 * the Rust side attached to it (the same vocabulary the connect form reads).
 *
 * Three phrases, all of them about what this machine observed: nothing
 * answered, or a connection that had come up went away. Nothing here guesses
 * at a *cause* — this app cannot tell a network that blocks a transport from
 * one that is merely bad, and a panel that named the difference would be
 * inventing it. An unrecognised code gets the neutral phrase rather than its
 * raw text.
 */
const FALLBACK_KEY: Readonly<Record<string, TranslationKey>> = {
  DIAL_FAILED: 'quality.fallback.noAnswer',
  TRANSPORT_LOST: 'quality.fallback.lost',
};

/**
 * The name under which the session's own transport and path are shown.
 *
 * One label rather than two: on the obfuscated transport the path is always
 * one direct UDP path (ADR 0052), so "obfuscated, direct" would be saying the
 * same thing twice, and iroh's own paths are the only ones there is a choice
 * between.
 */
function pathLabel(stats: ConnectionStats): TranslationKey {
  return stats.transport === 'obfuscated' ? 'quality.path.obfuscated' : PATH_KEY[stats.path];
}

/** Permille to whole percent, for a figure a person reads rather than sums. */
const PERMILLE_PER_PERCENT = 10;

/**
 * Formats a measured number, or the unknown marker when nothing measured it.
 *
 * The distinction is the whole point of the panel: "0 ms" and "not measured
 * yet" are different facts, and a diagnostics view that conflates them sends
 * someone to debug a reading that does not exist.
 */
function measured(value: number | null, locale: Locale, key: TranslationKey): string {
  return value === null ? t(locale, 'quality.unknown') : t(locale, key, String(value));
}

/** One label/value row of the disclosure. */
function detail(locale: Locale, label: TranslationKey, value: string): TemplateResult {
  return html`
    <div class="quality-row">
      <span class="quality-label">${t(locale, label)}</span>
      <span class="quality-value">${value}</span>
    </div>
  `;
}

/**
 * The pill for one session, or nothing at all when the actor has no row for
 * this peer — a session whose connection has already gone has no link to
 * describe, and an empty pill would only claim otherwise.
 */
export function connectionQuality(
  stats: ConnectionStats | undefined,
  locale: Locale,
): TemplateResult {
  if (!stats) {
    return html``;
  }
  const rtt = measured(stats.rtt_ms, locale, 'quality.ms');
  const loss =
    stats.loss_permille === null
      ? t(locale, 'quality.unknown')
      : t(locale, 'quality.percent', (stats.loss_permille / PERMILLE_PER_PERCENT).toFixed(1));
  return html`
    <details
      class="quality"
      data-testid="quality"
      data-path=${stats.path}
      data-transport=${stats.transport}
    >
      <summary class="quality-pill" data-testid="quality-pill">
        <span class="quality-dot" data-path=${stats.path} aria-hidden="true"></span>
        <span class="quality-path">${t(locale, pathLabel(stats))}</span>
        <span class="quality-sep" aria-hidden="true">·</span>
        <span class="quality-rtt">${rtt}</span>
      </summary>
      <div class="quality-details" data-testid="quality-details">
        ${detail(locale, 'quality.rttLabel', rtt)}
        ${detail(locale, 'quality.lossLabel', loss)}
        ${detail(
          locale,
          'quality.goodputLabel',
          measured(stats.goodput_kbps, locale, 'quality.kbps'),
        )}
        ${detail(
          locale,
          'quality.bitrateLabel',
          measured(stats.bitrate_kbps, locale, 'quality.kbps'),
        )}
        ${detail(locale, 'quality.fpsLabel', measured(stats.fps, locale, 'quality.fpsValue'))}
        ${stats.codec ? detail(locale, 'quality.codecLabel', CODEC_NAME[stats.codec]) : html``}
        ${stats.relay_region
          ? detail(locale, 'quality.relayLabel', stats.relay_region)
          : html``}
        ${stats.fallbacks.map((fallback) =>
          detail(
            locale,
            'quality.fallbackLabel',
            t(
              locale,
              FALLBACK_KEY[fallback.failure] ?? 'quality.fallback.failed',
              t(locale, TRANSPORT_KEY[fallback.transport]),
            ),
          ),
        )}
      </div>
    </details>
  `;
}
