# ADR-0003: Byzantine-robust aggregation rule

**Status:** accepted · **Date:** 2026-06-12 · **Context:** ROADMAP-AI-V3 Feature A, decision 2.

## Decision

Aggregation is **coordinate-wise trimmed mean** with trim fraction
`β = 0.2` per side (configurable per model). For each parameter coordinate,
sort the per-worker update values, drop the top and bottom `⌈β·n⌉`, average
the rest. Plain FedAvg is rejected permanently (roadmap non-goal #2): one
malicious worker can move an averaged coordinate arbitrarily.

Krum/Bulyan are deferred: they need O(n²) pairwise distances over full update
vectors, and their advantage matters most for high-dimensional collusion —
re-evaluate after A6 pilot data.

## Robustness bound and its preconditions

Trimmed mean tolerates `f < β·n` malicious updates per shard. Two
preconditions, without which the bound is meaningless:

1. **Sybil resistance comes from stake, not the rule.** Updates count toward
   `n` only from miners holding the minimum training stake for the epoch's
   job. The rule bounds what a *stake-bounded* minority can do; identities
   must not be free.
2. **Per-coordinate trimming needs all updates materialized.** Unlike a
   BytePS-style summation service (streaming, O(1) per update), trimmed mean
   costs **O(n × shard_size) memory at the aggregation point**. This caps
   workers-per-shard: `max_workers = mem_budget / shard_size_bytes`. The cap
   is enforced at assignment (A3), and the constant lives in `qfc-ps` config.

## Consequences

- `qfc-ps` aggregation buffers updates per epoch window and aggregates at the
  SSP barrier (ADR-0006); update values are f32, aggregation accumulates in
  f64 for determinism-friendly summation order (sorted by worker address).
- Property tests (A2) must demonstrate: bound holds at `f < β·n` with extreme
  adversarial values; bound *breaks* at `f ≥ β·n` (test documents the cliff,
  so the assignment cap is load-bearing and visible).
- Being trimmed is NOT evidence of malice (honest outliers exist); trimming
  never triggers slashing by itself. Slashing requires failed re-execution
  (ADR-0005).
