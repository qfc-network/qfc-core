//! Consensus engine implementation

use crate::error::{ConsensusError, Result};
use crate::scoring::{calculate_contribution_score, NetworkState};
use parking_lot::RwLock;
use qfc_crypto::{blake3_hash, vrf_verify_with_seed, VrfKeypair};
use qfc_pow::{calculate_hashrate, initial_difficulty, verify_proof};
use qfc_storage;
use qfc_types::{
    Address, Block, BlockHeader, DifficultyConfig, DoubleSignEvidence, Epoch, Hash, InferenceProof,
    MiningTask, Receipt, Signature, Transaction, ValidatorCheckpoint, ValidatorNode, Vote,
    WorkProof, BLOCK_INTERVAL_MS, BLOCK_VERSION, DEFAULT_BLOCK_GAS_LIMIT, EPOCH_DURATION_MS,
    FINALITY_THRESHOLD, MAX_TIMESTAMP_DRIFT_MS, SLASH_DOUBLE_SIGN_PERCENT,
};
use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{debug, info, warn};

/// How many recent validator checkpoints to retain in the `checkpoints`
/// column family. Older checkpoints are pruned when a new one is written,
/// keeping the CF bounded (one checkpoint per epoch would otherwise grow
/// without limit). Restart only ever needs the newest usable checkpoint;
/// earlier ones exist purely as fallback against corruption.
pub const CHECKPOINT_RETENTION: usize = 64;

/// Consensus engine configuration
///
/// NOTE: slot length and epoch duration are deliberately NOT configurable —
/// they are chain constants ([`BLOCK_INTERVAL_MS`] / [`EPOCH_DURATION_MS`]).
/// A per-node value for either is a silent consensus fork (§6 of
/// docs/adr/0012-consensus-convergence-fixes.md).
#[derive(Clone, Debug)]
pub struct ConsensusConfig {
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
            blocks_per_epoch: 3,
            finality_threshold: FINALITY_THRESHOLD,
            vote_timeout: Duration::from_secs(5),
        }
    }
}

/// Block info for double-sign detection
#[derive(Clone, Debug)]
struct BlockRecord {
    hash: Hash,
    producer: Address,
    signature: Signature,
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
    /// Block cache for double-sign detection: height -> list of blocks
    block_cache: RwLock<HashMap<u64, Vec<BlockRecord>>>,
    /// Maximum blocks to cache per height (for memory limits)
    max_blocks_per_height: usize,
    /// Cache depth (how many heights to keep)
    cache_depth: u64,
    /// Genesis epoch seed, set exactly once at chain initialization (from the
    /// chain's genesis hash, before any producer/miner/sync task spawns).
    /// Anchors the deterministic per-epoch seed derivation so every node
    /// computes the same seed for a given (wall-clock) epoch number. It is
    /// never adopted from the network and survives checkpoint restore
    /// (restore does not touch it).
    genesis_seed: RwLock<Option<[u8; 32]>>,
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
            block_cache: RwLock::new(HashMap::new()),
            max_blocks_per_height: 10,
            cache_depth: 100,
            genesis_seed: RwLock::new(None),
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
            block_cache: RwLock::new(HashMap::new()),
            max_blocks_per_height: 10,
            cache_depth: 100,
            genesis_seed: RwLock::new(None),
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

        // Calculate totals for normalization - use total stake (direct + delegated)
        let total_stake: u128 = validators.iter().map(|v| v.total_stake().low_u128()).sum();
        let total_hashrate: u64 = validators
            .iter()
            .filter(|v| v.provides_compute)
            .map(|v| v.hashrate)
            .sum();
        let total_storage: u64 = validators
            .iter()
            .map(|v| v.storage_provided_gb as u64)
            .sum();

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

    /// Set the genesis seed exactly once, from the chain's genesis hash.
    ///
    /// Called by `Chain::new` during initialization — i.e. before any
    /// producer/miner/sync task can run — so there is no capture race
    /// (defect D11). Subsequent calls are ignored.
    pub fn set_genesis_seed(&self, seed: [u8; 32]) {
        let mut gs = self.genesis_seed.write();
        if gs.is_none() {
            *gs = Some(seed);
        } else if *gs != Some(seed) {
            warn!("Ignoring attempt to change the genesis seed");
        }
    }

    /// Whether the genesis seed has been initialized.
    pub fn has_genesis_seed(&self) -> bool {
        self.genesis_seed.read().is_some()
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

    /// Deterministic epoch seed: `blake3(genesis_seed || epoch_number)`.
    /// O(1) and identical on every node (all share the genesis seed), so a
    /// wall-clock epoch number — which can be very large — maps to a shared
    /// seed without walking a hash chain.
    ///
    /// Errors with [`ConsensusError::GenesisSeedUnset`] if the genesis seed
    /// has not been initialized — there is deliberately NO fallback to a
    /// default seed (an all-zero anchor was one of the fork defects, D11).
    pub fn derive_epoch_seed(&self, epoch_number: u64) -> Result<[u8; 32]> {
        let genesis = self
            .genesis_seed
            .read()
            .ok_or(ConsensusError::GenesisSeedUnset)?;
        let h = blake3_hash(&[&genesis[..], &epoch_number.to_le_bytes()[..]].concat());
        let mut seed = [0u8; 32];
        seed.copy_from_slice(h.as_bytes());
        Ok(seed)
    }

    /// The wall-clock slot containing `timestamp_ms`.
    pub fn slot_of_timestamp(timestamp_ms: u64) -> u64 {
        timestamp_ms / BLOCK_INTERVAL_MS
    }

    /// The epoch a slot belongs to. `EPOCH_DURATION_MS` is a multiple of
    /// `BLOCK_INTERVAL_MS`, so a slot never straddles an epoch boundary and
    /// this is exact.
    pub fn epoch_of_slot(slot: u64) -> u64 {
        slot / (EPOCH_DURATION_MS / BLOCK_INTERVAL_MS)
    }

    /// Advance to the epoch implied by the current wall-clock time, if it has
    /// changed. Returns the (possibly updated) epoch number.
    ///
    /// The epoch number is `now_ms / EPOCH_DURATION_MS` — a global function of
    /// wall-clock time (NTP-synced across nodes), NOT of when this node
    /// started. Anchoring to wall-clock makes every node agree on the current
    /// epoch — and therefore on the seed and the elected producer. The seed is
    /// derived directly from the genesis seed (see `derive_epoch_seed`).
    ///
    /// The tracked `current_epoch` is observability/mining state only —
    /// block validation never reads it (validation derives everything from
    /// the block's own timestamp; §2 of ADR-0012).
    pub fn maybe_advance_epoch(&self) -> u64 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let target = now / EPOCH_DURATION_MS;

        if target == self.get_epoch().number {
            return target;
        }

        match self.derive_epoch_seed(target) {
            Ok(seed) => {
                self.start_epoch(target, seed);
                target
            }
            Err(_) => {
                // Genesis seed not initialized yet — do not anchor an epoch
                // to a bogus seed; keep the current epoch until it is.
                self.get_epoch().number
            }
        }
    }

