# QFC Node Dockerfile
# Uses cargo-chef for dependency caching across CI runners

# syntax=docker/dockerfile:1

# ============================================
# Stage 1: Chef planner — analyse dependencies
# ============================================
FROM rust:1-bookworm AS chef
RUN cargo install cargo-chef --locked
WORKDIR /build

# ============================================
# Stage 2: Prepare recipe (depends only on Cargo.toml / Cargo.lock)
# ============================================
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ============================================
# Stage 3: Cook dependencies (cached Docker layer)
# ============================================
FROM chef AS builder

# Install build dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    libclang-dev \
    cmake \
    && rm -rf /var/lib/apt/lists/*

# Cook dependencies — this layer is cached until Cargo.toml/Cargo.lock change
COPY --from=planner /build/recipe.json recipe.json
RUN cargo chef cook --release --features candle --recipe-path recipe.json

# Copy source and build actual binaries (only project crates recompile)
COPY . .
RUN cargo build --release --features candle --bin qfc-node --bin qfc-miner

# ============================================
# Stage 4: Runtime
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
COPY --from=builder /build/target/release/qfc-node /usr/local/bin/qfc-node
COPY --from=builder /build/target/release/qfc-miner /usr/local/bin/qfc-miner

# Create data directory
RUN mkdir -p /data /config /models

# Environment defaults (read directly by clap via env attributes)
ENV QFC_DATA_DIR=/data
ENV QFC_RPC_ADDR=0.0.0.0:8545
ENV QFC_P2P_PORT=30303
ENV QFC_LOG_LEVEL=info
ENV RUST_LOG=info
ENV QFC_METRICS_ADDR=0.0.0.0:6060
ENV QFC_COMPUTE_MODE=pow
ENV QFC_INFERENCE_BACKEND=auto
ENV QFC_MODEL_DIR=/models
# Set QFC_MINER_MODE=true to run qfc-miner instead of qfc-node
ENV QFC_MINER_MODE=false

# Expose ports
EXPOSE 8545 8546 30303 6060

# Health check
HEALTHCHECK --interval=30s --timeout=10s --start-period=5s --retries=3 \
    CMD curl -f http://localhost:8545 -X POST -H "Content-Type: application/json" \
        -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' || exit 1

# Entrypoint: clap reads QFC_* env vars directly, no manual translation needed
COPY <<'EOF' /entrypoint.sh
#!/bin/bash
set -e

if [ "$QFC_MINER_MODE" = "true" ] || [ "$QFC_MINER_MODE" = "1" ]; then
    # Miner mode: clap reads QFC_MINER_* env vars directly
    echo "Starting QFC miner (wallet: ${QFC_MINER_WALLET:-not set})"
    exec qfc-miner "$@"
else
    # Node mode: clap reads QFC_* env vars directly
    echo "Starting QFC node (validator: ${QFC_VALIDATOR_KEY:+yes}${QFC_VALIDATOR_KEY:-no})"
    exec qfc-node "$@"
fi
EOF

RUN chmod +x /entrypoint.sh

ENTRYPOINT ["/entrypoint.sh"]
