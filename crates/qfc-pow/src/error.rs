//! PoW error types

use thiserror::Error;

/// Errors that can occur during PoW operations
#[derive(Debug, Error)]
pub enum PowError {
    /// Epoch mismatch between proof and task
    #[error("Epoch mismatch: expected {expected}, got {got}")]
    EpochMismatch { expected: u64, got: u64 },

    /// Hash computation doesn't match
    #[error("Invalid hash: computed hash doesn't match proof hash")]
    InvalidHash,

    /// Hash doesn't meet difficulty target
    #[error("Difficulty not met: hash is greater than target")]
    DifficultyNotMet,

    /// Invalid signature on work proof
    #[error("Invalid signature on work proof")]
    InvalidSignature,

    /// Mining task has expired
    #[error("Mining task has expired")]
    TaskExpired,

    /// Miner is not running
    #[error("Miner is not running")]
    MinerNotRunning,

    /// Channel error
    #[error("Channel error: {0}")]
    ChannelError(String),
}
