# QFC AI v3 Roadmap — Decentralized Training (Parameter Server) & Sharded Inference

*🌐 [中文](ROADMAP-AI-V3.zh.md)*

**Status:** design roadmap, not scheduled. Builds on the shipped v2.x AI-compute stack.
**Owner:** Larry. **Created:** 2026-06-10.

## Where v2.x leaves us

The pieces this roadmap builds on, all live today:

- `qfc-inference` — multi-backend inference runtime (CPU / CUDA / Metal / ROCm / OpenCL via candle + ONNX Runtime); governance-approved frozen models (BERT, Qwen2, Whisper) downloaded from HuggingFace, CID-addressed via IPFS.
- `qfc-ai-coordinator` — task pool, capability-based miner assignment (`assignment.rs`: backend, `GpuTier`, `memory_mb`, models), ~5% spot-check re-execution verification (`verification.rs`, `redundant.rs`, `challenge.rs`), governance model registry (`registry.rs`), treasury.
- `qfc-storage` — RocksDB engine (18 CFs, atomic `WriteBatch`, BE-ordered keys) — the persistence layer any new component reuses.
- Consensus integration — `WorkProof { flops_estimated, … }` per epoch, `inference_score` on validators.

**The two v3 features below share one principle: the chain stores *commitments and incentives*; heavy ML data flows off-chain.** A parameter server's relaxed consistency can never sit on the consensus path (state must be deterministically re-derivable), so every design here keeps PS-shaped components strictly off-chain with on-chain anchoring.

---

## Feature A — Decentralized training via an off-chain Parameter Server

### Why

v2.x miners contribute *inference* (frozen weights). The natural next step is contributing *training*: miners compute gradient updates on assigned data shards, and the network aggregates them into new model versions. That requires exactly a parameter server — a sharded KV holding mutable model parameters, aggregating updates from **untrusted** workers. This is the federated-learning × blockchain space; QFC's existing verification machinery is the differentiator.

### Architecture sketch

```
            ┌── on-chain (qfc-core) ─────────────────────────────┐
            │ model registry: version commitments (CID + hash)   │
            │ training-epoch records: who contributed, how much  │
            │ stake / slash for PS operators & training miners   │
            └────────────────▲───────────────────────────────────┘
                             │ version commit / reward / slash
┌── off-chain ───────────────┴───────────────────────────────────┐
│ qfc-ps (new crate): sharded parameter service                  │
│   · key range → shard (ps-lite-style range push/pull)          │
│   · aggregation = byzantine-robust rule (trimmed mean / Krum), │
│     NOT plain averaging — workers are untrusted                │
│   · bounded-staleness async (SSP); epoch barrier at commit     │
│ training miners: pull params → train on data shard → push      │
│ verifiers: spot-check re-execution of sampled training steps   │
│   (generalizes proof_pool / redundant / challenge)             │
└────────────────────────────────────────────────────────────────┘
```

Rendered version of the same architecture:

```mermaid
flowchart TB
    subgraph CHAIN["On-chain (qfc-core)"]
        REG["Model registry:<br/>version commitments (CID + hash)"]
        EPOCH["Training-epoch records:<br/>who contributed, how much"]
        STAKE["Stake / slash:<br/>PS operators & training miners"]
    end

    subgraph OFF["Off-chain"]
        subgraph PS["qfc-ps (new crate) — sharded parameter service"]
            AGG["Byzantine-robust aggregation<br/>(trimmed mean / Krum — NOT plain FedAvg)"]
            SHARDS["Key-range shards<br/>(ps-lite-style range push/pull,<br/>qfc-storage as local persistence)"]
        end
        MINERS["Training miners:<br/>pull params → train on data shard → push"]
        VERIF["Verifiers:<br/>sampled re-execution of training steps<br/>(generalizes proof_pool / redundant / challenge)"]
    end

    MINERS <-->|"push / pull<br/>(SSP bounded staleness)"| SHARDS
    SHARDS --- AGG
    VERIF -.->|"spot-check gradient hash<br/>(tolerance band)"| MINERS
    PS ==>|"epoch barrier:<br/>commit new version (CID)"| REG
    VERIF -->|"accept / reject contribution"| EPOCH
    EPOCH --> STAKE
    STAKE -.->|"slash on failed verification"| MINERS

    classDef chain fill:#e8f0fe,stroke:#1a56db;
    class REG,EPOCH,STAKE chain
```

Key design decisions (each needs an ADR before code):

1. **Who runs PS shards?** Proposal: staked PS operators (a new role, slashable), assigned shards like validators get epochs. Alternative: validators double as PS operators (simpler, couples failure domains).
2. **Aggregation rule.** Plain FedAvg is poisonable. Start with coordinate-wise trimmed mean; evaluate Krum/Bulyan against cost. The rule choice bounds how much a malicious minority can move the model.
3. **Contribution metering.** Reuse `flops_estimated`-style metering; reward per *accepted* (verification-passing) update, not per claimed work.
4. **Verification.** Sampled re-execution: verifier replays a miner's training step from the committed (params, data-shard, seed) tuple and checks the gradient hash. Determinism requires pinned kernels/seeds — hardest open problem (GPU nondeterminism); fallback is tolerance-band comparison.
5. **Sync model.** SSP with a small staleness bound during an epoch; hard barrier + on-chain version commit at epoch end. RPO story mirrors Monolith's: snapshot per epoch, replay the sample stream.

### Milestones (estimates = Claude session hours, not human time)

