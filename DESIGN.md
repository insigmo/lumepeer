---
name: Lumepeer
description: Peer-to-peer remote desktop that sits on the desk like a household appliance, in light and in dark.
colors:
  accent-key: "#f2a31b"
  accent-key-hover: "#e6950e"
  accent-key-hover-dark: "#f7b23a"
  accent-text: "#8a5a00"
  accent-text-dark: "#f5b544"
  accent-soft: "#fcf0d6"
  accent-soft-strong: "#f8e2b0"
  on-accent: "#1c1c1e"
  live-green: "#3fa34d"
  live-green-dark: "#4cbb5b"
  live-green-text: "#23702f"
  stop-red: "#b8321f"
  stop-red-dark: "#ff6b5b"
  danger-solid: "#c8372a"
  warning-amber: "#8f5b00"
  warning-amber-dot: "#e8a21c"
  warning-amber-dark: "#f2b441"
  satin-body: "#f4f4f2"
  satin-body-dark: "#1b1b1c"
  side-panel: "#ececea"
  side-panel-dark: "#151516"
  pressed-surface: "#e5e5e1"
  pressed-surface-dark: "#262628"
  faceplate: "#ffffff"
  faceplate-dark: "#232325"
  group-plate: "#fafaf8"
  control-dark: "#2a2a2d"
  hairline: "#e0e0db"
  hairline-dark: "#2e2e31"
  hairline-strong: "#c8c8c2"
  hairline-strong-dark: "#3f3f43"
  ink: "#1c1c1e"
  ink-dark: "#ededea"
  ink-2: "#4d4d48"
  ink-2-dark: "#b5b5af"
  ink-3: "#66665f"
  ink-3-dark: "#92928c"
  ink-disabled: "#a3a39d"
  ink-disabled-dark: "#5f5f5a"
  readout-glass: "#232325"
  readout-glass-dark: "#0f0f10"
  readout-text: "#f1f1ec"
  readout-dim: "#a5a59f"
  readout-dim-dark: "#92928c"
typography:
  display:
    fontFamily: "'Segoe UI Variable Text', 'Segoe UI', -apple-system, BlinkMacSystemFont, system-ui, 'Helvetica Neue', Arial, sans-serif"
    fontSize: "22px"
    fontWeight: 700
    letterSpacing: "-0.015em"
  title:
    fontFamily: "'Segoe UI Variable Text', 'Segoe UI', -apple-system, BlinkMacSystemFont, system-ui, 'Helvetica Neue', Arial, sans-serif"
    fontSize: "16px"
    fontWeight: 700
    letterSpacing: "-0.01em"
  headline:
    fontFamily: "'Segoe UI Variable Text', 'Segoe UI', -apple-system, BlinkMacSystemFont, system-ui, 'Helvetica Neue', Arial, sans-serif"
    fontSize: "15px"
    fontWeight: 700
  body:
    fontFamily: "'Segoe UI Variable Text', 'Segoe UI', -apple-system, BlinkMacSystemFont, system-ui, 'Helvetica Neue', Arial, sans-serif"
    fontSize: "13px"
    fontWeight: 400
    lineHeight: 1.5
  label:
    fontFamily: "'Segoe UI Variable Text', 'Segoe UI', -apple-system, BlinkMacSystemFont, system-ui, 'Helvetica Neue', Arial, sans-serif"
    fontSize: "12px"
    fontWeight: 500
    letterSpacing: "0.01em"
  readout:
    fontFamily: "ui-monospace, 'Cascadia Mono', 'SF Mono', Menlo, Consolas, monospace"
    fontSize: "14px"
    fontWeight: 500
    letterSpacing: "0.03em"
rounded:
  sm: "6px"
  md: "10px"
  lg: "14px"
  xl: "18px"
  key: "999px"
spacing:
  xs: "6px"
  sm: "8px"
  md: "12px"
  lg: "18px"
  xl: "26px"
  panel: "36px"
