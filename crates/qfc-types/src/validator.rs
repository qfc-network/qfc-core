//! Validator and Vote types

use crate::{Address, Hash, PublicKey, Signature, U256};
use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

/// Validator node information
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct ValidatorNode {
    /// Validator address
    pub address: Address,

    /// Public key for signing
    pub public_key: PublicKey,

    /// Staked amount (direct stake)
    pub stake: U256,

    /// Delegated stake from other accounts
    pub delegated_stake: U256,

    /// Commission rate for delegation rewards (0-10000 = 0-100%)
    pub commission_rate: u32,

    /// Number of delegators
    pub delegator_count: u32,

    /// Contribution score (calculated from multiple factors)
    pub contribution_score: u64,

    /// Uptime percentage (0-100 scaled to 0-10000 for precision)
    pub uptime: u32,

    /// Validation accuracy (0-100 scaled to 0-10000)
    pub accuracy: u32,

    /// Average latency in milliseconds
    pub avg_latency_ms: u32,

    /// Bandwidth in Mbps
    pub bandwidth_mbps: u32,

    /// Storage provided in GB
    pub storage_provided_gb: u32,

    /// Whether this node provides compute (mining)
    pub provides_compute: bool,

    /// Hashrate if provides compute
    pub hashrate: u64,

    /// Reputation score (0-100 scaled to 0-10000)
    pub reputation: u32,

    /// Registration timestamp
    pub registered_at: u64,

    /// Last active timestamp
    pub last_active: u64,

    /// Whether validator is jailed
    pub is_jailed: bool,

    /// Jail release timestamp (0 if not jailed, u64::MAX for permanent)
    pub jail_until: u64,

    /// Total blocks produced
    pub blocks_produced: u64,

    /// Total valid votes cast
    pub valid_votes: u64,

    /// Total invalid votes cast
    pub invalid_votes: u64,

    /// Accumulated rewards pending distribution
    pub pending_rewards: U256,
}

impl Default for ValidatorNode {
    fn default() -> Self {
        Self {
            address: Address::ZERO,
            public_key: PublicKey::ZERO,
            stake: U256::ZERO,
            delegated_stake: U256::ZERO,
            commission_rate: 1000, // 10% default commission
            delegator_count: 0,
            contribution_score: 0,
            uptime: 10000, // 100%
            accuracy: 10000, // 100%
            avg_latency_ms: 100,
            bandwidth_mbps: 100,
            storage_provided_gb: 0,
            provides_compute: false,
            hashrate: 0,
            reputation: 5000, // 50% (neutral starting point)
            registered_at: 0,
            last_active: 0,
            is_jailed: false,
            jail_until: 0,
            blocks_produced: 0,
            valid_votes: 0,
            invalid_votes: 0,
            pending_rewards: U256::ZERO,
        }
    }
}

impl ValidatorNode {
    /// Create a new validator
    pub fn new(address: Address, public_key: PublicKey, stake: U256) -> Self {
        Self {
            address,
            public_key,
            stake,
            registered_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
            ..Default::default()
        }
    }

    /// Get total stake (direct + delegated)
    pub fn total_stake(&self) -> U256 {
        self.stake.saturating_add(self.delegated_stake)
    }

    /// Check if validator is active (not jailed and has stake)
    pub fn is_active(&self) -> bool {
        !self.is_jailed && !self.total_stake().is_zero()
    }

    /// Check if validator is permanently jailed
    pub fn is_permanently_jailed(&self) -> bool {
        self.is_jailed && self.jail_until == u64::MAX
    }

    /// Check if validator can be unjailed
    pub fn can_unjail(&self, current_time: u64) -> bool {
        self.is_jailed && !self.is_permanently_jailed() && current_time >= self.jail_until
    }

    /// Get commission rate as float (0.0 - 1.0)
    pub fn commission_ratio(&self) -> f64 {
        self.commission_rate as f64 / 10000.0
    }

    /// Get uptime as float (0.0 - 1.0)
    pub fn uptime_ratio(&self) -> f64 {
        self.uptime as f64 / 10000.0
    }

    /// Get accuracy as float (0.0 - 1.0)
    pub fn accuracy_ratio(&self) -> f64 {
        self.accuracy as f64 / 10000.0
    }

    /// Get reputation as float (0.0 - 1.0)
    pub fn reputation_ratio(&self) -> f64 {
        self.reputation as f64 / 10000.0
    }

    /// Serialize validator
    pub fn to_bytes(&self) -> Vec<u8> {
        borsh::to_vec(self).expect("serialization should not fail")
    }

