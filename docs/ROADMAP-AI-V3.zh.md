# QFC AI v3 路线图 — 去中心化训练（Parameter Server）与分片推理

*🌐 [English](ROADMAP-AI-V3.md)*

**状态：** 进行中 — B-1 已交付、Feature A 核心（A0–A6）已交付、B-2 门控 spike 已完成。剩余工作（B-2 构建、Feature A 节点集成）受限于真实多区域 / 测试网基础设施，而非纯库代码。见 [进度](#进度)。
**Owner：** Larry。**创建：** 2026-06-10。**最后更新：** 2026-06-14。

## v2.x 留给我们的基础

本路线图所依托的组件，今天全部在线：

- `qfc-inference` — 多后端推理运行时（candle + ONNX Runtime 支撑 CPU / CUDA / Metal / ROCm / OpenCL）；治理批准的冻结模型（BERT、Qwen2、Whisper），从 HuggingFace 下载、IPFS CID 寻址。
- `qfc-ai-coordinator` — 任务池、按能力分配矿工（`assignment.rs`：backend、`GpuTier`、`memory_mb`、模型列表）、约 5% 抽查重执行验证（`verification.rs`、`redundant.rs`、`challenge.rs`）、治理模型注册表（`registry.rs`）、treasury。
- `qfc-storage` — RocksDB 引擎（18 个 CF、原子 `WriteBatch`、大端有序 key）——任何新组件复用的持久层。
- 共识集成 — 每 epoch 的 `WorkProof { flops_estimated, … }`、validator 上的 `inference_score`。

**下面两个 v3 特性共享一条原则：链上只存*承诺与激励*；繁重的 ML 数据在链下流动。** Parameter server 的松弛一致性永远不能进共识路径（状态必须可确定性重放），因此这里的所有设计都把 PS 形状的组件严格放在链下，用链上锚定。

---

## 进度

_更新于 2026-06-14。每个里程碑都按同一流程交付——专用 git worktree → 实现 → 对抗式评审 → 修复 → PR；每次评审都在合并前抓出并修复了真实问题（投毒、抽样刷分、free-jail 骚扰、整数溢出、聚合单位不匹配）。_

### 已交付

| 里程碑 | PR | 落地内容 |
|---|---|---|
| **B-1**（B0–B2） | [#102](https://github.com/qfc-network/qfc-core/pull/102) | ADR-0001；注册表分片 manifest；逐分片校验的可断点续传 IPFS 下载；跨版本分片缓存复用 |
| **A0** | [#107](https://github.com/qfc-network/qfc-core/pull/107) | ADR 0002–0008（**7 篇**，而非 5 篇——评审追加了数据可用性 + 验证经济学） |
| **A1 + A2** | [#107](https://github.com/qfc-network/qfc-core/pull/107) | `qfc-ps` crate：区间分片 `ShardStore`、SSP `SspClock`、`ShardService`；坐标级 trimmed-mean 聚合 + 带种子的投毒属性测试 |
| **A3** | [#110](https://github.com/qfc-network/qfc-core/pull/110) | `qfc-ai-coordinator` 中的训练作业类型 + 确定性 epoch 分配；按 worker 的梯度累加（n = worker 数，而非行数） |
| **A4** | [#114](https://github.com/qfc-network/qfc-core/pull/114) | 抽样训练步重执行；防刷分抽样（熵 = 已承诺的 `params_hash`）；精确比较 + 逐坐标容差带比较 |
| **A5** | [#120](https://github.com/qfc-network/qfc-core/pull/120) | ADR-0009；确定性 `settle_epoch` → 版本承诺 + 按比例奖励 + 罚没；`TrainingChainStore`；`SlashableOffense::InvalidTraining` |
| **A6** | [#122](https://github.com/qfc-network/qfc-core/pull/122) | ADR-0010；`qfc-training-pilot`——基于真实 A1–A5 API 的进程内 ≥3-miner 端到端试点（确定性 CPU 模型，作弊者被抓并罚没，loss 5.67→0.11） |
| **B-2 spike** | [#124](https://github.com/qfc-network/qfc-core/pull/124) | ADR-0011；真实 WAN 实测 + 可复现 calculator（`cargo run -p qfc-inference --example pipeline_latency`）；B-2 go/no-go |

### 未开始 — 受限于真实基础设施（非纯库代码）

- **B-2 构建（B3–B5）。** spike 结论（[ADR-0011](adr/0011-b2-pipeline-scope.md)）：交互式 WAN 推理是 **no-go**（RTT 主导——一个 7B 模型切 4 份，洲际间跑 100 token 光网络传输就要 ~37s）；批处理可行，但**受带宽门控、而非仅受 RTT 门控**（7B prefill 单跳在实测 20 Mbit/s 下约 12s）。B-2 重新定为**仅批处理** + 带宽 / 局部性感知分配 + 激活压缩。**开建前的硬门槛：** 用真实跨区 *miner-to-miner* 测量重跑 calculator——需要 ≥2 个位于不同区域的 miner（单区域测试网产生不了这种数据）。
- **Feature A 节点集成。** A6 在进程内证明了整个回路；要把它落到真实网络上，需要 `qfc-node`/`qfc-consensus`/`qfc-state`/`qfc-network` 改动 + 真实测试网：把 `settle_epoch` 输出应用到链状态（余额入账 + 一个绝对额罚没入口）、P2P 广播、N-of-M 运营者对规范结果 + 惩罚集合达成一致、VRF 抽样熵、按作业的质押锁定执行、签名的 `ParamUpdate`。这些在代码里都可 grep 为 `A6` / `live` / `node` 标记；试点就是它们的可执行规范。
- **真实多区域测试网试点**（A6 字面上的"测试网 ≥3 miner"形态）以及 **candle / BERT 级 GPU 模型 + A4b GPU 容差带验证**——这是外部基础设施加上 A4 刻意推迟的 GPU 确定性问题。

---

## 特性 A — 通过链下 Parameter Server 实现去中心化训练

### 为什么

v2.x 的矿工贡献的是*推理*（冻结权重）。自然的下一步是贡献*训练*：矿工在分配到的数据分片上计算梯度更新，网络把它们聚合成新的模型版本。这恰恰需要一个 parameter server——持有可变模型参数的分片 KV，聚合来自**不可信** worker 的更新。这是联邦学习 × 区块链的领域；QFC 已有的验证机制是差异化优势。

### 架构草图

```
            ┌── 链上 (qfc-core) ──────────────────────────────────┐
            │ 模型注册表: 版本承诺 (CID + hash)                     │
            │ 训练 epoch 记录: 谁贡献了、贡献了多少                  │
            │ PS 运营者与训练矿工的质押 / 罚没                       │
            └────────────────▲───────────────────────────────────┘
                             │ 版本提交 / 奖励 / 罚没
┌── 链下 ────────────────────┴───────────────────────────────────┐
│ qfc-ps（新 crate）: 分片参数服务                                 │
│   · key 区间 → 分片（ps-lite 风格的区间 push/pull）              │
│   · 聚合 = 拜占庭鲁棒规则（trimmed mean / Krum），               │
│     不是裸平均——worker 不可信                                   │
│   · 有界过期异步（SSP）；提交时 epoch 屏障                       │
│ 训练矿工: 拉参数 → 在数据分片上训练 → 推回                       │
│ 验证者: 对抽样训练步做重执行抽查                                 │
│   （推广自 proof_pool / redundant / challenge）                 │
└────────────────────────────────────────────────────────────────┘
```

同一架构的渲染版：

```mermaid
flowchart TB
    subgraph CHAIN["链上 (qfc-core)"]
        REG["模型注册表:<br/>版本承诺 (CID + hash)"]
        EPOCH["训练 epoch 记录:<br/>谁贡献了、贡献了多少"]
        STAKE["质押 / 罚没:<br/>PS 运营者与训练矿工"]
    end

    subgraph OFF["链下"]
        subgraph PS["qfc-ps（新 crate）— 分片参数服务"]
            AGG["拜占庭鲁棒聚合<br/>(trimmed mean / Krum — 禁用裸 FedAvg)"]
            SHARDS["key 区间分片<br/>(ps-lite 风格区间 push/pull,<br/>qfc-storage 作本地持久层)"]
        end
        MINERS["训练矿工:<br/>拉参数 → 在数据分片上训练 → 推回"]
        VERIF["验证者:<br/>对抽样训练步重执行<br/>(推广自 proof_pool / redundant / challenge)"]
    end

    MINERS <-->|"push / pull<br/>(SSP 有界过期)"| SHARDS
    SHARDS --- AGG
    VERIF -.->|"抽查梯度 hash<br/>(容差带)"| MINERS
    PS ==>|"epoch 屏障:<br/>提交新版本 (CID)"| REG
    VERIF -->|"接受 / 拒绝贡献"| EPOCH
    EPOCH --> STAKE
    STAKE -.->|"验证失败即罚没"| MINERS

    classDef chain fill:#e8f0fe,stroke:#1a56db;
    class REG,EPOCH,STAKE chain
```

关键设计决策（动代码前每条都需要一个 ADR）：

1. **谁来运行 PS 分片？** 提议：有质押的 PS 运营者（新角色，可罚没），像 validator 分 epoch 一样分配分片。备选：validator 兼任 PS 运营者（更简单，但耦合故障域）。
2. **聚合规则。** 裸 FedAvg 可被投毒。从坐标级 trimmed mean 起步；按成本评估 Krum/Bulyan。规则选择决定了恶意少数能把模型拉偏多少。
3. **贡献计量。** 复用 `flops_estimated` 风格的计量；按*被接受*（通过验证）的更新付酬，而不是按声称的工作量。
4. **验证。** 抽样重执行：验证者从已承诺的（参数、数据分片、seed）三元组重放矿工的训练步并核对梯度 hash。确定性要求固定 kernel/seed——最难的开放问题（GPU 非确定性）；兜底方案是容差带比较。
5. **同步模型。** epoch 内 SSP 小过期上界；epoch 结束时硬屏障 + 链上版本提交。RPO 故事与 Monolith 同构：每 epoch 快照，重放样本流。

### 里程碑（估算 = Claude session 小时，不是人力时间）

| # | 里程碑 | 交付物 | 估算（Claude session h） | 状态 |
|---|---|---|---|---|
| A0 | 决策 1–5 的 ADR | `docs/adr/`（实际交付 **7** 篇 ADR，0002–0008） | 2–3 h | ✅ [#107](https://github.com/qfc-network/qfc-core/pull/107) |
| A1 | `qfc-ps` crate 骨架 | 区间分片 KV、push/pull API、ps-lite 风格 timestamp；复用 `qfc-storage` 作分片本地持久层 | 4–6 h | ✅ [#107](https://github.com/qfc-network/qfc-core/pull/107) |
| A2 | 拜占庭鲁棒聚合 | trimmed-mean 聚合器 + 针对投毒场景的属性测试 | 3–4 h | ✅ [#107](https://github.com/qfc-network/qfc-core/pull/107) |
| A3 | 训练任务类型 | 扩展 `qfc-ai-coordinator` 任务池 + 训练作业分配（数据分片 manifest 走 IPFS） | 3–4 h | ✅ [#110](https://github.com/qfc-network/qfc-core/pull/110) |
| A4 | 验证路径 | `verification.rs` 中的抽样步重执行；容差带梯度比较 | 4–6 h | ✅ [#114](https://github.com/qfc-network/qfc-core/pull/114) |
| A5 | 链集成 | 模型版本承诺、贡献记录、共识中的奖励/罚没钩子 | 4–6 h | ✅ [#120](https://github.com/qfc-network/qfc-core/pull/120) — 结算层；节点侧应用受门控（见进度） |
| A6 | Testnet 试点 | 小模型（如 BERT 级）在 testnet 上跨 ≥3 个矿工训练 | 3–4 h | ✅ [#122](https://github.com/qfc-network/qfc-core/pull/122) — 进程内 ≥3-miner 试点；真实多机受门控 |

墙钟时间取决于 session 间隔；合计约 **23–33 Claude session 小时**。顺序：A0 → A1/A2 并行 → A3 → A4 → A5 → A6。**全部 A0–A6 已交付**（进程内实现）；真实测试网部署 + 节点集成仍待完成——见 [进度](#进度)。

---

## 特性 B — 超大模型的分片推理

### 为什么

`assignment.rs` 已经按 `GpuTier` + `memory_mb` 给矿工匹配任务；今天一个*任何*单矿工都装不下的模型干脆无法服务。权重分片让网络能承载比任何单个参与者都大的模型——这是 serving-PS 读路径的链上版本：**按需拉分片，逐分片 hash 校验**。

### 刻意分两个阶段

**B-1：分片*分发*、单矿工*执行***（便宜，先发布）
- 注册表条目变成**分片 manifest**：(分片 CID, hash, 大小, 层区间) 的列表，替代单一 CID。
- 总内存足够的矿工从 IPFS 逐分片组装模型并逐分片校验（今天的整文件 hash 检查推广而来）——可断点续传、部分缓存、跨模型版本共享分片复用（fine-tune 之间大多数层不变）。
- 没有新的执行语义；纯分发收益。

**B-2：多矿工*流水线执行***（难的部分——WAN 延迟 spike 已完成，[ADR-0011](adr/0011-b2-pipeline-scope.md)：**交互式 WAN = no-go；仅批处理、带宽 / 局部性感知、激活压缩。** 构建受真实跨区测量门控。）
- Coordinator 分配一个**分片组**：一组有序矿工各持一段层区间；activation 在矿工间流动（流水线并行）。
- 预先声明的诚实约束：广域网延迟下的流水线并行对交互式推理未经证明——先做批量/异步负载（任务池本来就建模异步任务），交互式只有在延迟数据支持时才做。
- 验证：每 stage 的 activation 承诺（层边界张量的 hash），让抽查能只重执行*一个 stage* 而不是整条流水线。
- 故障处理：组内成员失联即作废本次分配 → 重新分配；奖励按 stage 拆分，按 `flops_estimated` 风格计量。

### 架构（两个阶段）

```mermaid
flowchart LR
    subgraph CHAIN2["链上"]
        MAN["注册表分片 manifest:<br/>(CID, hash, 大小, 层区间) × N"]
    end
    IPFS["IPFS — 权重分片"]

    subgraph B1["B-1: 分片分发、单矿工执行"]
        SOLO["总内存足够的矿工:<br/>逐分片拉取 → 逐分片 hash 校验 →<br/>组装; 跨版本缓存分片"]
    end

    subgraph B2["B-2: 多矿工流水线 (gate 在 WAN 延迟上)"]
        direction LR
        G1["矿工 1<br/>第 0–15 层"] -->|"activations"| G2["矿工 2<br/>第 16–31 层"] -->|"activations"| G3["矿工 3<br/>第 32–47 层"]
    end

    STAGECHK["分段抽查:<br/>从 activation 承诺 (hash)<br/>只重执行一个 stage"]

    MAN --> SOLO
    IPFS -->|"逐分片拉取 + 校验"| SOLO
    MAN --> G1
    IPFS --> G1
    IPFS --> G2
    IPFS --> G3
    G1 -.->|"activation 承诺"| STAGECHK
    G2 -.-> STAGECHK
    G3 -.-> STAGECHK

    classDef chain fill:#e8f0fe,stroke:#1a56db;
    class MAN chain
```

### 里程碑（估算 = Claude session 小时）

| # | 里程碑 | 交付物 | 估算（Claude session h） | 状态 |
|---|---|---|---|---|
| B0 | ADR：分片 manifest 格式 + assignment 改动 | `docs/adr/`（ADR-0001） | 1–2 h | ✅ [#102](https://github.com/qfc-network/qfc-core/pull/102) |
| B1 | 注册表分片 manifest + 逐分片校验的 IPFS 分片下载 | `registry.rs`、`download.rs`（`shard.rs`） | 3–4 h | ✅ [#102](https://github.com/qfc-network/qfc-core/pull/102) |
| B2 | 组装 + 跨版本缓存复用 | `qfc-inference` data_store | 2–3 h | ✅ [#102](https://github.com/qfc-network/qfc-core/pull/102) |
| — | **WAN 延迟 spike**（B-2 门控） | 真实实测 + 可复现 calculator + go/no-go | 1 h | ✅ [#124](https://github.com/qfc-network/qfc-core/pull/124)（ADR-0011） |
| B3 | 分片组分配 | `assignment.rs` 扩展——现在**带宽 + 局部性**感知（ADR-0011） | 3–4 h | ⛔ 受跨区测量门控 |
| B4 | 流水线执行原型（2 矿工，**批处理**任务） | `qfc-inference` runtime + coordinator router；需**激活压缩**（ADR-0011） | 6–8 h | ⛔ 受门控 |
| B5 | 每 stage 验证 | activation 承诺（对**量化后**传输字节）+ 分段抽查 | 3–4 h | ⛔ 受门控 |

B-1（B0–B2）约 **6–9 Claude session 小时**——**已交付**（[#102](https://github.com/qfc-network/qfc-core/pull/102)）。B-2（B3–B5）约 **12–16 h**：WAN 延迟 spike 已完成（[ADR-0011](adr/0011-b2-pipeline-scope.md)），并把 B-2 重新定为仅批处理 + 带宽 / 局部性感知 + 激活压缩；**构建 B3–B5 受门控于用真实跨区 miner-to-miner RTT + 带宽重跑 calculator**（需要 ≥2 个位于不同区域的 miner）。

---

## 顺序与非目标

- **推荐顺序：B-1 → A → B-2。** ✅ **已按此顺序执行：** 先 B-1（[#102](https://github.com/qfc-network/qfc-core/pull/102)），再 Feature A 核心（[#107](https://github.com/qfc-network/qfc-core/pull/107)–[#122](https://github.com/qfc-network/qfc-core/pull/122)），最后 B-2 延迟 spike（[#124](https://github.com/qfc-network/qfc-core/pull/124)）。B-1 的分片 manifest 管线确实被 Feature A 复用（快照 / 版本分发，ADR-0007），也是 B-2 压缩激活传输的基础。B-2 仍受门控于延迟现实，但现已量化。
- **永久非目标：** 共识路径上的 PS 语义（确定性不可妥协）；裸 FedAvg 聚合（可投毒）；信任任何单个 PS 运营者（永远 ≥2-of-N 验证或有质押可罚没的运营者）。
- 上述估算是 **Claude session 小时**——墙钟时间取决于 session 怎么排。Claude 控制之外的外部依赖：A6/B4 的 testnet 矿工志愿者、大分片集的 IPFS 网关容量。
