//! Mempool error types

use thiserror::Error;

#[derive(Debug, Error)]
pub enum MempoolError {
    #[error("Transaction already known")]
    AlreadyKnown,

    #[error("Pool is full")]
    PoolFull,

    #[error("Account pool is full")]
    AccountPoolFull,

    #[error("Gas price too low: minimum {minimum}, provided {provided}")]
    GasPriceTooLow { minimum: String, provided: String },

    #[error("Transaction expired")]
    Expired,

    #[error("Invalid nonce: expected {expected}, got {actual}")]
    InvalidNonce { expected: u64, actual: u64 },

    #[error("State error: {0}")]
    State(String),
}

pub type Result<T> = std::result::Result<T, MempoolError>;
