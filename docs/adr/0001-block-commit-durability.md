# ADR 0001: Crash-atomic block commit and durability policy

- Status: Accepted
- Date: 2026-06-10
- Related: `docs/ROADMAP-SRE.md` T3.2 (items 3–4)

## Context

Before this change, committing a canonical block touched the database in
**four separate writes**:

1. `store_block()` — one `WriteBatch` covering `block_headers`,
   `block_bodies`, `block_hash_index`, `transactions`, `tx_index`;
2. a loop of individual `db.put` calls for `receipts`;
3. `db.put` for `metadata/latest_block_number`;
4. `db.put` for `metadata/latest_state_root`.

A crash between (1) and (2)–(4) could leave a stored block **without its
receipts** or with a **stale head pointer**, and a crash between (3) and (4)
could leave the head number pointing at a block whose state root metadata
belongs to its parent. The same sequence existed in both the import path
(`Chain::import_block`) and the producer path (`Chain::store_produced_block`),
and genesis init wrote `genesis_hash` / `chain_id` separately from the genesis
block itself.

In addition, all writes used RocksDB's default `WriteOptions`
(`sync = false`): the write-ahead log (WAL) is appended but **not fsynced**,
so a power loss or kernel panic can lose writes that were already
acknowledged to consensus and possibly broadcast to peers.

## Decision

### 1. Single atomic WriteBatch per block commit

`Chain::commit_block()` assembles headers, bodies, hash index, transactions,
tx locations, **receipts**, and **head metadata**
(`latest_block_number`, `latest_state_root`) into one `WriteBatch` and writes
it in a single RocksDB write. RocksDB applies a batch atomically across
column families (one WAL record), so after a crash the database either
contains the whole block commit or none of it. Both `import_block` and
`store_produced_block` use this path; genesis init likewise commits the
genesis block together with `genesis_hash`, `chain_id`, and head metadata in
one batch.

The in-memory head (`Chain::head`) is updated only **after** the durable
commit succeeds, so the node never advertises a head it could lose.

### 2. `WriteOptions::set_sync(true)` for canonical block commits

The block-commit batch is written via `Database::write_batch_sync()`, which
sets `WriteOptions::set_sync(true)`: RocksDB fsyncs the WAL before returning.

Why sync rather than the WAL default:

- **Cost is bounded by block rate, not tx rate.** One fsync per block
  (block interval is on the order of seconds) is negligible write
  amplification; this is not a per-transaction fsync. All other writes
  (state trie, mempool, indexes, checkpoints) keep the default non-sync
  WAL write, so steady-state write throughput is unaffected.
- **The WAL is sequential, so one fsync covers everything before it.**
  `StateDB::commit()` (trie nodes) runs earlier in the same import and uses a
  non-sync batch; the fsync on the block-commit batch also persists those
  earlier WAL entries. The block's *entire* effect — state, block data,
  receipts, head — is durable once `commit_block` returns.
- **A validator must not un-know a block it acted on.** After commit the node
  votes on / broadcasts the block. Losing it to a power failure and restarting
  on an older head risks double-signing-adjacent behavior and triggers an
  avoidable peer re-sync.

### 3. Exemptions

- `eth_tx_index` (`store_eth_tx_hash_mapping`) stays a separate non-sync
  `put`: its only caller is the RPC `eth_sendRawTransaction` path, which
  records the keccak256→blake3 mapping at **submission time**, before the tx
  is in any block — there is no block commit to be atomic with, and losing it
  only degrades hash translation for a not-yet-mined tx.
- All non-canonical writes (state trie commits, pruning, snap sync,
  consensus checkpoints, double-sign evidence) keep default `WriteOptions`.

## Consequences / RPO

- **Process crash (panic, OOM-kill):** RPO = 0 blocks regardless of the sync
  flag — the WAL survives the process.
- **Power loss / kernel panic:** RPO = 0 *committed* blocks. At most the
  block currently being imported (its non-synced state-trie writes included)
  is lost, and it is re-fetched from peers on restart. Without `sync=true`
  the loss window would have been "everything since the last OS page-cache
  flush" (potentially several blocks plus their state).
- **Latency:** block commit gains one fsync (typically well under 10 ms on
  SSD/NVMe). Acceptable at ≥1 s block intervals.
- **Follow-up (not in scope here):** `latest_state_root` durability now
  matches the head pointer, but state-trie writes between block commits are
  only as durable as the last block fsync — which is exactly the guarantee we
  want (state is recomputed/re-synced for any lost in-flight block).

## Alternatives considered

- **Default WAL (`sync = false`) everywhere** — cheaper, but the RPO on power
  loss is unbounded in block terms and depends on OS flush timing; rejected
  because validators act on committed blocks immediately.
- **`fsync` on an interval / every N blocks** — complexity without benefit at
  current block rates; revisit only if profiling (T1) shows WAL-sync latency
  in the block-commit critical path.
- **Disable WAL + rely on flush** — unacceptable RPO and recovery semantics
  for chain data.
