//! Block producer - handles block production loop
//!
//! State-transition note (ADR-0012 / spec §1): the producer performs NO
//! direct state mutation. The whole transition — undelegations, transactions,
//! rewards — lives in `Chain::execute_at`, shared byte-identically with block
//! import, and the produced block is committed through
//! `Chain::store_produced_block`, which self-validates exactly like an
//! importer. Voter splits and inference-fee settlement were REMOVED from the
//! block path (they consumed node-local inputs and forked every import).

use crate::sync::SyncManager;
use parking_lot::RwLock;
use qfc_ai_coordinator::{ProofPool, TaskPool};
use qfc_chain::Chain;
use qfc_consensus::ConsensusEngine;
use qfc_crypto::blake3_hash;
use qfc_mempool::Mempool;
use qfc_network::NetworkService;
use qfc_types::{
    Heartbeat, Transaction, ValidatorMessage, BLOCK_INTERVAL_MS, MAX_INFERENCE_PROOFS_PER_BLOCK,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;
use tracing::{debug, error, info, warn};

/// Boot-relative grace period before the first block may be produced
/// (spec §5). Gives libp2p time to form the mesh and the status poller time
/// to learn peer heads, so a restarting node never races its own peers.
pub(crate) const PRODUCE_BOOT_GRACE_MS: u64 = 10_000;

/// When the gate has held us back as strictly-behind for more than this many
/// consecutive slots, force a catch-up attempt even below the catch-up lag
/// threshold (the lag 1–2 dead zone; spec §5).
const FORCE_SYNC_AFTER_GATED_SLOTS: u32 = 2;

/// Block producer configuration
#[derive(Clone, Debug)]
pub struct ProducerConfig {
    /// Maximum transactions per block
    pub max_txs_per_block: usize,
    /// Whether to produce empty blocks
    pub produce_empty_blocks: bool,
    /// Whether the gate may produce with zero connected peers
    /// (QFC_PRODUCE_WHEN_ALONE / --produce-when-alone; ADR-0012 §Phase B).
    pub produce_when_alone: bool,
}

impl Default for ProducerConfig {
    fn default() -> Self {
        Self {
            max_txs_per_block: 1000,
            produce_empty_blocks: true, // For dev mode, produce even if no txs
            produce_when_alone: false,
        }
    }
}

/// Outcome of the sync-before-produce gate for one slot (spec §5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GateDecision {
    /// Safe to run leader election / block production this slot.
    Produce,
    /// A fresh verified peer head is strictly above ours — sync first.
    Behind { our: u64, peer: u64 },
    /// Still inside the boot grace period.
    BootGrace,
    /// Zero connected peers and `produce_when_alone` is off.
    Alone,
    /// Peers are connected but none has delivered a fresh verified status
    /// yet — data before liveness (the 2s status poll guarantees progress).
    AwaitingPeerStatus,
}

/// The sync-before-produce gate (spec §5 pseudocode, verbatim semantics).
///
/// Decision order is load-bearing:
/// 1. strictly behind a fresh verified peer head → gate (STRICT `>` so a
///    simultaneous cold start — all heads 0 — passes);
/// 2. boot grace (10s, boot-relative) → gate;
/// 3. zero peers → produce only with the explicit `produce_when_alone`;
/// 4. peers but no fresh verified status yet → gate;
/// 5. otherwise produce.
///
/// `max_fresh_peer_head` must come from ACTIVE GetStatus polling of
/// genesis-matching peers (never passive gossip/heartbeat heights — those
/// are unauthenticated and absent exactly when everyone is gated). Pure;
/// unit-tested with injected clock/peer views.
pub fn gate_decision(
    our_height: u64,
    max_fresh_peer_head: Option<u64>,
    connected_peers: usize,
    ms_since_boot: u64,
    produce_when_alone: bool,
) -> GateDecision {
    let max_head = max_fresh_peer_head.unwrap_or(0);
    if max_head > our_height {
        return GateDecision::Behind {
            our: our_height,
            peer: max_head,
        };
    }
    if ms_since_boot < PRODUCE_BOOT_GRACE_MS {
        return GateDecision::BootGrace;
    }
    if connected_peers == 0 {
        return if produce_when_alone {
            GateDecision::Produce
        } else {
            GateDecision::Alone
        };
    }
    if max_fresh_peer_head.is_none() {
        return GateDecision::AwaitingPeerStatus;
    }
    GateDecision::Produce
}

