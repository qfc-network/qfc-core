//! State error types

use thiserror::Error;

#[derive(Debug, Error)]
pub enum StateError {
    #[error("Account not found: {0}")]
    AccountNotFound(String),

    #[error("Insufficient balance: need {need}, have {have}")]
    InsufficientBalance { need: String, have: String },

    #[error("Invalid nonce: expected {expected}, got {actual}")]
    InvalidNonce { expected: u64, actual: u64 },

    #[error("Storage error: {0}")]
    Storage(String),

    #[error("Trie error: {0}")]
    Trie(String),

    #[error("Serialization error: {0}")]
    Serialization(String),
}

impl From<qfc_storage::StorageError> for StateError {
    fn from(e: qfc_storage::StorageError) -> Self {
        StateError::Storage(e.to_string())
    }
}

impl From<qfc_trie::TrieError> for StateError {
    fn from(e: qfc_trie::TrieError) -> Self {
        StateError::Trie(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, StateError>;