    /// The election set: genesis-registered validators with `stake > 0`, in
    /// canonical (address-sorted) order.
    ///
    /// Deliberately deterministic-only inputs (§3 of ADR-0012):
    /// - NO jail flags — local jailing comes from gossip evidence without
    ///   in-block proof and diverges between nodes; it must not affect
    ///   consensus. Slashing moves on-chain later.
    /// - NO contribution scores — latency EMAs, votes, hashrate etc. are
    ///   node-local observations. Scores remain for metrics/observability.
    fn election_set(&self) -> Vec<(Address, qfc_types::PublicKey)> {
        let validators = self.validators.read();
        let mut set: Vec<(Address, qfc_types::PublicKey)> = validators
            .iter()
            .filter(|v| v.stake > qfc_types::U256::ZERO)
            .map(|v| (v.address, v.public_key))
            .collect();
        set.sort_by_key(|(addr, _)| addr.0);
        set
    }

    /// Deterministic leader for `slot` under `epoch_seed`: round-robin over
    /// the address-sorted `stake > 0` set, rotated by a seed-derived offset.
    /// A pure function of (validator set, epoch seed, slot) — identical on
    /// every node.
    pub fn select_producer_with_seed(&self, slot: u64, epoch_seed: &[u8; 32]) -> Option<Address> {
        let set = self.election_set();
        if set.is_empty() {
            return None;
        }
        let offset = u64::from_le_bytes(epoch_seed[..8].try_into().unwrap());
        let idx = (slot.wrapping_add(offset) % set.len() as u64) as usize;
        Some(set[idx].0)
    }

    /// Select the block producer for a wall-clock slot. The epoch seed is
    /// derived from the slot itself (genesis-anchored), never from
    /// `current_epoch`. Returns `None` when the genesis seed is unset or the
    /// election set is empty.
    pub fn select_producer(&self, slot: u64) -> Option<Address> {
        let seed = self.derive_epoch_seed(Self::epoch_of_slot(slot)).ok()?;
        self.select_producer_with_seed(slot, &seed)
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

    /// Produce a block with the given header timestamp (milliseconds).
    ///
    /// The timestamp is a parameter — not read here — because the caller must
    /// execute the block body against the parent state with the SAME
    /// timestamp that lands in the header (undelegation maturity is
    /// timestamp-driven). The VRF proof is generated against the seed of the
    /// epoch containing the timestamp's slot, exactly mirroring what
    /// validation derives from the header (§2 of ADR-0012).
    pub fn produce_block(
        &self,
        parent: &Block,
        transactions: Vec<Transaction>,
        receipts: Vec<Receipt>,
        state_root: Hash,
        gas_used: u64,
        inference_proofs: Vec<InferenceProof>,
        timestamp_ms: u64,
    ) -> Result<Block> {
        let validator_key = self
            .validator_key
            .as_ref()
            .ok_or(ConsensusError::NotValidator)?;

        // Generate the VRF proof against the block-derived epoch seed — the
        // same derivation every validator applies when importing this block.
        let slot = Self::slot_of_timestamp(timestamp_ms);
        let seed = self.derive_epoch_seed(Self::epoch_of_slot(slot))?;
        let vrf_proof = validator_key.prove_with_seed(&seed);

        let now = timestamp_ms;

        // Compute transaction and receipts roots
        let tx_hashes: Vec<Hash> = transactions
            .iter()
            .map(|tx| blake3_hash(&tx.to_bytes_without_signature()))
            .collect();
        let transactions_root = qfc_crypto::merkle_root(&tx_hashes);

        let receipt_hashes: Vec<Hash> = receipts
            .iter()
            .map(|r| blake3_hash(&r.to_bytes()))
            .collect();
        let receipts_root = qfc_crypto::merkle_root(&receipt_hashes);

        // Compute inference proofs root (v2.0)
        let proof_hashes: Vec<Hash> = inference_proofs
            .iter()
            .map(|p| blake3_hash(&p.to_bytes_without_signature()))
            .collect();
        let proofs_root = qfc_crypto::merkle_root(&proof_hashes);

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
            proofs_root,
            producer: our_address,
            contribution_score: validator.contribution_score,
            vrf_proof,
            timestamp: now,
            gas_limit: DEFAULT_BLOCK_GAS_LIMIT,
            gas_used,
            extra_data: Vec::new(),
        };

        let mut block = Block::new(header, transactions);
        block.inference_proofs = inference_proofs;

        // Sign the block
        let block_hash = blake3_hash(&block.header_bytes());
        let signature = validator_key.prove(block_hash.as_bytes()).proof;
        block.signature = Signature::new(signature);

        info!("Produced block {} at height {}", block_hash, block.number());

        Ok(block)
    }

