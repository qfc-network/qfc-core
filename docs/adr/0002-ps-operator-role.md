# ADR-0002: Who runs parameter-server shards

**Status:** accepted · **Date:** 2026-06-12 · **Context:** ROADMAP-AI-V3 Feature A, decision 1.

## Decision

PS shards are run by **staked PS operators — a new, distinct role**, not by
validators. Operators stake QFC, are assigned contiguous parameter ranges per
training epoch (same epoch machinery validators use), and are slashable for:
unavailability during an epoch they accepted, serving parameters that fail
hash audit, and failing the data-pinning duty of ADR-0007.

## Why not validators-double-as-operators

- **Failure domains stay decoupled.** A PS shard saturating disk/network must
  not degrade block production. (The 2026-05 VPS incidents are exactly this
  class of coupling.)
- **Hardware profiles differ.** Validators need low-latency consensus I/O;
  PS shards need RAM + disk bandwidth. Separate roles let the market provision
  each.
- **Consensus stays clean.** PS state is relaxed-consistency by design
  (ADR-0006) and must never be re-derivable consensus state. A separate role
  makes the boundary structural, not conventional.

Operationally, one machine MAY run both binaries on testnet (A6 pilot does);
the protocol roles, stakes, and slash conditions remain separate.

## Consequences

- New on-chain role registry (stake, shard-range acceptance, liveness
  heartbeats) lands in A5; `qfc-ps` (A1) is role-agnostic — it implements the
  shard service an operator runs.
- ≥2 operators per shard range (primary + replica) from day one; a single
  operator is never trusted (roadmap non-goal #3).
