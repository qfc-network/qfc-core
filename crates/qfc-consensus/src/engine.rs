//! Consensus engine implementation

use crate::error::{ConsensusError, Result};
use crate::scoring::{calculate_contribution_score, NetworkState};
use parking_lot::RwLock;
use qfc_crypto::{blake3_hash, vrf_output_to_f64, vrf_verify_with_seed, VrfKeypair};
use qfc_types::{
    Address, Block, BlockHeader, Epoch, Hash, Receipt, Signature, Transaction, ValidatorNode,
    Vote, BLOCK_VERSION, DEFAULT_BLOCK_GAS_LIMIT, FINALITY_THRESHOLD,
};
use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{debug, info};

/// Consensus engine configuration
#[derive(Clone, Debug)]
pub struct ConsensusConfig {
    /// Epoch duration in milliseconds
    pub epoch_duration_ms: u64,
    /// Blocks per epoch
    pub blocks_per_epoch: u64,
    /// Finality threshold (fraction of total weight needed)
    pub finality_threshold: f64,
    /// Vote timeout
    pub vote_timeout: Duration,
}

impl Default for ConsensusConfig {
    fn default() -> Self {
        Self {
            epoch_duration_ms: 10_000, // 10 seconds
            blocks_per_epoch: 3,
            finality_threshold: FINALITY_THRESHOLD,
            vote_timeout: Duration::from_secs(5),
        }
    }
}

/// Consensus engine
pub struct ConsensusEngine {
    /// Configuration
    config: ConsensusConfig,
    /// Our validator keypair (if validator)
    validator_key: Option<VrfKeypair>,
    /// Our address
    address: Option<Address>,
    /// Current epoch
    current_epoch: RwLock<Epoch>,
    /// Active validators
    validators: RwLock<Vec<ValidatorNode>>,
    /// Pending votes for blocks
    pending_votes: RwLock<HashMap<Hash, Vec<Vote>>>,
    /// Finalized blocks
    finalized_height: RwLock<u64>,
    /// Current network state for dynamic scoring
    network_state: RwLock<NetworkState>,
}

impl ConsensusEngine {
    /// Create a new consensus engine
    pub fn new(config: ConsensusConfig) -> Self {
        Self {
            config,
            validator_key: None,
            address: None,
            current_epoch: RwLock::new(Epoch::default()),
            validators: RwLock::new(Vec::new()),
            pending_votes: RwLock::new(HashMap::new()),
            finalized_height: RwLock::new(0),
            network_state: RwLock::new(NetworkState::default()),
        }
    }

    /// Create a consensus engine for a validator
    pub fn new_validator(config: ConsensusConfig, key: VrfKeypair, address: Address) -> Self {
        Self {
            config,
            validator_key: Some(key),
            address: Some(address),
            current_epoch: RwLock::new(Epoch::default()),
            validators: RwLock::new(Vec::new()),
            pending_votes: RwLock::new(HashMap::new()),
            finalized_height: RwLock::new(0),
            network_state: RwLock::new(NetworkState::default()),
        }
    }

    /// Check if we are a validator
    pub fn is_validator(&self) -> bool {
        self.validator_key.is_some()
    }

    /// Get our address
    pub fn our_address(&self) -> Option<Address> {
        self.address
    }

    /// Update the validator set
    pub fn update_validators(&self, validators: Vec<ValidatorNode>) {
        *self.validators.write() = validators;
    }

    /// Get current validators
    pub fn get_validators(&self) -> Vec<ValidatorNode> {
        self.validators.read().clone()
    }

    /// Set network state for dynamic scoring adjustments
    pub fn set_network_state(&self, state: NetworkState) {
        *self.network_state.write() = state;
    }

    /// Get current network state
    pub fn get_network_state(&self) -> NetworkState {
        *self.network_state.read()
    }

