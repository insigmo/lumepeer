# ADR 0034 — The address book is the gate on unattended access

Status: accepted
Date: 2026-08-27

## Context

`crates/core/src/address_book.rs` has shipped `AddressEntry`, `AddressBook`
and a per-`NodeId` `trusted` flag since the phase-7 catch-up (ADR 0023 §3),
with one reference to it in the whole repository: `pub mod address_book;`. Its
header cited an "ADR 0022" that was never written.

With ADR 0033 giving a host a way to admit a guest on credentials alone, the
question the book was built to answer finally has stakes: *who is allowed to
try*.

## Decisions

### 1. Trust narrows the set of devices allowed to attempt, and nothing else

`AddressBook::is_trusted(peer)` is a precondition of the credential path, not
a substitute for it. A trusted device still has to present the device password
and, when one is configured, the one-time code.

This is worth stating because the opposite reading is the obvious one, and it
is wrong in a way that matters: the lockout of §18 is a *shared* budget. If
any peer holding a valid invite could spend attempts against it, a stranger
could lock the owner's own devices out by failing five times. Trust is what
keeps the budget spendable only by machines the host named in advance.

Deny-by-default survives every degradation: a device absent from the book is
never trusted, a corrupt book file loads as an empty book, and a book that
cannot be persisted at all runs in memory and trusts nobody.

### 2. Trust is never earned by connecting

Nothing about a successful connection, an accepted invite or a completed
session sets the flag. The only thing that moves it is
`address_book_set_trusted`, callable from the host's own main window and
nowhere else, and the desktop UI puts a confirmation with the consequence
written out in front of turning it on.

Saving a device and trusting it are separate operations on purpose.
`address_book_upsert` preserves whatever the trust flag already was, so
renaming a device or adding a tag can never widen what it may do, and a device
saved from a live session lands untrusted.

The change is audited (`AuditEvent::DeviceTrustChanged`) for the same reason
`GrantChanged` is: it widens the host's own exposure. The device's name, tags
and notes stay out of the log — they are host-identifying free text (§15), and
the pseudonymized peer hash already names the row.

### 3. Trust is re-read at the moment it is used, not cached at challenge time

`may_try_unattended` is asked again when the credentials arrive, not only when
the challenge went out. A host user who withdraws trust while a guest is
typing gets the withdrawal honored — the same per-event re-check every
injected key gets under §8.1.

### 4. Persistence copies `ConnectionHistory`, and the file stays public-only

`apps/desktop/src-tauri/src/address_book_store.rs` takes the shape
`connection_history.rs` already worked out: `open(None)` for tests and for an
unresolvable config directory, an unreadable file becomes a warning and an
empty book rather than a panic, and the serialization is
`AddressBook::to_json`/`from_json` rather than a second one written locally.

The file lives beside the other configuration, through `config::config_dir()`,
not in the app data directory: it is host-owned policy, in the same place
`control_policy.toml` lives. It holds public keys and text a human typed, and
by construction no secrets — a `NodeId` is a public key. That property is not
incidental and nothing may be added that changes it.

A corrupt file is an *empty* book, never a partially parsed one.
`AddressBook::from_json` refuses the whole file for exactly this reason:
guessing at half of it is how a `trusted` flag survives an edit meant to
remove it.

### 5. The book is keyed by `NodeId`; the UI never sees one

Entries are keyed by base32 of the `NodeId`, so trust is per public key and
never per label — the label is what a human typed and means nothing to the
authorization. `AddressBook::peer_of_key` and `peers()` decode keys back;
`peers()` skips a key that no longer decodes rather than failing the listing,
because the file is editable by hand and one bad line must not hide the rest.

What crosses into the webview is the pseudonymized per-run label every other
panel names a peer by, and the address book's entries are registered in the
actor's label table so a saved-but-disconnected device is still addressable by
the commands.

## Consequences

- Four IPC commands (`address_book_list`, `address_book_upsert`,
  `address_book_remove`, `address_book_set_trusted`), main window only. A
  guest cannot read or edit the host's book, and a remote-view window cannot
  reach any of them.
- A device must be seen once, through the ordinary consent path, before it can
  be saved and trusted. There is no way to trust a machine that has never
  connected, because there would be no label to name it by.
- Names, tags and notes are free text rendered through `lit-html` bindings.
  Nothing on that screen builds markup by string concatenation, and a test
  pins a hostile device name rendering as text.
- The book is never logged as a whole: device names are host-identifying data
  (§15).