    /// Deserialize validator
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, borsh::io::Error> {
        borsh::from_slice(bytes)
    }
}

/// Vote decision
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, BorshSerialize, BorshDeserialize,
)]
pub enum VoteDecision {
    /// Accept the block
    Accept,
    /// Reject the block
    Reject,
    /// Abstain from voting
    Abstain,
}

impl Default for VoteDecision {
    fn default() -> Self {
        Self::Abstain
    }
}

/// Rejection reason
#[derive(
    Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize,
)]
pub enum RejectReason {
    /// Invalid block producer
    InvalidProducer,
    /// Invalid VRF proof
    InvalidVRF,
    /// Invalid state transition
    InvalidStateTransition,
    /// Invalid signature
    InvalidSignature,
    /// Invalid timestamp
    InvalidTimestamp,
    /// Block too large
    BlockTooLarge,
    /// Other reason
    Other(String),
}

/// Vote for a block
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct Vote {
    /// Block hash being voted on
    pub block_hash: Hash,

    /// Block height
    pub block_height: u64,

    /// Voter address
    pub voter: Address,

    /// Vote decision
    pub decision: VoteDecision,

    /// Rejection reason (if rejected)
    pub reject_reason: Option<RejectReason>,

    /// Timestamp
    pub timestamp: u64,

    /// Voter's signature
    pub signature: Signature,
}

impl Default for Vote {
    fn default() -> Self {
        Self {
            block_hash: Hash::ZERO,
            block_height: 0,
            voter: Address::ZERO,
            decision: VoteDecision::Abstain,
            reject_reason: None,
            timestamp: 0,
            signature: Signature::ZERO,
        }
    }
}

impl Vote {
    /// Create an accept vote
    pub fn accept(block_hash: Hash, block_height: u64, voter: Address, timestamp: u64) -> Self {
        Self {
            block_hash,
            block_height,
            voter,
            decision: VoteDecision::Accept,
            reject_reason: None,
            timestamp,
            signature: Signature::ZERO,
        }
    }

    /// Create a reject vote
    pub fn reject(
        block_hash: Hash,
        block_height: u64,
        voter: Address,
        reason: RejectReason,
        timestamp: u64,
    ) -> Self {
        Self {
            block_hash,
            block_height,
            voter,
            decision: VoteDecision::Reject,
            reject_reason: Some(reason),
            timestamp,
            signature: Signature::ZERO,
        }
    }

    /// Check if this is an accept vote
    pub fn is_accept(&self) -> bool {
        self.decision == VoteDecision::Accept
    }

    /// Check if this is a reject vote
    pub fn is_reject(&self) -> bool {
        self.decision == VoteDecision::Reject
    }

    /// Set signature
    pub fn set_signature(&mut self, signature: Signature) {
        self.signature = signature;
    }

    /// Serialize vote without signature for hashing
    pub fn to_bytes_without_signature(&self) -> Vec<u8> {
        let unsigned = UnsignedVote {
            block_hash: self.block_hash,
            block_height: self.block_height,
            voter: self.voter,
            decision: self.decision,
            reject_reason: self.reject_reason.clone(),
            timestamp: self.timestamp,
        };
        borsh::to_vec(&unsigned).expect("serialization should not fail")
    }

    /// Serialize vote
    pub fn to_bytes(&self) -> Vec<u8> {
        borsh::to_vec(self).expect("serialization should not fail")
    }

    /// Deserialize vote
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, borsh::io::Error> {
        borsh::from_slice(bytes)
    }
}

#[derive(BorshSerialize, BorshDeserialize)]
struct UnsignedVote {
    pub block_hash: Hash,
    pub block_height: u64,
    pub voter: Address,
    pub decision: VoteDecision,
    pub reject_reason: Option<RejectReason>,
    pub timestamp: u64,
}

/// Slashable offense types
#[derive(
    Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize,
)]
pub enum SlashableOffense {
    /// Double signing (signing multiple blocks at same height)
    DoubleSign,
    /// Producing invalid block
    InvalidBlock,
    /// Censorship (not including valid transactions)
    Censorship,
    /// Extended offline period
    Offline,
    /// Voting for invalid block
    FalseVote,
}

/// Slash result
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct SlashResult {
    /// Validator address
    pub validator: Address,

    /// The offense
    pub offense: SlashableOffense,

    /// Amount slashed
    pub slashed_amount: U256,

    /// Jail until timestamp (u64::MAX for permanent)
    pub jail_until: u64,
}