    /// Recalculate contribution scores for all validators
    /// This should be called at epoch boundaries or periodically
    pub fn update_contribution_scores(&self) {
        let mut validators = self.validators.write();
        let network_state = *self.network_state.read();

        // Calculate totals for normalization
        let total_stake: u128 = validators.iter().map(|v| v.stake.low_u128()).sum();
        let total_hashrate: u64 = validators
            .iter()
            .filter(|v| v.provides_compute)
            .map(|v| v.hashrate)
            .sum();
        let total_storage: u64 = validators.iter().map(|v| v.storage_provided_gb as u64).sum();

        // Update each validator's contribution score
        for validator in validators.iter_mut() {
            let new_score = calculate_contribution_score(
                validator,
                total_stake,
                total_hashrate,
                total_storage,
                network_state,
            );
            validator.contribution_score = new_score;
        }

        debug!(
            "Updated contribution scores for {} validators (total_stake={}, total_hashrate={}, total_storage={})",
            validators.len(),
            total_stake,
            total_hashrate,
            total_storage
        );
    }

    /// Get current epoch
    pub fn get_epoch(&self) -> Epoch {
        self.current_epoch.read().clone()
    }

    /// Start a new epoch
    pub fn start_epoch(&self, epoch_number: u64, seed: [u8; 32]) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let epoch = Epoch::new(epoch_number, seed, now);
        *self.current_epoch.write() = epoch;

        // Recalculate contribution scores at epoch boundary
        self.update_contribution_scores();