/// Block producer
pub struct BlockProducer {
    chain: Arc<Chain>,
    consensus: Arc<ConsensusEngine>,
    mempool: Arc<RwLock<Mempool>>,
    network: Option<Arc<NetworkService>>,
    config: ProducerConfig,
    /// v2.0: Pool of verified inference proofs awaiting block inclusion
    proof_pool: Arc<RwLock<ProofPool>>,
    /// v2.0: Shared task pool (housekeeping only — no fee settlement in the
    /// block path)
    task_pool: Arc<RwLock<TaskPool>>,
    /// Gate input source (verified peer statuses) + forced catch-up target.
    /// None only when networking is disabled (`--no-network`).
    sync_manager: Option<Arc<SyncManager>>,
}

impl BlockProducer {
    /// Create a new block producer
    pub fn new(
        chain: Arc<Chain>,
        consensus: Arc<ConsensusEngine>,
        mempool: Arc<RwLock<Mempool>>,
        network: Option<Arc<NetworkService>>,
        config: ProducerConfig,
        proof_pool: Arc<RwLock<ProofPool>>,
        task_pool: Arc<RwLock<TaskPool>>,
    ) -> Self {
        Self {
            chain,
            consensus,
            mempool,
            network,
            config,
            proof_pool,
            task_pool,
            sync_manager: None,
        }
    }

    /// Attach the sync manager (gate peer view + forced catch-up).
    pub fn with_sync_manager(mut self, sync_manager: Arc<SyncManager>) -> Self {
        self.sync_manager = Some(sync_manager);
        self
    }

    /// Gate inputs for the current slot: connected peer count and the
    /// highest fresh verified (genesis-matching) peer head.
    fn gate_peer_view(&self) -> (usize, Option<u64>) {
        match (&self.sync_manager, &self.network) {
            (Some(sm), _) => sm.gate_peer_view(),
            // Network without sync manager (not wired in practice): count
            // peers but treat all statuses as unknown — fail gated, not open.
            (None, Some(net)) => (net.peer_count(), None),
            (None, None) => (0, None),
        }
    }

    /// Start the block production loop
    pub async fn start(self) {
        if !self.consensus.is_validator() {
            info!("Not a validator, block production disabled");
            return;
        }

        let our_address = self.consensus.our_address().unwrap();
        info!("Starting block producer for validator {}", our_address);

        // NOTE: no start_epoch here. The genesis seed is anchored once in
        // Chain::new (before this task spawns); epochs advance from wall
        // clock via maybe_advance_epoch. This removes the D11 capture race.

        // Slot length is the chain constant — never a per-node setting
        // (a per-node slot length is a silent consensus fork, spec §6).
        let boot = Instant::now();
        let mut heartbeat_counter: u64 = 0;
        let heartbeat_interval = 3; // Send heartbeat every 3 slots
        let mut last_slot: u64 = u64::MAX;
        let mut gated_behind_slots: u32 = 0;

        loop {
            // Slot-aligned tick (spec §5): sleep until the next wall-clock
            // multiple of BLOCK_INTERVAL_MS instead of a boot-phase-locked
            // interval, so the elected leader produces at the START of its
            // slot and the block timestamp lands inside the elected epoch.
            sleep_until_next_slot_boundary().await;

            // Global wall-clock slot: now_ms / BLOCK_INTERVAL_MS. Every node
            // computes the same slot at the same instant (clocks are
            // NTP-synced), so exactly one validator is elected network-wide
            // per slot.
            let now_ms = now_ms();
            let slot = now_ms / BLOCK_INTERVAL_MS;

            // Process each slot at most once (guards against timer jitter
            // firing twice within one slot window).
            if slot == last_slot {
                continue;
            }
            last_slot = slot;
            heartbeat_counter += 1;

            // Advance the observability epoch (wall-clock anchored).
            self.consensus.maybe_advance_epoch();

            // Send periodic heartbeat — heartbeats keep flowing while gated
            // (the gate below skips ONLY the produce step).
            if heartbeat_counter >= heartbeat_interval {
                heartbeat_counter = 0;
                self.send_heartbeat().await;
            }

            // Sync-before-produce gate (spec §5).
            let (connected_peers, max_fresh_peer_head) = self.gate_peer_view();
            let decision = gate_decision(
                self.chain.block_number(),
                max_fresh_peer_head,
                connected_peers,
                boot.elapsed().as_millis() as u64,
                self.config.produce_when_alone,
            );
            match decision {
                GateDecision::Produce => {
                    gated_behind_slots = 0;
                }
                GateDecision::Behind { our, peer } => {
                    gated_behind_slots += 1;
                    info!(
                        "Slot {}: gated — behind verified peer head ({} < {}), {} gated slot(s)",
                        slot, our, peer, gated_behind_slots
                    );
                    // Dead-zone escape: CATCH_UP_LAG_THRESHOLD only triggers
                    // the periodic catch-up at lag > 2, but the gate holds at
                    // lag ≥ 1 — force a sync so a 1–2 block lag can't gate us
                    // forever.
                    if gated_behind_slots > FORCE_SYNC_AFTER_GATED_SLOTS {
                        if let Some(sm) = &self.sync_manager {
                            sm.spawn_forced_catch_up();
                        }
                        gated_behind_slots = 0;
                    }
                    continue;
                }
                GateDecision::BootGrace => {
                    debug!("Slot {}: gated — inside boot grace period", slot);
                    gated_behind_slots = 0;
                    continue;
                }
                GateDecision::Alone => {
                    info!(
                        "Slot {}: gated — no connected peers (set QFC_PRODUCE_WHEN_ALONE=1 \
                         to bootstrap a network from this node)",
                        slot
                    );
                    gated_behind_slots = 0;
                    continue;
                }
                GateDecision::AwaitingPeerStatus => {
                    info!(
                        "Slot {}: gated — {} peer(s) connected but no fresh verified status yet",
                        slot, connected_peers
                    );
                    // Deliberately NOT resetting gated_behind_slots (review
                    // fix 16): a peer whose status flaps between fresh and
                    // stale produces Behind → AwaitingPeerStatus → Behind
                    // cycles that would otherwise never reach the forced
                    // catch-up threshold, starving the node forever.
                    continue;
                }
            }

            // Check if we should produce
            if !self.consensus.should_produce(slot) {
                debug!("Slot {}: Not our turn to produce", slot);
                continue;
            }

            let start = Instant::now();
            match self.produce_block().await {
                Ok(block_hash) => {
                    let elapsed = start.elapsed();
                    info!("Produced block {} in {:?}", block_hash, elapsed);
                }
                Err(e) => {
                    error!("Failed to produce block: {}", e);
                }
            }
        }
    }

