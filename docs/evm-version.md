# QFC EVM 版本分析与升级方案

## 当前状态

### 实际情况：运行时已经是 LATEST（>= Cancun）

QFC 使用 **revm v8.0** 作为 EVM 引擎。关键发现：

```
revm-primitives v3.1.1 — SpecId 枚举：
  CANCUN = 17,
  #[default]
  LATEST = u8::MAX   ← QFC 当前使用的默认值
```

在 `crates/qfc-executor/src/evm.rs` 中，`create_evm()` 没有设置任何 `spec_id`：

```rust
fn create_evm(&self, db) -> Evm {
    let mut evm = Evm::builder().with_db(db).build();  // 默认 LATEST
    evm.block_mut().number = ...;
    evm.cfg_mut().chain_id = self.chain_id;
    // 没有设置 spec_id → 使用默认 LATEST
    evm
}
```

这意味着 **QFC 节点实际上已经支持所有操作码**，包括：
- `PUSH0`（Shanghai）
- `MCOPY`、`TSTORE`、`TLOAD`（Cancun）

### 但是：合约编译端锁在了 Paris

`qfc-contracts/hardhat.config.ts` 设置了 `evmVersion: "paris"`（通过 Solidity 默认值），导致：
- 编译产物不使用 `PUSH0`（每次用 `PUSH1 0` 替代，多花 gas）
- 不使用 `MCOPY`、`TSTORE`/`TLOAD` 等优化
- OpenZeppelin 被迫降级到 5.1.0（5.2+ 用了 `mcopy`）

### 结论

| 层 | 当前版本 | 实际能力 |
|----|---------|---------|
| revm（运行时） | LATEST（>= Cancun） | 全部操作码可用 |
| Solidity 编译 | Paris | 限制了编译产物 |
| OpenZeppelin | 5.1.0（降级） | 被迫回退 |

**节点层面不需要任何改动**，只需要升级编译端配置。

---

## 升级方案

### 方案 A：仅升级编译端（推荐，零风险）

因为 revm 运行时已经是 LATEST，只需要改合约编译配置：

**1. `qfc-contracts/hardhat.config.ts`**
```typescript
solidity: {
  version: "0.8.24",
  settings: {
    evmVersion: "cancun",  // 从 paris → cancun
    optimizer: { enabled: true, runs: 200 },
    viaIR: true,
  },
},
```

**2. 升级 OpenZeppelin**
```bash
npm install @openzeppelin/contracts@^5.6.0  # 回到最新版
```

**3. `qfc-explorer-api` 合约验证**
- 更新 solc 验证参数的 `evmVersion` 为 `"cancun"`

**好处：**
- 零停机，无需升级节点
- PUSH0 节省 ~2-5% gas
- 使用最新 OpenZeppelin
- 瞬态存储（TSTORE/TLOAD）可用于 reentrancy guard 优化

### 方案 B：显式设置 SpecId（长期推荐）

当前 revm 默认 LATEST 意味着 QFC 的 EVM 行为随 revm 版本升级而隐式变化。应该显式锁定：

**`crates/qfc-executor/src/evm.rs`**
```rust
use revm::primitives::SpecId;

fn create_evm(&self, db) -> Evm {
    let mut evm = Evm::builder().with_db(db).build();
    evm.cfg_mut().spec_id = SpecId::CANCUN;  // 显式锁定
    // ...
}
```

**好处：**
- EVM 行为可预测，不随 revm 版本变化
- 未来升级 revm 时不会意外启用 Prague 等新特性
- 可以按区块高度实现硬分叉（不同高度用不同 SpecId）

### 方案 C：实现硬分叉机制（远期）

为未来的链升级做准备：

```rust
fn spec_for_block(height: u64) -> SpecId {
    if height >= PRAGUE_FORK_HEIGHT {
        SpecId::PRAGUE
    } else if height >= CANCUN_FORK_HEIGHT {
        SpecId::CANCUN
    } else {
        SpecId::CANCUN  // genesis 即 Cancun
    }
}
```

---

## 操作码对照表

| 操作码 | EVM 版本 | 用途 | Gas 节省 |
|--------|---------|------|---------|
| `PUSH0` | Shanghai | 压入 0 值 | 每次省 3 gas（vs PUSH1 0） |
| `MCOPY` | Cancun | 内存复制 | 替代 mload+mstore 循环 |
| `TSTORE` | Cancun | 瞬态存储写入 | ~100 gas（vs SSTORE 20000） |
| `TLOAD` | Cancun | 瞬态存储读取 | ~100 gas（vs SLOAD 2100） |
| `BLOBHASH` | Cancun | Blob 交易哈希 | QFC 不用 blob，可忽略 |
| `BLOBBASEFEE` | Cancun | Blob 基础费 | 同上 |

### Prague（EIP-7692 EOF）— 暂不需要

Prague 引入 EOF（EVM Object Format）和新操作码 `EXTCALL`、`RETURNDATALOAD` 等。生态尚未广泛采用，建议暂缓。

---

## 执行计划

| 步骤 | 内容 | 复杂度 | 影响 |
|------|------|--------|------|
| 1 | hardhat.config.ts 改 `evmVersion: "cancun"` | 1 行 | 新合约用 Cancun 编译 |
| 2 | 升级 OpenZeppelin 到 5.6.x | `npm install` | 可用最新库 |
| 3 | qfc-executor 显式设 `SpecId::CANCUN` | 1 行 | 行为可预测 |
| 4 | qfc-explorer-api 合约验证更新 evmVersion | 1 行 | 验证兼容 |
| 5 | 重新编译并测试所有合约 | 测试 | 确认兼容 |
| 6 | 已部署合约不受影响 | 无 | Paris 编译的字节码在 Cancun 上完全兼容 |

**向后兼容性：** Paris 编译的合约在 Cancun 运行时上 100% 兼容，无需重新部署。

---

## 相关文件

| 文件 | 说明 |
|------|------|
| `crates/qfc-executor/src/evm.rs:234-250` | EVM 实例创建（无 SpecId） |
| `Cargo.toml:61` | revm v8.0 依赖 |
| `~/.cargo/.../revm-primitives-3.1.1/src/specification.rs` | SpecId 定义，默认 LATEST |
| `qfc-contracts/hardhat.config.ts` | Solidity 编译配置 |
| `qfc-explorer-api/src/routes/contracts.ts` | 合约验证 solc 参数 |