    /// Validate a block — SELF-CONTAINED (§2 of ADR-0012).
    ///
    /// Everything is derived from the block itself + chain constants:
    /// slot/epoch come from the block's timestamp, the epoch seed from the
    /// genesis-anchored derivation, and the expected producer from the
    /// deterministic election. `current_epoch` is never consulted, so blocks
    /// from any past epoch validate identically on every node at any time
    /// (fixes D8: historical imports used to fail `InvalidVrfProof` because
    /// they were checked against the receiver's *current* rotating seed).
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

        // 3. Check timestamp: strictly increasing, and at most MAX_DRIFT into
        // our future (trivially true for historical imports).
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        if block.timestamp() > now + MAX_TIMESTAMP_DRIFT_MS {
            return Err(ConsensusError::InvalidTimestamp);
        }

        if block.timestamp() <= parent.timestamp() {
            return Err(ConsensusError::InvalidTimestamp);
        }

        // 4. Producer must exist in the election set's validator registry.
        // Local jail flags deliberately do NOT gate validation (§3): jailing
        // is driven by gossip evidence without in-block proof, so it is
        // node-local and cannot be a consensus input.
        let producer_key = {
            let validators = self.validators.read();
            validators
                .iter()
                .find(|v| v.address == block.producer())
                .map(|v| v.public_key)
                .ok_or(ConsensusError::InvalidProducer)?
        };

        // 5. Enforce the deterministic schedule: the producer must be the
        // elected leader of the block's own slot (derived from its
        // timestamp), with a one-slot tolerance ONLY when the timestamp sits
        // within MAX_DRIFT of the slot boundary (clock skew at the edge).
        // The VRF proof is verified against the seed of the matched slot's
        // epoch — the same seed the producer used.
        let slot = Self::slot_of_timestamp(block.timestamp());
        let offset_in_slot = block.timestamp() % BLOCK_INTERVAL_MS;

        let mut candidate_slots = vec![slot];
        if offset_in_slot < MAX_TIMESTAMP_DRIFT_MS && slot > 0 {
            candidate_slots.push(slot - 1);
        }
        if offset_in_slot > BLOCK_INTERVAL_MS - MAX_TIMESTAMP_DRIFT_MS {
            candidate_slots.push(slot + 1);
        }

        let mut producer_matched = false;
        let mut vrf_ok = false;
        for candidate in candidate_slots {
            let seed = self.derive_epoch_seed(Self::epoch_of_slot(candidate))?;
            if self.select_producer_with_seed(candidate, &seed) != Some(block.producer()) {
                continue;
            }
            producer_matched = true;
            // Producer is the leader of this candidate slot; the VRF proof
            // must verify against that slot's epoch seed. (Zero public key
            // means "unknown key" — only possible for hand-built test sets;
            // genesis validators always carry a real key.)
            if producer_key == qfc_types::PublicKey::ZERO
                || vrf_verify_with_seed(&producer_key, &seed, block.vrf_proof()).is_ok()
            {
                vrf_ok = true;
                break;
            }
        }
        if !producer_matched {
            return Err(ConsensusError::InvalidProducer);
        }
        if !vrf_ok {
            return Err(ConsensusError::InvalidVrfProof);
        }

        // 6. Check block size
        if block.transactions.len() > qfc_types::MAX_TRANSACTIONS_PER_BLOCK {
            return Err(ConsensusError::BlockTooLarge);
        }

