# ADR-0017: Consensus-window EVM opcode fixes (BLOCKHASH / EXTCODEHASH / PREVRANDAO)

Status: accepted
Date: 2026-07-06
Related: ADR-0015 (EVM timestamp + gas accounting), ADR-0016 (eth RPC compat batch), ADR-0012 (consensus convergence)

## Context

The EVM-compat audit left three consensus-affecting divergences from
Ethereum semantics in `qfc-executor`:

1. **BLOCKHASH returned 0 unconditionally** — `block_hash_ref` in
   `crates/qfc-executor/src/evm.rs` always returned `B256::ZERO`. Ethereum
   returns the hash of one of the 256 most recent blocks (and 0 for the
   current block, future blocks, or anything older than 256).
2. **EXTCODEHASH returned blake3(code)** — the revm-facing
   `AccountInfo.code_hash` was set to `blake3_hash(code)`. Ethereum
   mandates `keccak256(code)`; contracts doing proxy/clone detection
   against known keccak hashes (EIP-1167 checks, factory verification)
   break silently.
3. **PREVRANDAO / DIFFICULTY read 0** — the revm `BlockEnv.prevrandao` was
   never set (revm's default is `Some(B256::ZERO)`), so post-merge
   contracts using `block.prevrandao` / `block.difficulty` saw a constant 0.

All three change execution results — and therefore state roots — for any
transaction that touches the opcodes.

## Decision

Fix all three, gated behind a hardfork activation height.

### Activation gating (consensus safety)

New consensus constant in `crates/qfc-types/src/constants.rs`:

```rust
pub const EVM_OPCODE_ACTIVATION_HEIGHT: u64 = 13_000;
```

Fresh nodes syncing from genesis RE-EXECUTE every historical block with
the current binary. If the semantics changed unconditionally, any
historical transaction that touched these opcodes would produce a
different state root and hard-fail sync. Therefore:

- **Below the height**: exact historical behavior — BLOCKHASH = 0, blake3
  code_hash, prevrandao = 0 (revm default).
- **At/after the height**: corrected behavior.
- The gate keys off the **executing block's number** (`block_number` in
  `EvmExecutor`), identically on every path: produce, import, reorg
  re-execution, and `simulate_call`/eth_call.

Height choice: scheduled when the testnet head was ~9,551, with an
observed cadence of ~6.6 s/block — activation lands roughly 6 hours after
merge, safely after the rolling validator deploy completes.

The height is threaded as `ChainConfig::evm_opcode_activation_height`
(default = the constant) solely so tests can exercise post-activation
semantics on short chains. **It is a consensus constant**: production code
must never set a non-default value — a per-node activation height is a
silent consensus fork (same rule as `BLOCK_INTERVAL_MS`, ADR-0012).

### Rolling-deploy requirement (hard)

**Every validator must run the upgraded binary BEFORE height 13,000.** An
un-upgraded node diverges on the first post-activation block whose
transactions actually use these opcodes. (Strictly, state roots only
diverge when a contract executes BLOCKHASH/EXTCODEHASH/PREVRANDAO —
setting `BlockEnv.prevrandao` alone does not alter results — but this must
be treated as a hard requirement, not a probabilistic one.) Deploy plan:
merge → image build → rolling restart, completing hours before 13,000.

### BLOCKHASH: ancestor-walk resolution (reorg safety)

The provider resolves hashes along the **executing block's own ancestor
chain**, NOT the current canonical number index. Rationale: `reorg_to`
re-executes branch blocks while the number-keyed canonical store still
holds the OLD branch — the atomic batch swaps it only after the whole
branch re-executed (ADR-0012/0013). A number-index read during that window
would return the wrong ancestors, fail the state-root check, and refuse
every reorg containing a BLOCKHASH-using transaction.

Design:

