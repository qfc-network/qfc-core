# Consolidated convergence fix — spec (defects #6–#12)

Status: implementation spec, distilled from a 3-lens consolidated review (2026-07-04) of the full
block-production/sync/boot path. Supersedes the one-defect-at-a-time approach. Goal: a 3-validator
testnet converges from fresh genesis and STAYS converged; one final reset after this lands.

## Root causes confirmed (all three reviews independently converged)

- **D7 — rewards never replayed on import (THE dominant blocker).** `producer.rs:242-298` mutates
  state (producer reward, voter split, treasury 5%, miner earnings, inference fee settlement)
  before `state.commit()`; `chain.rs:456-467` import executes only undelegations + txs then
  compares roots → **every peer block fails "State root mismatch"**. Each node can only extend its
  own chain. This alone explains the 3-way fork from #1 regardless of leader election.
- **D8 — VRF verified against receiver's CURRENT epoch seed** (`engine.rs:453-459`); epochs rotate
  every 10 s → any block ≥1 epoch old fails `InvalidVrfProof` → catch-up can never import → a
  sync-before-produce gate alone would deadlock (gated + can't sync = wait forever).
- **D9 — schedule not enforced, and election inputs are node-local.** `validate_block` never checks
  producer == select_producer (any active validator accepted at any height, first-seen wins).
  Weighted election reads local mutable `contribution_score` (latency EMA measured per-observer,
  gossip-dependent votes/reputation/hashrate, double-counted inference score) and local jail flags
  (gossip evidence, no proof required) → schedules drift within one 10 s epoch of boot.
- **D10 — no fork choice, no reorg; import executes against HEAD state (not parent), commit happens
  before root comparison with no revert (poisons live state), imports not serialized (gossip vs
  catch-up race on shared StateDB).** Storage is number-keyed with a single head pointer — cannot
  even represent a second branch. Any transient fork is permanent.
- **D6 — no sync-before-produce gate** (original known defect): producer's first tick fires
  milliseconds after boot, before libp2p mesh forms; fresh node produces its own #1.
  Plus: peer-head knowledge is passive-only (gossip/heartbeats; `GetStatus` only sent after we
  already think we're behind — circular), `is_syncing()` is false during forward catch-up,
  bootnodes dialed exactly once (no redial), catch-up picks `peers().first()` arbitrarily,
  dead zone between gate threshold (lag ≥1) and `CATCH_UP_LAG_THRESHOLD` (>2).
- **D11 — genesis_seed capture race** (miner's `maybe_advance_epoch` races producer's
  `start_epoch(1, genesis_hash)`; loser anchors to `[0;32]` via `unwrap_or_default()`);
  non-validators never advance the epoch at all; `handle_epoch_announcement` (`sync.rs:888-903`)
  adopts arbitrary (epoch, seed) from any single validator signature; checkpoint restore drops
  genesis_seed.
- **D12 — non-deterministic inputs inside the state transition**: `settle_inference_fees` reads the
  producer's in-memory TaskPool + wall clock; `process_mature_undelegations` uses `SystemTime::now`
  (called with different clocks on produce vs import); voter set = "all locally-active validators";
  `--dev` flips slot length 5000→3000 ms per-node (genesis identical! silent fork); epoch duration
  hardcoded in 3 places; chain_id not in genesis hash and never re-checked against DB.

## Design

### 1. Shared deterministic state transition (fixes D7, D12)
Extract `execute_block(parent_state_root, block) -> {state_root, receipts, gas_used}` (qfc-chain),
used byte-identically by BOTH produce (to build the header) and import (to verify it). Contents,
in order:
1. mature undelegations — maturity clock = `block.timestamp()`, never SystemTime
2. execute transactions (existing executor path)
3. `apply_block_rewards(block, parent_state)` — pure function:
   - producer reward = `block_reward_for_year(year_of(block.number))` to `block.producer`
   - treasury: existing 5% rule
   - **voter split REMOVED** (blocks carry no votes; "all active validators" is node-local →
     non-deterministic). Document the economics change in the ADR; restore when votes are in-block.
   - **inference fee settlement REMOVED from the block path** (TaskPool is node-local). TaskPool /
     RPC behavior otherwise unchanged; settlement moves on-chain later.
   - dynamic network-state multipliers: hard-code Normal (no production setter exists anyway).

### 2. Self-contained block validation (fixes D8, D9-enforcement, D11 fallout)
`validate_block` derives everything from the block itself + chain constants:
- `slot = block.timestamp_ms / BLOCK_INTERVAL_MS`; `epoch = block.timestamp_ms / EPOCH_DURATION_MS`
- `seed = derive_epoch_seed(epoch)` (genesis-anchored hash chain, O(1)) — **never** `current_epoch`
- VRF verified against that seed; producer must equal `select_producer(slot, epoch)` (see §3)
- timestamp sanity: `block.timestamp > parent.timestamp`; `block.timestamp <= now + MAX_DRIFT_MS`
  (drift allowance ~1500 ms; trivially true for historical imports)
- producer proving path uses the same derivation (`prove_with_seed(derive_epoch_seed(epoch_of(now)))`)
- `genesis_seed` set ONCE at engine construction from the chain's genesis hash (in main.rs/Chain
  init, before any task spawns); `derive_epoch_seed` returns Err if unset (no `unwrap_or_default`);
  checkpoint restore preserves it; **delete** the `handle_epoch_announcement` adoption path (or
  reduce to: verify `seed == derive_epoch_seed(number)` else ignore).
- one slot-boundary tolerance: accept producer of `slot` or `slot ± 1` ONLY if timestamp is within
  MAX_DRIFT of the boundary — keep tight, document.

### 3. Deterministic leader election (fixes D9-inputs)
Election inputs = on-chain/deterministic ONLY:
- validator set = genesis-registered validators with `stake > 0` **read from parent state**, sorted
  by address
- local jail flags NO LONGER affect election or the active set used in validation (gossip evidence
  without in-block proof cannot be consensus; slashing moves on-chain later — out of scope)
- selection = round-robin with seed offset: `leader = sorted[(slot + offset(epoch_seed)) % len]`
  where `offset = u64::from_le_bytes(seed[..8])`. Contribution scores no longer feed election
  (keep for metrics/observability only). This intentionally retires the weighted branch until
  scores are consensus state.

### 4. Import hardening + minimal fork choice (fixes D10)
- **Serialize** all imports through one `tokio::sync::Mutex` in `Chain` (gossip, catch-up,
  backward-walk, pending rescans, and `store_produced_block`).
- Execute against the **parent's** state (`StateDB::with_root(parent.state_root)` — trie is
  root-addressed), never the live head overlay; nothing is committed unless the root matches
  (kills state poisoning). `store_produced_block` must run the SAME validate+execute path as
  import (self-validation) before commit.
