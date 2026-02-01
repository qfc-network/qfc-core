# QFC Core

QFC (Quantum-Flux Chain) 区块链核心实现。

## 项目结构

```
qfc-core/
├── Cargo.toml                 # Workspace 配置
├── crates/
│   ├── qfc-types/             # 核心类型 (Hash, Address, Block, Transaction)
│   ├── qfc-crypto/            # 加密 (Blake3, Ed25519, VRF)
│   ├── qfc-storage/           # RocksDB 存储层
│   ├── qfc-trie/              # Merkle Patricia Trie
│   ├── qfc-state/             # 状态管理
│   ├── qfc-executor/          # 交易执行
│   ├── qfc-mempool/           # 交易池
│   ├── qfc-consensus/         # PoC 共识引擎
│   ├── qfc-chain/             # 链管理
│   ├── qfc-network/           # P2P 网络 (libp2p)
│   ├── qfc-rpc/               # JSON-RPC API
│   └── qfc-node/              # 节点主程序
```

## 常用命令

```bash
# 构建
cargo build
cargo build --release

# 测试
cargo test --all

# 运行开发节点 (自动出块)
cargo run --bin qfc-node -- --dev

# 运行验证者节点
cargo run --bin qfc-node -- --validator <SECRET_KEY_HEX> --p2p-port 30303

# 连接到其他节点
cargo run --bin qfc-node -- --bootnodes "/ip4/127.0.0.1/tcp/30303/p2p/<PEER_ID>"

# 禁用 P2P 网络
cargo run --bin qfc-node -- --dev --no-network

# RPC 测试
curl -s http://127.0.0.1:8545 -X POST -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}'
```

## 技术栈

- **哈希**: Blake3
- **签名**: Ed25519
- **VRF**: Ed25519-based VRF (区块生产者选择)
- **存储**: RocksDB
- **网络**: libp2p (GossipSub, Kademlia)
- **RPC**: jsonrpsee (兼容 Ethereum eth_* API)
- **序列化**: Borsh (内部), JSON (RPC)

## 共识机制

Proof of Contribution (PoC) - 多维度贡献评分:
- 质押权重: 30%
- 计算贡献: 20%
- 在线时长: 15%
- 验证准确率: 15%
- 网络贡献: 10%
- 存储贡献: 5%
- 历史信誉: 5%

## RPC 端点

### Ethereum 兼容 (eth_*)
- `eth_chainId` - 链 ID
- `eth_blockNumber` - 当前区块高度
- `eth_getBalance` - 查询余额
- `eth_sendRawTransaction` - 发送交易
- `eth_getBlockByNumber/Hash` - 查询区块
- `eth_getTransactionReceipt` - 交易收据

### QFC 特有 (qfc_*)
- `qfc_getValidators` - 验证者列表
- `qfc_getContributionScore` - 贡献分数
- `qfc_getValidatorScoreBreakdown` - 验证者分数详情 (含各维度分数)
- `qfc_getStake` - 质押金额
- `qfc_getEpoch` - 当前 epoch 信息
- `qfc_getFinalizedBlock` - 最终确认区块
- `qfc_getNetworkState` - 网络状态 (normal/congested/storage_shortage/under_attack)
- `qfc_nodeInfo` - 节点信息

## 开发状态

- [x] Phase 1: 基础框架 (单节点出块)
- [x] Phase 2: 网络同步 (多节点测试网)
  - [x] P2P 节点连接 (libp2p)
  - [x] 区块广播 (GossipSub)
  - [x] 区块同步协议 (request-response)
  - [x] 区块同步响应处理
  - [x] 初始同步 (sync from genesis)
  - [x] 创世验证者注册
  - [x] 交易广播与同步
- [x] Phase 3: 完整功能 (生产就绪)
  - [x] eth_call / eth_estimateGas 实现
  - [x] 多验证者测试 (3节点测试网)
  - [x] 集成测试
  - [x] 多维度 PoC 评分集成 (7维权重计算)
  - [x] VRF 证明验证 (区块验证安全性)
  - [x] 交易位置索引 (block_height, tx_index)
  - [x] 区块签名存储 (BlockBody)
  - [x] 同步状态报告 (SyncStatusProvider trait)
  - [x] Ed25519 公钥派生发送者地址
  - [x] 投票处理 (finality votes)
  - [x] 验证者消息处理 (heartbeat, epoch, slashing)

  - [x] EVM 集成 (revm - 智能合约执行)
  - [x] 状态剪枝 (StatePruner - 保留最近N个区块状态)
  - [x] 快照同步 (SnapSyncManager - 快速状态下载)

## 设计文档

参考 `../qfc-design/` 目录:
- `01-BLOCKCHAIN-DESIGN.md` - 数据结构和 RPC
- `02-CONSENSUS-MECHANISM.md` - PoC 共识算法
