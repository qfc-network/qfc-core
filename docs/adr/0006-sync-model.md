# ADR-0006: Synchronization model (SSP + epoch barrier)

**Status:** accepted · **Date:** 2026-06-12 · **Context:** ROADMAP-AI-V3 Feature A, decision 5.

## Decision

**Stale-synchronous parallel (SSP) within an epoch, hard barrier at the
epoch boundary.**

- Each worker carries a logical clock = its step count within the epoch.
  Within an epoch, pulls serve the committed version-N parameters: in-flight
  updates are buffered and never visible before the barrier, so reads are
  version-consistent. The staleness bound `s` (default 3 — ps-lite's
  bounded-delay knob, configurable per job) gates PUSH admission: how far
  behind the fastest worker a step may be and still enter aggregation.
- Clocks advance exactly one step per accepted push — there is no
  out-of-band clock report — which bounds clock-ratchet DoS: ratcheting the
  admission floor costs accepted, metered, audit-eligible work.
- A push older than the bound is rejected as stale. Stale pushes earn NO
  acceptance record and are NOT slashable; they are simply refused
  (consistent with ADR-0004(a)) and do not enter aggregation.
- At epoch end: stop accepting pushes, run the aggregation rule (ADR-0003)
  over the epoch's buffered updates, apply to the parameter set, snapshot
  (ADR-0007), and emit the new `(version, assembled_hash)` for the on-chain
  commit (A5). Training for epoch N+1 pulls from the committed version N.

Fully-async (no barrier) is rejected: without a barrier there is no
well-defined version to commit on-chain, and the chain anchoring *is* the
product. BSP (barrier every step) is rejected: WAN stragglers would set the
pace for every step; SSP tolerates them inside the epoch.

## Consequences

- The on-chain consensus path never sees SSP state — it sees only the
  epoch-end `(model_id, version, hash, CID)` commit, which is deterministic
  given the accepted-update set. Roadmap non-goal #1 holds structurally.
- The barrier as implemented is per-operator; cross-operator agreement on
  the accepted-update set (and hence identical `params_hash` across the ≥2
  operators of a range) is the A5 chain-integration deliverable.
- Epoch length for training jobs is minutes-scale (not the 10s consensus
  epoch); `qfc-ps` takes the training-epoch id as an opaque u64 from the
  coordinator rather than reading qfc-consensus — no dependency coupling.
- Dense models make delta-sync pointless (every epoch touches every weight),
  so version publication is full-CID, reusing the B-1 shard manifest format
  for distribution of the new version. (Monolith-style minute-level delta
  sync is a sparse-table optimization that does not transfer here.)