    /// Send a heartbeat to the network
    async fn send_heartbeat(&self) {
        let Some(network) = &self.network else {
            return;
        };

        let our_address = match self.consensus.our_address() {
            Some(addr) => addr,
            None => return,
        };

        let head = match self.chain.head() {
            Some(h) => h,
            None => return,
        };

        let now = now_ms();

        // Create heartbeat
        let mut heartbeat = Heartbeat::new(our_address, head.block.number(), head.hash, now);

        // Sign the heartbeat
        let heartbeat_hash = blake3_hash(&heartbeat.to_bytes_without_signature());
        match self.consensus.sign_hash(&heartbeat_hash) {
            Ok(sig) => heartbeat.set_signature(sig),
            Err(_) => return,
        }

        // Broadcast
        let msg = ValidatorMessage::Heartbeat(heartbeat);
        if let Err(e) = network.broadcast_validator_msg(msg.to_bytes()).await {
            debug!("Failed to broadcast heartbeat: {}", e);
        } else {
            debug!("Sent heartbeat at block #{}", head.block.number());
        }
    }

    /// Produce a single block
    async fn produce_block(&self) -> anyhow::Result<qfc_types::Hash> {
        // Get parent block
        let parent = self
            .chain
            .head()
            .ok_or_else(|| anyhow::anyhow!("No parent block"))?;

        let parent_block = parent.block.clone();
        let our_address = self.consensus.our_address().unwrap();

        // Select transactions from mempool
        let transactions = self.select_transactions();
        let tx_count = transactions.len();

        // Skip if no transactions and not producing empty blocks
        if transactions.is_empty() && !self.config.produce_empty_blocks {
            debug!("No transactions to include, skipping block");
            return Err(anyhow::anyhow!("No transactions"));
        }

        // Drain inference proofs from pool (v2.0). From here until the
        // block is durably stored, any failure must REQUEUE the drained
        // proofs (review fix 14) — e.g. a head that moved under a
        // concurrent import would otherwise silently discard them.
        let inference_proofs = self
            .proof_pool
            .write()
            .drain(MAX_INFERENCE_PROOFS_PER_BLOCK);

        // Fix the header timestamp BEFORE executing: undelegation maturity is
        // timestamp-driven, so the execution and the header must agree.
        let timestamp = now_ms();
        let block_number = parent_block.number() + 1;

        // Deterministic shared state transition against the parent state
        // root (same code path import runs — D7). No live-state mutation.
        let outcome = match self.chain.execute_at(
            parent_block.state_root(),
            parent.hash,
            block_number,
            timestamp,
            &our_address,
            &transactions,
        ) {
            Ok(o) => o,
            Err(e) => {
                self.requeue_proofs(&inference_proofs);
                return Err(e.into());
            }
        };

        // Seal the block (VRF proved against the block-derived epoch seed).
        let block = match self.consensus.produce_block(
            &parent_block,
            transactions.clone(),
            outcome.receipts.clone(),
            outcome.state_root,
            outcome.gas_used,
            inference_proofs.clone(),
            timestamp,
        ) {
            Ok(b) => b,
            Err(e) => {
                self.requeue_proofs(&inference_proofs);
                return Err(anyhow::anyhow!("Consensus error: {}", e));
            }
        };

        let block_hash = blake3_hash(&block.header_bytes());
        let block_number = block.number();

        // Store the block: self-validates + re-executes exactly like an
        // importer, under the chain-wide import lock.
        if let Err(e) = self.chain.store_produced_block(&block).await {
            self.requeue_proofs(&inference_proofs);
            return Err(e.into());
        }

        // Node-local task-pool housekeeping (no chain-state writes).
        self.maintain_task_pool();

        // Broadcast to network
        if let Some(network) = &self.network {
            let block_data = borsh::to_vec(&block).unwrap();
            if let Err(e) = network.broadcast_block(block_data).await {
                warn!("Failed to broadcast block: {}", e);
            } else {
                debug!("Broadcasted block #{} to network", block_number);
            }

            // Cast and broadcast our own vote for the block we produced.
            // try_record_own_vote keeps the one-vote-per-height invariant
            // (review fix 2a) so incoming votes for this block cannot make
            // us vote a second time.
            if self.consensus.try_record_own_vote(block_number, block_hash) {
                if let Ok(vote) = self.consensus.vote(&block, true) {
                    let vote_data = vote.to_bytes();
                    if let Err(e) = network.broadcast_vote(vote_data).await {
                        warn!("Failed to broadcast vote: {}", e);
                    } else {
                        debug!("Broadcasted accept vote for block #{}", block_number);
                    }
                    // Add our vote to pending votes
                    self.consensus.add_vote(vote);
                }
            }
        }

        // Remove included transactions from mempool
        for tx in &transactions {
            let tx_hash = blake3_hash(&tx.to_bytes_without_signature());
            self.mempool.write().remove(&tx_hash);
        }

        info!(
            "Block #{} produced: {} txs, {} gas used",
            block_number, tx_count, outcome.gas_used
        );

        Ok(block_hash)
    }