        info!("Started epoch {} with seed {:?}", epoch_number, &seed[..8]);
    }

    /// Select block producer for current epoch slot
    pub fn select_producer(&self, slot: u64) -> Option<Address> {
        let validators = self.validators.read();
        if validators.is_empty() {
            return None;
        }

        let epoch = self.current_epoch.read();

        // Compute slot-specific seed
        let mut slot_seed = [0u8; 32];
        let hash = blake3_hash(&[&epoch.seed[..], &slot.to_le_bytes()[..]].concat());
        slot_seed.copy_from_slice(hash.as_bytes());

        // Calculate total score
        let total_score: u64 = validators.iter().map(|v| v.contribution_score).sum();
        if total_score == 0 {
            return Some(validators[0].address);
        }

        // Select based on VRF output and contribution scores
        let random_value = vrf_output_to_f64(&slot_seed);
        let mut cumulative = 0.0f64;

        for validator in validators.iter() {
            if !validator.is_active() {
                continue;
            }

            let probability = validator.contribution_score as f64 / total_score as f64;
            cumulative += probability;

            if random_value < cumulative {
                return Some(validator.address);
            }
        }

        // Fallback to first active validator
        validators.iter().find(|v| v.is_active()).map(|v| v.address)
    }

    /// Check if we should produce a block
    pub fn should_produce(&self, slot: u64) -> bool {
        if let Some(our_address) = self.address {
            if let Some(producer) = self.select_producer(slot) {
                return producer == our_address;
            }
        }
        false
    }

    /// Produce a block
    pub fn produce_block(
        &self,
        parent: &Block,
        transactions: Vec<Transaction>,
        receipts: Vec<Receipt>,
        state_root: Hash,
        gas_used: u64,
    ) -> Result<Block> {
        let validator_key = self
            .validator_key
            .as_ref()
            .ok_or(ConsensusError::NotValidator)?;

        let epoch = self.current_epoch.read();

        // Generate VRF proof
        let vrf_proof = validator_key.prove_with_seed(&epoch.seed);

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        // Compute transaction and receipts roots
        let tx_hashes: Vec<Hash> = transactions
            .iter()
            .map(|tx| blake3_hash(&tx.to_bytes_without_signature()))
            .collect();
        let transactions_root = qfc_crypto::merkle_root(&tx_hashes);

        let receipt_hashes: Vec<Hash> = receipts.iter().map(|r| blake3_hash(&r.to_bytes())).collect();
        let receipts_root = qfc_crypto::merkle_root(&receipt_hashes);

        let our_address = self.address.ok_or(ConsensusError::NotValidator)?;
        let validator = self
            .validators
            .read()
            .iter()
            .find(|v| v.address == our_address)
            .cloned()
            .ok_or(ConsensusError::NotValidator)?;

        let header = BlockHeader {
            version: BLOCK_VERSION,
            number: parent.number() + 1,
            parent_hash: blake3_hash(&parent.header_bytes()),
            state_root,
            transactions_root,
            receipts_root,
            producer: our_address,
            contribution_score: validator.contribution_score,
            vrf_proof,
            timestamp: now,
            gas_limit: DEFAULT_BLOCK_GAS_LIMIT,
            gas_used,
            extra_data: Vec::new(),
        };

        let mut block = Block::new(header, transactions);

        // Sign the block
        let block_hash = blake3_hash(&block.header_bytes());
        let signature = validator_key.prove(block_hash.as_bytes()).proof;
        block.signature = Signature::new(signature);

        info!(
            "Produced block {} at height {}",
            block_hash,
            block.number()
        );

        Ok(block)
    }

    /// Validate a block
    pub fn validate_block(&self, block: &Block, parent: &Block) -> Result<()> {
        // 1. Check block number
        if block.number() != parent.number() + 1 {
            return Err(ConsensusError::InvalidStateTransition);
        }

        // 2. Check parent hash
        let expected_parent_hash = blake3_hash(&parent.header_bytes());
        if block.parent_hash() != expected_parent_hash {
            return Err(ConsensusError::InvalidStateTransition);
        }

        // 3. Check timestamp
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        if block.timestamp() > now + 30_000 {
            // Allow 30 seconds future tolerance
            return Err(ConsensusError::InvalidTimestamp);
        }

        if block.timestamp() <= parent.timestamp() {
            return Err(ConsensusError::InvalidTimestamp);
        }

        // 4. Check producer is valid
        let validators = self.validators.read();
        let producer = validators
            .iter()
            .find(|v| v.address == block.producer())
            .ok_or(ConsensusError::InvalidProducer)?;

        if !producer.is_active() {
            return Err(ConsensusError::ValidatorJailed);
        }

        // 5. Verify VRF proof against epoch seed
        let epoch = self.current_epoch.read();
        if producer.public_key != qfc_types::PublicKey::ZERO {
            // Only verify if producer has a public key set
            vrf_verify_with_seed(&producer.public_key, &epoch.seed, block.vrf_proof())
                .map_err(|_| ConsensusError::InvalidVrfProof)?;
        }

        // 6. Check block size
        if block.transactions.len() > qfc_types::MAX_TRANSACTIONS_PER_BLOCK {
            return Err(ConsensusError::BlockTooLarge);
        }

        Ok(())
    }

    /// Create a vote for a block
    pub fn vote(&self, block: &Block, accept: bool) -> Result<Vote> {
        let validator_key = self
            .validator_key
            .as_ref()
            .ok_or(ConsensusError::NotValidator)?;

        let our_address = self.address.ok_or(ConsensusError::NotValidator)?;
        let block_hash = blake3_hash(&block.header_bytes());

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let mut vote = if accept {
            Vote::accept(block_hash, block.number(), our_address, now)
        } else {
            Vote::reject(
                block_hash,
                block.number(),
                our_address,
                qfc_types::RejectReason::InvalidStateTransition,
                now,
            )
        };

        // Sign the vote
        let vote_hash = blake3_hash(&vote.to_bytes_without_signature());
        let signature = validator_key.prove(vote_hash.as_bytes()).proof;
        vote.signature = Signature::new(signature);

        Ok(vote)
    }

    /// Add a vote to pending votes
    pub fn add_vote(&self, vote: Vote) {
        self.pending_votes
            .write()
            .entry(vote.block_hash)
            .or_default()
            .push(vote);
    }

    /// Check if a block has reached finality
    pub fn check_finality(&self, block_hash: &Hash) -> bool {
        let votes = self.pending_votes.read();
        let block_votes = match votes.get(block_hash) {
            Some(v) => v,
            None => return false,
        };

        let validators = self.validators.read();

        // Count accept votes weighted by contribution score
        let accept_weight: u64 = block_votes
            .iter()
            .filter(|v| v.is_accept())
            .filter_map(|v| validators.iter().find(|val| val.address == v.voter))
            .map(|val| val.contribution_score)
            .sum();

        let total_weight: u64 = validators.iter().map(|v| v.contribution_score).sum();

        if total_weight == 0 {
            return false;
        }

        let ratio = accept_weight as f64 / total_weight as f64;
        ratio >= self.config.finality_threshold
    }

    /// Get finalized height
    pub fn finalized_height(&self) -> u64 {
        *self.finalized_height.read()
    }

    /// Set finalized height
    pub fn set_finalized_height(&self, height: u64) {
        *self.finalized_height.write() = height;
    }

    /// Clear votes for blocks below finalized height
    pub fn prune_old_votes(&self, finalized_height: u64) {
        self.pending_votes.write().retain(|_, votes| {
            votes
                .first()
                .map(|v| v.block_height > finalized_height)
                .unwrap_or(false)
        });
    }

    /// Record that a validator produced a block successfully
    pub fn record_block_produced(&self, producer: &Address) {
        let mut validators = self.validators.write();
        if let Some(validator) = validators.iter_mut().find(|v| v.address == *producer) {
            validator.blocks_produced += 1;
            validator.last_active = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;

            // Slight reputation boost for successful block production
            validator.reputation = (validator.reputation + 10).min(10000);
        }
    }

    /// Record a vote from a validator
    pub fn record_vote(&self, voter: &Address, is_valid: bool) {
        let mut validators = self.validators.write();
        if let Some(validator) = validators.iter_mut().find(|v| v.address == *voter) {
            if is_valid {
                validator.valid_votes += 1;
                // Update accuracy with EMA
                validator.accuracy = ((validator.accuracy as u64 * 99 + 10000) / 100) as u32;
            } else {
                validator.invalid_votes += 1;
                // Decrease accuracy
                validator.accuracy = ((validator.accuracy as u64 * 99) / 100) as u32;
            }

            validator.last_active = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;
        }
    }

    /// Update validator uptime based on expected vs actual block production
    pub fn update_validator_uptime(&self, address: &Address, expected: u64, actual: u64) {
        if expected == 0 {
            return;
        }

        let mut validators = self.validators.write();
        if let Some(validator) = validators.iter_mut().find(|v| v.address == *address) {
            // Calculate period uptime (0-10000)
            let period_uptime = (actual * 10000 / expected).min(10000) as u32;

            // Exponential moving average: 90% old + 10% new
            validator.uptime = ((validator.uptime as u64 * 9 + period_uptime as u64) / 10) as u32;
        }
    }

    /// Record network latency measurement for a validator
    pub fn record_latency(&self, address: &Address, latency_ms: u32) {
        let mut validators = self.validators.write();
        if let Some(validator) = validators.iter_mut().find(|v| v.address == *address) {
            // EMA for latency
            validator.avg_latency_ms =
                ((validator.avg_latency_ms as u64 * 9 + latency_ms as u64) / 10) as u32;
        }
    }

    /// Slash a validator for misbehavior
    pub fn slash_validator(&self, address: &Address, slash_percent: u8, jail_duration_ms: u64) {
        let mut validators = self.validators.write();
        if let Some(validator) = validators.iter_mut().find(|v| v.address == *address) {
            // Reduce stake
            let slash_amount = validator.stake * qfc_types::U256::from_u64(slash_percent as u64)
                / qfc_types::U256::from_u64(100);
            validator.stake = validator.stake.saturating_sub(slash_amount);

            // Reduce reputation significantly
            validator.reputation = (validator.reputation as i32 - 2000).max(0) as u32;

            // Jail the validator
            if jail_duration_ms > 0 {
                validator.is_jailed = true;
                validator.jail_until = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64
                    + jail_duration_ms;
            }

            info!(
                "Slashed validator {}: {}% stake, jailed for {}ms",
                address, slash_percent, jail_duration_ms
            );
        }
    }

    /// Check and unjail validators whose jail period has expired
    pub fn process_unjails(&self) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let mut validators = self.validators.write();
        for validator in validators.iter_mut() {
            if validator.can_unjail(now) {
                validator.is_jailed = false;
                validator.jail_until = 0;
                info!("Validator {} unjailed", validator.address);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_validators(count: usize) -> Vec<ValidatorNode> {
        (0..count)
            .map(|i| {
                let mut v = ValidatorNode::default();
                v.address = Address::new([i as u8; 20]);
                v.stake = qfc_types::U256::from_u64(10000);
                v.contribution_score = 1000;
                v.uptime = 9500;
                v.accuracy = 9800;
                v.reputation = 8000;
                v
            })
            .collect()
    }

    #[test]
    fn test_consensus_engine_creation() {
        let engine = ConsensusEngine::new(ConsensusConfig::default());
        assert!(!engine.is_validator());
    }

    #[test]
    fn test_validator_engine() {
        let key = VrfKeypair::generate();
        let address = Address::new([0x11; 20]);
        let engine =
            ConsensusEngine::new_validator(ConsensusConfig::default(), key, address);

        assert!(engine.is_validator());
        assert_eq!(engine.our_address(), Some(address));
    }

    #[test]
    fn test_producer_selection() {
        let engine = ConsensusEngine::new(ConsensusConfig::default());
        let validators = create_test_validators(3);

        engine.update_validators(validators);
        engine.start_epoch(1, [0xab; 32]);

        // Should select a producer
        let producer = engine.select_producer(0);
        assert!(producer.is_some());
    }

    #[test]
    fn test_network_state() {
        let engine = ConsensusEngine::new(ConsensusConfig::default());

        assert_eq!(engine.get_network_state(), NetworkState::Normal);

        engine.set_network_state(NetworkState::Congested);
        assert_eq!(engine.get_network_state(), NetworkState::Congested);
    }

    #[test]
    fn test_contribution_score_update() {
        let engine = ConsensusEngine::new(ConsensusConfig::default());
        let validators = create_test_validators(3);

        engine.update_validators(validators);
        engine.update_contribution_scores();

        // All validators should have non-zero scores now
        let updated = engine.get_validators();
        for v in updated {
            assert!(v.contribution_score > 0);
        }
    }

    #[test]
    fn test_record_block_produced() {
        let engine = ConsensusEngine::new(ConsensusConfig::default());
        let validators = create_test_validators(1);
        let address = validators[0].address;

        engine.update_validators(validators);
        engine.record_block_produced(&address);

        let updated = engine.get_validators();
        assert_eq!(updated[0].blocks_produced, 1);
        assert!(updated[0].reputation >= 8000); // Should have slight increase
    }

    #[test]
    fn test_record_valid_vote() {
        let engine = ConsensusEngine::new(ConsensusConfig::default());
        let validators = create_test_validators(1);
        let address = validators[0].address;

        engine.update_validators(validators);
        engine.record_vote(&address, true);

        let updated = engine.get_validators();
        assert_eq!(updated[0].valid_votes, 1);
        assert_eq!(updated[0].invalid_votes, 0);
    }

    #[test]
    fn test_record_invalid_vote() {
        let engine = ConsensusEngine::new(ConsensusConfig::default());
        let validators = create_test_validators(1);
        let address = validators[0].address;

        engine.update_validators(validators);
        engine.record_vote(&address, false);

        let updated = engine.get_validators();
        assert_eq!(updated[0].valid_votes, 0);
        assert_eq!(updated[0].invalid_votes, 1);
        // Accuracy should decrease
        assert!(updated[0].accuracy < 9800);
    }

    #[test]
    fn test_update_uptime() {
        let engine = ConsensusEngine::new(ConsensusConfig::default());
        let validators = create_test_validators(1);
        let address = validators[0].address;

        engine.update_validators(validators);

        // 80% production rate should decrease uptime
        engine.update_validator_uptime(&address, 10, 8);

        let updated = engine.get_validators();
        assert!(updated[0].uptime < 9500);
    }

    #[test]
    fn test_record_latency() {
        let engine = ConsensusEngine::new(ConsensusConfig::default());
        let validators = create_test_validators(1);
        let address = validators[0].address;

        engine.update_validators(validators);
        engine.record_latency(&address, 200);

        let updated = engine.get_validators();
        // EMA should move towards 200 from default 100
        assert!(updated[0].avg_latency_ms > 100);
    }

    #[test]
    fn test_slash_validator() {
        let engine = ConsensusEngine::new(ConsensusConfig::default());
        let validators = create_test_validators(1);
        let address = validators[0].address;

        engine.update_validators(validators);

        // Slash 10% with 1 hour jail
        engine.slash_validator(&address, 10, 3600_000);

        let updated = engine.get_validators();
        // Stake should be reduced by 10%
        assert_eq!(updated[0].stake, qfc_types::U256::from_u64(9000));
        // Reputation should be significantly reduced
        assert!(updated[0].reputation < 8000);
        // Should be jailed
        assert!(updated[0].is_jailed);
    }

    #[test]
    fn test_epoch_updates_scores() {
        let engine = ConsensusEngine::new(ConsensusConfig::default());
        let validators = create_test_validators(3);

        engine.update_validators(validators);

        // Start epoch should trigger score update
        engine.start_epoch(1, [0xab; 32]);

        let updated = engine.get_validators();
        for v in updated {
            assert!(v.contribution_score > 0);
        }
    }
}