components:
  button-key:
    backgroundColor: "{colors.faceplate}"
    textColor: "{colors.ink}"
    rounded: "{rounded.key}"
    padding: "6px 16px"
    height: "32px"
  button-key-hover:
    backgroundColor: "{colors.pressed-surface}"
  button-primary:
    backgroundColor: "{colors.accent-key}"
    textColor: "{colors.on-accent}"
    rounded: "{rounded.key}"
    padding: "6px 16px"
    height: "32px"
  button-primary-hover:
    backgroundColor: "{colors.accent-key-hover}"
  button-primary-disabled:
    backgroundColor: "{colors.pressed-surface}"
    textColor: "{colors.ink-disabled}"
  button-connect:
    backgroundColor: "{colors.accent-key}"
    textColor: "{colors.on-accent}"
    rounded: "{rounded.key}"
    padding: "11px 24px"
    height: "44px"
  input-field:
    backgroundColor: "{colors.faceplate}"
    textColor: "{colors.ink}"
    rounded: "{rounded.md}"
    height: "32px"
  input-connect:
    backgroundColor: "{colors.faceplate}"
    textColor: "{colors.ink}"
    rounded: "{rounded.lg}"
    padding: "11px 16px"
    height: "44px"
  readout:
    backgroundColor: "{colors.readout-glass}"
    textColor: "{colors.readout-text}"
    typography: "{typography.readout}"
    rounded: "{rounded.md}"
    padding: "12px 13px"
  faceplate-card:
    backgroundColor: "{colors.faceplate}"
    rounded: "{rounded.lg}"
  status-plate:
    backgroundColor: "{colors.faceplate}"
    textColor: "{colors.ink}"
    rounded: "{rounded.md}"
    padding: "9px 12px"
  legend:
    textColor: "{colors.ink-3}"
    typography: "{typography.label}"
  settings-tab-active:
    backgroundColor: "{colors.accent-soft}"
    textColor: "{colors.accent-text}"
    rounded: "8px"
    padding: "8px 10px"
---

# Design System: Lumepeer

## Overview

**Creative North Star: "The Appliance on the Desk"**

Lumepeer is built like a well-made household appliance: a satin body, faceplates moulded one step lighter, a few round-ended keys each with a printed legend, a dark readout window for the values that are live, and on-lights that say a machine's state before any word does. It refuses the dark-navy admin dashboard with a glowing blue accent that remote-access tools ship by default. The same body, keys and lamps appear in all six windows (main, view, host bar, agent bar, file manager, terminal), in a light and a dark theme that every window follows.

Density is moderate and quiet. Type is the platform's own UI face at small sizes (13px body), so a Mac looks like a Mac and an installed build loads no fonts. Chrome stays small because the windows frame someone else's screen or sit over someone else's work; the remote picture itself is never recoloured. Colour appears only where it signals: yellow for the press target, green for live, red for stop, amber for a warning.

The build lands the world unevenly in two places, and this file records the build as shipped. Saved computers are faceplate cards with a readout screen, not compact rows. The readout window appears in the main window (invite code, card screens) and the view window's stats overlay; the host bar, file manager and terminal carry the yellow key and the lamps but no readout.

**Key Characteristics:**
- Satin body, faceplates one step lighter, 1px hairlines; moulded shadows with a hard 1px lower edge.
- Every button is a round-ended key (999px).
- The readout: charcoal glass, dark in both themes, inset-shadowed, for live values.
- On-lights: 8px lamps with a 3px soft halo when live, plain when merely present, a hollow ring when offline.
- Small lowercase legends above the controls they name; tabular figures for times and numbers.
- One token file (`apps/desktop/src/tokens.css`) holds every colour for both themes.

## Colors

A warm-neutral grey body with four signal colours; nothing in the palette is decorative.

### Primary
- **Key Yellow** (accent-key): fills the press target of a window, which is the Connect key, Create invite code, a dialog's "yes" (full control, terminal), Upload/Download in the file manager, the chat Send key. It also lights a switch that is on and fills progress bars. The same yellow in both themes, always with charcoal text (on-accent).
- **Key Yellow, pressed** (accent-key-hover / accent-key-hover-dark): hover state of every yellow key; darker in light, lighter in dark.
- **Amber Ink** (accent-text / accent-text-dark): yellow said as text. Used for caret colour, outgoing chat lines, the active settings section, "tunnel carrying traffic", and hotkey names.
- **Yellow Wash** (accent-soft, accent-soft-strong; 14% and 24% alpha in dark): the tint for selected-but-not-pressed: the active settings tab, the update chip, an active toolbar glyph, file selection.

