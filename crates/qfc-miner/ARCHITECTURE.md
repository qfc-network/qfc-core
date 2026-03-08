# QFC Inference Miner — Architecture

## Overview

`qfc-miner` is a standalone binary that connects to a QFC validator node and contributes AI inference compute to earn block rewards. It fetches tasks via JSON-RPC, executes inference using local hardware (GPU/CPU), and submits cryptographic proofs of work.

## System Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│                        start-miner.sh                               │
│  1. Auto-update check (GitHub Releases)                             │
│  2. Download binary / Build from source                             │
│  3. Generate wallet (Ed25519)                                       │
│  4. Request faucet tokens                                           │
│  5. Launch qfc-miner binary                                        │
└─────────────────────────┬───────────────────────────────────────────┘
                          │
                          ▼
┌─────────────────────────────────────────────────────────────────────┐
│                      qfc-miner binary                               │
│                                                                     │
│  main.rs                                                            │
│  ┌──────────────┐  ┌───────────────┐  ┌──────────────────────────┐ │
│  │  MinerCli    │  │  gpu.rs       │  │  config.rs               │ │
│  │  (clap)      │  │  detect_hw()  │  │  MinerConfig {           │ │
│  │  --wallet    │  │  ┌──────────┐ │  │    wallet_address        │ │
│  │  --backend   │──│  │ CUDA     │ │──│    backend               │ │
│  │  --model-dir │  │  │ Metal    │ │  │    validator_rpc         │ │
│  │  --hot-models│  │  │ CPU      │ │  │    secret_key            │ │
│  │              │  │  └──────────┘ │  │    max_memory_mb         │ │
│  └──────────────┘  └───────────────┘  │  }                       │ │
│                                       └──────────────────────────┘ │
│         │                    │                    │                 │
│         ▼                    ▼                    ▼                 │
│  ┌──────────────┐  ┌────────────────┐  ┌───────────────────────┐  │
│  │InferenceEngine│  │ ModelScheduler │  │  InferenceWorker      │  │
│  │(qfc-inference)│  │               │  │  (worker.rs)          │  │
│  │              │  │  VramBudget    │  │                       │  │
│  │ ┌──────────┐ │  │  Hot/Warm/Cold │  │  Main Loop:           │  │
│  │ │CpuEngine │ │  │  layers       │  │  ┌───────────────────┐│  │
│  │ │MetalEng. │ │  │               │  │  │ 10s: fetch task   ││  │
│  │ │CudaEng.  │ │  │  LRU eviction │  │  │ 30s: report status││  │
│  │ │OnnxEng.  │ │  │               │  │  └─────────┬─────────┘│  │
│  │ └──────────┘ │  └───────┬───────┘  │            │          │  │
│  │              │          │          │            │          │  │
│  │ ┌──────────┐ │◄─────────┘          │            │          │  │
│  │ │ModelCache│ │  ensure_loaded()    │            │          │  │
│  │ │(download)│ │                     │            │          │  │
│  │ └──────────┘ │                     │            │          │  │
│  └──────┬───────┘                     │            │          │  │
│         │ run_inference()             │            │          │  │
│         ▼                             │            │          │  │
│  ┌────────────────────────────────┐   │            │          │  │
│  │    Inference Pipeline          │   │            │          │  │
│  │                                │   │            │          │  │
│  │  ┌───────────┐ ┌────────────┐  │   │            │          │  │
│  │  │ Embedding │ │ TextGen    │  │   │            │          │  │
│  │  │ (BERT)    │ │ (Qwen)    │  │   │            │          │  │
│  │  ├───────────┤ ├────────────┤  │   │            │          │  │
│  │  │ Whisper   │ │ ImageClass │  │   │            │          │  │
│  │  │ (STT)     │ │            │  │   │            │          │  │
│  │  ├───────────┤ ├────────────┤  │   │            │          │  │
│  │  │ SD/FLUX   │ │ ONNX      │  │   │            │          │  │
│  │  │ (ImageGen)│ │ (generic) │  │   │            │          │  │
│  │  └───────────┘ └────────────┘  │   │            │          │  │
│  └────────────┬───────────────────┘   │            │          │  │
│               │ InferenceResult       │            │          │  │
│               ▼                       │            │          │  │
│  ┌────────────────────────────────┐   │            │          │  │
│  │    Proof Builder               │   │            │          │  │
│  │                                │   │            │          │  │
│  │  InferenceProof {              │   │            │          │  │
│  │    input_hash, output_hash,    │   │            │          │  │
│  │    flops_estimated,            │   │            │          │  │
│  │    execution_time_ms,          │   │            │          │  │
│  │    signature (Ed25519)         │   │            │          │  │
│  │  }                             │   │            │          │  │
│  └────────────┬───────────────────┘   │            │          │  │
│               │                       │            │          │  │
│               ▼                       │            │          │  │
│  ┌────────────────────────────────┐   │            │          │  │
│  │    submit.rs (RPC Client)      │◄──┘◄───────────┘          │  │
│  │                                │                           │  │
│  │  fetch_task()          ─────────────────────────────►      │  │
│  │  submit_proof()        ─────────────────────────────►      │  │
│  │  register_miner()      ─────────────────────────────►      │  │
│  │  report_miner_status() ─────────────────────────────►      │  │
│  │                                │                           │  │
│  │  ProofResult {                 │  ╔══════════════════════╗ │  │
│  │    accepted, spot_checked,     │  ║ +0.05 QFC            ║ │  │
│  │    reward_estimate ────────────┼─►║ Session: 1.2 QFC     ║ │  │
│  │  }                             │  ╚══════════════════════╝ │  │
│  └────────────────────────────────┘                           │  │
│                                                               │  │
└───────────────────────────┬───────────────────────────────────┘  │
                            │ JSON-RPC / HTTP                      │
                            ▼                                      │
