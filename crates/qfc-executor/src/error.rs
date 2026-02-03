//! Executor error types

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ExecutorError {
    #[error("Invalid signature")]
    InvalidSignature,

    #[error("Invalid chain ID: expected {expected}, got {actual}")]
    InvalidChainId { expected: u64, actual: u64 },

    #[error("Invalid nonce: expected {expected}, got {actual}")]
    InvalidNonce { expected: u64, actual: u64 },

    #[error("Insufficient balance: need {need}, have {have}")]
    InsufficientBalance { need: String, have: String },

    #[error("Gas too low: minimum {minimum}, provided {provided}")]
    GasTooLow { minimum: u64, provided: u64 },

    #[error("Out of gas")]
    OutOfGas,

    #[error("Execution reverted: {0}")]
    Reverted(String),

    #[error("Missing recipient for transfer")]
    MissingRecipient,

    #[error("Contract execution error: {0}")]
    ContractError(String),

    #[error("State error: {0}")]
    State(String),

    #[error("Stake too low: minimum {minimum}, provided {provided}")]
    StakeTooLow { minimum: String, provided: String },

    #[error("Already a validator")]
    AlreadyValidator,

    #[error("Not a validator")]
    NotValidator,

    #[error("Internal error: {0}")]
    Internal(String),

    #[error("EVM error: {0}")]
    EvmError(String),

    #[error("Delegation too low: minimum {minimum}, provided {provided}")]
    DelegationTooLow { minimum: String, provided: String },

    #[error("Already delegated to another validator")]
    AlreadyDelegated,

    #[error("Invalid validator (not registered or no stake)")]
    InvalidValidator,

    #[error("No active delegation")]
    NoDelegation,

    #[error("Insufficient delegation: need {need}, have {have}")]
    InsufficientDelegation { need: String, have: String },
}

impl From<qfc_state::StateError> for ExecutorError {
    fn from(e: qfc_state::StateError) -> Self {
        ExecutorError::State(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, ExecutorError>;
