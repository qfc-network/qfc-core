# ADR-0005: Training-step verification

**Status:** accepted · **Date:** 2026-06-12 · **Context:** ROADMAP-AI-V3 Feature A, decision 4.

## Decision

**Sampled re-execution**, generalizing the v2.x spot-check machinery
(`verification.rs` / `redundant.rs` / `challenge.rs`). An update commits to
the tuple `(param_version_hash, data_shard_cid, batch_indices, seed,
update_hash)`. A verifier replays the training step from that tuple and
compares.

Two comparison modes, staged:

1. **A6 pilot: exact match, CPU-only determinism.** Pilot training jobs run
   `SafetensorsFp32` on CPU with pinned seeds and a fixed reduction order —
   the same determinism class the inference spot-check already relies on
   (`CanonicalFormat` exists precisely for this). Verifier checks
   `blake3(update) == update_hash`.
2. **GPU tolerance-band (A4b, after pilot).** GPU kernel nondeterminism makes
   exact gradient equality unachievable across vendors — this is the hardest
   open problem in the roadmap and we do not pretend otherwise. Fallback:
   verifier computes the update independently and accepts iff every
   coordinate satisfies `|Δ_claimed,i − Δ_replayed,i| ≤
   max(ε·|Δ_replayed,i|, abs_floor)` — a per-coordinate relative compare
   with an absolute floor. The band is per-coordinate (a global ∞-norm band
   would let one large honest coordinate hide a targeted poison in a small
   one), and the absolute floor exists because the relative band collapses
   on near-zero gradient coordinates where benign kernel noise dominates.
   BOTH knobs (ε and abs_floor) are calibrated on pilot data. Tolerance
   bands weaken the slash guarantee (a cheater can move band-far); the
   calibration report is a gate for enabling GPU training jobs.

Sampling rate starts at **5%** (matches `SPOT_CHECK_RATE`), adaptive per
miner reputation later. The sampling entropy must be worker-independent:
the decision coin is `blake3(epoch_entropy ‖ update_hash)` where
`epoch_entropy` is the epoch's committed post-barrier `params_hash` — it
does not exist at push time, so a worker cannot grind `flops_estimated`
(or value bits) to evade the sample; a colluding worker *majority* retains
weak influence over the aggregate `params_hash`, which A5 closes by
replacing `epoch_entropy` with on-chain VRF randomness. Challenge-style
traps (pre-computed steps with known updates, indistinguishable from real
assignments) reuse `challenge.rs`'s pattern for cheap always-on coverage.

## Consequences

- Verification economics are a protocol parameter, not an afterthought —
  see ADR-0008 for the slash/reward ratio that makes 5% sampling sufficient.
- Re-executing a step costs ≈1× the original work; at 5% sampling the
  network-wide verification overhead is ≈5% of training compute, paid from
  the same epoch reward pool.
- `qfc-ps` must serve historical `(version, range)` reads for the epoch
  being audited (snapshot retention ≥ audit window, ADR-0007).
