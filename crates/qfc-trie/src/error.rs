//! Trie error types

use thiserror::Error;

#[derive(Debug, Error)]
pub enum TrieError {
    #[error("Node not found: {0}")]
    NodeNotFound(String),

    #[error("Invalid node encoding")]
    InvalidNodeEncoding,

    #[error("Invalid proof")]
    InvalidProof,

    #[error("Storage error: {0}")]
    Storage(String),

    #[error("Serialization error: {0}")]
    Serialization(String),

    #[error("Key not found")]
    KeyNotFound,
}

impl From<qfc_storage::StorageError> for TrieError {
    fn from(e: qfc_storage::StorageError) -> Self {
        TrieError::Storage(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, TrieError>;
