//! Bridge data types

use borsh::{BorshDeserialize, BorshSerialize};
use qfc_types::{Address, Hash};
use serde::{Deserialize, Serialize};

/// Bridge errors
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BridgeError {
    #[error("Deposit not found: {0}")]
    DepositNotFound(Hash),
    #[error("Withdrawal not found: {0}")]
    WithdrawalNotFound(Hash),
    #[error("Duplicate deposit: {0}")]
    DuplicateDeposit(Hash),
    #[error("Insufficient signatures: have {have}, need {need}")]
    InsufficientSignatures { have: usize, need: usize },
    #[error("Invalid validator: {0}")]
    InvalidValidator(Address),
    #[error("Already signed by {0}")]
    AlreadySigned(Address),
    #[error("Deposit already completed")]
    AlreadyCompleted,
    #[error("Invalid amount")]
    InvalidAmount,
    #[error("Bridge is paused")]
    Paused,
}

/// Status of a deposit (ETH → QFC)
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub enum DepositStatus {
    /// Detected on Ethereum, waiting for confirmations
    Pending,
    /// Enough confirmations, collecting validator signatures
    Confirmed,
    /// Threshold signatures reached, minting on QFC
    Minting,
    /// Wrapped tokens minted on QFC
    Completed,
    /// Failed (e.g., Ethereum reorg)
    Failed,
}

/// Status of a withdrawal (QFC → ETH)
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub enum WithdrawalStatus {
    /// Burn transaction detected on QFC
    Pending,
    /// Collecting validator signatures for unlock
    Signing,
    /// Unlock transaction submitted on Ethereum
    Submitted,
    /// Unlock confirmed on Ethereum
    Completed,
    /// Failed
    Failed,
}

/// A deposit event from Ethereum to QFC
#[derive(Clone, Debug, BorshSerialize, BorshDeserialize)]
pub struct BridgeDeposit {
    /// Unique deposit ID (hash of Ethereum tx hash + log index)
    pub deposit_id: Hash,
    /// Ethereum transaction hash where tokens were locked
    pub eth_tx_hash: Hash,
    /// Ethereum block number of the lock event
    pub eth_block_number: u64,
    /// Sender address on Ethereum (20 bytes)
    pub eth_sender: Address,
    /// Recipient address on QFC
    pub qfc_recipient: Address,
    /// Token address on Ethereum (zero address = native ETH)
    pub token_address: Address,
    /// Amount locked (in wei)
    pub amount: u128,
    /// Required confirmations on Ethereum
    pub required_confirmations: u64,
    /// Current status
    pub status: DepositStatus,
    /// Validator signatures collected
    pub signatures: Vec<(Address, Vec<u8>)>,
    /// Timestamp when first observed
    pub observed_at: u64,
}

impl BridgeDeposit {
    pub fn signature_count(&self) -> usize {
        self.signatures.len()
    }
}

/// A withdrawal event from QFC to Ethereum
#[derive(Clone, Debug, BorshSerialize, BorshDeserialize)]
pub struct BridgeWithdrawal {
    /// Unique withdrawal ID (hash of QFC burn tx hash)
    pub withdrawal_id: Hash,
    /// QFC transaction hash where tokens were burned
    pub qfc_tx_hash: Hash,
    /// QFC block number of the burn
    pub qfc_block_number: u64,
    /// Sender address on QFC (who burned tokens)
    pub qfc_sender: Address,
    /// Recipient address on Ethereum
    pub eth_recipient: Address,
    /// Token address on Ethereum (zero = native ETH)
    pub token_address: Address,
    /// Amount to unlock (in wei)
    pub amount: u128,
    /// Current status
    pub status: WithdrawalStatus,
    /// Validator signatures for the unlock
    pub signatures: Vec<(Address, Vec<u8>)>,
    /// Timestamp when first observed
    pub observed_at: u64,
    /// Ethereum tx hash of the unlock (once submitted)
    pub eth_unlock_tx: Option<Hash>,
}

impl BridgeWithdrawal {
    pub fn signature_count(&self) -> usize {
        self.signatures.len()
    }
}

/// Overall bridge status
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BridgeStatus {
    /// Whether the bridge is active
    pub active: bool,
    /// Number of bridge validators
    pub validator_count: usize,
    /// Signature threshold
    pub threshold: usize,
    /// Total deposits processed
    pub total_deposits: u64,
    /// Total withdrawals processed
    pub total_withdrawals: u64,
    /// Pending deposits awaiting signatures
    pub pending_deposits: u64,
    /// Pending withdrawals awaiting signatures
    pub pending_withdrawals: u64,
    /// Total value locked (ETH, in wei)
    pub total_value_locked: String,
}

/// Well-known wrapped token address on QFC for bridged ETH
pub const WRAPPED_ETH_ADDRESS: [u8; 20] = [
    0xBE, 0xEF, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x01,
];

/// Required Ethereum confirmations for deposit finality
pub const DEFAULT_ETH_CONFIRMATIONS: u64 = 12;

/// Default bridge validator threshold (e.g., 5-of-7)
pub const DEFAULT_THRESHOLD: usize = 5;
