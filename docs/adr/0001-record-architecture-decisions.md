# ADR 0001 — Record architecture decisions

Status: accepted
Date: 2026-08-12

## Context

The design document (`p2p-iroh-tauri-design-v12.md`) fixes exactly one accepted
option per architectural question (§2.6). Any departure from it has to be
visible, not silent (§24.4).

## Decision

Every deviation from the design document gets a numbered ADR in `/docs/adr`,
stating what the document says, what we do instead, and why. Code comments
referring to a design section are not a substitute.

An ADR is also required to:

- relax a threshold of §15,
- add `quinn` as a direct dependency,
- change a constant of §14, which additionally requires updating the prose of
  the design document wherever it is mentioned in words (§2.7).

## Numbering

The number is the first free one **counting numbers already cited in the code**,
not "the highest file in `docs/adr` plus one". A comment may reference an ADR
before the file lands, and taking that number for something else makes the
comment point at the wrong decision:

```sh
grep -rho "ADR 0[0-9][0-9][0-9]" crates apps services --include=*.rs | sort -u
ls docs/adr
```

Every number the first command prints must have a file. This rule exists
because it was missing: 0021, 0022 and 0028 were cited in module headers by
changes that never wrote the file, 0012 and 0013 were taken twice across
merged branches (fixed in `94e71e4` by renumbering the loser to 0014), and a
`PROTOCOL_MINOR` note once pointed at `0017-host-media-unavailable-wire-message.md`
while the decision had landed as 0024.

A number that turns out never to have carried a decision gets a short `void`
file saying where the decision actually lives (see ADR 0021 and ADR 0022)
rather than being silently reused. Numbers are never reassigned: code, commit
messages and other ADRs cite them.

## Consequences

The design document stays the single source of truth; the ADR log is the list
of exceptions to it.
