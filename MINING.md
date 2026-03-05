# QFC AI Inference Mining Guide (macOS)

This guide explains how to compile and run the QFC miner with real AI inference on macOS with Apple Silicon GPU acceleration.

## System Requirements

- **macOS 13 (Ventura)** or later
- **Apple Silicon** (M1 / M2 / M3 / M4) — Intel Macs can use CPU-only mode
- **Xcode Command Line Tools**: `xcode-select --install`
- **Rust 1.75+**: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
- **16 GB+ RAM** recommended (8 GB minimum for Cold tier)

## Build the Miner

```bash
cd qfc-core

# Apple Silicon (Metal GPU + candle ML backend)
cargo build --release --features metal,candle --bin qfc-miner

# CPU-only (Intel Mac or no GPU acceleration)
cargo build --release --features candle --bin qfc-miner
```

The first build takes several minutes. The `candle` feature enables real BERT model inference; the `metal` feature enables Apple Metal GPU acceleration.

## Build the Node

If you want to run your own local validator node:

```bash
cargo build --release --features metal,candle --bin qfc-node
```

## Local Testing (Dev Mode)

### 1. Start a dev node

```bash
# Terminal 1: Start a dev node with mining enabled
cargo run --release --features metal,candle --bin qfc-node -- \
  --dev --mine --no-network --compute-mode inference
```

The `--dev` flag creates a single-node chain with auto-block-production. `--no-network` disables P2P (for local testing only).

### 2. Connect the miner

```bash
# Terminal 2: Run the miner pointing to the local node
cargo run --release --features metal,candle --bin qfc-miner -- \
  --validator-rpc http://127.0.0.1:8545 \
  --wallet 0000000000000000000000000000000000000001 \
  --backend auto
```

### 3. Verify it works

```bash
# Terminal 3: Check block production
curl -s http://127.0.0.1:8545 -X POST -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}'

# Check epoch info
curl -s http://127.0.0.1:8545 -X POST -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","method":"qfc_getEpoch","params":[],"id":1}'
```

## Connect to Testnet

```bash
# Point the miner at the public testnet RPC
./target/release/qfc-miner \
  --validator-rpc https://rpc.testnet.qfc.network \
  --wallet <YOUR_WALLET_ADDRESS> \
  --backend auto
```

Replace `<YOUR_WALLET_ADDRESS>` with your 40-character hex address (no `0x` prefix).

## Expected Behavior

1. **Model download** (~500 MB): On first run, the miner downloads BERT model weights from Hugging Face. Cached in `~/.cache/huggingface/` for subsequent runs.
2. **Task fetching**: The miner polls the validator node every 10 seconds for inference tasks.
3. **Inference execution**: Tasks are executed using Metal GPU (or CPU fallback). Typical embedding tasks complete in 50-300ms.
4. **Proof submission**: Results are hashed and submitted as `InferenceProof` to the validator.
5. **Spot-check** (~5%): Validators randomly re-execute proofs. Honest miners always pass.

## GPU Tiers

Your hardware determines which tasks you can accept:

| Tier | Memory | Hardware Examples | Task Types |
|------|--------|-------------------|------------|
| **Hot** | 32 GB+ | M2 Ultra, M3 Max (96GB) | All tasks, large LLMs |
| **Warm** | 16-31 GB | M1 Pro, M2 Pro, M3 Pro | Medium models, embeddings |
| **Cold** | < 16 GB | M1, M2, M3 (base) | Small models, embeddings |

The miner auto-detects your tier and only accepts tasks within your capability.

## Docker (Linux / CI)

Docker images use CPU-only candle (no Metal in containers):

```bash
# Build the node image (includes candle for CPU inference)
docker build -t qfc-node .

# Build the standalone miner image
docker build -f Dockerfile.miner -t qfc-miner .
```

## Troubleshooting

### `Metal (not compiled with metal feature)`

You compiled without `--features metal`. Rebuild:
```bash
cargo build --release --features metal,candle --bin qfc-miner
```

### `Model not found` or download errors

- Check your internet connection (model weights are ~500 MB)
- Ensure `~/.cache/huggingface/` is writable
- Try setting `HF_HOME` to a custom directory: `export HF_HOME=/path/to/cache`

### `Insufficient memory`

Your system doesn't have enough RAM for the requested model. The miner will auto-select tasks matching your tier, but if you're running other heavy applications, free up memory.

### Miner exits with `Failed to fetch task`

- Verify the RPC URL is reachable: `curl http://127.0.0.1:8545`
- For testnet: ensure you have network access to the RPC endpoint
- The node may not have inference tasks available yet (wait for next epoch)

### Compilation errors with `candle`

- Ensure Xcode CLT is installed: `xcode-select --install`
- Update Rust: `rustup update stable`
- Clean build: `cargo clean && cargo build --release --features metal,candle --bin qfc-miner`

### Spot-check failures (honest miner)

If you see "Spot-check FAILED" as an honest miner, this is a bug — please report it. The v2.0 branch includes a fix for the synthetic task reconstruction issue that caused false positives.
