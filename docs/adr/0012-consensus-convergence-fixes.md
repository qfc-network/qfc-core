# ADR 0012 — Consolidated consensus-convergence fixes (defects D7–D12)

Status: accepted (Phase A implemented; Phase B = sync-before-produce gate pending)
Date: 2026-07-04
Spec: `docs/FIX-SPEC-convergence.md` (distilled from a 3-lens consolidated review)
Supersedes the one-defect-at-a-time approach of PRs #126–#129.

## Context

The 3-validator testnet forked three ways from block #1 and never converged.
A consolidated review found the fork was overdetermined — five independent
root causes, any one of which prevents convergence:

- **D7** — rewards were applied by the producer *before* commit
  (`producer.rs`) but never replayed on import (`chain.rs` executed only
  undelegations + transactions), so **every** peer block failed
  "State root mismatch" and each node could only extend its own chain.
- **D8** — VRF proofs were verified against the *receiver's current* epoch
  seed; epochs rotate every 10 s, so any block ≥ 1 epoch old was
  unimportable (`InvalidVrfProof`) and catch-up could never succeed.
- **D9** — `validate_block` never checked the producer against the schedule
  (first-seen-wins at any height), and the election itself consumed
  node-local inputs (latency EMAs, gossip votes/reputation/hashrate,
  local jail flags), so schedules drifted between nodes within seconds.
- **D10** — no fork choice, no reorg, single-branch number-keyed storage;
  imports executed against HEAD state and committed *before* the root
  comparison (poisoning live state on failure); imports were not
  serialized (gossip vs catch-up races on the shared StateDB).
- **D11** — the genesis epoch-seed anchor was captured racily
  (`unwrap_or_default()` → some nodes anchored to `[0;32]`), and any peer
  could re-anchor a node via a signed `EpochAnnouncement`.
- **D12** — non-deterministic inputs inside the state transition:
  `SystemTime::now` for undelegation maturity, node-local TaskPool fee
  settlement, "all locally-active validators" as the voter set, a `--dev`
  flag that silently changed the slot length per node, and no chain_id
  re-check against the database.

## Decision

### 1. One shared deterministic state transition (`Chain::execute_at`)

Produce and import run the *same* function, byte-identically:

1. mature undelegations — maturity clock = `block.timestamp` (never wall
   clock);
2. transaction execution (existing executor path);
3. `apply_block_rewards` — a pure function of the block contents:
   - producer: full year-halved block reward (halving math now uses the
     real `BLOCK_INTERVAL_MS`) + 47 % fee share (`FEE_PRODUCER_PERCENT`);
   - treasury: 5 % of fees (`FEE_TREASURY_PERCENT`);
   - the remaining fee share is burned (never re-credited).

Execution happens on a **scratch `StateDB::with_root(parent.state_root)`**
(the trie is root-addressed); the live head state is untouched until the
block is accepted.

**Economics changes (deliberate, temporary):**

- **Voter split removed** from the block path. Blocks carry no votes, and
  "all locally-active validators" is a node-local set — it cannot be a
  consensus input. The former 25 % voter share of the block reward and 28 %
  of fees now effectively accrue to nobody (reward portion goes to the
  producer as the full base reward; the fee portion is burned). Restore
  when votes are carried in-block.
- **Inference-fee settlement removed** from the block path (TaskPool is
  node-local). TaskPool/RPC behavior is otherwise unchanged; the producer
  still does pool housekeeping (reassign stale, prune expired, persist),
  but **expired public tasks are no longer refunded on-chain** — the
  submitter's fee is effectively burned until settlement moves into
  transactions. Miner rewards for in-block inference proofs are also
  deferred to that work.
- Dynamic network-state multipliers are gone from the block path
  (equivalent to hard-coding `Normal`; no production setter existed).
- `RewardDistribution` / `MinerEarning` records are no longer written by
  the block path (they recorded the now-removed splits).

### 2. Self-contained block validation

`validate_block` derives everything from the block itself + chain constants:

- `slot = timestamp / BLOCK_INTERVAL_MS`, `epoch = epoch_of(slot)`,
  `seed = derive_epoch_seed(epoch)` — **never** `current_epoch`;
- VRF verified against that block-derived seed (fixes D8; historical
  imports validate identically on every node forever);
- producer **must equal** `select_producer(slot)`; one-slot tolerance
  (`slot ± 1`) only when the timestamp is within `MAX_TIMESTAMP_DRIFT_MS`
  (1500 ms) of the slot boundary, and the VRF is then checked against the
  matched slot's epoch seed;
- timestamp sanity: `> parent.timestamp` and `≤ now + MAX_TIMESTAMP_DRIFT_MS`;
- local jail flags no longer gate validation (see §3).

### 3. Deterministic leader election

