// Lumepeer logomark (PRODUCT.md Brand Commitments; ADR 0170): two peers, and
// the light where they meet. Two rings overlap on the dark readout-glass tile
// and the lens they share is lit in the accent key's amber — two machines and
// nothing between them, which is what serverless means here.
//
// The same drawing as `src-tauri/icons/icon.svg`, the master every app,
// installer and tray icon is rendered from; change the two together. Inline
// rather than a shipped raster so it stays crisp at every size CSS puts it at.

import { svg, type SVGTemplateResult } from 'lit-html';

export function logoMark(): SVGTemplateResult {
  return svg`
    <svg class="logo-icon" viewBox="0 0 32 32" role="img" aria-hidden="true">
      <rect width="32" height="32" rx="7" fill="#232325" />
      <path d="M16 9.77A7 7 0 0 1 16 22.23A7 7 0 0 1 16 9.77Z" fill="#F2A31B" />
      <g fill="none" stroke="#F1F1EC" stroke-width="2.2">
        <circle cx="12.8" cy="16" r="7" />
        <circle cx="19.2" cy="16" r="7" />
      </g>
    </svg>
  `;
}
