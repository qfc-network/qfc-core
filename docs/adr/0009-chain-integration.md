# ADR-0009: Chain integration — settlement, commitments, reward/slash

**Status:** accepted · **Date:** 2026-06-14 · **Context:** ROADMAP-AI-V3 Feature A, milestone A5.

## Problem

A0–A4 produced off-chain machinery: PS aggregation (`EpochOutcome`), training
assignment (`TrainingJobSpec`/`TrainingAssignment`), and sampled verification
(`TrainingPenalty`). None of it touches the chain yet — every "A5 scope" note
in `training.rs` / `training_verification.rs` / `qfc-ps` defers the on-chain
half here. A5 wires the epoch result into chain state: a version commitment,
contribution-driven rewards, and slashing for failed verification.

## Decision 1 — A pure settlement layer; the chain stores only commitments

Settlement is a **pure, deterministic function** of an epoch's result — it
computes *what* changes on-chain, not *how* the node applies it:

```
settle_epoch(outcome, spec, penalties, reward_pool, config) -> EpochSettlement
EpochSettlement {
    commit:   TrainingEpochCommit,   // the on-chain anchor
    rewards:  Vec<RewardCredit>,     // (worker, amount) to credit
    slashes:  Vec<SlashResult>,      // qfc-types SlashResult, offense=InvalidTraining
    verifier_budget_flops: u64,      // reserved for verification work (ADR-0008)
}
```

Purity is the **cross-operator-agreement primitive** (the open item from the A4
review): two of the ≥2 operators of a range (ADR-0002) that received the same
accepted-update set produce a **byte-identical** `commit` and therefore the same
`commit_hash()`. A5 gives the node a cheap equality check; the N-of-M voting that
*selects* the canonical outcome is consensus-path work, deferred (Decision 6).

Lives in `qfc-ai-coordinator/src/settlement.rs` — that crate already depends on
`qfc-ps` (EpochOutcome/AcceptanceRecord), `qfc-types` (SlashResult/U256), and the
training types. No new inter-crate edges for the computation.

## Decision 2 — Commitment shape: full record set off-chain, anchored by a root

```rust
TrainingEpochCommit {
    job_id: Hash,
    epoch: u64,
    params_hash: Hash,            // from EpochOutcome — the new model version
    records_root: Hash,          // blake3 over canonically-sorted AcceptanceRecords
    accepted_count: u64,
    total_flops_accepted: u64,
}
```

The full `Vec<AcceptanceRecord>` stays off-chain (it can be large); only
`records_root` is committed. `records_root` sorts records canonically by
`(worker bytes, epoch, clock, range.start, range.end, update_hash)` then streams
blake3 — so any party holding the records can recompute and verify the root, and
two operators with the same set get the same root. Borsh + serde, like every
on-chain-adjacent type. `params_hash` duplicates `EpochOutcome.params_hash`
deliberately so the commit is self-contained.

## Decision 3 — Rewards: pro-rata over accepted, non-voided FLOPs

Per ADR-0004, the per-epoch training reward pool splits pro-rata over accepted
updates' `flops_estimated`. A penalized worker's records are **voided first**
(ADR-0004 `void_epoch_records`) — excluded before pro-rating, earning nothing.
Integer division: `worker_reward = pool * worker_flops / total_flops`. The
rounding remainder is **carried forward** (left in the pool, not distributed) —
deterministic and avoids a "remainder to whoever sorts first" bias. Workers with
voided-to-zero remaining FLOPs are omitted from `rewards`.

## Decision 4 — Slashing: new offense, absolute amount

Add `SlashableOffense::InvalidTraining` to `qfc-types`, mirroring
`InvalidInference`. `penalty_to_slash(penalty, now) -> SlashResult`:

```
SlashResult {
    validator: penalty.worker,
    offense:   InvalidTraining,
    slashed_amount: U256(penalty.slash_amount),   // = slash_multiple × per_step_reward (ADR-0008)
    jail_until:     now + penalty.jail_duration_ms, // 6h, INVALID_TRAINING_JAIL_MS
}
```

**Note the model mismatch (flagged for A6):** the existing inference slash path
(`consensus::slash_validator(percent, jail_ms)`) is **percent-of-stake**; ADR-0008
training slashing is an **absolute amount** (40r). `SlashResult.slashed_amount`
is already absolute, so the settlement output is correct — but the node needs an
absolute-amount slash entry point to apply it (the percent path can't express
"slash exactly 40r"). That node wiring is Decision 6, deferred. Capping the slash
at the worker's locked training stake also happens at application time (settlement
has no stake view).

## Decision 5 — Commitment persistence

New column family `TRAINING_COMMITMENTS` in `qfc-storage` `cf::ALL`, keyed
`job_id ‖ epoch(BE)`, value = Borsh(`TrainingEpochCommit`). A thin
`TrainingChainStore` (new `qfc-ai-coordinator/src/chain_store.rs`, adds the
`qfc-storage` dep) wraps a dedicated-or-shared `Database`: `put_commit`,
`get_commit`, `commits_for_job` (range scan), append-only. **Risk to verify:**
adding a CF to `cf::ALL` must not break opening existing on-disk DBs —
`Database::open` must use `create_missing_column_families`; the implementor
confirms this before relying on it, else the CF addition is gated behind a
migration note.

## Decision 6 — Non-goals (deferred to A6 / node integration)

Each stays grep-able as an `A6`/`node` scope note where the seam is:

- **State mutation at the epoch boundary** — actually crediting `rewards` to
  balances and applying `slashes` to validator stake (needs the node's State,
  validator set, and clock). Settlement produces the deltas; the node applies.
- **Absolute-amount slash entry point** in consensus (Decision 4).
- **P2P broadcast** of commitments and `SlashingEvidence`.
- **N-of-M operator agreement** that selects the canonical `EpochOutcome`
  (Decision 1 provides the deterministic-equality primitive only).
- **VRF sampling entropy** (A4 used committed `params_hash`; A6 upgrades to
  on-chain VRF), **per-job stake locking** enforcement, **signed `ParamUpdate`**.

## Consequences

- A5 is fully unit-testable without standing up a node: settlement is pure;
  persistence uses `Database::open_temp`.
- `settle_epoch` determinism is property-tested (shuffled record/penalty order →
  identical `commit_hash` and `rewards`).
- Adding `InvalidTraining` touches every exhaustive `match` on `SlashableOffense`
  — the implementor finds and updates all of them.
