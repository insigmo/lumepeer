// Lumepeer logomark (PRODUCT.md Brand Commitments): rounded-square tile,
// dark navy fill (#0F1724), abstract blue-gradient "L" glyph with a small
// violet accent triangle at its foot and a solid blue dot at bottom-right.
//
// Inline SVG rather than a shipped raster: the confirmed asset file itself
// was never handed to this repo (only its description and pixel-picked
// palette), and an inline mark stays crisp at every size CSS puts it at.

import { svg, type SVGTemplateResult } from 'lit-html';

export function logoMark(): SVGTemplateResult {
  return svg`
    <svg class="logo-icon" viewBox="0 0 32 32" role="img" aria-hidden="true">
      <defs>
        <linearGradient id="lumepeer-l-gradient" x1="10" y1="7" x2="23" y2="24" gradientUnits="userSpaceOnUse">
          <stop offset="0" stop-color="#6C8CF5" />
          <stop offset="1" stop-color="#3564E4" />
        </linearGradient>
      </defs>
      <rect width="32" height="32" rx="7" fill="#0F1724" />
      <path d="M12 7.5h3.4v13.6h7.4v3.4H12z" fill="url(#lumepeer-l-gradient)" />
      <path d="M12 21.1h4.6L12 25.5z" fill="#7C5CFC" />
      <circle cx="23" cy="23.5" r="2.6" fill="#3564E4" />
    </svg>
  `;
}
