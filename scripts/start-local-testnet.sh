#!/bin/bash
# Start a local 3-node testnet

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
DATA_DIR="${PROJECT_DIR}/testnet-data"
LOG_DIR="${DATA_DIR}/logs"

# Node configurations
NODE1_PORT=30303
NODE1_RPC=8545
NODE1_SECRET="1111111111111111111111111111111111111111111111111111111111111111"

NODE2_PORT=30304
NODE2_RPC=8546
NODE2_SECRET="2222222222222222222222222222222222222222222222222222222222222222"

NODE3_PORT=30305
NODE3_RPC=8547
NODE3_SECRET="3333333333333333333333333333333333333333333333333333333333333333"

cleanup() {
    echo "Stopping nodes..."
    pkill -f "qfc-node" 2>/dev/null || true
    exit 0
}

trap cleanup SIGINT SIGTERM

# Clean up previous run
rm -rf "$DATA_DIR"
mkdir -p "$LOG_DIR"

# Build if needed
echo "Building qfc-node..."
cd "$PROJECT_DIR"
cargo build --bin qfc-node --release

NODE_BIN="$PROJECT_DIR/target/release/qfc-node"

echo "Starting Node 1 (validator)..."
$NODE_BIN \
    --datadir "$DATA_DIR/node1" \
    --p2p-port $NODE1_PORT \
    --rpc-addr "127.0.0.1:$NODE1_RPC" \
    --validator "$NODE1_SECRET" \
    --log-level info \
    > "$LOG_DIR/node1.log" 2>&1 &
NODE1_PID=$!
echo "Node 1 started (PID: $NODE1_PID)"

# Wait for node1 to start listening
sleep 2

# Get node1's multiaddr for bootnodes
# In a real setup, we'd discover this dynamically
NODE1_MULTIADDR="/ip4/127.0.0.1/tcp/$NODE1_PORT"

echo "Starting Node 2 (validator)..."
$NODE_BIN \
    --datadir "$DATA_DIR/node2" \
    --p2p-port $NODE2_PORT \
    --rpc-addr "127.0.0.1:$NODE2_RPC" \
    --validator "$NODE2_SECRET" \
    --bootnodes "$NODE1_MULTIADDR" \
    --log-level info \
    > "$LOG_DIR/node2.log" 2>&1 &
NODE2_PID=$!
echo "Node 2 started (PID: $NODE2_PID)"

echo "Starting Node 3 (validator)..."
$NODE_BIN \
    --datadir "$DATA_DIR/node3" \
    --p2p-port $NODE3_PORT \
    --rpc-addr "127.0.0.1:$NODE3_RPC" \
    --validator "$NODE3_SECRET" \
    --bootnodes "$NODE1_MULTIADDR" \
    --log-level info \
    > "$LOG_DIR/node3.log" 2>&1 &
NODE3_PID=$!
echo "Node 3 started (PID: $NODE3_PID)"

echo ""
echo "============================================="
echo "Local testnet started!"
echo "============================================="
echo "Node 1: RPC http://127.0.0.1:$NODE1_RPC  P2P :$NODE1_PORT"
echo "Node 2: RPC http://127.0.0.1:$NODE2_RPC  P2P :$NODE2_PORT"
echo "Node 3: RPC http://127.0.0.1:$NODE3_RPC  P2P :$NODE3_PORT"
echo ""
echo "Logs: $LOG_DIR/"
echo ""
echo "Test with:"
echo "  curl -s http://127.0.0.1:$NODE1_RPC -X POST -H 'Content-Type: application/json' -d '{\"jsonrpc\":\"2.0\",\"method\":\"eth_blockNumber\",\"params\":[],\"id\":1}'"
echo ""
echo "Press Ctrl+C to stop all nodes"
echo ""

# Wait for all nodes
wait
