//! Network error types

use thiserror::Error;

#[derive(Debug, Error)]
pub enum NetworkError {
    #[error("Transport error: {0}")]
    Transport(String),

    #[error("Connection error: {0}")]
    Connection(String),

    #[error("Protocol error: {0}")]
    Protocol(String),

    #[error("Peer not found: {0}")]
    PeerNotFound(String),

    #[error("Dial error: {0}")]
    Dial(String),

    #[error("Listen error: {0}")]
    Listen(String),

    #[error("Serialization error: {0}")]
    Serialization(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, NetworkError>;
