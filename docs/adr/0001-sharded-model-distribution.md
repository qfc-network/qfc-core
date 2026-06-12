# ADR-0001: Sharded model distribution (Roadmap AI-V3, B0)

**Status:** accepted
**Date:** 2026-06-10
**Context:** [ROADMAP-AI-V3.md](../ROADMAP-AI-V3.md) Feature B, stage B-1.

## Problem

A registry model today is one artifact with one optional whole-file Blake3 hash
(`ModelInfo.weights_hash`). A model that exceeds a single download — or that no
single miner wants to re-download after a fine-tune that changed 3 of 30 layers —
has no representation. B-1 introduces *sharded distribution with single-miner
execution*: split the weight file into verifiable shards, download shard-by-shard,
reuse unchanged shards across model versions.

## Decision 1 — Manifest shape

A `ShardManifest` describes the weight file as an ordered list of byte-range shards:

```rust
ShardEntry {
    cid: String,                     // IPFS CID of the shard bytes
    hash: Hash,                      // Blake3 of the shard bytes (verification key)
    size_bytes: u64,
    layer_range: Option<(u32, u32)>, // [start, end) — metadata only in B-1; B-2 uses it
}
ShardManifest {
    shards: Vec<ShardEntry>,         // concatenation order = file order
    total_size_bytes: u64,
    assembled_hash: Hash,            // Blake3 of the concatenated file
}
```

- **Byte-range, not tensor-aware, in B-1.** Assembly is plain concatenation; no
  format knowledge needed. `layer_range` is carried as optional metadata so a
  governance proposal *may* align shard boundaries to layer boundaries — which is
  what makes cross-version reuse effective (fine-tunes that freeze layers produce
  byte-identical shards) and is what B-2 pipeline execution will require.
- **`assembled_hash` duplicates `weights_hash` deliberately.** The manifest is
  self-contained (verifiable without consulting the registry entry), and a
  registry entry with both set must have them equal — checked at assembly time.
- Borsh + serde derives, same as every on-chain-adjacent type.

## Decision 2 — Where the manifest lives

`ModelInfo` gains `shard_manifest: Option<ShardManifest>`. `None` means the
existing single-artifact path (HuggingFace download + whole-file hash check) —
**fully backward compatible; no existing registry entry changes shape semantics.**
Governance approves the manifest as part of the model entry, same as
`weights_hash` today.

Rejected alternative: a separate manifest registry keyed by `ModelId`. More
moving parts, and the manifest has the same lifecycle/trust model as the rest of
`ModelInfo`.

## Decision 3 — Shard cache is content-addressed, shared across models

Shards are stored by Blake3 hash via the existing `LocalDataStore` pattern
(`<cache>/shards/<hash>`). Consequences, all free:

- **Resumable:** a shard already present and hash-valid is never re-downloaded;
  re-running an interrupted download fetches only missing shards.
- **Cross-version reuse:** v1.1 of a model lists mostly the same shard hashes as
  v1.0 → those shards hit the cache regardless of which model brought them in.
- **Tamper-evident:** `LocalDataStore::get` re-verifies hash on read.

Eviction: shard files are owned by the shard store, not `ModelCache`'s LRU.
B-1 ships with manual/size-cap cleanup deferred — assembled models dominate disk
anyway and shards can be deleted after assembly (kept by default for re-share).

## Decision 4 — Download & verification flow

```
for entry in manifest.shards:           (sequential in B-1; parallelism later)
    if store.contains(entry.hash): skip
    fetch <gateway>/ipfs/<entry.cid>    (curl subprocess, same as ipfs.rs)
    verify blake3(bytes) == entry.hash  → store, else fail that shard
assemble: concat shards in order → temp file, streaming Blake3
verify assembled == manifest.assembled_hash (and == weights_hash if set)
atomic rename into place
```

Per-shard hash check is the generalization of today's `verify_weights_hash`;
the assembled-hash check keeps the end-to-end guarantee identical to v2.x.

## Decision 5 — Assignment unchanged in B-1

Execution is still single-miner: `assignment.rs` continues to match on
`min_memory_mb`/`size_mb`/tier against the *total* model. Shard-group assignment
is B-2 (`B3` milestone) and explicitly out of scope here.

## Non-goals (B-1)

- Multi-miner pipeline execution (B-2, gated on WAN latency data).
- Tensor-format-aware splitting; governance tooling may align boundaries but the
  protocol doesn't require it.
- P2P shard exchange between miners (IPFS gateway is the transport, as today).