/// Epoch information
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct Epoch {
    /// Epoch number
    pub number: u64,

    /// Random seed for VRF
    pub seed: [u8; 32],

    /// Start timestamp
    pub start_time: u64,

    /// Duration in milliseconds
    pub duration_ms: u64,

    /// Selected validators for this epoch
    pub validators: Vec<Address>,
}

impl Default for Epoch {
    fn default() -> Self {
        Self {
            number: 0,
            seed: [0u8; 32],
            start_time: 0,
            duration_ms: crate::EPOCH_DURATION_SECS * 1000,
            validators: Vec::new(),
        }
    }
}

impl Epoch {
    /// Create a new epoch
    pub fn new(number: u64, seed: [u8; 32], start_time: u64) -> Self {
        Self {
            number,
            seed,
            start_time,
            duration_ms: crate::EPOCH_DURATION_SECS * 1000,
            validators: Vec::new(),
        }
    }

    /// Check if timestamp is within this epoch
    pub fn contains(&self, timestamp: u64) -> bool {
        timestamp >= self.start_time && timestamp < self.start_time + self.duration_ms
    }

    /// Get end time
    pub fn end_time(&self) -> u64 {
        self.start_time + self.duration_ms
    }
}

/// Validator message types for network communication
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub enum ValidatorMessage {
    /// Heartbeat to signal liveness
    Heartbeat(Heartbeat),
    /// Epoch announcement
    EpochAnnouncement(EpochAnnouncement),
    /// Slashing evidence
    SlashingEvidence(SlashingEvidence),
}

impl ValidatorMessage {
    /// Serialize message
    pub fn to_bytes(&self) -> Vec<u8> {
        borsh::to_vec(self).expect("serialization should not fail")
    }

    /// Deserialize message
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, borsh::io::Error> {
        borsh::from_slice(bytes)
    }
}

/// Validator heartbeat message
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct Heartbeat {
    /// Validator address
    pub validator: Address,
    /// Current block height
    pub block_height: u64,
    /// Current block hash
    pub block_hash: Hash,
    /// Timestamp
    pub timestamp: u64,
    /// Signature
    pub signature: Signature,
}

impl Heartbeat {
    /// Create a new heartbeat
    pub fn new(validator: Address, block_height: u64, block_hash: Hash, timestamp: u64) -> Self {
        Self {
            validator,
            block_height,
            block_hash,
            timestamp,
            signature: Signature::ZERO,
        }
    }

    /// Serialize for signing (without signature)
    pub fn to_bytes_without_signature(&self) -> Vec<u8> {
        let unsigned = UnsignedHeartbeat {
            validator: self.validator,
            block_height: self.block_height,
            block_hash: self.block_hash,
            timestamp: self.timestamp,
        };
        borsh::to_vec(&unsigned).expect("serialization should not fail")
    }

    /// Set signature
    pub fn set_signature(&mut self, signature: Signature) {
        self.signature = signature;
    }
}

#[derive(BorshSerialize, BorshDeserialize)]
struct UnsignedHeartbeat {
    pub validator: Address,
    pub block_height: u64,
    pub block_hash: Hash,
    pub timestamp: u64,
}

/// Epoch announcement message
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct EpochAnnouncement {
    /// Epoch number
    pub epoch_number: u64,
    /// Epoch seed
    pub seed: [u8; 32],
    /// Start timestamp
    pub start_time: u64,
    /// Announcing validator
    pub announcer: Address,
    /// Signature
    pub signature: Signature,
}

impl EpochAnnouncement {
    /// Create a new epoch announcement
    pub fn new(epoch_number: u64, seed: [u8; 32], start_time: u64, announcer: Address) -> Self {
        Self {
            epoch_number,
            seed,
            start_time,
            announcer,
            signature: Signature::ZERO,
        }
    }

    /// Serialize for signing (without signature)
    pub fn to_bytes_without_signature(&self) -> Vec<u8> {
        let unsigned = UnsignedEpochAnnouncement {
            epoch_number: self.epoch_number,
            seed: self.seed,
            start_time: self.start_time,
            announcer: self.announcer,
        };
        borsh::to_vec(&unsigned).expect("serialization should not fail")
    }

    /// Set signature
    pub fn set_signature(&mut self, signature: Signature) {
        self.signature = signature;
    }
}

#[derive(BorshSerialize, BorshDeserialize)]
struct UnsignedEpochAnnouncement {
    pub epoch_number: u64,
    pub seed: [u8; 32],
    pub start_time: u64,
    pub announcer: Address,
}

