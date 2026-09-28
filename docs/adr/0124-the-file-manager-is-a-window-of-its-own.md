# ADR 0124 — The file manager is a window of its own

Status: accepted
Date: 2026-09-28

Replaces the file panel of
[ADR 0075](0075-listing-a-directory-is-its-own-grant.md) and
[ADR 0076](0076-naming-a-file-on-the-host-is-browse-plus-transfer.md) with a
two-pane file manager, and extends
[ADR 0101](0101-a-terminal-session-is-the-same-session-with-no-media-connection.md)
with a third thing a session can be.

## Report

Moving files between the two machines was a small panel laid over the remote
picture: one selected entry at a time, a Download and an Upload button, no
way to make a folder, rename or delete, and it hid the very part of the
screen somebody was copying from. People expected what the comparable tools
have — this computer on one side, the remote one on the other, drag between
them — and a way to open it from a remembered host without watching its
screen.

## Decision

### 1. A window, not a panel

The toolbar's Files button opens a window labelled `files-{peer}` beside the
view (`files_window_open`), or raises the one already open. It loads
`files.html`: two panes — this computer, the remote one — and a transfer list
along the bottom. Files move by dragging a row onto the other pane (or onto a
folder in it), by Ctrl+C in one pane and Ctrl+V in the other, by the Upload
and Download buttons, by Enter on a file, or by dropping files from the
desktop onto the remote pane. Each pane can make a folder (F7), rename (F2)
and delete (Del, after a second confirmation).

Rows move between panes with pointer events, not HTML drag and drop: native
drag and drop stays on so desktop files arrive with their paths, and on
Windows that handler swallows the page's own drag events.

The window closes with its view (`TauriViewWindows::close` destroys it) and is
never hidden on close: hidden, it would keep polling a session nobody can see.

### 2. "File manager" on a remembered host

The host card's menu in the main window gets **File manager**
(`history_connect` with `files_only`). It dials the same session with the same
role and no media connection, exactly as ADR 0101's terminal does, and the
view window loads `files.html` instead of the picture; closing it ends the
session. When that host is already connected, no second dial is made: the
file manager opens beside the running session.

Unlike the terminal item, it is not tied to full control: browsing is a grant
of its own (ADR 0075) the host can give any role, and a host that did not give
it is told in the window itself.

### 3. Three changes on the wire (protocol minor 20)

`FileOpRequest { id, op }` guest to host, `FileOpResult { id, refused }` back.
`op` is `MakeDir`, `Rename` (a new basename in the same directory) or
`Delete` (a directory with everything in it). The host acts only for a guest
holding both `file_browse` and `file_transfer`, re-read when the request
lands, runs at most `MAX_FILE_OPS_IN_FLIGHT` (4) at once and answers `Busy`
past that. A root, a relative or climbing path, a device name or a name with a
separator in it is `BadPath` before anything touches the disk.

The local pane's changes (`local_file_op`) run through the same plan
(`FileOpPlan`) the host runs, so the two panes refuse the same things.

### 4. The window keeps its own queue

The actor moves one file per request and says nothing about an upload that
went nowhere. The window therefore queues every file of a drag and feeds the
actor one job at a time, and every job ends in "done" or in a reason. The host
now counts refused downloads and uploads per session (`fetches_refused`,
`uploads_refused` on `remote_dir_status`) so a job can tell its own refusal
from the one before.

## Consequences

- A host older than minor 20 lists and transfers as before; its file manager
  says it cannot make folders, rename or delete (`PEER_TOO_OLD`).
- The `files-*` window has its own capability (`capabilities/files.json`):
  the file commands and nothing else — no screen, input, shell, tunnel or
  settings command.
