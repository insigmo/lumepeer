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

## Consequences

The design document stays the single source of truth; the ADR log is the list
of exceptions to it.