/// Slashing evidence for misbehaving validators
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct SlashingEvidence {
    /// The misbehaving validator
    pub offender: Address,
    /// Type of offense
    pub offense: SlashableOffense,
    /// Evidence data (e.g., conflicting signatures)
    pub evidence: Vec<u8>,
    /// Block height where offense occurred
    pub block_height: u64,
    /// Reporter
    pub reporter: Address,
    /// Reporter's signature
    pub signature: Signature,
}

impl SlashingEvidence {
    /// Create new slashing evidence
    pub fn new(
        offender: Address,
        offense: SlashableOffense,
        evidence: Vec<u8>,
        block_height: u64,
        reporter: Address,
    ) -> Self {
        Self {
            offender,
            offense,
            evidence,
            block_height,
            reporter,
            signature: Signature::ZERO,
        }
    }

    /// Serialize for signing (without signature)
    pub fn to_bytes_without_signature(&self) -> Vec<u8> {
        let unsigned = UnsignedSlashingEvidence {
            offender: self.offender,
            offense: self.offense.clone(),
            evidence: self.evidence.clone(),
            block_height: self.block_height,
            reporter: self.reporter,
        };
        borsh::to_vec(&unsigned).expect("serialization should not fail")
    }

    /// Set signature
    pub fn set_signature(&mut self, signature: Signature) {
        self.signature = signature;
    }
}

#[derive(BorshSerialize, BorshDeserialize)]
struct UnsignedSlashingEvidence {
    pub offender: Address,
    pub offense: SlashableOffense,
    pub evidence: Vec<u8>,
    pub block_height: u64,
    pub reporter: Address,
}

// ============ Reward Distribution ============

/// Reward distribution record for a block
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct RewardDistribution {
    /// Block height where rewards were distributed
    pub block_height: u64,
    /// Reward given to block producer
    pub producer_reward: U256,
    /// Total reward distributed to voters
    pub voter_reward: U256,
    /// Amount of fees burned
    pub fee_burned: U256,
    /// Timestamp of distribution
    pub timestamp: u64,
}

impl RewardDistribution {
    /// Create a new reward distribution record
    pub fn new(
        block_height: u64,
        producer_reward: U256,
        voter_reward: U256,
        fee_burned: U256,
        timestamp: u64,
    ) -> Self {
        Self {
            block_height,
            producer_reward,
            voter_reward,
            fee_burned,
            timestamp,
        }
    }

    /// Serialize reward distribution
    pub fn to_bytes(&self) -> Vec<u8> {
        borsh::to_vec(self).expect("serialization should not fail")
    }

    /// Deserialize reward distribution
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, borsh::io::Error> {
        borsh::from_slice(bytes)
    }
}

// ============ Delegation System ============

/// Delegation record: a delegator's stake in a validator
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct Delegation {
    /// Delegator address
    pub delegator: Address,
    /// Validator address
    pub validator: Address,
    /// Delegated amount
    pub amount: U256,
    /// Timestamp when delegation was created
    pub delegated_at: u64,
    /// Pending rewards to be claimed
    pub pending_rewards: U256,
}

impl Delegation {
    /// Create a new delegation
    pub fn new(delegator: Address, validator: Address, amount: U256, delegated_at: u64) -> Self {
        Self {
            delegator,
            validator,
            amount,
            delegated_at,
            pending_rewards: U256::ZERO,
        }
    }

    /// Serialize delegation
    pub fn to_bytes(&self) -> Vec<u8> {
        borsh::to_vec(self).expect("serialization should not fail")
    }

    /// Deserialize delegation
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, borsh::io::Error> {
        borsh::from_slice(bytes)
    }

    /// Create storage key for delegation (delegator + validator)
    pub fn storage_key(delegator: &Address, validator: &Address) -> Vec<u8> {
        let mut key = Vec::with_capacity(40);
        key.extend_from_slice(delegator.as_bytes());
        key.extend_from_slice(validator.as_bytes());
        key
    }
}

/// Undelegation record: pending stake withdrawal
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct Undelegation {
    /// Delegator address
    pub delegator: Address,
    /// Validator address
    pub validator: Address,
    /// Amount being undelegated
    pub amount: U256,
    /// Timestamp when funds can be withdrawn
    pub unlock_at: u64,
}

impl Undelegation {
    /// Create a new undelegation
    pub fn new(delegator: Address, validator: Address, amount: U256, unlock_at: u64) -> Self {
        Self {
            delegator,
            validator,
            amount,
            unlock_at,
        }
    }

