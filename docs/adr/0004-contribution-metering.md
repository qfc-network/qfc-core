# ADR-0004: Training contribution metering & rewards

**Status:** accepted · **Date:** 2026-06-12 · **Context:** ROADMAP-AI-V3 Feature A, decision 3.

## Decision

Reward **per accepted update**, where accepted means: (a) submitted within
the epoch's staleness bound (ADR-0006), (b) well-formed for the assigned
(range, data-shard, step) tuple, and (c) **not failed** by sampled
re-execution (ADR-0005). Claimed-but-unverified work earns the same as
verified work — verification is a random audit, not a payment gate; this is
exactly the v2.x inference model (`flops_estimated` + 5% spot-check)
generalized to training steps.

Metering unit: `flops_estimated` computed from the task descriptor
(model params × tokens per step × steps), same estimator family as
`task_types.rs::task_requirements`. The per-epoch training reward pool is
split pro-rata over accepted updates' estimated FLOPs.

## Explicitly rejected

- **Reward-if-aggregated** (paying only untrimmed updates): honest outliers
  get trimmed (ADR-0003); tying pay to trimming punishes honest gradient
  noise and incentivizes update-copying toward the median.
- **Loss-improvement bounties** ("pay for AUC delta"): unattributable per
  worker, gameable by withholding.

## Consequences

- `qfc-ps` records per-epoch `(worker, range, step, update_hash, flops_estimated)`
  acceptance records; A5 anchors the epoch's record-set hash on-chain with the
  version commit, which is what the reward distribution reads.
- A failed re-execution retroactively voids the miner's acceptance records
  for that epoch and triggers slashing (ADR-0005's ratio makes the audit
  lottery unprofitable to cheat).