- `Chain::execute_at` gained a `parent_hash` parameter (the executing
  block's parent). Callers: produce (head hash), import
  (`block.parent_hash()`), reorg re-execution (`block.parent_hash()` via
  `execute_block`), and `simulate_call` (current head hash).
- `qfc-executor` defines `BlockHashLookup =
  Arc<dyn Fn(&Hash, u64) -> Option<Hash> + Send + Sync>` — a callback so
  the executor stays decoupled from `qfc-chain`. The chain implements it
  by walking parent hashes through the hash-keyed block store
  (`cf::BLOCKS_BY_HASH`, the same store fork choice uses) from the parent
  down to the wanted height.
- The walk is **lazy** (runs only when a contract actually executes
  BLOCKHASH), bounded by the opcode's 256-block window, and resolved
  (height, hash) pairs are cached in a per-execution map.
- Spec semantics enforced executor-side: `BLOCKHASH(n)` is valid only for
  `block_number - 256 <= n < block_number`; everything else is 0 and never
  consults the lookup. Walking past genesis yields 0.
- The returned hash is the chain's native block hash —
  `blake3(header_bytes)`, the **same hash the eth RPC reports**. No
  separate keccak block hash is invented.
- Every block within 256 of any post-activation height is present in
  `BLOCKS_BY_HASH`: all current binaries write it in the atomic commit
  batch, and fresh syncs rewrite history through the same path.

### EXTCODEHASH: storage-vs-revm hash split

Only the **revm-facing** `AccountInfo.code_hash` changes to
`keccak256(code)` post-activation. The storage layer is untouched: the
`CODE` column family stays keyed by blake3 (`state_db.rs::set_code`).
This split is safe because revm receives bytecode inline
(`code: Some(...)` in `basic_ref`), so `code_by_hash_ref` is never used
for loading — it now carries a `debug_assert` documenting that
unreachability. EOAs keep `KECCAK_EMPTY` (EIP-3607), which was already
correct on both sides of the fork.

Side note: revm internally already assigned keccak code hashes to
contracts created *within* the same transaction, so pre-fork EXTCODEHASH
was inconsistent between same-tx and cross-tx reads; post-activation both
read keccak256.

### PREVRANDAO

Post-activation, `BlockEnv.prevrandao = Some(keccak256(parent_hash))` —
deterministic on all execution paths and different every block.

**Security caveat**: this is NOT secure randomness. The block producer
knows `parent_hash` before selecting transactions and can grind inclusion
against it; even on Ethereum, PREVRANDAO is only weakly manipulable-
resistant. Acceptable for the testnet; revisit before mainnet (e.g. mix
the leader-election VRF output into the header and derive prevrandao from
it).

## Consequences

- `EvmExecutor::new` takes `parent_hash` + `Option<BlockHashLookup>`;
  `Executor::set_block_context` takes `parent_hash`; `Chain::execute_at`
  takes `parent_hash`. All consensus paths flow through the same context.
- eth_call/eth_estimateGas simulate in the head block's context: BLOCKHASH
  of recent heights resolves along the canonical head's ancestry.
- Solidity patterns that were silently broken start working at 13,000:
  commit-reveal against `blockhash(...)`, EIP-1167 clone verification via
  `extcodehash`, randomness lotteries reading `block.prevrandao`
  (with the caveat above).
- Historical sync is byte-identical: below 13,000 all three behaviors are
  reproduced exactly.

## Tests

- `qfc-executor` unit tests: gating on both sides of the height for all
  three opcodes; BLOCKHASH 256-window boundary (`n-256` valid, `n-257`
  zero, current block zero without consulting the lookup); PREVRANDAO
  determinism; EXTCODEHASH blake3-pre/keccak-post at the DB-adapter level.
- `qfc-chain` convergence tests: a probe contract deployed and called
  through the REAL produce/import path stores `blockhash(number-1)` and
  `prevrandao`; post-activation they equal the parent's blake3 header hash
  and `keccak256(parent_hash)`; below activation both stay 0; two
  independent chains importing the same opcode-using blocks converge on
  identical state roots on both sides of the fork.
