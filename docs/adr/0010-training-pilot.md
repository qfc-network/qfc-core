# ADR-0010: Training pilot — in-process end-to-end proof

**Status:** accepted · **Date:** 2026-06-14 · **Context:** ROADMAP-AI-V3 Feature A, milestone A6.

## Problem

A1–A5 built the full decentralized-training pipeline (assignment → PS
aggregation → sampled verification → deterministic settlement) but nothing has
ever run the loop end to end. A6's roadmap deliverable is "a tiny model trained
across ≥3 miners on testnet." A live multi-machine testnet needs things outside
Claude's control — the roadmap itself flags miner volunteers and IPFS gateway
capacity as external. This ADR scopes A6 into the part that is buildable and
testable as code now, and names the part that genuinely needs live infra.

## Decision 1 — The pilot is an in-process ≥3-miner harness

`qfc-training-pilot` (new crate: lib + demo binary + integration tests) runs N
simulated miners **in one process** through the real A1–A5 APIs: a real
`MinerRegistry`/`TrainingPool` assignment, a real `ShardService` doing
trimmed-mean aggregation, the real `TrainingVerifier`, and the real
`settle_epoch`. This is the faithful code realization of "trained across ≥3
miners" — the only thing simulated is the network/process boundary between
miners, not any of the protocol logic.

What it proves (the integration test gates):
1. With ≥3 honest miners, the model's loss decreases monotonically and converges.
2. A planted cheater's update fails sampled re-execution (exact-match,
   ADR-0005 stage 1) → a `TrainingPenalty` is produced.
3. `settle_epoch` slashes the cheater, voids its rewards, and pays honest miners
   pro-rata.
4. Two independent "operators" fed the same accepted-update set in different
   order produce a byte-identical `commit_hash` (cross-operator determinism on
   real training data, not just the A5 property test's synthetic records).
5. The commitment persists and reloads via `TrainingChainStore`.

## Decision 2 — Deterministic CPU model, no candle

The pilot model is **linear regression with MSE loss**, full-batch (or
seed-selected-batch) gradient descent, samples summed in fixed index order,
accumulated in f64 and narrowed to f32. This makes every step **bit-reproducible
on CPU**, which is exactly what ADR-0005 stage-1 exact-match verification
requires — an honest worker's pushed `ParamUpdate` equals the verifier's
`replay_step` output bit-for-bit, and any deviation is caught.

Candle / a real BERT-class model is deliberately **not** used: GPU kernel
nondeterminism is the precise problem stage-1 sidesteps and A4b's tolerance-band
defers. A tiny deterministic model is the honest demonstration of the
determinism property; a real model on GPU is A4b + live-testnet territory.

Parameters are the flattened weight vector of length `param_count`; the job's
`range_partition` partitions `[0, param_count)`; each worker trains the full
model on its data shards and pushes the delta segment `-lr · grad[range]` per
registered range per step. The PS trimmed-mean-aggregates the per-worker deltas
and applies them — standard byzantine-robust distributed SGD.

## Decision 3 — Data is synthetic, manifest is real-but-local

Training data is synthetic, resolved from an in-process `shard_index → rows`
map. The `TrainingJobSpec` still carries a **valid** `ShardManifest` (non-empty
alphanumeric CIDs, `shards.len() ≥ min_updates`) so it passes A3 validation — but
the executor reads rows from the local map by `data_shard_indices`, not from
IPFS. (Surfacing that `split_file` leaves CIDs empty while training validation
demands them is itself a useful pilot finding.)

## Decision 4 — Settlement application is demonstrated in-process

The pilot includes `apply_settlement(settlement, &mut balances, &mut stakes)`
over plain `HashMap<Address, u128>` maps — crediting `rewards`, deducting
`slashes.slashed_amount` (capped at locked stake), as a **reference
implementation** for the real node wiring. This shows the A5 settlement output
is directly applyable without pulling `qfc-state` / `qfc-consensus` /
`qfc-node` into the pilot; porting `apply_settlement` to real chain state is the
node-integration follow-up.

## Decision 5 — Deferred (genuinely needs live infra or node changes)

Grep `live`/`node` in the pilot for the seams:

- **Live multi-machine testnet** — ≥3 separate hosts, real miner volunteers,
  IPFS gateway capacity. External; roadmap-acknowledged.
- **P2P broadcast** of updates/commitments/`SlashingEvidence` (needs qfc-network).
- **N-of-M operator agreement** that selects the canonical `EpochOutcome` +
  penalty set (the pilot has one operator; A5 gives the determinism primitive).
- **Porting `apply_settlement` into qfc-node/qfc-consensus/qfc-state** — the
  absolute-amount slash entry point, balance crediting at the epoch boundary.
- **VRF sampling entropy**, **per-job stake-locking enforcement**, **signed
  `ParamUpdate`** — the remaining A5-review honesty notes.
- **candle / BERT-class models on GPU** + **A4b tolerance-band** verification.

## Consequences

- A6 lands as a runnable demo (`cargo run -p qfc-training-pilot`) plus a gating
  integration test — the first time Feature A executes end to end.
- The pilot is the executable spec for the node-integration work: whatever the
  real node does at an epoch boundary must match `apply_settlement`.