    /// Return drained-but-unused inference proofs to the pool (review fix
    /// 14): a failed seal/store must not silently discard verified proofs.
    fn requeue_proofs(&self, proofs: &[qfc_types::InferenceProof]) {
        if proofs.is_empty() {
            return;
        }
        let mut pool = self.proof_pool.write();
        for proof in proofs {
            pool.add(proof.clone());
        }
        debug!(
            "Requeued {} inference proof(s) after failed block production",
            proofs.len()
        );
    }

    /// Node-local task-pool housekeeping (v2.0).
    ///
    /// IMPORTANT: this must never touch chain state. Fee settlement and
    /// expired-task refunds were removed from the block path (ADR-0012):
    /// the TaskPool is node-local, so paying/refunding from it during block
    /// production produced state roots no importer could reproduce.
    /// Settlement moves on-chain later; until then completed/expired tasks
    /// are only tracked and persisted for querying.
    fn maintain_task_pool(&self) {
        let mut task_pool = self.task_pool.write();

        // Re-queue tasks assigned to miners that timed out
        let reassigned = task_pool.reassign_stale_tasks();
        if reassigned > 0 {
            info!("Reassigned {} stale inference tasks", reassigned);
        }

        // Prune expired tasks (no on-chain refund — see above)
        let expired = task_pool.prune_expired_public(now_ms());

        // Persist expired tasks to RocksDB for long-term querying
        let db = self.chain.db();
        for task in &expired {
            if let Some(bytes) = TaskPool::serialize_task(task) {
                if let Err(e) = db.put(qfc_storage::cf::TASKS, task.task_id.as_bytes(), &bytes) {
                    warn!("Failed to persist expired task: {}", e);
                }
            }
        }

        // Prune in-memory completed tasks that have passed retention period,
        // persisting them to RocksDB first
        let pruned = task_pool.prune_retained_completed();
        for task in &pruned {
            if let Some(bytes) = TaskPool::serialize_task(task) {
                if let Err(e) = db.put(qfc_storage::cf::TASKS, task.task_id.as_bytes(), &bytes) {
                    warn!("Failed to persist completed task: {}", e);
                }
            }
        }
        if !pruned.is_empty() {
            debug!("Persisted {} completed tasks to storage", pruned.len());
        }
    }

