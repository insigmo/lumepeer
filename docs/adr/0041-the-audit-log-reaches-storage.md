# ADR 0041 — The audit log reaches storage, and what it still refuses to hold

Status: accepted
Date: 2026-08-28

## Context

`crates/core/src/audit.rs` has shipped since phase 4: twelve `AuditEvent`
cases, an `AuditRecord` carrying a BLAKE3 peer hash and a wall-clock second, a
`peer_hash` function, an `AuditSink` trait, and `NullAuditSink`. Its own doc
comment named the missing half — "Phase 4 backs this with an append-only SQLite
table with a 30 day retention and UI export/delete (§15)".

Nothing backed it. The only use of the module outside itself was `peer_hash`,
and that for display labels in `network.rs`. Every place an audit event
actually happened wrote it into the `tracing` stream instead:

```rust
tracing::info!(event = ?AuditEvent::UnattendedLogin { accepted: true }, "…");
```

which is a log line, not a record: it has no retention, no export, no way for
the host user to read or erase it, and it lands in the same rotating file as
everything else. §15 asks for something a person can be handed and can delete.

Four questions had to be answered to close that, and each has a way of being
answered wrongly that would be worse than the gap.

## Decisions

### 1. SQLite in the app's data directory, through the workspace's own `sqlx`

`apps/desktop/src-tauri/src/audit_store.rs`, one table, `audit.db` next to the
connection history. `sqlx` rather than a new synchronous SQLite binding: the
workspace already pins it for `services/broker`, so this adds a crate to one
binary's link and no new C build to the workspace. §5's rule about dependencies
is easier to keep by using the one already there.

The trade is that `sqlx` is async, so the writer is a **task**, not the OS
thread `crates/.../recorder.rs` uses. The shape that matters is copied intact —
a bounded queue, `try_send`, a drop counter, never a blocked caller — and only
the unit of concurrency differs, because a thread would have to carry a runtime
of its own to run the same query.

The store stays out of `crates/core`. That is the whole point of `AuditSink`
being a trait: the TCB decides what an event is, and knows nothing about where
it is kept.

### 2. Append-only is enforced where it can be, and bounded where it cannot

A `BEFORE UPDATE` trigger aborts every update, so the guarantee holds against
anything that opens the file, this process included.

`DELETE` cannot be guarded the same way, because retention and the user's own
purge are both deletes, and SQLite gives a trigger no way to tell an
administrative delete from a mischievous one. So the rule "never a single row"
is kept by the module offering exactly two delete statements — `WHERE
at_unix_secs < cutoff`, and the unqualified purge — and no third. This is
stated rather than implied: it is a weaker guarantee than the update trigger,
and pretending otherwise would be the kind of quiet overclaim an audit log
should not make about itself.

### 3. The audit salt is persistent, and is *not* the display salt

`peer_hash` needs a 32-byte install salt. The actor already has one —
`Actor::install_salt` — and it is regenerated on every start on purpose, so a
displayed peer label cannot be correlated across runs. An audit log needs
exactly the opposite: two visits by the same device must read as one device, or
the log answers no question worth asking.

So a second salt, minted once and kept in the keystore under
`AUDIT_SALT_ENTRY`. It sits with the unattended TOTP secret rather than beside
the database because it is what stops a reader of an exported log from
confirming a guessed `NodeId` by re-hashing it: not a key, but secret material.

A missing salt over a **non-empty** log is an error, not a fresh start. Minting
a new one there would silently split every peer's history in two while looking
like it worked; the store refuses to open instead, the host runs on
`NullAuditSink`, and the existing log stays readable.

### 4. Storage failure degrades the log, never the session

Every failure on the way to a working log — no data directory, a database that
will not open, a keystore that refuses, the lost salt above — is a warning and
a `None`. The host then runs without an audit trail and says so, in its own log
and in the panel (§18).

The alternative is worse than it looks: refusing to start on a broken audit
database hands anyone who can corrupt that file a way to take the machine
offline, and an audit log is evidence, not an authorization input.

For the same reason the records keep **wall-clock** time. §12.3's
clock-rollback defence belongs to licensing; a record has to say when it claims
to have happened, even on a machine that was lying about the date.

### 5. The vocabulary is closed, and mapped by hand

`event_columns` matches every `AuditEvent` onto a `kind` tag and a small
`detail` string, exhaustively and without a `_` arm. It is deliberately not a
`Serialize` derive: a derive would carry a future variant's free text — a file
name, a chat line — into the log the moment somebody added one, and §15's list
of what must never be stored is exactly that kind of text. `FileAction`'s
`&'static str` tag is part of the same discipline and stays a tag.

`EVENT_KINDS` is served to the UI from Rust so the filter cannot drift away
from what is actually written.

### 6. The panel reads, exports and erases; the path is chosen in Rust

`audit_list`, `audit_kinds`, `audit_status`, `audit_export` and `audit_clear`,
main-window only, each named in `capabilities/main.json`. The export writes CSV
of the stored rows through the OS save dialog driven from Rust — the webview
holds no `fs` permission and never names a path (§2.3) — and CSV rather than a
copy of the database, so what leaves the machine is the pseudonymized rows and
not a file that also carries SQLite's free pages.

`audit_status` exists because "nothing happened yet" and "nothing is being
recorded" are the same empty table otherwise, and §18 does not allow those to
look alike.

Erasing asks first. §15 requires the host user be able to erase the log, and a
one-click irreversible purge next to a list is not a way to offer that.

## Consequences

- The desktop binary links `sqlx` and SQLite. Build time for
  `lumepeer-desktop` grows; the workspace's dependency set does not.
- Records can outlive their retention by up to `AUDIT_RETENTION_SWEEP_SECS`
  (one day): the sweep runs at startup and daily, not per append. Sweeping per
  record would put a table scan on the consent path.
- A `list` call is capped at 500 rows. A thirty-day log on a busy host is not
  handed to the webview in one message; the date and kind filters are how a
  host reaches past the cap.
- `ProtocolViolation` is recorded only for `NetError::Framing` closes. Every
  other read error — an ordinary hang-up, a lost link — ends a session without
  anyone having broken §9.1, and recording those would bury the real ones.
- `InputToggled` is written on every grant, since a role is the only thing that
  moves `input` (ADR 0029). A host that re-grants the same role twice gets two
  records saying the same thing, which is the honest reading of "the host
  decided again".
- The guest microphone, the SAS button and the monitor picker (ADR 0028) are
  not audited. They ride grants that are, and `AuditEvent` has no case for
  them; adding one is a new enum variant and a discussion of what it carries,
  not free text through an existing case.

## Verification

`cargo test -p lumepeer-desktop` covers the store directly: that records reach
the table, that only the peer hash lands in a row, that retention removes old
records including protocol violations, that the database itself aborts an
`UPDATE`, that a lost salt over existing records refuses to open, and that the
export carries what the purge then removes. `apps/desktop/src/audit-log.test.ts`
covers the panel — the day-inclusive filter bounds, the "no log at all" state,
that erasing never happens on the first press, and that a failed read says so
instead of showing an empty log.
