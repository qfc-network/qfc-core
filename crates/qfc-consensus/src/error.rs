//! Consensus error types

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConsensusError {
    #[error("Invalid block producer")]
    InvalidProducer,

    #[error("Invalid VRF proof")]
    InvalidVrfProof,

    #[error("Invalid block signature")]
    InvalidSignature,

    #[error("Invalid state transition")]
    InvalidStateTransition,

    #[error("Invalid timestamp")]
    InvalidTimestamp,

    #[error("Block too large")]
    BlockTooLarge,

    #[error("Not a validator")]
    NotValidator,

    #[error("Validator is jailed")]
    ValidatorJailed,

    #[error("Not our turn to produce")]
    NotOurTurn,

    #[error("Finality not reached")]
    FinalityNotReached,

    #[error("State error: {0}")]
    State(String),

    #[error("Executor error: {0}")]
    Executor(String),

    #[error("Network error: {0}")]
    Network(String),

    #[error("Storage error: {0}")]
    StorageError(String),

    #[error("Double sign detected")]
    DoubleSign,
}

pub type Result<T> = std::result::Result<T, ConsensusError>;
