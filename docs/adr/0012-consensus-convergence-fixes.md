# ADR 0012 — Consolidated consensus-convergence fixes (defects D7–D12)

Status: accepted (Phase A + Phase B implemented)
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

### 5. Sync-before-produce gate — **Phase B (implemented)**

See §Phase B below. The APIs it needs were put in place by Phase A:
`Chain::import_block` / `store_produced_block` are async and serialize on
the chain import lock.

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

## Phase B — sync-before-produce gate + sync-layer fixes (spec §5)

### Active peer-status tracking

`SyncManager` keeps a per-peer `{head, genesis_hash, last_seen}` map fed by
**active** `GetStatus` polling: a poll loop ticks every 2 s and polls every
connected peer that has no status yet (new connection) or whose status is
older than one slot (5 s). Requests are spawned per-peer with an in-flight
guard, so a dead peer's 30 s request timeout never stalls polling of the
others. Entries expire on disconnect (pruned each tick against the live
peer set); statuses older than 3 slots (15 s) are stale and ignored by
every consumer. Passive gossip/heartbeat heights deliberately do **not**
feed the gate — they are unauthenticated and absent exactly when every
validator is gated (they still feed `highest_peer_block` for observability
and can still trigger the periodic catch-up, which itself only ever syncs
from a status-verified peer).

### The gate

`gate_decision` (pure function, `producer.rs`) runs each slot **after** the
heartbeat send — heartbeats keep flowing while gated; the gate skips only
the produce step. Decision order (load-bearing, spec §5 pseudocode):

1. a fresh, genesis-matching verified peer head **strictly** above ours →
   gated (`>` not `>=`, so a simultaneous cold start with all heads 0
   passes);
2. within 10 s of producer boot (boot-relative grace) → gated;
3. zero connected peers → produce **only** with the explicit
   `produce_when_alone`;
4. peers connected but no fresh verified status yet → gated (data before
   liveness; the 2 s poll guarantees this state resolves);
5. otherwise produce.

When the gate has held a node strictly-behind for more than 2 consecutive
slots, the producer forces a `sync_with_peer` attempt (spawned, no-op if a
sync is already running). This escapes the dead zone where lag 1–2 gates
production but sits below `CATCH_UP_LAG_THRESHOLD` (> 2) and would never
trigger the periodic catch-up.

### Zero-peer production rule (`QFC_PRODUCE_WHEN_ALONE`)

Explicit `--produce-when-alone[=BOOL]` / `QFC_PRODUCE_WHEN_ALONE=0|1`
always wins. When unset it defaults to **true only for `--dev` with no
bootnodes configured** — that covers single-node dev
(`cargo run -- --dev`, with or without `--no-network`) and the
release-binary integration tests (`--dev --no-network`), which must keep
producing blocks with zero peers. Any node with bootnodes configured (every
testnet/compose node) defaults to **false**; the compose bootstrap node
(node-1) is the only one that sets it. It is never inferred from runtime
conditions such as "no peers reachable".

### is_syncing fix

`is_syncing()` was false during forward catch-up (it required pending-queue
activity, which only the gossip backward-walk produces). Now:
`catching_up` (an `AtomicBool` held around `sync_with_peer`) OR pending /
backward-walk activity OR a fresh verified peer head more than
`CATCH_UP_LAG_THRESHOLD` above our height.

### Bootnode redial

The one-shot dial at startup is backed by a redial loop inside
`NetworkService`: while the peer set is empty, re-dial every configured
bootnode and re-trigger the Kademlia bootstrap, with exponential backoff
(5 s base, 60 s cap) that resets whenever at least one peer is held.
Startup now **hard-errors** when bootnodes are configured but none parse
(previously a warning — an isolated node with a gate would sit gated, or
produce alone, forever).

### Catch-up peer selection

Catch-up syncs from the peer with the highest verified (status-confirmed,
genesis-matching, fresh) head — never `peers().first()`. A rotation cursor
advances to the next-best candidate whenever a sync attempt fails (status
failure, foreign genesis, or a hard failure mid-range) and resets on
success. With no verified candidate, catch-up waits for the poller rather
than syncing from an arbitrary peer.

### Producer slot alignment

The producer's boot-phase-locked `tokio::time::interval` is replaced by
`sleep` until the next wall-clock multiple of `BLOCK_INTERVAL_MS`,
recomputed every iteration (self-correcting). Leaders now produce at the
start of their slot, so block timestamps land inside the elected
slot/epoch instead of up to a full interval late. The per-slot dedup
(`last_slot`) is kept as a guard against timer jitter.

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
