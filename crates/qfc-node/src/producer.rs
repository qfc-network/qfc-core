//! Block producer - handles block production loop

use parking_lot::RwLock;
use qfc_chain::Chain;
use qfc_consensus::ConsensusEngine;
use qfc_crypto::blake3_hash;
use qfc_executor::Executor;
use qfc_mempool::Mempool;
use qfc_types::{Transaction, ValidatorNode};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{interval, Instant};
use tracing::{debug, error, info};

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
    executor: Executor,
    config: ProducerConfig,
}

impl BlockProducer {
    /// Create a new block producer
    pub fn new(
        chain: Arc<Chain>,
        consensus: Arc<ConsensusEngine>,
        mempool: Arc<RwLock<Mempool>>,
        config: ProducerConfig,
        chain_id: u64,
    ) -> Self {
        Self {
            chain,
            consensus,
            mempool,
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

        // Set up initial validator set (in dev mode, just ourselves)
        let mut validator = ValidatorNode::default();
        validator.address = our_address;
        validator.stake = qfc_types::U256::from_u64(1_000_000);
        validator.contribution_score = 1000;
        self.consensus.update_validators(vec![validator]);

        let mut block_timer = interval(Duration::from_millis(self.config.block_interval_ms));
        let mut slot: u64 = 0;

        loop {
            block_timer.tick().await;
            slot += 1;

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

        // Take snapshot before execution
        let snapshot = state.snapshot();

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
