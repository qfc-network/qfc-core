#!/bin/bash
# Run a 3-validator testnet

set -e

# Validator keys (Ed25519 secret keys matching genesis validators)
# Set these env vars before running, or pass as arguments.
# Generate with: cargo run --example keygen -p qfc-crypto
VAL1_KEY="${VAL1_KEY:?Set VAL1_KEY env var (32-byte hex)}"
VAL2_KEY="${VAL2_KEY:?Set VAL2_KEY env var (32-byte hex)}"
VAL3_KEY="${VAL3_KEY:?Set VAL3_KEY env var (32-byte hex)}"

# Data directories
DATA1="/tmp/qfc_val1"
DATA2="/tmp/qfc_val2"
DATA3="/tmp/qfc_val3"

# Clean up
rm -rf $DATA1 $DATA2 $DATA3

# Build
echo "Building..."
cargo build --release

# Start validator 1 (dev mode - has the dev genesis validator)
echo "Starting validator 1..."
RUST_LOG=info ./target/release/qfc-node \
  --dev \
  --datadir $DATA1 \
  --rpc --rpc-addr 127.0.0.1:8545 \
  --p2p-port 30303 \
  > /tmp/val1.log 2>&1 &
VAL1_PID=$!
sleep 3

# Get validator 1's peer ID
VAL1_PEER=$(grep "Local peer ID" /tmp/val1.log | awk '{print $NF}')
echo "Validator 1 peer ID: $VAL1_PEER"

# Start validator 2
echo "Starting validator 2..."
RUST_LOG=info ./target/release/qfc-node \
  --validator $VAL2_KEY \
  --datadir $DATA2 \
  --rpc --rpc-addr 127.0.0.1:8546 \
  --p2p-port 30304 \
  --bootnodes "/ip4/127.0.0.1/tcp/30303/p2p/$VAL1_PEER" \
  > /tmp/val2.log 2>&1 &
VAL2_PID=$!

# Start validator 3
echo "Starting validator 3..."
RUST_LOG=info ./target/release/qfc-node \
  --validator $VAL3_KEY \
  --datadir $DATA3 \
  --rpc --rpc-addr 127.0.0.1:8547 \
  --p2p-port 30305 \
  --bootnodes "/ip4/127.0.0.1/tcp/30303/p2p/$VAL1_PEER" \
  > /tmp/val3.log 2>&1 &
VAL3_PID=$!

echo "All validators started!"
echo "PIDs: $VAL1_PID, $VAL2_PID, $VAL3_PID"
echo ""
echo "Logs:"
echo "  tail -f /tmp/val1.log"
echo "  tail -f /tmp/val2.log"
echo "  tail -f /tmp/val3.log"
echo ""
echo "RPC endpoints:"
echo "  Validator 1: http://127.0.0.1:8545"
echo "  Validator 2: http://127.0.0.1:8546"
echo "  Validator 3: http://127.0.0.1:8547"
echo ""
echo "Press Ctrl+C to stop all validators"

# Wait and monitor
sleep 5
echo ""
echo "Checking block heights..."
for i in 1 2 3; do
  port=$((8544 + i))
  height=$(curl -s http://127.0.0.1:$port -X POST -H "Content-Type: application/json" \
    -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' | grep -o '"result":"[^"]*"' | cut -d'"' -f4)
  echo "  Validator $i: $height"
done

# Wait for user interrupt
trap "kill $VAL1_PID $VAL2_PID $VAL3_PID 2>/dev/null; echo 'Stopped all validators'" EXIT
wait