┌──────────────────────────────────────────────────────────────────┘
│
▼
┌──────────────────────────────────────────────────────────────────────┐
│                     Validator Node (qfc-node)                        │
│                                                                      │
│  qfc-rpc (JSON-RPC Server, port 8545)                                │
│  ┌────────────────────────────────────────────────────────────────┐  │
│  │ qfc_getInferenceTask      → TaskPool.assign_task()             │  │
│  │ qfc_submitInferenceProof  → verify + spot-check (5%)           │  │
│  │ qfc_registerMiner         → MinerRegistry.register()           │  │
│  │ qfc_reportMinerStatus     → update loaded models               │  │
│  └────────────────────────────────────────────────────────────────┘  │
│                            │                                         │
│  qfc-ai-coordinator        ▼                                         │
│  ┌────────────────────────────────────────────────────────────────┐  │
│  │ TaskPool         — pending / assigned / completed tasks        │  │
│  │ MinerRegistry    — registered miners & capabilities           │  │
│  │ Spot-checker     — 5% re-execution verification               │  │
│  │ Fee settlement   — 70% miner / 10% validator / 20% burn      │  │
│  └────────────────────────────────────────────────────────────────┘  │
│                            │                                         │
│  Block Producer            ▼                                         │
│  ┌────────────────────────────────────────────────────────────────┐  │
│  │ distribute_rewards()                                           │  │
│  │   ├── Producer:   60%                                          │  │
│  │   ├── Voters:     25%                                          │  │
│  │   └── Miners:     15%  (proportional to FLOPS)                │  │
│  │       └── state.add_balance(miner, reward)                    │  │
│  └────────────────────────────────────────────────────────────────┘  │
└──────────────────────────────────────────────────────────────────────┘
```

## Data Flow

```
Miner startup → Detect hardware → Register with Validator → Enter main loop
                                                                  │
    ┌─────────────────────────────────────────────────────────────┘
    │
    ▼
 [every 10s] fetch_task() ──→ Receive task from TaskPool
    │
    ▼
 Load model (ModelScheduler — Hot/Warm/Cold VRAM management)
    │
    ▼
 Run inference (Embedding / LLM / Whisper / ImageGen / ONNX)
    │                       ↓ on failure
    │                 Auto-fallback to CPU backend
    ▼
 Build proof (Blake3 hash + FLOPS measurement + Ed25519 signature)
    │
    ▼
 submit_proof() ──→ Validator verifies
    │                    │
    │                    ├── Signature check
    │                    ├── Timestamp freshness
    │                    ├── 5% spot-check (re-execute & compare output hash)
    │                    └── Return reward_estimate
    ▼
 ╔════════════════════════════════════════════╗
 ║  PROOF ACCEPTED — REWARD EARNED           ║
 ║  This task:     +0.05 QFC                 ║
 ║  Session total:  1.20 QFC                 ║
 ╚════════════════════════════════════════════╝
```

## Module Map

```
crates/qfc-miner/src/
├── main.rs      Entry point: CLI parsing, hardware detection, engine creation, worker launch
├── config.rs    MinerCli (clap) + MinerConfig runtime config
├── gpu.rs       Hardware detection wrapper (CUDA / Metal / CPU)
├── submit.rs    JSON-RPC client: fetch_task, submit_proof, register_miner, report_status
└── worker.rs    InferenceWorker main loop: task fetch → inference → proof → submit
```

## Key Dependencies

| Crate | Role |
|-------|------|
| `qfc-inference` | InferenceEngine trait, backends (CPU/Metal/CUDA/ONNX), ModelCache, ModelScheduler |
| `qfc-crypto` | Ed25519 keypair, Blake3 hashing, proof signing |
| `qfc-types` | Hash, Address, core type definitions |
| `candle-core` | Tensor computation (when `candle` feature enabled) |
| `candle-transformers` | Model implementations (BERT, Qwen, Whisper) |
| `hf-hub` | HuggingFace model downloading |

## Reward Economics

- Block rewards are split: **60% producer, 25% voters, 15% miner pool**
- Miner pool is distributed **proportional to FLOPS** contributed in that block
- Different task types have fee multipliers: Embedding 1x, TextGen 1x, Whisper 2x, ImageGen 5x
- Estimated reward is returned in `ProofResult.reward_estimate` (actual settlement at block time)