        // 7. Verify inference proofs root (v2.0, skip for version < 2)
        if block.header.version >= 2 || !block.inference_proofs.is_empty() {
            let proof_hashes: Vec<Hash> = block
                .inference_proofs
                .iter()
                .map(|p| blake3_hash(&p.to_bytes_without_signature()))
                .collect();
            let expected_proofs_root = qfc_crypto::merkle_root(&proof_hashes);
            if block.header.proofs_root != expected_proofs_root {
                return Err(ConsensusError::InvalidStateTransition);
            }

            if block.inference_proofs.len() > qfc_types::MAX_INFERENCE_PROOFS_PER_BLOCK {
                return Err(ConsensusError::BlockTooLarge);
            }
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

    /// Sign a message hash with our validator key
    pub fn sign_hash(&self, hash: &Hash) -> Result<Signature> {
        let validator_key = self
            .validator_key
            .as_ref()
            .ok_or(ConsensusError::NotValidator)?;

        let signature = validator_key.prove(hash.as_bytes()).proof;
        Ok(Signature::new(signature))
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

    // ============ Validator Persistence ============

    /// Save validators to database
    pub fn save_validators(&self, db: &qfc_storage::Database) -> Result<()> {
        let validators = self.validators.read();

        for validator in validators.iter() {
            let key = validator.address.as_bytes();
            let value = validator.to_bytes();
            db.put(qfc_storage::cf::VALIDATORS, key, &value).map_err(
                |e: qfc_storage::StorageError| ConsensusError::StorageError(e.to_string()),
            )?;
        }

        debug!("Saved {} validators to database", validators.len());
        Ok(())
    }

    /// Load validators from database
    pub fn load_validators(&self, _db: &qfc_storage::Database) -> Result<Vec<ValidatorNode>> {
        let validators = Vec::new();

        // Iterate over all validators in the VALIDATORS column family
        // Note: This is a simplified implementation. A full implementation would
        // use an iterator over the column family.
        // For now, we rely on validators being registered through genesis.

        debug!("Loading validators from database");
        Ok(validators)
    }

    /// Create a validator checkpoint at epoch boundary
    ///
    /// The checkpoint blob, the per-validator records, and retention pruning
    /// of old checkpoints are all written in a single atomic `WriteBatch`
    /// (non-sync, per ADR 0001 §3 — checkpoints are derived data and are
    /// rebuilt at the next epoch boundary if lost).
    pub fn create_checkpoint(
        &self,
        db: &qfc_storage::Database,
        block_height: u64,
    ) -> Result<ValidatorCheckpoint> {
        let epoch = self.current_epoch.read();
        let validators = self.validators.read();
        let finalized = *self.finalized_height.read();

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let checkpoint = ValidatorCheckpoint::new(
            epoch.number,
            block_height,
            now,
            validators.clone(),
            epoch.seed,
            finalized,
        );

        // Big-endian epoch key: lexicographic order == numeric order, so the
        // latest checkpoint is found by a single reverse-iterator step.
        let key = epoch.number.to_be_bytes().to_vec();

        let mut batch = qfc_storage::WriteBatch::new();
        batch.put(
            qfc_storage::cf::CHECKPOINTS,
            key.clone(),
            checkpoint.to_bytes(),
        );

        // Save individual validators in the same atomic batch
        for validator in validators.iter() {
            batch.put(
                qfc_storage::cf::VALIDATORS,
                validator.address.as_bytes().to_vec(),
                validator.to_bytes(),
            );
        }

        // Retention: keep only the newest CHECKPOINT_RETENTION checkpoints
        // (including the one being written). The CF stays bounded, so this
        // scan touches at most CHECKPOINT_RETENTION + 1 small entries.
        match db.try_iter(qfc_storage::cf::CHECKPOINTS) {
            Ok(iter) => {
                let existing: Vec<Vec<u8>> = iter
                    .filter_map(|r| r.ok())
                    .map(|(k, _)| k.to_vec())
                    .filter(|k| *k != key)
                    .collect();
                let total = existing.len() + 1;
                if total > CHECKPOINT_RETENTION {
                    // `existing` is in ascending key order (BE epoch), so the
                    // front entries are the oldest.
                    for old_key in existing.into_iter().take(total - CHECKPOINT_RETENTION) {
                        batch.delete(qfc_storage::cf::CHECKPOINTS, old_key);
                    }
                }
            }
            Err(e) => {
                warn!("Failed to scan checkpoints for retention pruning: {}", e);
            }
        }

        db.write_batch(batch)
            .map_err(|e: qfc_storage::StorageError| ConsensusError::StorageError(e.to_string()))?;

        info!(
            "Created checkpoint for epoch {} at height {}",
            epoch.number, block_height
        );

        Ok(checkpoint)
    }

    /// Load checkpoint from database
    pub fn load_checkpoint(
        &self,
        db: &qfc_storage::Database,
        epoch: u64,
    ) -> Result<Option<ValidatorCheckpoint>> {
        let key = epoch.to_be_bytes();

        match db.get(qfc_storage::cf::CHECKPOINTS, &key) {
            Ok(Some(data)) => {
                let checkpoint = ValidatorCheckpoint::from_bytes(&data)
                    .map_err(|e: borsh::io::Error| ConsensusError::StorageError(e.to_string()))?;
                Ok(Some(checkpoint))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(ConsensusError::StorageError(e.to_string())),
        }
    }

    /// Load the latest checkpoint from the database.
    ///
    /// Iterates the `checkpoints` column family in reverse (keys are
    /// big-endian epoch numbers, so the first entry is the newest epoch) and
    /// returns the first checkpoint that deserializes and passes sanity
    /// checks. Corrupt or inconsistent entries are skipped with a warning so
    /// restart falls back to an earlier checkpoint — or to genesis if none
    /// is usable — instead of failing.
    pub fn load_latest_checkpoint(
        &self,
        db: &qfc_storage::Database,
    ) -> Result<Option<ValidatorCheckpoint>> {
        let iter = db
            .try_iter_reverse(qfc_storage::cf::CHECKPOINTS)
            .map_err(|e: qfc_storage::StorageError| ConsensusError::StorageError(e.to_string()))?;

        for entry in iter {
            let (key, value) = match entry {
                Ok(kv) => kv,
                Err(e) => {
                    // Iterator-level error (e.g. corrupt SST): the iterator
                    // is invalid past this point; fall back to genesis.
                    warn!(
                        "Checkpoint scan failed, falling back to genesis initialization: {}",
                        e
                    );
                    break;
                }
            };

            match ValidatorCheckpoint::from_bytes(&value) {
                Ok(checkpoint) => {
                    if key.as_ref() != checkpoint.epoch.to_be_bytes() {
                        warn!(
                            "Checkpoint key/epoch mismatch (key {:02x?}, epoch {}); trying earlier checkpoint",
                            key, checkpoint.epoch
                        );
                        continue;
                    }
                    if checkpoint.validators.is_empty() {
                        warn!(
                            "Checkpoint for epoch {} has an empty validator set; trying earlier checkpoint",
                            checkpoint.epoch
                        );
                        continue;
                    }
                    return Ok(Some(checkpoint));
                }
                Err(e) => {
                    warn!(
                        "Corrupt checkpoint entry (key {:02x?}): {}; trying earlier checkpoint",
                        key, e
                    );
                    continue;
                }
            }
        }

        Ok(None)
    }

    /// Restore state from checkpoint
    pub fn restore_from_checkpoint(&self, checkpoint: &ValidatorCheckpoint) {
        // Restore validators
        *self.validators.write() = checkpoint.validators.clone();

        // Restore epoch
        let epoch = Epoch::new(
            checkpoint.epoch,
            checkpoint.epoch_seed,
            checkpoint.timestamp,
        );
        *self.current_epoch.write() = epoch;

        // Restore finalized height
        *self.finalized_height.write() = checkpoint.finalized_height;

        info!(
            "Restored from checkpoint: epoch={}, height={}, finalized={}",
            checkpoint.epoch, checkpoint.block_height, checkpoint.finalized_height
        );
    }

    // ============ Double-Sign Detection ============

    /// Add a block to the cache for double-sign detection
    pub fn cache_block(&self, block: &Block) {
        let height = block.number();
        let hash = blake3_hash(&block.header_bytes());
        let producer = block.producer();
        let signature = block.signature.clone();

        let record = BlockRecord {
            hash,
            producer,
            signature,
        };

        let mut cache = self.block_cache.write();

        // Add to cache
        cache.entry(height).or_insert_with(Vec::new).push(record);

        // Prune old entries
        let finalized = *self.finalized_height.read();
        if finalized > self.cache_depth {
            let prune_below = finalized - self.cache_depth;
            cache.retain(|&h, _| h >= prune_below);
        }

        // Limit blocks per height
        if let Some(blocks) = cache.get_mut(&height) {
            if blocks.len() > self.max_blocks_per_height {
                blocks.truncate(self.max_blocks_per_height);
            }
        }
    }

    /// Check for double-sign: returns evidence if found
    pub fn check_double_sign(&self, block: &Block) -> Option<DoubleSignEvidence> {
        let height = block.number();
        let hash = blake3_hash(&block.header_bytes());
        let producer = block.producer();
        let signature = &block.signature;

        let cache = self.block_cache.read();

        if let Some(blocks) = cache.get(&height) {
            for existing in blocks {
                // Same producer, different block at same height = double sign
                if existing.producer == producer && existing.hash != hash {
                    let now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as u64;

                    let evidence = DoubleSignEvidence::new(
                        producer,
                        existing.hash,
                        hash,
                        height,
                        existing.signature.clone(),
                        signature.clone(),
                        now,
                    );

                    info!(
                        "Double-sign detected: validator {} at height {}",
                        producer, height
                    );

                    return Some(evidence);
                }
            }
        }

        None
    }

    /// Process double-sign evidence and apply slashing
    pub fn process_double_sign_evidence(
        &self,
        evidence: &DoubleSignEvidence,
        db: &qfc_storage::Database,
    ) -> Result<()> {
        let validator_addr = evidence.validator;

        // Verify the evidence is valid (both signatures are from the same validator)
        // In a full implementation, we would verify the signatures cryptographically

        // Apply 50% slash and permanent jail
        let mut validators = self.validators.write();
        if let Some(validator) = validators.iter_mut().find(|v| v.address == validator_addr) {
            // Slash 50% of stake (including delegated stake)
            let total_stake = validator.total_stake();
            let slash_amount = total_stake * qfc_types::U256::from_u64(SLASH_DOUBLE_SIGN_PERCENT)
                / qfc_types::U256::from_u64(100);

            // Reduce direct stake first
            let direct_slash = slash_amount.min(validator.stake);
            validator.stake = validator.stake.saturating_sub(direct_slash);

            // Reduce delegated stake if needed
            let remaining_slash = slash_amount - direct_slash;
            if !remaining_slash.is_zero() {
                validator.delegated_stake =
                    validator.delegated_stake.saturating_sub(remaining_slash);
            }

            // Permanent jail
            validator.is_jailed = true;
            validator.jail_until = u64::MAX;

            // Zero reputation
            validator.reputation = 0;

            info!(
                "Processed double-sign evidence: validator {} slashed {} ({}%), permanently jailed",
                validator_addr, slash_amount, SLASH_DOUBLE_SIGN_PERCENT
            );
        }

        // Store evidence in database
        let key = format!("{}:{}", evidence.height, evidence.validator);
        db.put(
            qfc_storage::cf::METADATA,
            key.as_bytes(),
            &evidence.to_bytes(),
        )
        .map_err(|e: qfc_storage::StorageError| ConsensusError::StorageError(e.to_string()))?;

        Ok(())
    }

    /// Update delegated stake for a validator
    pub fn add_delegated_stake(&self, validator: &Address, amount: qfc_types::U256) {
        let mut validators = self.validators.write();
        if let Some(v) = validators.iter_mut().find(|v| v.address == *validator) {
            v.delegated_stake = v.delegated_stake.saturating_add(amount);
            v.delegator_count += 1;
            debug!(
                "Added delegated stake to {}: {} (total: {})",
                validator, amount, v.delegated_stake
            );
        }
    }

    /// Remove delegated stake from a validator
    pub fn sub_delegated_stake(&self, validator: &Address, amount: qfc_types::U256) {
        let mut validators = self.validators.write();
        if let Some(v) = validators.iter_mut().find(|v| v.address == *validator) {
            v.delegated_stake = v.delegated_stake.saturating_sub(amount);
            if v.delegator_count > 0 {
                v.delegator_count -= 1;
            }
            debug!(
                "Removed delegated stake from {}: {} (remaining: {})",
                validator, amount, v.delegated_stake
            );
        }
    }

    // ========================
    // Mining/Compute Methods
    // ========================

    /// Set whether a validator provides compute contribution
    pub fn set_provides_compute(&self, validator: &Address, provides: bool) {
        let mut validators = self.validators.write();
        if let Some(v) = validators.iter_mut().find(|v| v.address == *validator) {
            v.provides_compute = provides;
            debug!(
                "Validator {} provides_compute set to {}",
                validator, provides
            );
        }
    }

    /// Update a validator's hashrate from mining
    pub fn update_hashrate(&self, validator: &Address, hashrate: u64) {
        let mut validators = self.validators.write();
        if let Some(v) = validators.iter_mut().find(|v| v.address == *validator) {
            v.hashrate = hashrate;
            debug!("Updated hashrate for {}: {} H/s", validator, hashrate);
        }
    }

    /// Update a validator's inference score from AI compute (v2.0)
    /// Register a miner's GPU profile (P2)
    pub fn register_miner_profile(
        &self,
        address: &Address,
        gpu_model: String,
        benchmark_score: u32,
        gpu_tier: u8,
        gpu_memory_mb: u64,
        compute_backend: Option<qfc_types::BackendType>,
    ) {
        let mut validators = self.validators.write();
        if let Some(v) = validators.iter_mut().find(|v| v.address == *address) {
            v.gpu_model = gpu_model;
            v.benchmark_score = benchmark_score;
            v.gpu_tier = gpu_tier;
            v.gpu_memory_mb = gpu_memory_mb;
            v.provides_compute = true;
            if let Some(backend) = compute_backend {
                v.compute_backend = Some(backend);
            }
            info!(
                "Registered miner profile for {}: T{}, score={}",
                address, gpu_tier, benchmark_score
            );
        }
    }

    /// Reduce a validator's reputation by `reduction_bps` basis points (P2)
    pub fn reduce_reputation(&self, address: &Address, reduction_bps: u32) {
        let mut validators = self.validators.write();
        if let Some(v) = validators.iter_mut().find(|v| v.address == *address) {
            v.reputation = v.reputation.saturating_sub(reduction_bps);
            debug!(
                "Reduced reputation for {} by {} bps, now {}",
                address, reduction_bps, v.reputation
            );
        }
    }

    pub fn update_inference_score(&self, validator: &Address, flops: u64, tasks_completed: u64) {
        let mut validators = self.validators.write();
        if let Some(v) = validators.iter_mut().find(|v| v.address == *validator) {
            v.inference_score = v.inference_score.saturating_add(flops);
            v.tasks_completed = v.tasks_completed.saturating_add(tasks_completed);
            debug!(
                "Updated inference score for {}: score={}, tasks={}",
                validator, v.inference_score, v.tasks_completed
            );
        }
    }

    /// Get a validator's current hashrate
    pub fn get_hashrate(&self, validator: &Address) -> u64 {
        let validators = self.validators.read();
        validators
            .iter()
            .find(|v| v.address == *validator)
            .map(|v| v.hashrate)
            .unwrap_or(0)
    }

    /// Get total network hashrate
    pub fn total_hashrate(&self) -> u64 {
        let validators = self.validators.read();
        validators
            .iter()
            .filter(|v| v.provides_compute)
            .map(|v| v.hashrate)
            .sum()
    }

    /// Process and verify a work proof from a miner
    /// Returns the calculated hashrate if valid
    pub fn process_work_proof(&self, proof: &WorkProof, task: &MiningTask) -> Result<u64> {
        // 1. Verify the validator exists and is active
        let validators = self.validators.read();
        let validator = validators
            .iter()
            .find(|v| v.address == proof.validator)
            .ok_or(ConsensusError::InvalidProducer)?;

        if !validator.is_active() {
            return Err(ConsensusError::ValidatorJailed);
        }

        if !validator.provides_compute {
            return Err(ConsensusError::InvalidProducer);
        }
        drop(validators);

        // 2. Verify the proof is valid (correct hash computation and meets difficulty)
        verify_proof(proof, task).map_err(|_| ConsensusError::InvalidVrfProof)?;

        // 3. Calculate hashrate from the proof
        let hashrate = calculate_hashrate(proof, task);

        // 4. Update the validator's hashrate
        self.update_hashrate(&proof.validator, hashrate);

        debug!(
            "Processed work proof from {}: epoch={}, work_count={}, hashrate={}",
            proof.validator, proof.epoch, proof.work_count, hashrate
        );

        Ok(hashrate)
    }

    /// Create a mining task for the current epoch
    pub fn create_mining_task(&self, difficulty_config: &DifficultyConfig) -> MiningTask {
        let epoch = self.current_epoch.read();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        // Use initial difficulty if no previous difficulty data
        let difficulty = initial_difficulty(difficulty_config);

        MiningTask::new(
            epoch.number,
            epoch.seed,
            difficulty,
            now,
            now + EPOCH_DURATION_MS,
        )
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
        let engine = ConsensusEngine::new_validator(ConsensusConfig::default(), key, address);

        assert!(engine.is_validator());
        assert_eq!(engine.our_address(), Some(address));
    }

    #[test]
    fn test_producer_selection() {
        let engine = ConsensusEngine::new(ConsensusConfig::default());
        let validators = create_test_validators(3);

        engine.update_validators(validators);
        engine.set_genesis_seed([0xab; 32]);

        // Should select a producer
        let producer = engine.select_producer(0);
        assert!(producer.is_some());
    }

    #[test]
    fn test_producer_selection_requires_genesis_seed() {
        let engine = ConsensusEngine::new(ConsensusConfig::default());
        engine.update_validators(create_test_validators(3));

        // No genesis seed -> no election, no bogus default anchor (D11).
        assert!(engine.derive_epoch_seed(1).is_err());
        assert_eq!(engine.select_producer(0), None);
        assert!(!engine.should_produce(0));
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

    // ============ Checkpoint persistence ============

    #[test]
    fn test_checkpoint_write_load_roundtrip() {
        let db = qfc_storage::Database::open_temp().unwrap();
        let engine = ConsensusEngine::new(ConsensusConfig::default());
        engine.update_validators(create_test_validators(3));

        engine.start_epoch(1, [0x01; 32]);
        engine.set_finalized_height(2);
        let cp1 = engine.create_checkpoint(&db, 3).unwrap();

        engine.start_epoch(2, [0x02; 32]);
        engine.set_finalized_height(5);
        let cp2 = engine.create_checkpoint(&db, 6).unwrap();

        // Epoch 256 exercises big-endian key ordering: with little-endian
        // keys, epoch 256 (00 01 00 ..) would sort *before* epoch 2.
        engine.start_epoch(256, [0x03; 32]);
        engine.set_finalized_height(767);
        let cp256 = engine.create_checkpoint(&db, 768).unwrap();

        // Latest checkpoint is the highest epoch
        let latest = engine.load_latest_checkpoint(&db).unwrap().unwrap();
        assert_eq!(latest, cp256);
        assert_eq!(latest.epoch, 256);
        assert_eq!(latest.validators.len(), 3);
        assert_eq!(latest.finalized_height, 767);
        assert_eq!(latest.epoch_seed, [0x03; 32]);

        // Point lookups still work
        assert_eq!(engine.load_checkpoint(&db, 1).unwrap().unwrap(), cp1);
        assert_eq!(engine.load_checkpoint(&db, 2).unwrap().unwrap(), cp2);
        assert_eq!(engine.load_checkpoint(&db, 99).unwrap(), None);
    }

    #[test]
    fn test_load_latest_checkpoint_empty_db() {
        let db = qfc_storage::Database::open_temp().unwrap();
        let engine = ConsensusEngine::new(ConsensusConfig::default());
        assert_eq!(engine.load_latest_checkpoint(&db).unwrap(), None);
    }

    #[test]
    fn test_load_latest_checkpoint_skips_corrupt_entry() {
        let db = qfc_storage::Database::open_temp().unwrap();
        let engine = ConsensusEngine::new(ConsensusConfig::default());
        engine.update_validators(create_test_validators(2));

        engine.start_epoch(1, [0x01; 32]);
        let cp1 = engine.create_checkpoint(&db, 3).unwrap();

        // A corrupt entry at a *newer* epoch must be skipped, falling back
        // to the older good checkpoint.
        db.put(
            qfc_storage::cf::CHECKPOINTS,
            &2u64.to_be_bytes(),
            b"not a valid borsh checkpoint",
        )
        .unwrap();

        let latest = engine.load_latest_checkpoint(&db).unwrap().unwrap();
        assert_eq!(latest, cp1);
        assert_eq!(latest.epoch, 1);
    }

    #[test]
    fn test_load_latest_checkpoint_all_corrupt_returns_none() {
        let db = qfc_storage::Database::open_temp().unwrap();
        let engine = ConsensusEngine::new(ConsensusConfig::default());

        db.put(qfc_storage::cf::CHECKPOINTS, &1u64.to_be_bytes(), b"junk1")
            .unwrap();
        db.put(qfc_storage::cf::CHECKPOINTS, &2u64.to_be_bytes(), b"junk2")
            .unwrap();

        assert_eq!(engine.load_latest_checkpoint(&db).unwrap(), None);
    }

    #[test]
    fn test_load_latest_checkpoint_skips_empty_validator_set() {
        let db = qfc_storage::Database::open_temp().unwrap();
        let engine = ConsensusEngine::new(ConsensusConfig::default());

        // Checkpoint with validators (epoch 1)
        engine.update_validators(create_test_validators(2));
        engine.start_epoch(1, [0x01; 32]);
        let cp1 = engine.create_checkpoint(&db, 3).unwrap();

        // Newer checkpoint with an empty validator set (epoch 2) — restoring
        // it would brick producer selection, so it must be skipped.
        engine.update_validators(Vec::new());
        engine.start_epoch(2, [0x02; 32]);
        engine.create_checkpoint(&db, 6).unwrap();

        let latest = engine.load_latest_checkpoint(&db).unwrap().unwrap();
        assert_eq!(latest, cp1);
    }

    #[test]
    fn test_checkpoint_retention_prunes_old_epochs() {
        let db = qfc_storage::Database::open_temp().unwrap();
        let engine = ConsensusEngine::new(ConsensusConfig::default());
        engine.update_validators(create_test_validators(1));

        let extra = 5u64;
        let total = CHECKPOINT_RETENTION as u64 + extra;
        for epoch in 1..=total {
            engine.start_epoch(epoch, [epoch as u8; 32]);
            engine
                .create_checkpoint(&db, epoch * qfc_types::BLOCKS_PER_EPOCH)
                .unwrap();
        }

        // Only the newest CHECKPOINT_RETENTION checkpoints remain
        let count = db.iter(qfc_storage::cf::CHECKPOINTS).unwrap().count();
        assert_eq!(count, CHECKPOINT_RETENTION);

        // Oldest epochs were pruned, newest are intact
        assert_eq!(engine.load_checkpoint(&db, 1).unwrap(), None);
        assert_eq!(engine.load_checkpoint(&db, extra).unwrap(), None);
        assert!(engine.load_checkpoint(&db, extra + 1).unwrap().is_some());
        let latest = engine.load_latest_checkpoint(&db).unwrap().unwrap();
        assert_eq!(latest.epoch, total);
    }

    #[test]
    fn test_restore_from_checkpoint_state() {
        let db = qfc_storage::Database::open_temp().unwrap();
        let engine = ConsensusEngine::new(ConsensusConfig::default());
        engine.update_validators(create_test_validators(3));
        engine.start_epoch(42, [0xcd; 32]);
        engine.set_finalized_height(125);
        engine.create_checkpoint(&db, 126).unwrap();

        // Fresh engine, as after process restart
        let restarted = ConsensusEngine::new(ConsensusConfig::default());
        let checkpoint = restarted.load_latest_checkpoint(&db).unwrap().unwrap();
        restarted.restore_from_checkpoint(&checkpoint);

        assert_eq!(restarted.get_epoch().number, 42);
        assert_eq!(restarted.get_epoch().seed, [0xcd; 32]);
        assert_eq!(restarted.finalized_height(), 125);
        assert_eq!(restarted.get_validators().len(), 3);
    }

    // ---- consensus convergence (testnet three-way-fork fix) ----

    /// Two nodes holding the same validator set but in DIFFERENT internal
    /// order must elect the SAME producer for every slot. This is the core
    /// invariant the fork violated: selection is now a pure function of
    /// (address-sorted stake>0 set, epoch seed, slot), independent of list
    /// order.
    #[test]
    fn test_producer_selection_is_order_independent() {
        let mut a = create_test_validators(4);
        let mut b = a.clone();
        b.reverse(); // node B stores them in the opposite order
        assert_ne!(a[0].address, b[0].address);

        let ea = ConsensusEngine::new(ConsensusConfig::default());
        let eb = ConsensusEngine::new(ConsensusConfig::default());
        ea.update_validators(std::mem::take(&mut a));
        eb.update_validators(std::mem::take(&mut b));
        ea.set_genesis_seed([0x5a; 32]);
        eb.set_genesis_seed([0x5a; 32]); // same shared genesis anchor

        for slot in 0..200u64 {
            assert_eq!(
                ea.select_producer(slot),
                eb.select_producer(slot),
                "nodes disagree on producer for slot {slot}"
            );
        }
    }

    /// Round-robin must visit every stake>0 validator over `len` consecutive
    /// slots within an epoch — not a fixed `validators[0]` that each node
    /// resolves to itself (the original fork trigger).
    #[test]
    fn test_round_robin_covers_all_validators() {
        let engine = ConsensusEngine::new(ConsensusConfig::default());
        engine.update_validators(create_test_validators(4));
        engine.set_genesis_seed([0x11; 32]);

        // Use a fixed epoch seed so all 4 consecutive slots share the seed
        // (in production 4 slots span 2 epochs; coverage per epoch-window is
        // what matters for the rotation property).
        let seed = engine.derive_epoch_seed(9).unwrap();
        let mut seen = std::collections::HashSet::new();
        for slot in 100..104u64 {
            seen.insert(engine.select_producer_with_seed(slot, &seed).unwrap());
        }
        assert_eq!(seen.len(), 4, "round-robin did not cover every validator");
    }

    /// REQUIRED TEST 4 (spec): two engines with wildly different local
    /// scores and jail flags must elect the same leader for the same slot.
    /// Contribution scores and local jailing are NOT consensus inputs.
    #[test]
    fn test_election_ignores_local_scores_and_jail_flags() {
        let base = create_test_validators(4);

        // Node A: pristine view.
        let a = ConsensusEngine::new(ConsensusConfig::default());
        a.update_validators(base.clone());

        // Node B: wildly different local observations — inflated/zero scores,
        // one validator locally jailed (gossip evidence, no in-block proof).
        let mut vb = base;
        vb[0].contribution_score = 0;
        vb[1].contribution_score = 1_000_000;
        vb[2].is_jailed = true;
        vb[2].jail_until = u64::MAX;
        vb[3].avg_latency_ms = 30_000;
        vb.reverse();
        let b = ConsensusEngine::new(ConsensusConfig::default());
        b.update_validators(vb);

        a.set_genesis_seed([0x07; 32]);
        b.set_genesis_seed([0x07; 32]);

        for slot in 0..200u64 {
            assert_eq!(
                a.select_producer(slot),
                b.select_producer(slot),
                "local scores/jail flags leaked into election at slot {slot}"
            );
        }
    }

    /// Zero-stake validators are excluded from the election set.
    #[test]
    fn test_zero_stake_excluded_from_election() {
        let mut validators = create_test_validators(3);
        let excluded = validators[1].address;
        validators[1].stake = qfc_types::U256::ZERO;

        let engine = ConsensusEngine::new(ConsensusConfig::default());
        engine.update_validators(validators);
        engine.set_genesis_seed([0x22; 32]);

        for slot in 0..50u64 {
            assert_ne!(
                engine.select_producer(slot),
                Some(excluded),
                "zero-stake validator elected at slot {slot}"
            );
        }
    }

    /// The epoch seed is `blake3(genesis_seed || epoch_number)` — a pure
    /// function of (genesis seed, epoch number), derived directly (O(1)) with
    /// no dependence on chain head or node start time.
    #[test]
    fn test_epoch_seed_is_deterministic() {
        const GENESIS: [u8; 32] = [0x07; 32];
        let engine = ConsensusEngine::new(ConsensusConfig::default());
        engine.set_genesis_seed(GENESIS);

        // Advance to the wall-clock epoch (now_ms / EPOCH_DURATION_MS ≫ 1).
        let n = engine.maybe_advance_epoch();
        assert!(n > 1, "expected a wall-clock epoch advance, got {n}");

        // Recompute independently: blake3(genesis || n).
        let h = blake3_hash(&[&GENESIS[..], &n.to_le_bytes()[..]].concat());
        let mut expected = [0u8; 32];
        expected.copy_from_slice(h.as_bytes());
        assert_eq!(
            engine.get_epoch().seed,
            expected,
            "epoch seed must be blake3(genesis_seed || epoch_number)"
        );
    }

    /// The genesis seed is set-once: later attempts (e.g. a malicious or
    /// buggy epoch announcement) cannot re-anchor the derivation.
    #[test]
    fn test_genesis_seed_is_set_once() {
        let engine = ConsensusEngine::new(ConsensusConfig::default());
        engine.set_genesis_seed([0x01; 32]);
        let s1 = engine.derive_epoch_seed(42).unwrap();
        engine.set_genesis_seed([0x02; 32]); // ignored
        assert_eq!(engine.derive_epoch_seed(42).unwrap(), s1);
    }

    /// Without a genesis seed, maybe_advance_epoch must NOT anchor an epoch
    /// to a bogus default seed — it stays put until the seed is set.
    #[test]
    fn test_maybe_advance_epoch_requires_genesis_seed() {
        let engine = ConsensusEngine::new(ConsensusConfig::default());
        let before = engine.get_epoch().number;
        assert_eq!(engine.maybe_advance_epoch(), before);
        assert_eq!(engine.get_epoch().number, before);

        engine.set_genesis_seed([0x03; 32]);
        assert!(engine.maybe_advance_epoch() > before);
    }

    /// The fix for the testnet fork: two nodes that start at DIFFERENT
    /// wall-clock times must still agree on the current epoch, seed, and
    /// elected producer — because scheduling is anchored to wall-clock, not to
    /// each node's local start time.
    #[test]
    fn test_nodes_agree_despite_different_start_times() {
        const GENESIS: [u8; 32] = [0x07; 32];
        let validators = create_test_validators(4);

        let a = ConsensusEngine::new(ConsensusConfig::default());
        a.update_validators(validators.clone());
        a.set_genesis_seed(GENESIS);

        // Node B boots later and stores its validators in the opposite order.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut vb = validators;
        vb.reverse();
        let b = ConsensusEngine::new(ConsensusConfig::default());
        b.update_validators(vb);
        b.set_genesis_seed(GENESIS);

        // Both advance to the wall-clock epoch.
        let ea = a.maybe_advance_epoch();
        let eb = b.maybe_advance_epoch();
        assert_eq!(ea, eb, "nodes disagree on the current epoch number");
        assert_eq!(
            a.get_epoch().seed,
            b.get_epoch().seed,
            "nodes derived different epoch seeds"
        );
        for slot in 0..200u64 {
            assert_eq!(
                a.select_producer(slot),
                b.select_producer(slot),
                "nodes elect different producers for slot {slot}"
            );
        }
    }
}
