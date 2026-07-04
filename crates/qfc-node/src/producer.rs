//! Block producer - handles block production loop
//!
//! State-transition note (ADR-0012 / spec §1): the producer performs NO
//! direct state mutation. The whole transition — undelegations, transactions,
//! rewards — lives in `Chain::execute_at`, shared byte-identically with block
//! import, and the produced block is committed through
//! `Chain::store_produced_block`, which self-validates exactly like an
//! importer. Voter splits and inference-fee settlement were REMOVED from the
//! block path (they consumed node-local inputs and forked every import).

use parking_lot::RwLock;
use qfc_ai_coordinator::{ProofPool, TaskPool};
use qfc_chain::Chain;
use qfc_consensus::ConsensusEngine;
use qfc_crypto::blake3_hash;
use qfc_mempool::Mempool;
use qfc_network::NetworkService;
use qfc_storage;
use qfc_types::{
    Heartbeat, Transaction, ValidatorMessage, BLOCK_INTERVAL_MS, MAX_INFERENCE_PROOFS_PER_BLOCK,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{interval, Instant};
use tracing::{debug, error, info, warn};

/// Block producer configuration
#[derive(Clone, Debug)]
pub struct ProducerConfig {
    /// Maximum transactions per block
    pub max_txs_per_block: usize,
    /// Whether to produce empty blocks
    pub produce_empty_blocks: bool,
}

impl Default for ProducerConfig {
    fn default() -> Self {
        Self {
            max_txs_per_block: 1000,
            produce_empty_blocks: true, // For dev mode, produce even if no txs
        }
    }
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
        let mut block_timer = interval(Duration::from_millis(BLOCK_INTERVAL_MS));
        let mut heartbeat_counter: u64 = 0;
        let heartbeat_interval = 3; // Send heartbeat every 3 slots
        let mut last_slot: u64 = u64::MAX;

        loop {
            block_timer.tick().await;

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

            // Send periodic heartbeat
            if heartbeat_counter >= heartbeat_interval {
                heartbeat_counter = 0;
                self.send_heartbeat().await;
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

        // Drain inference proofs from pool (v2.0)
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
        let outcome = self.chain.execute_at(
            parent_block.state_root(),
            block_number,
            timestamp,
            &our_address,
            &transactions,
        )?;

        // Seal the block (VRF proved against the block-derived epoch seed).
        let block = self
            .consensus
            .produce_block(
                &parent_block,
                transactions.clone(),
                outcome.receipts.clone(),
                outcome.state_root,
                outcome.gas_used,
                inference_proofs,
                timestamp,
            )
            .map_err(|e| anyhow::anyhow!("Consensus error: {}", e))?;

        let block_hash = blake3_hash(&block.header_bytes());
        let block_number = block.number();

        // Store the block: self-validates + re-executes exactly like an
        // importer, under the chain-wide import lock.
        self.chain.store_produced_block(&block).await?;

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

            // Cast and broadcast our own vote for the block we produced
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