    /// Check if undelegation can be completed
    pub fn is_unlocked(&self, current_time: u64) -> bool {
        current_time >= self.unlock_at
    }

    /// Serialize undelegation
    pub fn to_bytes(&self) -> Vec<u8> {
        borsh::to_vec(self).expect("serialization should not fail")
    }

    /// Deserialize undelegation
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, borsh::io::Error> {
        borsh::from_slice(bytes)
    }

    /// Create storage key for undelegation (delegator + validator + unlock_at)
    pub fn storage_key(delegator: &Address, validator: &Address, unlock_at: u64) -> Vec<u8> {
        let mut key = Vec::with_capacity(48);
        key.extend_from_slice(delegator.as_bytes());
        key.extend_from_slice(validator.as_bytes());
        key.extend_from_slice(&unlock_at.to_be_bytes());
        key
    }
}

// ============ Validator Checkpoint ============

/// Checkpoint for validator state persistence
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct ValidatorCheckpoint {
    /// Epoch number
    pub epoch: u64,
    /// Block height at checkpoint
    pub block_height: u64,
    /// Timestamp of checkpoint
    pub timestamp: u64,
    /// Validator set at this checkpoint
    pub validators: Vec<ValidatorNode>,
    /// Epoch seed
    pub epoch_seed: [u8; 32],
    /// Finalized block height
    pub finalized_height: u64,
}

impl ValidatorCheckpoint {
    /// Create a new checkpoint
    pub fn new(
        epoch: u64,
        block_height: u64,
        timestamp: u64,
        validators: Vec<ValidatorNode>,
        epoch_seed: [u8; 32],
        finalized_height: u64,
    ) -> Self {
        Self {
            epoch,
            block_height,
            timestamp,
            validators,
            epoch_seed,
            finalized_height,
        }
    }

    /// Serialize checkpoint
    pub fn to_bytes(&self) -> Vec<u8> {
        borsh::to_vec(self).expect("serialization should not fail")
    }

    /// Deserialize checkpoint
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, borsh::io::Error> {
        borsh::from_slice(bytes)
    }
}

// ============ Double-Sign Detection ============

/// Evidence of double-signing (producing conflicting blocks at same height)
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct DoubleSignEvidence {
    /// Misbehaving validator address
    pub validator: Address,
    /// First conflicting block hash
    pub block_hash_1: Hash,
    /// Second conflicting block hash
    pub block_hash_2: Hash,
    /// Block height where double-sign occurred
    pub height: u64,
    /// Signature on first block
    pub signature_1: Signature,
    /// Signature on second block
    pub signature_2: Signature,
    /// Timestamp when evidence was created
    pub timestamp: u64,
}

impl DoubleSignEvidence {
    /// Create new double-sign evidence
    pub fn new(
        validator: Address,
        block_hash_1: Hash,
        block_hash_2: Hash,
        height: u64,
        signature_1: Signature,
        signature_2: Signature,
        timestamp: u64,
    ) -> Self {
        Self {
            validator,
            block_hash_1,
            block_hash_2,
            height,
            signature_1,
            signature_2,
            timestamp,
        }
    }

    /// Serialize evidence
    pub fn to_bytes(&self) -> Vec<u8> {
        borsh::to_vec(self).expect("serialization should not fail")
    }

    /// Deserialize evidence
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, borsh::io::Error> {
        borsh::from_slice(bytes)
    }

