# ADR 0022 — Number never allocated (the address book)

Status: void — this number names no decision
Date: 2026-08-28

## Context

`crates/core/src/address_book.rs` used to cite "ADR 0022" in its module header.
As with ADR 0021, no such file was ever committed; the history holds no
`0022-*` blob, deleted or otherwise.

The decisions the reference meant are recorded elsewhere:

| What | Where it is actually recorded |
| --- | --- |
| Plain JSON on disk, keyed by base32 `NodeId`, trust per-`NodeId` and never per-label | ADR 0023 §3 |
| What trust actually gates — who may *attempt* unattended credentials, and why that is not a way past them | ADR 0034 |

ADR 0034 noted the dangling number when it gave the book its purpose.

## Decision

Do not write a reconstructed ADR 0022, for the same reason as ADR 0021: it
would duplicate ADR 0023 §3 and contradict ADR 0034's own account of the
number.

This file marks the number as spent. **0022 must not be reused** — ADR 0034 and
the task notes refer to it by number.

## Consequences

- The 0021–0022 gap in `docs/adr` is closed, so the numbering rule of ADR 0001
  can be applied by reading the directory listing.
- Old comments and branches citing "ADR 0022" land here and are sent one hop to
  ADR 0023 §3 or ADR 0034.