    /// Select transactions from mempool with nonce validation against on-chain state
    fn select_transactions(&self) -> Vec<Transaction> {
        let mempool = self.mempool.read();
        let state = self.chain.state();

        mempool.select_with_nonce(
            qfc_types::DEFAULT_BLOCK_GAS_LIMIT,
            self.config.max_txs_per_block,
            Some(state.as_ref()),
        )
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// Sleep until the next wall-clock multiple of [`BLOCK_INTERVAL_MS`].
///
/// Recomputed from the wall clock every call (self-correcting): however long
/// the previous produce/heartbeat step took, the next wake lands on a slot
/// boundary, not on `boot_phase + n * interval` like `tokio::time::interval`
/// did — that phase lock made late-in-slot booters produce with timestamps
/// that could cross into the next slot/epoch.
async fn sleep_until_next_slot_boundary() {
    let now = now_ms();
    let next_boundary = (now / BLOCK_INTERVAL_MS + 1) * BLOCK_INTERVAL_MS;
    tokio::time::sleep(Duration::from_millis(next_boundary.saturating_sub(now))).await;
}

#[cfg(test)]
mod tests {
    use super::{gate_decision, GateDecision, PRODUCE_BOOT_GRACE_MS};

    const AFTER_GRACE: u64 = PRODUCE_BOOT_GRACE_MS; // exactly the boundary is "past grace"
    const IN_GRACE: u64 = PRODUCE_BOOT_GRACE_MS - 1;

    /// Spec required-test 8a: simultaneous cold start — every node is at
    /// height 0 and every fresh verified peer status reports head 0. The
    /// STRICT `>` comparison must let production start once the grace period
    /// has elapsed (a `>=` here would deadlock the whole network forever).
    #[test]
    fn cold_start_all_heads_zero_produces_after_grace() {
        // Before grace: gated regardless of statuses.
        assert_eq!(
            gate_decision(0, Some(0), 2, IN_GRACE, false),
            GateDecision::BootGrace
        );
        // After grace: peers verified at the same height → produce.
        assert_eq!(
            gate_decision(0, Some(0), 2, AFTER_GRACE, false),
            GateDecision::Produce
        );
        // Also when we are level with a non-zero network.
        assert_eq!(
            gate_decision(7, Some(7), 3, AFTER_GRACE, false),
            GateDecision::Produce
        );
        // And when we are AHEAD of every verified peer.
        assert_eq!(
            gate_decision(9, Some(7), 3, AFTER_GRACE, false),
            GateDecision::Produce
        );
    }

    /// Spec required-test 8b: a node strictly behind a fresh verified peer
    /// head stays gated — even after grace, even by a single block (the
    /// lag-1 dead zone is escaped by the forced catch-up, not by producing).
    #[test]
    fn strictly_behind_node_stays_gated() {
        assert_eq!(
            gate_decision(3, Some(5), 2, AFTER_GRACE, false),
            GateDecision::Behind { our: 3, peer: 5 }
        );
        // One block behind is still behind.
        assert_eq!(
            gate_decision(4, Some(5), 2, AFTER_GRACE, true),
            GateDecision::Behind { our: 4, peer: 5 }
        );
        // Behind wins over the grace period (checked first).
        assert_eq!(
            gate_decision(0, Some(10), 1, IN_GRACE, true),
            GateDecision::Behind { our: 0, peer: 10 }
        );
    }

    /// Spec required-test 8c: a zero-peer node produces only with the
    /// explicit produce_when_alone opt-in — and even then only after grace.
    #[test]
    fn zero_peer_node_produces_only_when_alone_flag_set() {
        assert_eq!(
            gate_decision(0, None, 0, AFTER_GRACE, false),
            GateDecision::Alone
        );
        assert_eq!(
            gate_decision(0, None, 0, AFTER_GRACE, true),
            GateDecision::Produce
        );
        // Grace applies to the alone branch too (spec order: grace first).
        assert_eq!(
            gate_decision(0, None, 0, IN_GRACE, true),
            GateDecision::BootGrace
        );
    }

    /// "Peers but no status yet → wait": data before liveness. Connected
    /// peers whose statuses are missing or stale must gate production; the
    /// 2s status poll loop guarantees this state resolves.
    #[test]
    fn peers_without_fresh_status_gate_production() {
        assert_eq!(
            gate_decision(5, None, 2, AFTER_GRACE, false),
            GateDecision::AwaitingPeerStatus
        );
        // produce_when_alone does NOT bypass this branch — it is strictly
        // for the zero-peer case.
        assert_eq!(
            gate_decision(5, None, 2, AFTER_GRACE, true),
            GateDecision::AwaitingPeerStatus
        );
    }
}