### Secondary
- **Live Green** (live-green / live-green-dark, text live-green-text): a running session's lamp and the ring round its card screen, a reachable host's dot, a direct connection path, a granted permission, a running shell.
- **Stop Red** (stop-red / stop-red-dark): refusal, errors, destructive menu items, the Deny answer, a not-ready status lamp.
- **Signal Red** (danger-solid): solid red bars that must never go pale, the agent bar ("remote access is active") and the recording strip. The same red in both themes.

### Tertiary
- **Warning Amber** (warning-amber, warning-amber-dot / warning-amber-dark): a pending session, a relay path, an unread chat mark, a permission not yet granted, a shell being asked for.

### Neutral
- **Satin Body** (satin-body / satin-body-dark): the window ground and the main panel.
- **Side Panel** (side-panel / side-panel-dark): the main window's sidebar and the settings section column, one step back from the body.
- **Pressed Surface** (pressed-surface / pressed-surface-dark): hover on bordered keys, a disabled yellow key, the segmented control's well.
- **Faceplate** (faceplate / faceplate-dark, controls control-dark): cards, menus, dialogs, the status plate, the host bar, form fields.
- **Group Plate** (group-plate): settings cards and quality detail panels; in dark it equals pressed-surface-dark.
- **Hairline / Strong Hairline** (hairline, hairline-strong and their dark keys): 1px card and divider edges; strong edges on keys and fields.
- **Ink, Ink 2, Ink 3, Disabled** (ink to ink-disabled and their dark keys): primary text, secondary text, legends and quiet glyphs, unavailable controls.
- **Readout Glass** (readout-glass / readout-glass-dark, text readout-text, dim readout-dim): the readout window. Dark in both themes, deeper still in dark.

