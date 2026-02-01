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

    /// Staked amount
    pub stake: U256,

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

    /// Jail release timestamp (0 if not jailed)
    pub jail_until: u64,

    /// Total blocks produced
    pub blocks_produced: u64,

    /// Total valid votes cast
    pub valid_votes: u64,

    /// Total invalid votes cast
    pub invalid_votes: u64,
}

impl Default for ValidatorNode {
    fn default() -> Self {
        Self {
            address: Address::ZERO,
            public_key: PublicKey::ZERO,
            stake: U256::ZERO,
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

    /// Check if validator is active (not jailed and has stake)
    pub fn is_active(&self) -> bool {
        !self.is_jailed && !self.stake.is_zero()
    }

    /// Check if validator can be unjailed
    pub fn can_unjail(&self, current_time: u64) -> bool {
        self.is_jailed && current_time >= self.jail_until
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
}
