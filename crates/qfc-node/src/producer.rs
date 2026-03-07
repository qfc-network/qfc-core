//! Block producer - handles block production loop

use parking_lot::RwLock;
use qfc_ai_coordinator::{ProofPool, TaskPool};
use qfc_chain::Chain;
use qfc_consensus::ConsensusEngine;
use qfc_crypto::blake3_hash;
use qfc_executor::Executor;
use qfc_mempool::Mempool;
use qfc_network::NetworkService;
use qfc_storage;
use qfc_types::{
    block_reward_for_year, DoubleSignEvidence, Heartbeat, InferenceProof, RewardDistribution,
    Transaction, ValidatorMessage, BLOCK_TIME_MS, FEE_BURN_PERCENT, FEE_PRODUCER_PERCENT,
    FEE_VOTERS_PERCENT, INFERENCE_FEE_MINER_PERCENT, INFERENCE_FEE_VALIDATORS_PERCENT,
    MAX_INFERENCE_PROOFS_PER_BLOCK, PRODUCER_REWARD_PERCENT, U256, VOTERS_REWARD_PERCENT,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{interval, Instant};
use tracing::{debug, error, info, warn};

/// Epoch duration in milliseconds (matches EPOCH_DURATION_SECS)
const EPOCH_DURATION_MS: u64 = qfc_types::EPOCH_DURATION_SECS * 1000;

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
    /// v2.0: Pool of verified inference proofs awaiting block inclusion
    proof_pool: Arc<RwLock<ProofPool>>,
    /// v2.0: Shared task pool for fee settlement
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
        chain_id: u64,
        proof_pool: Arc<RwLock<ProofPool>>,
        task_pool: Arc<RwLock<TaskPool>>,
    ) -> Self {
        Self {
            chain,
            consensus,
            mempool,
            network,
            executor: Executor::new(chain_id),
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

            // Advance epoch if enough time has passed
            let head_hash = self.chain.head().map(|h| h.hash).unwrap_or_default();
            self.consensus
                .maybe_advance_epoch(EPOCH_DURATION_MS, head_hash);

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

        // Drain inference proofs from pool (v2.0)
        let inference_proofs = self
            .proof_pool
            .write()
            .drain(MAX_INFERENCE_PROOFS_PER_BLOCK);

        // Execute transactions
        let state = self.chain.state();

        // Take snapshot before execution (for potential rollback)
        let _snapshot = state.snapshot();

        let (receipts, gas_used) =
            self.executor
                .execute_transactions(&transactions, &state, &our_address);

        // Settle inference fees for proofs matched to public tasks (v2.0)
        self.settle_inference_fees(&inference_proofs, &our_address);

        // Commit state to get new state root
        let state_root = state.commit()?;

        // Produce the block
        let block = self
            .consensus
            .produce_block(
                &parent_block,
                transactions.clone(),
                receipts.clone(),
                state_root,
                gas_used,
                inference_proofs,
            )
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

        // Calculate total fees from receipts
        let total_fees = self.calculate_total_fees(&transactions, &receipts);

        // Get voters for this block (for reward distribution)
        let voters = self.get_block_voters(&block_hash);

        // Distribute rewards
        match self.distribute_rewards(block_number, &our_address, total_fees, &voters) {
            Ok(distribution) => {
                debug!(
                    "Distributed rewards for block #{}: producer={}, voters={}, burned={}",
                    block_number,
                    distribution.producer_reward,
                    distribution.voter_reward,
                    distribution.fee_burned
                );
            }
            Err(e) => {
                warn!(
                    "Failed to distribute rewards for block #{}: {}",
                    block_number, e
                );
            }
        }

        info!(
            "Block #{} produced: {} txs, {} gas used",
            block_number, tx_count, gas_used
        );

        Ok(block_hash)
    }

    /// Settle inference fees for proofs matched to public tasks (v2.0)
    /// 70% miner, 10% validators, 20% burn
    fn settle_inference_fees(&self, proofs: &[InferenceProof], _producer: &qfc_types::Address) {
        let state = self.chain.state();
        let voters = self.get_block_voters(&qfc_types::Hash::ZERO);
        let mut task_pool = self.task_pool.write();

        for proof in proofs {
            // Check if this proof completes a public task (match by input_hash)
            if let Some(public_task) = task_pool.get_public_task_by_input_hash(&proof.input_hash) {
                if matches!(
                    public_task.status,
                    qfc_ai_coordinator::task_pool::PublicTaskStatus::Pending
                        | qfc_ai_coordinator::task_pool::PublicTaskStatus::Assigned
                ) {
                    let fee = U256::from_u128(public_task.max_fee);

                    // 70% to miner (proof submitter)
                    let miner_share =
                        fee * U256::from_u64(INFERENCE_FEE_MINER_PERCENT) / U256::from_u64(100);
                    if let Err(e) = state.add_balance(&proof.validator, miner_share) {
                        warn!("Failed to pay miner inference fee: {}", e);
                    }

                    // 10% to validators
                    let validator_share = fee * U256::from_u64(INFERENCE_FEE_VALIDATORS_PERCENT)
                        / U256::from_u64(100);
                    if let Err(e) = self.distribute_voter_rewards(&validator_share, &voters) {
                        warn!("Failed to distribute inference validator fees: {}", e);
                    }

                    // 20% burned (not distributed)

                    // Mark task completed
                    task_pool.complete_public_task_by_input_hash(
                        &proof.input_hash,
                        proof.output_hash.as_bytes().to_vec(),
                        proof.validator,
                        proof.execution_time_ms,
                    );

                    info!(
                        "Settled inference fee for task {}: {} to miner {}",
                        hex::encode(&proof.input_hash.as_bytes()[..8]),
                        miner_share,
                        proof.validator
                    );
                }
            }
        }

        // C2: Re-queue tasks assigned to miners that timed out
        let reassigned = task_pool.reassign_stale_tasks();
        if reassigned > 0 {
            info!("Reassigned {} stale inference tasks", reassigned);
        }

        // Prune expired tasks and refund submitters
        let expired = task_pool.prune_expired_public(now_ms());
        for task in expired {
            if task.submitter != qfc_types::Address::ZERO {
                let refund = U256::from_u128(task.max_fee);
                if let Err(e) = state.add_balance(&task.submitter, refund) {
                    warn!("Failed to refund expired task: {}", e);
                } else {
                    info!(
                        "Refunded {} for expired task {} to {}",
                        refund,
                        hex::encode(&task.task_id.as_bytes()[..8]),
                        task.submitter
                    );
                }
            }
        }
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

    /// Calculate the current year based on block height for reward halving
    fn calculate_year(&self, block_height: u64) -> u64 {
        // Estimate blocks per year based on block time
        // BLOCK_TIME_MS = 3333ms => ~262,800 blocks per year (365 * 24 * 60 * 60 * 1000 / 3333)
        let blocks_per_year = 365 * 24 * 60 * 60 * 1000 / BLOCK_TIME_MS;
        block_height / blocks_per_year
    }

    /// Distribute block rewards and fees after block production
    ///
    /// Block rewards: 70% producer, 30% voters (proportional by contribution score)
    /// Transaction fees: 50% producer, 30% voters, 20% burned
    pub fn distribute_rewards(
        &self,
        block_height: u64,
        producer: &qfc_types::Address,
        total_fees: U256,
        voters: &[(qfc_types::Address, u64)], // (voter_address, contribution_score)
    ) -> anyhow::Result<RewardDistribution> {
        let state = self.chain.state();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        // Calculate block reward with halving
        let year = self.calculate_year(block_height);
        let block_reward = block_reward_for_year(year);

        // Producer block reward (70%)
        let producer_block_reward =
            block_reward * U256::from_u64(PRODUCER_REWARD_PERCENT) / U256::from_u64(100);

        // Producer fee share (50%)
        let producer_fee_share =
            total_fees * U256::from_u64(FEE_PRODUCER_PERCENT) / U256::from_u64(100);

        // Total producer reward
        let producer_reward = producer_block_reward + producer_fee_share;

        // Add producer reward to balance
        state.add_balance(producer, producer_reward)?;

        // Voter block reward pool (30%)
        let voters_block_reward =
            block_reward * U256::from_u64(VOTERS_REWARD_PERCENT) / U256::from_u64(100);

        // Voter fee pool (30%)
        let voters_fee_share =
            total_fees * U256::from_u64(FEE_VOTERS_PERCENT) / U256::from_u64(100);

        // Total voter reward pool
        let voters_reward_pool = voters_block_reward + voters_fee_share;

        // Distribute voter rewards proportionally by contribution score
        let voter_reward = self.distribute_voter_rewards(&voters_reward_pool, voters)?;

        // Fee burned (20%)
        let fee_burned = total_fees * U256::from_u64(FEE_BURN_PERCENT) / U256::from_u64(100);
        // Note: Burned fees are simply not distributed, effectively removed from circulation

        debug!(
            "Block #{} rewards: producer={}, voters={}, burned={}",
            block_height, producer_reward, voter_reward, fee_burned
        );

        // Create reward distribution record
        let distribution =
            RewardDistribution::new(block_height, producer_reward, voter_reward, fee_burned, now);

        // Store the distribution record
        self.store_reward_distribution(&distribution)?;

        Ok(distribution)
    }

    /// Distribute voter rewards proportionally by contribution score
    fn distribute_voter_rewards(
        &self,
        total_reward: &U256,
        voters: &[(qfc_types::Address, u64)], // (voter_address, contribution_score)
    ) -> anyhow::Result<U256> {
        if voters.is_empty() || total_reward.is_zero() {
            return Ok(U256::ZERO);
        }

        let state = self.chain.state();

        // Calculate total contribution score
        let total_score: u64 = voters.iter().map(|(_, score)| score).sum();
        if total_score == 0 {
            return Ok(U256::ZERO);
        }

        let mut distributed = U256::ZERO;

        // Distribute proportionally
        for (voter_address, score) in voters {
            if *score == 0 {
                continue;
            }

            // reward = total_reward * score / total_score
            let voter_reward = *total_reward * U256::from_u64(*score) / U256::from_u64(total_score);

            if !voter_reward.is_zero() {
                state.add_balance(voter_address, voter_reward)?;
                distributed = distributed + voter_reward;

                debug!(
                    "Voter {} reward: {} (score: {}/{})",
                    voter_address, voter_reward, score, total_score
                );
            }
        }

        Ok(distributed)
    }

    /// Store reward distribution record to database
    fn store_reward_distribution(&self, distribution: &RewardDistribution) -> anyhow::Result<()> {
        let db = self.chain.db();
        let key = distribution.block_height.to_be_bytes();
        db.put(qfc_storage::cf::REWARDS, &key, &distribution.to_bytes())?;
        Ok(())
    }

    /// Broadcast double-sign evidence to the network
    #[allow(dead_code)]
    pub async fn broadcast_double_sign_evidence(&self, evidence: &DoubleSignEvidence) {
        let Some(network) = &self.network else {
            return;
        };

        let our_address = match self.consensus.our_address() {
            Some(addr) => addr,
            None => return,
        };

        // Convert to slashing evidence for network broadcast
        let mut slashing = evidence.to_slashing_evidence(our_address);

        // Sign the evidence
        let evidence_hash = blake3_hash(&slashing.to_bytes_without_signature());
        match self.consensus.sign_hash(&evidence_hash) {
            Ok(sig) => slashing.set_signature(sig),
            Err(_) => return,
        }

        // Broadcast
        let msg = ValidatorMessage::SlashingEvidence(slashing);
        if let Err(e) = network.broadcast_validator_msg(msg.to_bytes()).await {
            warn!("Failed to broadcast double-sign evidence: {}", e);
        } else {
            info!(
                "Broadcasted double-sign evidence for validator {} at height {}",
                evidence.validator, evidence.height
            );
        }
    }

    /// Get voters who accepted a block with their contribution scores
    fn get_block_voters(&self, _block_hash: &qfc_types::Hash) -> Vec<(qfc_types::Address, u64)> {
        let validators = self.consensus.get_validators();

        // Get pending votes for this block (accept votes only)
        // Note: In a full implementation, we'd have access to the votes from consensus
        // For now, we return all active validators as potential voters
        validators
            .iter()
            .filter(|v| v.is_active())
            .map(|v| (v.address, v.contribution_score))
            .collect()
    }

    /// Calculate total fees from executed transactions
    fn calculate_total_fees(
        &self,
        transactions: &[Transaction],
        receipts: &[qfc_types::Receipt],
    ) -> U256 {
        let mut total = U256::ZERO;

        for (tx, receipt) in transactions.iter().zip(receipts.iter()) {
            // Fee = gas_used * gas_price
            let fee = U256::from_u64(receipt.gas_used) * tx.gas_price;
            total = total + fee;
        }

        total
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