- **Branch storage**: new CF `BLOCKS_BY_HASH` (header+body keyed by hash); number-keyed CFs become
  the canonical-chain index, rewritten on reorg; `BLOCK_HASH_INDEX` kept consistent.
- **Fork choice**: canonical = highest block number; tie-break = lowest block hash (helps
  same-height boundary races converge). Reorg: find common ancestor by walking parents (all by
  hash), re-execute the new branch from the ancestor's state root, rewrite canonical index +
  `LATEST_*`. Depth cap 64. Never reorg across finalized `(height, hash)` (record hash, not just
  height; genesis if no finality votes).
- Catch-up fork healing: on `InvalidParent` at the start of a range, walk back via
  `GetBlockByNumber`/hash comparison to the common ancestor and feed the peer branch through the
  reorg path; break the batch loop on first hard failure instead of hammering the rest.

### 5. Sync-before-produce gate (fixes D6) — spec agreed by two lenses
New state: per-peer `{head, genesis_hash, last_seen}` map fed by **active** `GetStatus` polling
(request/response works on silent peers; passive gossip/heartbeat feeds are NOT the gate's input —
they're absent exactly when everyone is gated, and gossip heights are unauthenticated). Poll each
connected peer at connect + every ~2 s until fresh.

```
may_produce(now):                                # per slot tick, AFTER heartbeat send
    our  = chain.block_number()
    max_head = max(fresh peer statuses with matching genesis, default 0)
    if max_head > our:            return false   # strictly behind (STRICT > is load-bearing:
                                                 # all-zero cold start must pass)
                                                 # gated > 2 slots → force sync_with_peer
                                                 # (bypass CATCH_UP_LAG_THRESHOLD dead zone)
    if now - boot < GRACE (10 s): return false
    if no connected peers:        return QFC_PRODUCE_WHEN_ALONE   # explicit env/flag, compose
                                                 # sets true ONLY on node-1; never inferred
    if peers but no status yet:   return false   # data before liveness; polling guarantees progress
    return true
```
- Heartbeats keep flowing while gated (gate skips only `produce_block`).
- `catching_up: AtomicBool` set around `sync_with_peer`; `is_syncing()` fixed to include forward
  catch-up (`highest_peer > our + threshold` OR catching_up OR pending-walk).
- Bootnode redial loop with backoff while `peer_count < 1` (+ kad re-bootstrap); startup ERROR if
  bootnodes are configured but none parse.
- Catch-up peer selection: peer with highest verified (status-confirmed, genesis-matching) head,
  rotate on failure — not `peers().first()`.
- Producer ticks aligned to slot boundaries (`sleep_until` next multiple of interval), so leaders
  produce early in their slot and timestamps sit inside the elected epoch.

### 6. Config hardening (fixes D12 remainder)
- `BLOCK_INTERVAL_MS` (5000) and `EPOCH_DURATION_MS` (10_000) become single-source chain constants
  consumed everywhere (engine, producer, miner, validation). `--dev` no longer changes slot length
  (dev convenience ≠ consensus parameter). Fix `BLOCK_TIME_MS`-based halving to use the real
  interval constant.
- chain_id: startup error if DB-recorded chain_id ≠ configured.
- Keep genesis compiled-in as today (dev==testnet). We are resetting anyway; do NOT change genesis
  content in this PR beyond what the above requires (avoid scope creep).

## Out of scope (document in ADR, do not implement)
On-chain slashing evidence / jailing, task-fee settlement via transactions, score-weighted
election, BFT finality gadget, full state-sync/snap-sync.

## Required tests (all new, all must pass)
1. cross-node import: chain A produces (with rewards), chain B imports, roots match — THE D7 test
2. historical import: block several epochs old validates (VRF vs block-derived epoch)
3. leader enforcement: block from wrong-slot producer rejected
4. election determinism: two engines with wildly different local scores/jail flags elect the same
   leader for the same slot
5. reorg: adopt longer branch; same-height tie-break by lower hash; refuses to cross finalized hash;
   depth cap respected
6. failed import leaves live state root untouched (no poisoning)
7. concurrent import calls serialize (no interleaved commit corruption)
8. gate: simultaneous cold start (all heads 0) produces after grace; strictly-behind node stays
   gated; zero-peer node produces only with QFC_PRODUCE_WHEN_ALONE
9. non-validator (no producer running) imports blocks fine (no current_epoch dependency)
10. all existing consensus/chain/node tests keep passing (fmt + clippy clean — CI has a strict
    cargo-fmt gate)