| # | Milestone | Deliverable | Est. (Claude session h) |
|---|---|---|---|
| A0 | ADRs for decisions 1–5 | `docs/adr/` ×5 | 2–3 h |
| A1 | `qfc-ps` crate skeleton | range-sharded KV, push/pull API, ps-lite-style timestamps; reuses `qfc-storage` as the shard's local persistence | 4–6 h |
| A2 | Byzantine-robust aggregation | trimmed-mean aggregator + property tests vs poisoning scenarios | 3–4 h |
| A3 | Training task type | extend `qfc-ai-coordinator` task pool + assignment for training jobs (data-shard manifest via IPFS) | 3–4 h |
| A4 | Verification path | sampled step re-execution in `verification.rs`; tolerance-band gradient compare | 4–6 h |
| A5 | Chain integration | model-version commitments, contribution records, reward/slash hooks in consensus | 4–6 h |
| A6 | Testnet pilot | tiny model (e.g. BERT-class) trained across ≥3 miners on testnet | 3–4 h |

Wall-clock depends on session spacing; total ≈ **23–33 Claude session hours**. Sequencing: A0 → A1/A2 in parallel → A3 → A4 → A5 → A6.

---

## Feature B — Sharded inference for oversized models

### Why

`assignment.rs` already matches tasks to miners by `GpuTier` + `memory_mb`; today a model that fits *no* single miner simply can't be served. Sharding weights lets the network host models larger than any one participant — the chain's version of the serving-PS read path: **pull shard by need, hash-verify each shard**.

### Two stages, deliberately

**B-1: Sharded *distribution*, single-miner *execution*** (cheap, ship first)
- Registry entry becomes a **shard manifest**: list of (shard CID, hash, size, layer range) instead of one CID.
- Miners with enough total memory assemble the model from IPFS shard-by-shard with per-shard verification (today's whole-file hash check generalized) — resumable downloads, partial caching, and shared shard reuse across model versions (most layers don't change between fine-tunes).
- No new execution semantics; pure distribution win.

**B-2: Multi-miner *pipeline execution*** (the hard part, gated on B-1 learnings)
- Coordinator assigns a **shard group**: an ordered set of miners each holding a layer range; activations flow miner→miner (pipeline parallelism).
- Honest constraint to state up front: pipeline parallelism over WAN latencies is unproven for interactive inference — target batch/async workloads first (the task pool already models async tasks), interactive only if latency data supports it.
- Verification: per-stage activation commitments (hash of layer-boundary tensors) so a spot-check can re-execute *one stage*, not the whole pipeline.
- Failure handling: a dead group member invalidates the assignment → reassign; reward splits per stage, metered like `flops_estimated`.

### Architecture (both stages)

```mermaid
flowchart LR
    subgraph CHAIN2["On-chain"]
        MAN["Registry shard manifest:<br/>(CID, hash, size, layer range) × N"]
    end
    IPFS["IPFS — weight shards"]

    subgraph B1["B-1: sharded distribution, single-miner execution"]
        SOLO["Miner with enough total memory:<br/>pull shard-by-shard → per-shard hash verify →<br/>assemble; cache shards across versions"]
    end

    subgraph B2["B-2: multi-miner pipeline (gated on WAN latency)"]
        direction LR
        G1["Miner 1<br/>layers 0–15"] -->|"activations"| G2["Miner 2<br/>layers 16–31"] -->|"activations"| G3["Miner 3<br/>layers 32–47"]
    end

    STAGECHK["Staged spot-check:<br/>re-execute ONE stage from<br/>activation commitments (hashes)"]

    MAN --> SOLO
    IPFS -->|"per-shard pull + verify"| SOLO
    MAN --> G1
    IPFS --> G1
    IPFS --> G2
    IPFS --> G3
    G1 -.->|"activation commitment"| STAGECHK
    G2 -.-> STAGECHK
    G3 -.-> STAGECHK

    classDef chain fill:#e8f0fe,stroke:#1a56db;
    class MAN chain
```

### Milestones (estimates = Claude session hours)

| # | Milestone | Deliverable | Est. (Claude session h) |
|---|---|---|---|
| B0 | ADR: shard manifest format + assignment changes | `docs/adr/` | 1–2 h |
| B1 | Shard manifest in registry + sharded IPFS download with per-shard verify | `registry.rs`, `download.rs` | 3–4 h |
| B2 | Assembly + cache reuse across versions | `qfc-inference` data_store | 2–3 h |
| B3 | Shard-group assignment | `assignment.rs` extension | 3–4 h |
| B4 | Pipeline execution prototype (2 miners, batch task) | `qfc-inference` runtime + coordinator router | 6–8 h |
| B5 | Per-stage verification | activation commitments + staged spot-check | 3–4 h |

B-1 (B0–B2) ≈ **6–9 Claude session hours** and is independently shippable. B-2 (B3–B5) ≈ **12–16 h**, gated on a WAN-latency measurement spike before committing.

---

## Sequencing & non-goals

- **Recommended order: B-1 → A → B-2.** B-1 is small, immediately useful (bigger models on today's network), and builds the shard-manifest plumbing A5/B-2 both want. A is the strategic feature. B-2 is gated on latency reality.
- **Non-goals, permanently:** PS semantics on the consensus path (determinism is non-negotiable); plain-FedAvg aggregation (poisonable); trusting any single PS operator (always ≥2-of-N verification or staked-slashable operators).
- Estimates above are **Claude session hours** — wall-clock time depends on how sessions are spaced. External dependencies outside Claude's control: testnet miner volunteers for A6/B4, and IPFS gateway capacity for large shard sets.
