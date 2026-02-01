#!/bin/bash
# Check status of local testnet nodes

NODES=("http://127.0.0.1:8545" "http://127.0.0.1:8546" "http://127.0.0.1:8547")
NAMES=("Node1" "Node2" "Node3")

echo "QFC Local Testnet Status"
echo "========================"
echo ""

for i in "${!NODES[@]}"; do
    node="${NODES[$i]}"
    name="${NAMES[$i]}"

    # Get block number
    block_response=$(curl -s "$node" -X POST -H "Content-Type: application/json" \
        -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' 2>/dev/null)

    if [ $? -eq 0 ] && [ -n "$block_response" ]; then
        block_hex=$(echo "$block_response" | jq -r '.result // "error"' 2>/dev/null)
        if [ "$block_hex" != "error" ] && [ "$block_hex" != "null" ]; then
            block_dec=$((block_hex))

            # Get node info
            info_response=$(curl -s "$node" -X POST -H "Content-Type: application/json" \
                -d '{"jsonrpc":"2.0","method":"qfc_nodeInfo","params":[],"id":1}' 2>/dev/null)
            peers=$(echo "$info_response" | jq -r '.result.peer_count // 0' 2>/dev/null)

            echo "$name: Block #$block_dec | Peers: $peers | $node"
        else
            echo "$name: Error response | $node"
        fi
    else
        echo "$name: OFFLINE | $node"
    fi
done

echo ""
echo "Check sync status:"
echo "  All nodes should have the same block number if synced."
