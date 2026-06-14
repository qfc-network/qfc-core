# ADR-0008: Verification economics (slash/reward ratio)

**Status:** accepted · **Date:** 2026-06-12 · **Context:** Feature A review finding 2 (sampling without a stated ratio is theater).

## Problem

Re-executing a training step costs ≈1× the original work, so audits are
sampled (p = 5%, ADR-0005), not universal. Sampling only deters cheating if
the expected value of fabricating work is negative — that is a ratio we must
pick and enforce, not an emergent property.

## Decision

For a per-step reward `r`, detection probability per fabricated step `p`,
the slash `S` on detection must satisfy `p·S > (1−p)·r`, i.e. at p = 5%:
`S > 19r`. We set:

> **S = 40× the per-step reward** (safety factor ≈2 over break-even),
> assessed against the miner's training stake; plus retroactive voiding of
> the epoch's acceptance records (ADR-0004); plus the existing jail mechanic
> from v2.x InvalidInference (6h).

Minimum training stake per epoch must therefore cover `40r × steps_assigned`
— this is the stake floor the assignment layer (A3) enforces, and it is the
same number that makes ADR-0003's "stake-bounded minority" real: an attacker
buying `β·n` worth of update slots is buying slashable stake, not identities.

Challenge traps (ADR-0005) raise effective `p` above the nominal 5% for free
on a per-miner basis; the ratio is calibrated to nominal `p` so traps are
margin, not load-bearing.

`p` holds only if the sampling entropy source is worker-independent — a
worker that can grind the sampling coin drives its effective `p` to 0 and
voids this entire derivation. The interim entropy is the epoch's committed
post-barrier `params_hash` (unknowable at push time, ADR-0005); A5 upgrades
it to on-chain VRF randomness, which also removes the residual influence a
colluding worker majority has over the aggregate `params_hash`.

GPU tolerance-band mode (A4b) weakens detection (cheats inside ε are
invisible) — the ε-calibration report must include a re-derivation of this
ratio before GPU training jobs are enabled. Until then the ratio assumes
exact-match CPU verification.

## Consequences

- Protocol constants in one place (`qfc-ps::config`): `p = 0.05`,
  `slash_multiple = 40`, `staleness_bound = 3`, `trim_beta = 0.2`,
  `snapshot_retention = 4`. A5 mirrors them on-chain as governance
  parameters.
- Per-epoch verifier budget = p × accepted FLOPs, paid from the epoch reward
  pool (verification is metered work like any other).
