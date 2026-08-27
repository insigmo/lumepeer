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
  /** Region of the relay in use; never its address (§15). */
  relay_region: string | null;
  /** Encoder bitrate this machine is sending at, or null when watching. */
  bitrate_kbps: number | null;
  /** Frame rate this machine is sending at, or null when watching. */
  fps: number | null;
}

const PATH_KEY: Readonly<Record<PathKind, TranslationKey>> = {
  direct: 'quality.path.direct',
  relay: 'quality.path.relay',
  mixed: 'quality.path.mixed',
  unknown: 'quality.path.unknown',
};

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
    <details class="quality" data-testid="quality" data-path=${stats.path}>
      <summary class="quality-pill" data-testid="quality-pill">
        <span class="quality-dot" data-path=${stats.path} aria-hidden="true"></span>
        <span class="quality-path">${t(locale, PATH_KEY[stats.path])}</span>
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
        ${stats.relay_region
          ? detail(locale, 'quality.relayLabel', stats.relay_region)
          : html``}
      </div>
    </details>
  `;
}
