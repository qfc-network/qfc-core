# QFC Node Dockerfile
# Multi-stage build with dependency caching for fast rebuilds

# syntax=docker/dockerfile:1

# ============================================
# Stage 1: Build
# ============================================
FROM rust:1.75-bookworm AS builder

WORKDIR /build

# Install build dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    libclang-dev \
    cmake \
    && rm -rf /var/lib/apt/lists/*

# Copy manifests first (changes rarely → cached layer)
COPY Cargo.toml ./
COPY Cargo.loc[k] ./
COPY crates/qfc-types/Cargo.toml crates/qfc-types/Cargo.toml
COPY crates/qfc-crypto/Cargo.toml crates/qfc-crypto/Cargo.toml
COPY crates/qfc-storage/Cargo.toml crates/qfc-storage/Cargo.toml
COPY crates/qfc-trie/Cargo.toml crates/qfc-trie/Cargo.toml
COPY crates/qfc-state/Cargo.toml crates/qfc-state/Cargo.toml
COPY crates/qfc-executor/Cargo.toml crates/qfc-executor/Cargo.toml
COPY crates/qfc-mempool/Cargo.toml crates/qfc-mempool/Cargo.toml
COPY crates/qfc-consensus/Cargo.toml crates/qfc-consensus/Cargo.toml
COPY crates/qfc-pow/Cargo.toml crates/qfc-pow/Cargo.toml
COPY crates/qfc-chain/Cargo.toml crates/qfc-chain/Cargo.toml
COPY crates/qfc-network/Cargo.toml crates/qfc-network/Cargo.toml
COPY crates/qfc-rpc/Cargo.toml crates/qfc-rpc/Cargo.toml
COPY crates/qfc-node/Cargo.toml crates/qfc-node/Cargo.toml
COPY crates/qfc-inference/Cargo.toml crates/qfc-inference/Cargo.toml
COPY crates/qfc-ai-coordinator/Cargo.toml crates/qfc-ai-coordinator/Cargo.toml
COPY crates/qfc-miner/Cargo.toml crates/qfc-miner/Cargo.toml
COPY crates/qfc-lsp/Cargo.toml crates/qfc-lsp/Cargo.toml
COPY crates/qfc-qsc/Cargo.toml crates/qfc-qsc/Cargo.toml
COPY crates/qfc-qvm/Cargo.toml crates/qfc-qvm/Cargo.toml

# Create stub lib.rs / main.rs for each crate so cargo can resolve deps
RUN find crates -name Cargo.toml -exec sh -c ' \
    dir=$(dirname "$1"); \
    mkdir -p "$dir/src"; \
    if grep -q "\\[\\[bin\\]\\]" "$1" || [ "$(basename "$dir")" = "qfc-node" ] || [ "$(basename "$dir")" = "qfc-miner" ]; then \
        echo "fn main() {}" > "$dir/src/main.rs"; \
    fi; \
    echo "" > "$dir/src/lib.rs" \
    ' _ {} \;

# Build dependencies only (cached unless Cargo.toml/Cargo.lock change)
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release --features candle --bin qfc-node --bin qfc-miner 2>&1 || true

# Now copy real source code
COPY . .

# Touch all source files to invalidate the stub builds but keep dep artifacts
RUN find crates -name "*.rs" -exec touch {} +

# Build actual binaries (only recompiles project crates, deps are cached)
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release --features candle --bin qfc-node --bin qfc-miner \
    && cp /build/target/release/qfc-node /usr/local/bin/qfc-node \
    && cp /build/target/release/qfc-miner /usr/local/bin/qfc-miner

# ============================================
# Stage 2: Runtime
# ============================================
FROM debian:bookworm-slim

WORKDIR /app

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    curl \
    && rm -rf /var/lib/apt/lists/*

# Copy binaries from builder
COPY --from=builder /usr/local/bin/qfc-node /usr/local/bin/qfc-node
COPY --from=builder /usr/local/bin/qfc-miner /usr/local/bin/qfc-miner

# Create data directory
RUN mkdir -p /data /config /models

# Environment variables
ENV QFC_DATA_DIR=/data
ENV QFC_RPC_ADDR=0.0.0.0:8545
ENV QFC_P2P_ADDR=0.0.0.0:30303
ENV QFC_LOG_LEVEL=info
ENV RUST_LOG=info
# v2.0: Compute mode (pow | inference, default: pow)
ENV QFC_COMPUTE_MODE=pow
ENV QFC_INFERENCE_BACKEND=auto
ENV QFC_MODEL_DIR=/models

# Expose ports
EXPOSE 8545 8546 30303 6060

# Health check
HEALTHCHECK --interval=30s --timeout=10s --start-period=5s --retries=3 \
    CMD curl -f http://localhost:8545 -X POST -H "Content-Type: application/json" \
        -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' || exit 1

# Entrypoint script
COPY <<'EOF' /entrypoint.sh
#!/bin/bash
set -e

# Build command arguments
ARGS="--datadir ${QFC_DATA_DIR:-/data}"
ARGS="$ARGS --rpc-addr ${QFC_RPC_ADDR:-0.0.0.0:8545}"
ARGS="$ARGS --p2p-port ${QFC_P2P_PORT:-30303}"
ARGS="$ARGS --log-level ${QFC_LOG_LEVEL:-info}"

# Add validator key if provided
if [ -n "$QFC_VALIDATOR_KEY" ]; then
    # Remove 0x prefix if present
    KEY="${QFC_VALIDATOR_KEY#0x}"
    ARGS="$ARGS --validator $KEY"
fi

# Enable mining if requested
if [ "$QFC_MINING_ENABLED" = "true" ] || [ "$QFC_MINING_ENABLED" = "1" ]; then
    ARGS="$ARGS --mine"
    if [ -n "$QFC_MINING_THREADS" ]; then
        ARGS="$ARGS --threads $QFC_MINING_THREADS"
    fi
    # v2.0: Compute mode and inference settings
    if [ -n "$QFC_COMPUTE_MODE" ]; then
        ARGS="$ARGS --compute-mode $QFC_COMPUTE_MODE"
    fi
    if [ -n "$QFC_INFERENCE_BACKEND" ]; then
        ARGS="$ARGS --inference-backend $QFC_INFERENCE_BACKEND"
    fi
    if [ -n "$QFC_MODEL_DIR" ]; then
        ARGS="$ARGS --model-dir $QFC_MODEL_DIR"
    fi
fi

# Add bootnodes if provided
if [ -n "$QFC_BOOTNODES" ]; then
    for node in $(echo $QFC_BOOTNODES | tr ',' ' '); do
        ARGS="$ARGS --bootnodes $node"
    done
fi

# Dev mode
if [ "$QFC_DEV_MODE" = "true" ] || [ "$QFC_DEV_MODE" = "1" ]; then
    ARGS="$ARGS --dev"
fi

# Disable network if requested
if [ "$QFC_NO_NETWORK" = "true" ] || [ "$QFC_NO_NETWORK" = "1" ]; then
    ARGS="$ARGS --no-network"
fi

echo "Starting QFC node with: qfc-node $ARGS"
exec qfc-node $ARGS
EOF

RUN chmod +x /entrypoint.sh

ENTRYPOINT ["/entrypoint.sh"]
