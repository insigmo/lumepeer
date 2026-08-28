# ADR 0021 — Number never allocated (unattended access)

Status: void — this number names no decision
Date: 2026-08-28

## Context

`crates/core/src/unattended.rs` used to cite "ADR 0021" in its module header,
and `crates/core/src/constants.rs` still cited it next to
`UNATTENDED_TOTP_STEP_SECS`. No such file was ever committed: neither
`git log --all --diff-filter=D -- 'docs/adr/*'` nor a search of the whole
history for a `0021-*` blob finds one.

The decisions the reference meant were taken, and recorded, under other
numbers:

| What | Where it is actually recorded |
| --- | --- |
| Argon2id for the device password, five-failure lockout | ADR 0023 §1 |
| RFC 6238 TOTP as an in-tree implementation, HMAC-SHA1 | ADR 0023 §2 |
| The admission path that finally calls the module, and the rule text it corrected | ADR 0033 |

ADR 0033 reached the same conclusion when it wired the module up: the dangling
reference is "a numbering artifact", and clearing it belongs to the ADR-debt
cleanup, which is this file.

## Decision

Do not write a reconstructed ADR 0021. Reconstructing one would duplicate
ADR 0023 §1–§2 and contradict ADR 0033, which already states on the record that
no such decision document exists.

This file exists so that the number is visibly spent rather than merely
missing. **0021 must not be reused.** Three ADRs (0023, 0033, 0034), the task
notes under `docs/tasks/` and the commit history all discuss "ADR 0021" by
number; a future decision taking the number would make every one of those texts
read as if it pointed at something else.

The code references were repointed at the real records in the same change.

## Consequences

- `ls docs/adr` shows no gap at 0021, so the next free number is read off the
  listing correctly — see the numbering rule in ADR 0001.
- Anyone arriving from an old comment, a commit message or an archived branch
  that still says "ADR 0021" lands here and is sent one hop to ADR 0023 §1–§2
  or ADR 0033.
