//! Error types for QFC blockchain

use thiserror::Error;

/// General QFC error type
#[derive(Debug, Error)]
pub enum QfcError {
    #[error("Invalid hash: {0}")]
    InvalidHash(String),

    #[error("Invalid address: {0}")]
    InvalidAddress(String),

    #[error("Invalid signature: {0}")]
    InvalidSignature(String),

    #[error("Invalid transaction: {0}")]
    InvalidTransaction(String),

    #[error("Invalid block: {0}")]
    InvalidBlock(String),

    #[error("Invalid chain ID: expected {expected}, got {actual}")]
    InvalidChainId { expected: u64, actual: u64 },

    #[error("Invalid nonce: expected {expected}, got {actual}")]
    InvalidNonce { expected: u64, actual: u64 },

    #[error("Insufficient balance: need {need}, have {have}")]
    InsufficientBalance { need: String, have: String },

    #[error("Gas too low: minimum {minimum}, provided {provided}")]
    GasTooLow { minimum: u64, provided: u64 },

    #[error("Gas price too low: minimum {minimum}, provided {provided}")]
    GasPriceTooLow { minimum: String, provided: String },

    #[error("Gas limit exceeded: limit {limit}, used {used}")]
    GasLimitExceeded { limit: u64, used: u64 },

    #[error("Account not found: {0}")]
    AccountNotFound(String),

    #[error("Block not found: {0}")]
    BlockNotFound(String),

    #[error("Transaction not found: {0}")]
    TransactionNotFound(String),

    #[error("Validator not found: {0}")]
    ValidatorNotFound(String),

    #[error("Missing recipient for transfer")]
    MissingRecipient,

    #[error("Transaction already known")]
    AlreadyKnown,

    #[error("Account pool full")]
    AccountPoolFull,

    #[error("Pool full")]
    PoolFull,

    #[error("Transaction expired")]
    TransactionExpired,

    #[error("Invalid VRF proof")]
    InvalidVrfProof,

    #[error("Invalid producer: expected {expected}, got {actual}")]
    InvalidProducer { expected: String, actual: String },

    #[error("Invalid state transition")]
    InvalidStateTransition,

    #[error("Double sign detected")]
    DoubleSign,

    #[error("Validator jailed until {0}")]
    ValidatorJailed(u64),

    #[error("Stake too low: minimum {minimum}, provided {provided}")]
    StakeTooLow { minimum: String, provided: String },

    #[error("Storage error: {0}")]
    Storage(String),

    #[error("Serialization error: {0}")]
    Serialization(String),

    #[error("Network error: {0}")]
    Network(String),

    #[error("Crypto error: {0}")]
    Crypto(String),

    #[error("Consensus error: {0}")]
    Consensus(String),

    #[error("Execution error: {0}")]
    Execution(String),

    #[error("Trie error: {0}")]
    Trie(String),

    #[error("RPC error: {0}")]
    Rpc(String),

    #[error("Internal error: {0}")]
    Internal(String),
}

impl From<std::io::Error> for QfcError {
    fn from(e: std::io::Error) -> Self {
        QfcError::Storage(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, QfcError>;
