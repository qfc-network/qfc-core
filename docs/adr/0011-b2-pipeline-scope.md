# ADR-0011: B-2 scope — batch-only, locality-aware pipeline inference

**Status:** accepted · **Date:** 2026-06-14 · **Context:** ROADMAP-AI-V3
Feature B-2, gated on the WAN-latency spike
([docs/spikes/B2-wan-pipeline-latency.md](../spikes/B2-wan-pipeline-latency.md)).

## Problem

The roadmap gated B-2 (multi-miner pipeline execution) on a WAN-latency spike
and hypothesised "batch first, interactive only if latency supports it." The
spike measured real WAN (SG↔us-east-1: 242 ms RTT, 20 Mbit/s; intra-VPC LAN:
0.25 ms) and modelled pipeline cost across model sizes and stage counts. The
data decides the scope.

## Decision

### 1. Interactive autoregressive inference over WAN is OUT of B-2

Per-token full-pipeline traversal makes latency `T · (K−1) · RTT/2`. For
qwen2.5-7b, K=4, 100 tokens: 6 s continental, 37 s intercontinental — network
*alone*. Only same-datacenter/same-region paths pass a 2 s bar, and those negate
the point of *geographically distributed* miners. B-2 does **not** target
interactive inference. (Interactive stays single-miner, as today.)

### 2. B-2 targets batch / async single-forward workloads only

Prefill-style single-pass workloads (embeddings, classification, batched
scoring, async generation where latency is not user-facing) pipeline acceptably
**when network-local**. This is the B-2 product.

### 3. Bandwidth is a first-class assignment constraint, co-equal with RTT

The spike's correction to the roadmap: B-2's risk is not only RTT. At 20 Mbit/s,
a 7B prefill hop (B8/S512, 28.7 MB) is ~12 s — transfer-bound. Even regional
cross-AZ trips the 200 ms/hop threshold. Therefore the B3 shard-group assignment
(extending A3's assignment) must:
- group miners by **network proximity** (region/AS/measured RTT+BW), forming
  pipeline groups whose inter-stage links clear a bandwidth+latency floor;
- carry **activation-transfer cost** (bytes/hop ÷ link BW + RTT) as an explicit
  term in group formation and reject groups that exceed a per-hop budget;
- prefer **fewer stages** (smaller K) — latency and hop count both scale with K.

### 4. Activation compression is a B-2 requirement, not an option

Layer-boundary tensors must be quantized for transport (fp16→int8/fp8, ~2–4×
fewer bytes) before any cross-region group is viable. This becomes part of the
B4 pipeline-execution prototype and the B5 per-stage activation commitment
(hash the *quantized* transported tensor).

## Consequences

- **B3** (shard-group assignment): add the locality/bandwidth model above.
- **B4** (pipeline prototype): batch-only; include activation quantization;
  measure on a real ≥2-region miner pair, not a single-region stand-in.
- **B5** (per-stage verification): activation commitments hash the quantized
  transported bytes (so verifier re-execution compares the same artifact).
- The 12–16 h B-2 build estimate stands for the batch path; dropping interactive
  removes the hardest latency problem, so the estimate is, if anything, safer.
- **Hard gate before committing B3–B5:** re-run
  `cargo run -p qfc-inference --example pipeline_latency` against real
  cross-region miner-to-miner RTT+BW (needs ≥2 miners in different regions —
  the single-region testnet cannot produce this). Build only if a realistic
  locality tier (same-region/continental, post-compression) clears the per-hop
  budget for the target model sizes.

## Non-goals (unchanged)

Interactive WAN inference; any pipeline group that can't meet the per-hop
transfer budget; treating RTT as the sole network cost.
