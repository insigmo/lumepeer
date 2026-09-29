// How a peer is named on screen (ADR 0121): the operating system's icon and
// the machine's own name, with the pseudonymous label one hover away.
//
// The name is what the peer said about itself and nothing more. Every command
// the webview sends still names the peer by its label, so this module only
// ever draws — and a peer too old to say its name is drawn by its label, the
// way every peer was before.

import { html, type TemplateResult } from 'lit-html';

import type { Locale } from './i18n';
import { t } from './i18n';

/** The two fields a session row or a remembered host carries (ADR 0121). */
export interface PeerDeviceFields {
  peer_label: string;
  /** What the peer said its machine is called; absent from an older peer. */
  device_name?: string | null;
  /** Its operating-system tag: `windows`, `macos`, `linux`, or another. */
  device_os?: string | null;
}

/** Product names, and so the same in every locale. */
const OS_NAME: Partial<Record<string, string>> = {
  windows: 'Windows',
  macos: 'macOS',
  linux: 'Linux',
};

const WINDOWS_ICON = html`<svg viewBox="0 0 16 16" aria-hidden="true">
  <path d="M2 2h5.4v5.4H2zM8.6 2H14v5.4H8.6zM2 8.6h5.4V14H2zM8.6 8.6H14V14H8.6z" fill="currentColor" />
</svg>`;

// The apple from Bootstrap Icons (MIT), bi-apple.
const MACOS_ICON = html`<svg viewBox="0 0 16 16" aria-hidden="true">
  <path
    fill="currentColor"
    d="M11.182.008C11.148-.03 9.923.023 8.857 1.18c-1.066 1.156-.902 2.482-.878 2.516s1.52.087 2.475-1.258.762-2.391.728-2.43m3.314 11.733c-.048-.096-2.325-1.234-2.113-3.422s1.675-2.789 1.698-2.854-.597-.79-1.254-1.157a3.7 3.7 0 0 0-1.563-.434c-.108-.003-.483-.095-1.254.116-.508.139-1.653.589-1.968.607-.316.018-1.256-.522-2.267-.665-.647-.125-1.333.131-1.824.328-.49.196-1.422.754-2.074 2.237-.652 1.482-.311 3.83-.067 4.56s.625 1.924 1.273 2.796c.576.984 1.34 1.667 1.659 1.899s1.219.386 1.843.067c.502-.308 1.408-.485 1.766-.472.357.013 1.061.154 1.782.539.571.197 1.111.115 1.652-.105.541-.221 1.324-1.059 2.238-2.758q.52-1.185.473-1.282"
  />
</svg>`;

// A penguin: body with the belly and the eyes cut out of it.
const LINUX_ICON = html`<svg viewBox="0 0 16 16" aria-hidden="true">
  <path
    fill="currentColor"
    fill-rule="evenodd"
    d="M8 1c-1.9 0-3.1 1.5-3.1 3.5 0 1 .2 1.6-.4 2.6C3.7 8.4 2.9 9.8 2.9 11.3c0 .9.3 1.6.8 2.1L3 15h10l-.7-1.6c.5-.5.8-1.2.8-2.1 0-1.5-.8-2.9-1.6-4.2-.6-1-.4-1.6-.4-2.6C11.1 2.5 9.9 1 8 1Zm0 6.4c-1.4 0-2.4 1.6-2.4 3.6S6.6 14 8 14s2.4-1 2.4-3-1-3.6-2.4-3.6ZM6.9 3.6a.6.6 0 1 0 0 1.2.6.6 0 0 0 0-1.2Zm2.2 0a.6.6 0 1 0 0 1.2.6.6 0 0 0 0-1.2Z"
  />
</svg>`;

const OTHER_ICON = html`<svg viewBox="0 0 16 16" aria-hidden="true">
  <rect x="1.7" y="2.6" width="12.6" height="8.8" rx="1.2" stroke="currentColor" stroke-width="1.3" fill="none" />
  <path d="M5.5 13.6h5M8 11.4v2.2" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" />
</svg>`;

const OS_ICON: Partial<Record<string, TemplateResult>> = {
  windows: WINDOWS_ICON,
  macos: MACOS_ICON,
  linux: LINUX_ICON,
};

/** The name to show for a peer: its machine name, or its label without one. */
export function peerDisplayName(peer: PeerDeviceFields): string {
  return peer.device_name || peer.peer_label;
}

/**
 * The operating system's icon and the machine's name, in one line that
 * ellipsizes as a whole. The label is the tooltip, so the one fact the name
 * cannot vouch for — which peer this actually is — is still there to read.
 */
export function peerName(peer: PeerDeviceFields, locale: Locale): TemplateResult {
  const os = peer.device_os ?? '';
  const osName = OS_NAME[os];
  // An operating system this build knows is named to a screen reader; the
  // generic screen says nothing a sighted reader would not also have to guess.
  const icon = osName
    ? html`<span class="peer-os" data-os=${os} role="img" aria-label=${osName}>${OS_ICON[os]}</span>`
    : html`<span class="peer-os" data-os="other" aria-hidden="true">${OTHER_ICON}</span>`;
  return html`
    <span class="peer-name" data-testid="peer-name" title=${t(locale, 'connections.peerId', peer.peer_label)}>
      ${icon}<span class="peer-label">${peerDisplayName(peer)}</span>
    </span>
  `;
}