### Named Rules
**The Single Source Rule.** Every colour comes from `tokens.css`, with a light value on `:root` and a dark value under both `prefers-color-scheme: dark` and `[data-theme='dark']`. A colour written anywhere else will be wrong in one of the two themes. The exceptions are the platform close red (#e81123) on the window close button, white knobs and halos on solid red or yellow, and the terminal's ANSI palettes in `terminal.ts`, which mirror the tokens because xterm.js cannot read CSS.

**The Grey When Unlit Rule.** A disabled yellow key turns grey (pressed-surface fill, disabled ink), never a paler yellow, so a solid yellow fill always means a key you can press.

**The Ink Focus Rule.** Focus outlines and the field focus edge are drawn in ink (`--focus`: charcoal in light, near-white in dark), 2px with a 2px offset. A field adds a 3px yellow ring (accent-ring) outside the ink edge. Yellow alone is too light against the body to read as an outline.

**The Signal Not Chrome Rule.** The agent bar and the recording strip are solid red in both themes. A signal that turns pale in the light theme is one somebody stops seeing.

## Typography

**Display Font:** the platform UI sans: Segoe UI Variable Text / Segoe UI on Windows, SF Pro via -apple-system on macOS, system-ui on Linux (Helvetica Neue, Arial fallback)
**Body Font:** the same stack
**Label/Mono Font:** ui-monospace, Cascadia Mono, SF Mono, Menlo, Consolas (readouts, hotkeys, the terminal)

**Character:** Native and small, like the printing on an appliance's faceplate. Hierarchy comes from weight (500 to 700) and a short four-step scale, not from a distinctive face.

### Hierarchy
- **Display** (700, 22px, -0.015em): the main panel heading ("Connect to device"). Once per window.
- **Title** (700, 16px, -0.01em): sheet headings, the settings screen and the macOS permissions sheet.
- **Headline** (700, 15px): section headings (Connections), the brand name, dialog headings, empty-state titles. Settings cards use a 14px/650 variant.
- **Body** (400 to 600, 13px, 1.5 line-height for prose): all running text, keys and menu items (600 on keys). Prose is capped at 44 to 62ch.
- **Label** (500, 12px, 0.01em, lowercase): legends above controls ("your invite code"), the host bar's "guests" heading (11px there), and the view toolbar's popover headings. Also used for notes and meta lines, in sentence case.
- **Readout** (mono 500, 14px, 0.03em): the invite code in its window; the stats overlay and card screen captions use the UI face at 12px with tabular figures.

### Named Rules
**The Printed Legend Rule.** A legend sits above the control it names, small (12px), weight 500, ink 3, lowercased by CSS. The copy keeps its own case in every locale; lowercase is only the style.

**The Tabular Figures Rule.** Times, versions, sizes, transfer speeds and stats are set in `font-variant-numeric: tabular-nums`, so a live value never jitters its column.

## Layout

The main window is one screen with no navigation: a 236px sidebar (logo, invite legend and readout, copy key, update chip and status plate at its foot, version and gear) beside a main panel padded 6px 36px 36px holding the connect row (max 720px) and a grid of saved computers (`repeat(auto-fill, minmax(180px, 1fr))`, 14px gap). Settings is a modal sheet, 800 by 620px at most, with a 208px section column that lies down as a horizontal row below 640px. The file manager's two panes stack below 900px. The view window gives the remote picture the whole window; the toolbar floats over it as one plate, and fades to 8% opacity in full screen until hovered or focused.

The spacing rhythm is observed in the build rather than tokenized: 6 and 8px inside controls and between glyphs, 10 to 14px between related items and inside cards, 18px under a paragraph, 22 to 26px around dividers and settings sections, and 36px for the main panel's padding. Every layout uses logical properties so Arabic mirrors from `dir` alone, and layouts must survive long German and Russian strings: card status lines wrap to two lines rather than truncating.

## Elevation & Depth

The system is moulded, not floating. Surfaces step lighter toward the viewer (side panel, body, faceplate), and shadows add a hard 1px lower edge before any soft fall, like a plastic part pressed from a mould. In dark, the small shadow is the hard edge alone. The readout reverses the moulding: it is sunk into the body with an inset shadow.

### Shadow Vocabulary
- **Moulded edge** (`--shadow-sm`, light `0 1px 0 rgb(28 28 30 / 7%), 0 1px 2px rgb(28 28 30 / 5%)`): resting keys, cards, fields, the status plate.
- **Lifted** (`--shadow-md`, light `0 1px 0 rgb(28 28 30 / 6%), 0 10px 24px -10px rgb(28 28 30 / 22%)`): a hovered card, menus, the view toolbar and banner, the chat drawer.
- **Sheet** (`--shadow-lg`, light `0 28px 56px -18px rgb(28 28 30 / 32%), 0 0 0 1px rgb(28 28 30 / 6%)`): modal dialogs and the settings sheet, over a scrim.
- **Readout inset** (`--readout-inset`, light `inset 0 1px 3px rgb(0 0 0 / 45%), 0 1px 0 rgb(255 255 255 / 80%)`): the readout window and card screens.
- **Key lip** (`inset 0 -2px 0 rgb(0 0 0 / 12%)` plus the moulded edge): the Connect key only; pressing removes it and drops the key 1px.

### Named Rules
**The Sunk Readout Rule.** A live value sits in a window set into the body, with an inset shadow. Nothing else uses an inset.

## Shapes

There are two shape families. Every key is round-ended (999px): buttons, toolbar glyph keys, the segmented control, chips, pills, the view banner and the toolbar plate itself. Plates and windows use soft rectangles: 10px for fields, the readout, the status plate and menus, 14px for cards, the connect field and settings cards, 18px for dialogs and sheets. Inner radii step down (a card's 10px screen inside its 14px plate, 7px thumbnails, 6px menu items). Lamps are circles. Edges are 1px hairlines; a strong hairline marks anything you can press or type into.

**The Round Key Rule.** If it presses, its ends are round. A rectangular button does not exist in this system.

## Components

### Buttons
Plain keys with printed words, round-ended, weight 600.
- **Shape:** round-ended (999px), 32px tall at the default size, 34px in the sidebar, 40px in consent dialogs, 44px for the Connect key.
- **Primary:** yellow fill and border, charcoal text. One per block: Connect, Create invite code, a dialog's "yes", Upload/Download.
- **Default:** faceplate fill, strong hairline, ink text, moulded edge; hover drops to the pressed surface.
- **Connect key:** yellow with a moulded lower lip; on press it moves down 1px and the lip becomes an inner shadow. Its minimum width (9ch) holds both "Connect" and "Cancel" so the swap never resizes it.
- **Destructive answer:** red border and red text on the faceplate, red wash on hover.
- **Icon keys:** 26 to 32px round glyph keys (gear, refresh, three dots, close), transparent until hovered.
- **Disabled:** grey fill, disabled ink, no shadow.

### Chips
- **Quality pill:** round-ended, group plate, hairline, 12px text, a 7px dot coloured by path (green direct, amber relay or mixed); it opens onto a measured-values panel.
- **Update chip:** yellow wash, amber ink, 10px radius, above the status plate. It is news, not an alarm.
- **Clipboard note:** a hairline pill saying only that a clipboard synced.

### Cards / Containers
- **Corner Style:** 14px for saved-computer cards, 10px for their inset screens.
- **Background:** faceplate, with a readout-glass screen holding the machine's last picture (or a dimmed monitor glyph) and a two-line status line: what the guest may do on the left, time on the right in tabular figures.
- **Shadow Strategy:** moulded edge at rest, lifted on hover; a live session's screen gains an inset 1.5px green ring.
- **Border:** 1px hairline, strong on hover.
- **Internal Padding:** 6px around the screen, 9px 10px 9px 12px for the foot (lamp, OS icon, name, three-dot menu).
- **Settings cards:** group plate, hairline, 14px radius, 16px 18px padding, rows ruled apart with 1px hairlines.

### Inputs / Fields
- **Style:** faceplate fill, strong hairline, 10px radius, 32px minimum. The connect field is 44px with a 14px radius and the moulded edge. Select boxes draw their own chevron from two gradients in ink 3.
- **Focus:** the border turns ink and a 3px yellow ring (accent-ring) appears outside it.
- **Switches:** 36 by 20px track, grey when off, yellow when on, with a white 16px knob.
- **Segmented control:** a round-ended well in the pressed surface; the chosen segment is a raised faceplate with the moulded edge.

### Navigation
There is no app navigation. Settings sections are a vertical column of 13px items on the side panel: ink 2 at rest, a hover wash, and a yellow wash with amber ink and weight 600 when active. Below 640px the column becomes a horizontal scroller.

### The Readout (signature)
A charcoal window, dark in both themes, sunk with an inset shadow. It holds the invite code as one clipped line in mono (the whole code is on hover and on the clipboard, never wrapped), each saved computer's screen, and the view window's statistics overlay.

### On-lights (signature)
8px circular lamps. Live has a solid green lamp with a 3px soft halo. Pending has an amber lamp with a halo. A reachable host gets a plain green lamp with no halo. Offline is a hollow 1.5px ring in ink 3, an unlit lamp rather than an alarm. The terminal's state lamp uses the same rules and pulses amber while the shell is being asked for, unless reduced motion is set.

### View toolbar
A single round-ended plate of 32px glyph keys at 94% faceplate opacity over the picture, with a hairline and the lifted shadow. Active glyphs take the strong yellow wash. Glyphs are drawn SVG.

## Do's and Don'ts

### Do:
- **Do** take every colour from `tokens.css` and give it a value in both themes.
- **Do** make every button round-ended (`--radius-key`, 999px).
- **Do** put a live value (a code, a machine's screen, stats) in the readout window, dark in both themes, with tabular figures.
- **Do** keep a solid yellow fill for something the person can press or switch on, with charcoal text; one yellow key per block.
- **Do** say state with a lamp first and words second: green live, amber pending or warning, red stop, hollow ring offline.
- **Do** draw focus in ink (2px outline, 2px offset), and add the 3px yellow ring on fields.
- **Do** write legends small, weight 500, ink 3, lowercased by CSS, above the control they name.
- **Do** use the platform's UI face; installed builds load no fonts.

### Don't:
- **Don't** ship a dark-navy dashboard with a glowing blue accent; blue is not in this palette outside the logomark and the terminal's ANSI colours.
- **Don't** show a disabled yellow key as a paler yellow; it goes grey.
- **Don't** draw a focus outline in the key's yellow alone.
- **Don't** let the agent bar or the recording strip follow the theme into a pale colour.
- **Don't** recolour the remote picture or lay anything over it but the toolbar plate, banners, and the stats readout.
- **Don't** show an offline host in red; it is an unlit ring.
- **Don't** wrap or shorten the invite code in its readout; show one clipped line and copy it whole.
