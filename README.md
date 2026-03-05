# QFC Core

High-performance blockchain engine for QFC Network, built in Rust.

## Features

- **Proof of Contribution (PoC)** consensus with 7-dimension scoring
- **EVM compatible** via revm for Solidity smart contracts
- **QuantumScript VM** with JIT compilation and LSP support
- **Blake3 PoW mining** for compute contribution (20% weight)
- **libp2p networking** with GossipSub and Kademlia
- **Ethereum-compatible JSON-RPC** API (eth_* + qfc_* methods)
- **Delegation & staking** with slashing and jailing

## Architecture

```
crates/
├── qfc-node         # Main binary entry point
├── qfc-types        # Core types (Block, Transaction, Account)
├── qfc-crypto       # Blake3, Ed25519, VRF
├── qfc-storage      # RocksDB persistence
├── qfc-trie         # Merkle Patricia Trie
├── qfc-state        # State management
├── qfc-executor     # Transaction execution (EVM)
├── qfc-mempool      # Transaction pool
├── qfc-consensus    # PoC consensus engine
├── qfc-pow          # Blake3 PoW mining
├── qfc-chain        # Blockchain management
├── qfc-network      # P2P networking (libp2p)
├── qfc-rpc          # JSON-RPC server
├── qfc-qsc          # QuantumScript compiler
├── qfc-qvm          # QuantumScript VM
└── qfc-lsp          # Language Server Protocol
```

## Quick Start

```bash
# Prerequisites: Rust 1.75+, libclang-dev (Linux)

# Build
cargo build --release

# Run dev node (auto-produces blocks)
cargo run --bin qfc-node -- --dev

# Run with mining enabled
cargo run --bin qfc-node -- --dev --mine --threads 4
```

## CLI Options

```
--dev                       Development mode (single auto-validator)
--validator <HEX_KEY>       Validator mode with Ed25519 secret key
--mine                      Enable Blake3 PoW mining
--threads <N>               Mining threads (default: CPU count)
--rpc-addr <ADDR>           RPC listen address (default: 127.0.0.1:8545)
--p2p-port <PORT>           P2P port (default: 30303)
--bootnodes <MULTIADDR>     Bootstrap peer addresses
--no-network                Disable P2P networking
--datadir <PATH>            Data directory (default: ./data)
--chain-id <ID>             Chain ID (default: 9000)
--log-level <LEVEL>         Log level (default: info)
```

## Docker

```bash
# Build
docker build -t qfc-node .

# Run dev node
docker run -p 8545:8545 -p 30303:30303 -e QFC_DEV_MODE=true qfc-node

# Run validator
docker run -p 8545:8545 -p 30303:30303 \
  -e QFC_VALIDATOR_KEY=<hex> \
  -e QFC_MINING_ENABLED=true \
  -v qfc-data:/data qfc-node
```

## RPC API

Ethereum-compatible endpoints:

```bash
curl -X POST http://127.0.0.1:8545 \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}'
```

| Method | Description |
|--------|-------------|
| `eth_blockNumber` | Latest block number |
| `eth_getBalance` | Account balance |
| `eth_sendRawTransaction` | Broadcast signed transaction |
| `eth_call` | Read-only contract call |
| `qfc_getValidators` | List validators with scores |
| `qfc_getContributionScore` | Validator contribution score |
| `qfc_getEpoch` | Current epoch info |
| `qfc_nodeInfo` | Node status |

## Network Configuration

| Network | Chain ID | RPC URL |
|---------|----------|---------|
| Testnet | 9000 (0x2328) | https://rpc.testnet.qfc.network |
| Mainnet | 9001 (0x2329) | https://rpc.qfc.network |

## License

MIT OR Apache-2.0
