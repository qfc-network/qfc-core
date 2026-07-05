# ADR 0013: Phantom-receipt guard and reorg receipt cleanup + mempool re-injection

- Status: Accepted
- Date: 2026-07-05
- Related: ADR 0001 (crash-atomic block commit), ADR 0012 (consensus
  convergence / fork choice), `crates/qfc-chain/src/chain.rs`,
  `crates/qfc-rpc/src/server.rs`, `crates/qfc-node/src/main.rs`

## Context

A confirmed contract-deployment bug: a `CREATE` transaction returned a
`status = 1` receipt carrying a `contractAddress`, yet `eth_getCode` was
`0x` on every node — the contract never existed on the canonical chain.

### Mechanism

1. A deploy tx `T` lands in block `B1`, which is **briefly canonical**.
   `commit_canonical` writes `RECEIPTS[T]` and `TX_INDEX[T]` keyed by the
   tx hash, with **no canonicality tag**.
2. A same-height sibling `B2` (which does **not** contain `T`) wins fork
   choice, and `reorg_to` adopts it.
3. The old `reorg_to` phase 2 deleted **only** `BLOCK_HASH_INDEX` rows for
   the abandoned branch. `B1`'s `RECEIPTS` / `TX_INDEX` rows survived as
   orphans, and there was no mempool re-injection.
4. `T`'s producer had already purged `T` from its own mempool
   (`producer.rs`), and importers never held it (rapid-fire propagation
   gap), so `T` was never re-included.

Result: a **phantom `status = 1` receipt forever** plus an empty canonical
account. Retrying the identical tx succeeded because the canonical nonce was
never bumped (same `CREATE` address). Note: the nonce accounting itself was
verified correct (increments by exactly 1 per tx); this ADR does **not**
touch nonce/executor logic.

## Decision

Two independent fixes.

### Fix A — phantom-receipt guard (read path)

Introduce `Chain::canonical_tx_at(hash) -> Option<(Block, usize)>` as the
**single source of truth** for "is this tx on the canonical chain right
now?". It resolves the recorded `(height, index)` from `TX_INDEX`, loads the
canonical block at that height, and verifies the tx actually sits at that
index by re-hashing `blake3(tx.to_bytes_without_signature())`. Any
mismatch — stale row, missing block, or out-of-range index — yields `None`.

- `get_receipt_with_block_info` now returns `Ok(None)` for a tx that is not
  canonically at its recorded location, so `eth_getTransactionReceipt`
  reports the tx as pending (clients keep waiting / resubmit) instead of
  surfacing a phantom receipt.
- `eth_getTransactionByHash` (`server.rs`) routes its confirmed/pending
  decision through `canonical_tx_at` for the same guarantee.
- The previous behaviour of returning a receipt with a zero block hash when
  the location row was missing is removed — that was precisely the phantom
  path.

Behaviour for genuinely-canonical txs is unchanged.

### Fix B — reorg cleanup + mempool re-injection (write path)

In `reorg_to` phase 2, the existing loop over abandoned canonical blocks
(`ancestor+1 ..= old_head`) now also, in the **same** `WriteBatch`:

- `delete`s `RECEIPTS`, `TX_INDEX`, and `TRANSACTIONS` rows for each tx the
  abandoned block carried that is **not** present on the winning branch
  (a `HashSet` of new-branch tx hashes is built from the re-executed
  branch first);
- collects those displaced txs into a `Vec<Transaction>`.

Ordering is load-bearing: the deletes are emitted **before** the new
branch's `append_block_to_batch` / `append_receipts_and_head_to_batch`
writes. RocksDB applies batch ops in insertion order, so a tx present on
**both** branches keeps its correct new receipt/location (its row is never
deleted, and is rewritten anyway), while a displaced-only tx's rows are
purged.

`reorg_to` now returns `Result<Vec<Transaction>>` (the displaced txs) and
forwards each to an optional sink:

- `Chain` gains a `reorg_tx_sink: RwLock<Option<UnboundedSender<Transaction>>>`
  field and a `set_reorg_tx_sink(...)` setter. `reorg_to` sends each
  displaced tx into it (send errors ignored).
- In `qfc-node` startup (`main.rs`), an unbounded channel is created, wired
  via `set_reorg_tx_sink`, and a task drains it and re-adds each tx to the
  mempool through the same nonce-validated path the network handler uses
  (`Mempool::add_with_nonce_check`, with the chain's live state as the
  nonce lookup). Txs that fail validation (already included / stale nonce)
  are skipped.

This restores displaced txs regardless of which component purged them, so a
deploy/transfer orphaned by a fork is re-included on the canonical chain.

## Consequences

- A reorg no longer leaves phantom receipts; the RPC surface reports
  displaced txs as pending, and they are automatically re-queued for
  inclusion.
- `reorg_to`'s canonical rewrite remains a single atomic `WriteBatch`, so
  the new deletes inherit the existing crash-atomicity guarantee (ADR 0001).
- The read-path guard is defensive on its own: even a pre-existing orphan
  row in a database written before this fix is now filtered out at query
  time.
- Deviation from the original design sketch: `import_block` keeps its
  `Result<Hash>` signature — displaced txs travel via the `reorg_tx_sink`
  channel rather than being bubbled up through return types, which keeps the
  import call sites untouched.

## Tests

`crates/qfc-chain/tests/convergence.rs` (extends the real-`Chain` reorg
harness):

- `reorg_purges_phantom_receipt_of_displaced_tx` — a tx in a briefly-canonical
  block is abandoned by a strictly longer competing branch; afterwards
  `get_receipt_with_block_info` / `canonical_tx_at` return `None` and the
  `RECEIPTS` / `TX_INDEX` / `TRANSACTIONS` rows are gone.
- `reorg_forwards_only_displaced_txs_to_sink` — a reorg forwards exactly the
  displaced-only tx to the `reorg_tx_sink`; a tx present on both branches is
  not forwarded and remains canonically resolvable.
