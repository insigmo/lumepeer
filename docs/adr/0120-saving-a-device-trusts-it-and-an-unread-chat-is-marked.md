# ADR 0120 — Saving a device trusts it, and an unread chat is marked

Status: accepted
Date: 2026-09-27

Two reports from real use, and one change for each. Amends
[ADR 0034](0034-the-address-book-is-the-gate-on-unattended-access.md) §2
and extends [ADR 0023](0023-phase7-catchup-decisions.md)'s chat and
[ADR 0055](0055-an-always-on-top-session-bar-for-the-host.md)'s session bar.

## 1. Saving a device trusts it

**Report.** Letting a guest sign in with the device password took three
steps on the host: save the device from its session, go to the address book,
tick "Trusted" and pass the confirmation. Only then did the password work.
Nobody expected the third step; a saved device that still could not use the
password read as broken.

**Decision.** "Save this device" saves the entry and then trusts it. The host
pressing that button on a live session is the host naming the device in
advance, which is everything trust has meant since ADR 0034 §1: it narrows
who may *try* the password, and a trusted device still needs the password and,
when one is set, the one-time code.

What does not change:

- Trust still moves only through `address_book_set_trusted`. The desktop
  calls it right after `address_book_upsert`, so the change is audited as
  `DeviceTrustChanged` exactly as a tick on the panel is, and a save the core
  refused trusts nothing.
- `address_book_upsert` itself still preserves whatever the flag was:
  renaming a device or adding a tag is never a path to a permission.
- Nothing about connecting, being admitted or finishing a session trusts a
  device. It takes the host's own click on the host's own main window.
- Withdrawing trust is one untick on the address book, at any time, as
  before. Ticking it back on still goes through the confirmation.

## 2. An unread chat is marked on both sides

**Report.** A message from the other side went unnoticed. The guest's
toolbar marked it with a small pale-blue dot that disappeared entirely when
the toolbar was collapsed. The host had no mark at all: the chat drawer only
reads the transcript while it is open, so a guest's message sat in the actor
until the host happened to open the drawer.

**Decision.**

- **Host.** Whether a guest wrote something the host has not opened yet is
  kept by the actor (`ChatLog::is_unread`; set by an incoming message,
  cleared by `chat_mark_read` and by the session ending) and reported on
  every `session_status` row as `chat_unread`. The actor holds it, not either
  window, because two surfaces show it and opening the drawer in one has to
  clear it in both:
  - the session card in the main window gets a chat button with a yellow dot,
    out on the card rather than inside the three-dot menu;
  - the session bar gets the same mark on the guest's row, and on the edge tab
    when the bar is collapsed. Pressing the mark calls `host_bar_open_chat`,
    which raises the main window and asks it (`lumepeer://open-chat`, to the
    main window only) to open the drawer on that guest. The bar still reads no
    chat of its own.

  The main window clears the mark when the drawer opens, and while the drawer
  stays open only when the window has focus: a drawer left open in a
  minimized window has shown nobody anything.
- **Guest.** The toolbar's dot is yellow, the same colour as the host's mark.
  Collapsing the toolbar no longer hides the chat button while a message is
  unread or the chat is open.

## Consequences

- One new flag on `session_status`, two new commands: `chat_mark_read` (main
  window only) and `host_bar_open_chat` (session bar only).
- The unread mark is per peer and says only that there is something to read,
  never how much or what. It goes away with the session, like the transcript.
