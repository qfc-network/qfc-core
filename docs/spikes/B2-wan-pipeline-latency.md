# Spike: WAN latency for multi-miner pipeline inference (B-2 gate)

**Date:** 2026-06-14 · **Owner:** Larry · **Status:** complete → decision in
[ADR-0011](../adr/0011-b2-pipeline-scope.md).

ROADMAP-AI-V3 gates Feature B-2 (multi-miner pipeline execution) on "a
WAN-latency measurement spike before committing." The roadmap's own honest
constraint: *"pipeline parallelism over WAN latencies is unproven for
interactive inference — target batch/async workloads first… interactive only if
latency data supports it."* This spike gets the data and answers: does it?

**Answer:** interactive = **no-go** over real WAN; batch = **viable but
bandwidth-gated** (not just RTT-gated — the dimension the roadmap under-weighted).

## Method

Pipeline inference splits a transformer across K miners; layer-boundary
activations cross K−1 network hops per forward pass. Per-hop cost is
`one_way_latency + activation_bytes / bandwidth`. The reproducible calculator is
`crates/qfc-inference/examples/pipeline_latency.rs`
(`cargo run -p qfc-inference --example pipeline_latency`) — re-run it with real
numbers once geo-distributed miners exist.

### Measured anchors (real)

| Path | RTT | Bandwidth | How |
|---|---|---|---|
| **LAN** — AWS us-east-1 intra-VPC (testnet VPS-A→B/C/D) | **0.25 ms** | ~5 Gbit/s (assumed; typical 10–25 Gbit/s) | `ping` A→{B,C,D}, 5 samples each: 0.22–0.28 ms avg |
| **Intercontinental** — Singapore (Singtel) ↔ AWS us-east-1 (Virginia) | **242 ms** median | **20 Mbit/s** single-stream | TCP-handshake RTT ×8 (238–284 ms); 20 MB over SSH = 2.5 MB/s |

The live QFC testnet is **single-region** (B/C/D are `10.0.x.x` behind a
ProxyJump through A — LAN, not WAN), so it cannot directly measure cross-region
*miner* latency. The two measured anchors bracket reality: LAN is the
same-datacenter floor, SG↔Virginia is the intercontinental ceiling. The middle
two scenarios use published AWS inter-region figures:

| Scenario | RTT (ms) | BW (Mbit/s) | Provenance |
|---|---:|---:|---|
| `lan_same_vpc` | 0.25 | 5000 | **measured** RTT; BW assumed |
| `regional_cross_az` | 5 | 1000 | published |
| `continental` | 40 | 300 | published (us-east↔us-west ~60 ms; 40 mid-estimate) |
| `intercontinental` | 242 | 20 | **measured** (both) |

### Models (real registry configs), fp16 activations

bert-base (h768), qwen2.5-0.5b (h896), qwen2.5-3b (h2048), qwen2.5-7b (h3584).
Activation bytes/boundary = `batch · seq · hidden · 2`.

## Results

### Interactive autoregressive decode — network-only floor, 100 tokens (seconds)

Each generated token traverses the whole pipeline (K−1 hops); decode activations
are tiny (seq=1 → RTT-dominated). qwen2.5-7b:

| K | lan | regional | continental | intercontinental |
|---:|---:|---:|---:|---:|
| 2 | 0.014 | 0.256 | 2.02 | 12.4 |
| 4 | 0.041 | 0.767 | **6.06** | **37.2** |
| 8 | 0.096 | 1.79 | 14.1 | 86.7 |

This is **network alone**, compute excluded. Viability gate (100-token floor
< 2 s, qwen2.5-7b K=4): lan **PASS** (41 ms), regional **PASS** (767 ms),
continental **FAIL** (6.1 s), intercontinental **FAIL** (37 s). The only passing
scenarios are same-datacenter or same-region — i.e. *not* geographically
distributed miners.

### Batch prefill (B=8, S=512) — per-hop transfer (ms), the throughput bottleneck

Prefill activations are large (qwen2.5-7b B8/S512 = **28.7 MB/boundary**), so
bandwidth, not RTT, dominates. Per-hop transfer time:

| model | lan | regional | continental | intercontinental |
|---|---:|---:|---:|---:|
| bert-base | 10 | 53 | 188 | 2638 |
| qwen2.5-3b | 27 | 137 | 467 | 6832 |
| qwen2.5-7b | 47 | **237** | 803 | **11865** |

Bandwidth-bound flag (per-hop > 200 ms): regional **YES** (237 ms), continental
**YES**, intercontinental **YES** (≈12 s/hop). Only LAN keeps prefill transfer
under the compute timescale. **This is the spike's key finding the roadmap
missed:** B-2's risk was framed as RTT/latency, but for batch workloads the
20 Mbit/s measured inter-region bandwidth makes *transfer time* the wall — a 7B
prefill hop is ~12 s intercontinentally regardless of how the pipeline overlaps.

## Conclusion

- **Interactive inference over geo-distributed WAN: NO-GO.** Per-token RTT × token
  count is prohibitive at any real WAN RTT (≥ tens of ms). Drop it from B-2.
- **Batch/async prefill: VIABLE only with network locality.** Same-region
  (≤ regional, high-bandwidth) miner groups work; cross-region is bandwidth-
  walled until activations are compressed or links are fat.
- **Bandwidth is co-equal with RTT** as a B-2 constraint — assignment must model
  both.

Decision and B-2 re-scoping: [ADR-0011](../adr/0011-b2-pipeline-scope.md).

## Validation gate (before final B-2 commit)

These projections use one measured intercontinental path + published mid-range
figures. Before building B3–B5, re-run the calculator against **real
cross-region miner-to-miner** measurements (RTT + sustained multi-stream
bandwidth on the activation transport actually chosen). The single-region
testnet can't produce that today; it needs ≥2 miners deployed in different
regions.
