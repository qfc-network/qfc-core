//! Chain error types

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ChainError {
    #[error("Block not found: {0}")]
    BlockNotFound(String),

    #[error("Invalid block: {0}")]
    InvalidBlock(String),

    #[error("Invalid parent: expected {expected}, got {actual}")]
    InvalidParent { expected: String, actual: String },

    #[error("Block already known")]
    BlockAlreadyKnown,

    #[error("Genesis already initialized")]
    GenesisAlreadyInitialized,

    #[error("Genesis not found")]
    GenesisNotFound,

    #[error("Storage error: {0}")]
    Storage(String),

    #[error("State error: {0}")]
    State(String),

    #[error("Consensus error: {0}")]
    Consensus(String),

    #[error("Executor error: {0}")]
    Executor(String),
}

impl From<qfc_storage::StorageError> for ChainError {
    fn from(e: qfc_storage::StorageError) -> Self {
        ChainError::Storage(e.to_string())
    }
}

impl From<qfc_state::StateError> for ChainError {
    fn from(e: qfc_state::StateError) -> Self {
        ChainError::State(e.to_string())
    }
}

impl From<qfc_consensus::ConsensusError> for ChainError {
    fn from(e: qfc_consensus::ConsensusError) -> Self {
        ChainError::Consensus(e.to_string())
    }
}

impl From<qfc_executor::ExecutorError> for ChainError {
    fn from(e: qfc_executor::ExecutorError) -> Self {
        ChainError::Executor(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, ChainError>;
