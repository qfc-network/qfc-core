//! Storage error types

use thiserror::Error;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("Database error: {0}")]
    Database(String),

    #[error("Key not found: {0}")]
    KeyNotFound(String),

    #[error("Serialization error: {0}")]
    Serialization(String),

    #[error("Deserialization error: {0}")]
    Deserialization(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Column family not found: {0}")]
    ColumnFamilyNotFound(String),

    #[error("Database is closed")]
    DatabaseClosed,

    #[error("Invalid data: {0}")]
    InvalidData(String),
}

impl From<rocksdb::Error> for StorageError {
    fn from(e: rocksdb::Error) -> Self {
        StorageError::Database(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, StorageError>;