    /// Convert to SlashingEvidence for network broadcast
    pub fn to_slashing_evidence(&self, reporter: Address) -> SlashingEvidence {
        SlashingEvidence::new(
            self.validator,
            SlashableOffense::DoubleSign,
            self.to_bytes(),
            self.height,
            reporter,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validator_default() {
        let validator = ValidatorNode::default();
        assert!(!validator.is_active()); // No stake
        assert_eq!(validator.uptime_ratio(), 1.0);
    }

    #[test]
    fn test_validator_active() {
        let mut validator = ValidatorNode::default();
        validator.stake = U256::from_u64(10000);
        assert!(validator.is_active());

        validator.is_jailed = true;
        assert!(!validator.is_active());
    }

    #[test]
    fn test_vote_serialization() {
        let vote = Vote::accept(
            Hash::new([0x11; 32]),
            100,
            Address::new([0x22; 20]),
            12345,
        );

        let bytes = vote.to_bytes();
        let decoded = Vote::from_bytes(&bytes).unwrap();
        assert_eq!(vote, decoded);
    }

    #[test]
    fn test_epoch_contains() {
        let epoch = Epoch::new(1, [0u8; 32], 1000);

        assert!(!epoch.contains(999));
        assert!(epoch.contains(1000));
        assert!(epoch.contains(5000));
        assert!(!epoch.contains(epoch.end_time()));
    }

    #[test]
    fn test_validator_total_stake() {
        let mut validator = ValidatorNode::default();
        validator.stake = U256::from_u64(10000);
        validator.delegated_stake = U256::from_u64(5000);

        assert_eq!(validator.total_stake(), U256::from_u64(15000));
        assert!(validator.is_active());
    }

    #[test]
    fn test_validator_active_with_delegated_only() {
        let mut validator = ValidatorNode::default();
        // No direct stake but has delegated stake
        validator.delegated_stake = U256::from_u64(10000);

        assert!(validator.is_active());
    }

    #[test]
    fn test_validator_permanent_jail() {
        let mut validator = ValidatorNode::default();
        validator.stake = U256::from_u64(10000);
        validator.is_jailed = true;
        validator.jail_until = u64::MAX; // Permanent

        assert!(validator.is_permanently_jailed());
        assert!(!validator.can_unjail(u64::MAX - 1));
        assert!(!validator.is_active());
    }

    #[test]
    fn test_reward_distribution_serialization() {
        let reward = RewardDistribution::new(
            100,
            U256::from_u64(7000),
            U256::from_u64(3000),
            U256::from_u64(200),
            12345,
        );

        let bytes = reward.to_bytes();
        let decoded = RewardDistribution::from_bytes(&bytes).unwrap();
        assert_eq!(reward, decoded);
    }

    #[test]
    fn test_delegation_serialization() {
        let delegation = Delegation::new(
            Address::new([0x11; 20]),
            Address::new([0x22; 20]),
            U256::from_u64(1000),
            12345,
        );

        let bytes = delegation.to_bytes();
        let decoded = Delegation::from_bytes(&bytes).unwrap();
        assert_eq!(delegation, decoded);
    }

    #[test]
    fn test_delegation_storage_key() {
        let delegator = Address::new([0x11; 20]);
        let validator = Address::new([0x22; 20]);

        let key = Delegation::storage_key(&delegator, &validator);
        assert_eq!(key.len(), 40);
        assert_eq!(&key[0..20], delegator.as_bytes());
        assert_eq!(&key[20..40], validator.as_bytes());
    }

    #[test]
    fn test_undelegation_unlock() {
        let undelegation = Undelegation::new(
            Address::new([0x11; 20]),
            Address::new([0x22; 20]),
            U256::from_u64(1000),
            10000, // unlock_at
        );

        assert!(!undelegation.is_unlocked(9999));
        assert!(undelegation.is_unlocked(10000));
        assert!(undelegation.is_unlocked(10001));
    }

    #[test]
    fn test_validator_checkpoint_serialization() {
        let validator = ValidatorNode::default();
        let checkpoint = ValidatorCheckpoint::new(
            1,
            100,
            12345,
            vec![validator],
            [0xab; 32],
            90,
        );

        let bytes = checkpoint.to_bytes();
        let decoded = ValidatorCheckpoint::from_bytes(&bytes).unwrap();
        assert_eq!(checkpoint, decoded);
    }

    #[test]
    fn test_double_sign_evidence_serialization() {
        let evidence = DoubleSignEvidence::new(
            Address::new([0x11; 20]),
            Hash::new([0x22; 32]),
            Hash::new([0x33; 32]),
            100,
            Signature::new([0x44; 64]),
            Signature::new([0x55; 64]),
            12345,
        );

        let bytes = evidence.to_bytes();
        let decoded = DoubleSignEvidence::from_bytes(&bytes).unwrap();
        assert_eq!(evidence, decoded);
    }

    #[test]
    fn test_double_sign_to_slashing_evidence() {
        let evidence = DoubleSignEvidence::new(
            Address::new([0x11; 20]),
            Hash::new([0x22; 32]),
            Hash::new([0x33; 32]),
            100,
            Signature::new([0x44; 64]),
            Signature::new([0x55; 64]),
            12345,
        );

        let reporter = Address::new([0x66; 20]);
        let slashing = evidence.to_slashing_evidence(reporter);

        assert_eq!(slashing.offender, evidence.validator);
        assert_eq!(slashing.offense, SlashableOffense::DoubleSign);
        assert_eq!(slashing.block_height, evidence.height);
        assert_eq!(slashing.reporter, reporter);
    }
}
