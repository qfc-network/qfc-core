# QFC Node Dockerfile
# Multi-stage build for smaller image size

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

# Copy source
COPY . .

# Build release binary
RUN cargo build --release --bin qfc-node

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

# Copy binary from builder
COPY --from=builder /build/target/release/qfc-node /usr/local/bin/qfc-node

# Create data directory
RUN mkdir -p /data /config

# Environment variables
ENV QFC_DATA_DIR=/data
ENV QFC_RPC_ADDR=0.0.0.0:8545
ENV QFC_P2P_ADDR=0.0.0.0:30303
ENV QFC_LOG_LEVEL=info
ENV RUST_LOG=info

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