`leader(slot) = sorted_by_address(stake > 0)[(slot + offset(epoch_seed)) % n]`
with `offset = u64::from_le_bytes(seed[..8])`. The score-weighted branch is
retired until scores are consensus state; contribution scores remain for
metrics/observability only. Local jail flags no longer affect the election
or the set used in validation — gossip evidence without in-block proof
cannot be consensus (on-chain slashing is future work).

*Note (small deviation from the spec text):* the election set is the
engine's registered validator set (genesis-registered, checkpoint-restored)
filtered to `stake > 0`, not a per-block read of the parent state. The two
are identical today — the set only ever comes from genesis — and the
engine-held set keeps `select_producer` pure and state-free. When staking
becomes dynamic this must move on-chain.

`genesis_seed` is set **exactly once** in `Chain::new` (from the genesis
hash, before any task spawns); `derive_epoch_seed` errors when unset (the
`unwrap_or_default` fallback is gone); checkpoint restore does not touch
it; `handle_epoch_announcement` is reduced to verify-or-ignore (an
announcement can never change local epoch state).

### 4. Import hardening + minimal fork choice

- All canonical mutations (`import_block`, `store_produced_block`)
  serialize through **one `tokio::sync::Mutex` in `Chain`**. Phase B's
  gate and sync paths use the same lock via the async methods.
- Nothing is committed unless the recomputed root matches the header;
  a failed import leaves the live state root untouched. (Scratch-trie
  nodes written during a failed execution are unreferenced,
  content-addressed garbage — collected by pruning.)
- `store_produced_block` runs the same validate + execute path as import
  (self-validation) and requires the block to extend the current head.
- **Branch storage**: new `blocks_by_hash` CF holds every block ever
  imported/produced (keyed by header hash). The number-keyed CFs +
  `block_hash_index` remain the canonical index, rewritten on reorg.
- **Fork choice**: canonical = highest number; tie-break = lowest hash
  (converges same-height boundary races in either arrival order). Reorg
  walks parents by hash to the common ancestor, refuses to cross the
  finalized `(height, hash)` (recorded via `Chain::record_finalized`,
  genesis until finality votes land) or exceed depth 64, then re-executes
  the new branch from the ancestor's state root and rewrites the canonical
  index block-by-block. A refused reorg is not an import error — the block
  stays on its side branch.
- Catch-up: a range batch breaks on the first hard failure; on
  `InvalidParent` the block is queued and the backward hash-walk finds the
  common ancestor, after which fork choice reorgs naturally.
- Known limitations (accepted for a 4-validator testnet): a crash
  mid-reorg can leave a partially rewritten canonical index (repaired by
  the next reorg/restart sync); stale receipts/tx-index entries from
  displaced branches are not garbage-collected.

### 5. Sync-before-produce gate — **Phase B, not in this change**

Spec §5 (peer-status polling, `may_produce`, `QFC_PRODUCE_WHEN_ALONE`,
bootnode redial, catch-up peer selection, slot-aligned ticks) lands
separately. The APIs it needs are in place: `Chain::import_block` /
`store_produced_block` are async and serialize on the chain import lock.

### 6. Config hardening

- `BLOCK_INTERVAL_MS = 5000` and `EPOCH_DURATION_MS = 10_000` are
  single-source chain constants (`qfc-types`), consumed by engine,
  producer, miner and validation. They are no longer fields of
  `ConsensusConfig` / `ProducerConfig` / `MiningConfig`.
- `--dev` no longer changes the slot length (it was 3000 ms — a silent
  consensus fork against 5000 ms testnet nodes with the *same* genesis).
- `BLOCK_TIME_MS` (3333 — matched nothing) is deleted; halving and
  block-rate math use `BLOCK_INTERVAL_MS`.
- Startup errors (`ChainError::ChainIdMismatch`) when the database's
  recorded chain_id differs from the configured one.
- Genesis content is unchanged (dev == testnet); the network resets once
  after this lands.

## Out of scope (unchanged from spec)

On-chain slashing evidence / jailing, task-fee settlement via
transactions, score-weighted election, BFT finality gadget, full
state-sync/snap-sync.

## Consequences

- A testnet reset is required (state roots and schedules are incompatible
  with the old chain). This was already planned.
- Validators earn the full block reward; voters earn nothing until votes
  are in-block. Inference miners earn nothing from the block path until
  settlement moves on-chain.
- Historical sync from genesis now works from any node at any time.
- Two same-height blocks at a slot boundary self-heal via the lowest-hash
  tie-break instead of forking permanently.

## Tests

`crates/qfc-chain/tests/convergence.rs` (spec required tests 1–7, 9) and
the election/seed unit tests in `crates/qfc-consensus/src/engine.rs`
(required test 4 + convergence property tests). Required test 8 (gate)
lands with Phase B.
