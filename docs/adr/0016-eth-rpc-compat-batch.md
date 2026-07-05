# ADR-0016: Non-consensus RPC Ethereum-compatibility batch

Status: Accepted
Date: 2026-07-05

## Context

An EVM-compatibility audit found a batch of JSON-RPC output defects that broke
MetaMask, ethers v6, hardhat, and block explorers against a QFC node. Every
item in this ADR is **render-only / new-method** work in the RPC layer: none of
it changes transaction execution, block production/import, or any state root.
Consensus-affecting compatibility gaps found by the same audit are explicitly
**out of scope** and deferred (see below).

The affected areas were:

- Transaction and block hashes were rendered as the internal BLAKE3 hash even
  for Ethereum-submitted transactions, whose canonical identifier is the
  keccak256 hash returned by `eth_sendRawTransaction`. Wallets that track a tx
  by the hash they received could never find it in `eth_getBlockBy*` /
  `eth_getTransactionByHash`.
- Transaction `type` was emitted as QFC's internal `TransactionType`
  (`Transfer=0`, `ContractCreate=1`, `ContractCall=2`), which collides with the
  EIP-2718 envelope types (`0x1` EIP-2930, `0x2` EIP-1559) that tooling expects.
- The signature `v` for legacy (EIP-155) transactions was truncated to one byte
  (chain-9000 legacy `v` ≈ `0x4673` was stored as a single byte in the tx's
  `public_key` marker).
- EIP-1559 fee fields (`maxFeePerGas` / `maxPriorityFeePerGas`), receipt
  `effectiveGasPrice` / `type`, and the block-level `logsBloom` were missing.
- A set of standard RPC methods (`net_version`, `web3_clientVersion`,
  `eth_feeHistory`, `eth_maxPriorityFeePerGas`, `eth_syncing`, `eth_accounts`,
  block-tx-count and tx-by-index methods) were unimplemented, so wallet/tooling
  connection handshakes and fee estimation failed.

## Decision

### Render metadata sidecar (`EthTxMeta`)

QFC stores every transaction in its native `Transaction` form, indexed by an
internal BLAKE3 hash. That form is lossy for Ethereum tooling: the keccak tx
hash, the full-width `v`, the EIP-2718 envelope type, and the EIP-1559 fee
fields cannot be recovered from it, and the existing `ETH_TX_INDEX` column
family only maps *forward* (eth keccak hash → internal hash).

We add a new column family, `eth_tx_meta`, keyed by the **internal** hash,
storing a Borsh-encoded `EthTxMeta { eth_hash, v, tx_type, max_priority_fee_per_gas,
max_fee_per_gas }`. It is:

- written only by the RPC `eth_sendRawTransaction` path (alongside the existing
  forward hash mapping), and
- read only when rendering `eth_get*` responses.

It is **never** part of the block-commit batch, execution, or any state root —
identical to how `store_eth_tx_hash_mapping` already behaves. Losing it in a
crash only degrades rendering of a not-yet-mined tx; it can never orphan block
data or diverge state.

Rendering falls back to the internal BLAKE3 hash and a legacy (`0x0`) envelope
when no metadata exists (native QFC transactions, or Ethereum transactions whose
submitting node was a different peer — see Consequences).

### Output fixes

- `eth_getBlockBy*` (both hash-list and full-tx modes),
  `eth_getTransactionByHash`, txpool content, and the new tx-by-index methods
  render the keccak `eth_hash` from `EthTxMeta` when present, else the internal
  hash.
- Transaction `type` is the EIP-2718 envelope (`0x0`/`0x1`/`0x2`) derived from
  the decoded Ethereum tx (`EthTransaction::envelope_type()`), or `0x0` for
  native QFC transactions. It no longer leaks QFC's internal `TransactionType`.
- `v` is emitted at full width from `EthTxMeta.v`.
- EIP-1559 transactions surface `maxFeePerGas` / `maxPriorityFeePerGas`, with
  `gasPrice` mirroring the max fee.
- `RpcReceipt` gains `effectiveGasPrice` (the tx gas price) and `type`.
- Block `logsBloom` is the OR of all receipt blooms in the block; `baseFeePerGas`
  remains `0x0` (correct post-#139) and is always present.

### New methods

- `net_version` (namespace `net`) → chain id as a decimal string (`"9000"`);
  plus `net_listening`, `net_peerCount`.
- `web3_clientVersion` (namespace `web3`) → `qfc-node/v<pkg-version>`.
- `eth_maxPriorityFeePerGas` → `0x0` (flat gas model, no tip).
- `eth_feeHistory` → a spec-shaped `FeeHistory` (`baseFeePerGas` length
  `blockCount+1`, `gasUsedRatio` length `blockCount`, `reward` present only when
  percentiles are requested), all values `0x0` to match the zero base fee.
- `eth_syncing` → `false` when caught up, wired to the node's
  `SyncStatusProvider` when present.
- `eth_accounts` → `[]` (signing is client-side).
- `eth_getBlockTransactionCountBy{Number,Hash}` → hex count.
- `eth_getTransactionBy{BlockNumber,BlockHash}AndIndex` → the tx DTO, reusing the
  same render path.

## Out of scope — deferred as consensus-affecting

The following audit items change the EVM/state and must ship in a separate,
state-root-affecting window (they are **not** in this ADR):

- `BLOCKHASH` opcode (`evm.rs`).
- `EXTCODEHASH` / account `code_hash` using BLAKE3 instead of keccak256
  (`evm.rs`).
- `PREVRANDAO`.
- Anything in `crates/qfc-executor`.

## Consequences

- The change set is confined to `crates/qfc-rpc`, plus non-execution additions:
  a storage column family (`qfc-storage`), two chain accessor methods
  (`qfc-chain`, not part of block commit), and the `EthTxMeta` type helpers
  (`qfc-types`). No executor/state/consensus/trie code is touched; `git diff`
  contains no state-root-affecting change.
- The reverse metadata (like the pre-existing forward hash mapping) is populated
  only on the node that received the transaction via `eth_sendRawTransaction`.
  Nodes that received the transaction over P2P re-decode it but do not record the
  mapping, so on those nodes eth-hash lookups fall back to the internal hash.
  Closing this gap (recording the mapping on the P2P ingest path) is left as
  follow-up; it is orthogonal to this render-only batch.
