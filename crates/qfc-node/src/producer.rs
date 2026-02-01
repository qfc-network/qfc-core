//! Block producer - handles block production loop

use parking_lot::RwLock;
use qfc_chain::Chain;
use qfc_consensus::ConsensusEngine;
use qfc_crypto::blake3_hash;
use qfc_executor::Executor;
use qfc_mempool::Mempool;
use qfc_network::NetworkService;
use qfc_types::{Heartbeat, Transaction, ValidatorMessage};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{interval, Instant};
use tracing::{debug, error, info, warn};

/// Block producer configuration
#[derive(Clone, Debug)]
pub struct ProducerConfig {
    /// Block interval in milliseconds
    pub block_interval_ms: u64,
    /// Maximum transactions per block
    pub max_txs_per_block: usize,
    /// Whether to produce empty blocks
    pub produce_empty_blocks: bool,
}

impl Default for ProducerConfig {
    fn default() -> Self {
        Self {
            block_interval_ms: 3000, // 3 seconds
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
    executor: Executor,
    config: ProducerConfig,
}

impl BlockProducer {
    /// Create a new block producer
    pub fn new(
        chain: Arc<Chain>,
        consensus: Arc<ConsensusEngine>,
        mempool: Arc<RwLock<Mempool>>,
        network: Option<Arc<NetworkService>>,
        config: ProducerConfig,
        chain_id: u64,
    ) -> Self {
        Self {
            chain,
            consensus,
            mempool,
            network,
            executor: Executor::new(chain_id),
            config,
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

        // Initialize epoch with a deterministic seed based on genesis
        let genesis_hash = self.chain.genesis_hash().unwrap_or_default();
        let mut epoch_seed = [0u8; 32];
        epoch_seed.copy_from_slice(genesis_hash.as_bytes());
        self.consensus.start_epoch(1, epoch_seed);

        // Validators are already loaded from genesis in chain.rs
        // No need to override here

        let mut block_timer = interval(Duration::from_millis(self.config.block_interval_ms));
        let mut heartbeat_counter: u64 = 0;
        let heartbeat_interval = 3; // Send heartbeat every 3 slots
        let mut slot: u64 = 0;

        loop {
            block_timer.tick().await;
            slot += 1;
            heartbeat_counter += 1;

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
                    info!(
                        "Produced block {} in {:?}",
                        block_hash, elapsed
                    );
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

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

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

        // Execute transactions
        let state = self.chain.state();

        // Take snapshot before execution (for potential rollback)
        let _snapshot = state.snapshot();

        let (receipts, gas_used) = self
            .executor
            .execute_transactions(&transactions, &state, &our_address);

        // Commit state to get new state root
        let state_root = state.commit()?;

        // Produce the block
        let block = self
            .consensus
            .produce_block(&parent_block, transactions.clone(), receipts.clone(), state_root, gas_used)
            .map_err(|e| anyhow::anyhow!("Consensus error: {}", e))?;

        let block_hash = blake3_hash(&block.header_bytes());
        let block_number = block.number();

        // Store the block
        self.chain.store_produced_block(&block, &receipts)?;

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
            block_number, tx_count, gas_used
        );

        Ok(block_hash)
    }

    /// Select transactions from mempool
    fn select_transactions(&self) -> Vec<Transaction> {
        let mempool = self.mempool.read();

        // Get transactions sorted by gas price
        mempool.select(
            qfc_types::DEFAULT_BLOCK_GAS_LIMIT,
            self.config.max_txs_per_block,
        )
    }
}
