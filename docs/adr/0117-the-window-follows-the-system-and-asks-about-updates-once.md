# ADR 0117 — The window follows the system's light or dark, and asks about updates once at launch

Status: accepted
Date: 2026-09-26

Amends decision 3 of [ADR 0042](0042-updates-have-somewhere-to-come-from.md).

## Context

The main window had one palette, light, written as literal colours across
`app.css`. A remote-access client is often open for hours next to other tools,
at night as much as by day, and every other window on a dark desktop made it
the brightest thing on the screen.

It also never said which build it was. The version lived in the bundle and in
the release notes, and the update check sat at the bottom of a settings card,
behind a press, on a tab nobody had a reason to open. A client two releases
behind looked exactly like a current one until something failed in a way the
newer release had fixed.

## Decisions

### 1. Two palettes, one set of tokens, and "system" is the default

Every colour in `app.css` is a custom property. The light set is on `:root`;
the dark set applies under `prefers-color-scheme: dark`, and `data-theme` on
`<html>` overrides the system either way. "System" is the *absence* of the
attribute, so an OS that switches to dark at dusk takes the window with it
without a listener in `theme.ts`.

The choice (System / Light / Dark) is a window preference, not machine state,
so it lives in `localStorage` next to the language choice rather than behind
an IPC command. The theme applies before the first render.

Only the main window changes. The view window was dark already — it frames
somebody else's screen, and a light frame around a dark desktop is the wrong
way round — and the two bars sit over other applications on purpose.

### 2. The version is on screen

The sidebar footer shows the running version (`getVersion()`, already allowed
by `core:default`), and pressing it opens the settings screen on About, which
holds the version, the update check and the install.

### 3. One check at launch, still never an install

This amends ADR 0042's "checking is a press". The main window asks the
configured channel once when it starts; if a newer release exists, the sidebar
says so and links to About. Nothing is downloaded and nothing is installed:
installing stays a second, separate press, for the reason ADR 0042 gave — this
process can be carrying someone else's session, and only a person may decide
to end it.

What changed is the cost of *not* asking. The check reads `latest.json` from
the same place a manual press does and reveals nothing a manual press does not;
a failure at launch (offline, no channel configured) is logged and not shown,
because nobody asked. A switch on About turns the launch check off for anyone
who wants the old behaviour.

A build with no update channel (`UPDATE_OFF`) now says that, instead of
reporting a failed update that no retry could fix.

### 4. The settings screen is four sections

General (theme, language, autostart), Access (unattended access, saved
devices, invite revocation), Recordings & log, About. The saved devices moved
from the first tab to Access: which devices are trusted and what a trusted
device must still prove are one subject, and they now sit together.

## Consequences

- A colour written as a literal in `app.css` is a bug in one of the two themes.
  The only literals left are deliberate: the platform's close-button red, and
  white on the solid red of the reboot banner.
- Every launch makes one HTTPS request to the update channel unless the switch
  is off. It carries nothing but what GitHub (or the operator's own
  `manifest_base_url`) sees from any download.
- On Windows the updater exits the process to run the installer, so "Install"
  now warns that it may close the app and end a session in progress.
