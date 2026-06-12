//! Off-chain parameter server for decentralized training.
//!
//! ROADMAP-AI-V3 Feature A. Design: `docs/adr/0002`–`0008`.
//!
//! This crate is strictly off-chain (ADR-0002): it implements the shard
//! service a staked PS operator runs — range-sharded parameter storage
//! (ps-lite-style contiguous key ranges over dense model parameters),
//! push/pull with bounded-staleness admission (SSP, ADR-0006), and
//! byzantine-robust aggregation at the epoch barrier (ADR-0003). The chain
//! sees only the epoch-end version commit; nothing here is consensus state.

pub mod aggregate;
pub mod clock;
pub mod kv;
pub mod service;
pub mod types;

pub use aggregate::{AggregationRule, TrimmedMean, UpdateBuffer};
pub use clock::SspClock;
pub use kv::ShardStore;
pub use service::{EpochOutcome, ShardService};
pub use types::{AcceptanceRecord, Epoch, ParamRange, ParamUpdate, StepClock};

use thiserror::Error;

/// Errors from parameter-server operations
#[derive(Debug, Error)]
pub enum PsError {
    #[error("invalid range: {0}")]
    InvalidRange(String),

    #[error("invalid update: {0}")]
    InvalidUpdate(String),

    #[error("stale update: worker clock {worker_clock} below admission floor {floor} (staleness bound {bound})")]
    StaleUpdate {
        worker_clock: u64,
        floor: u64,
        bound: u64,
    },

    #[error("epoch {epoch} is closed for pushes")]
    EpochClosed { epoch: u64 },

    #[error("epoch mismatch: expected {expected}, got {got}")]
    EpochMismatch { expected: u64, got: u64 },

    /// Per-range worker cap hit. Because pushes are admitted only for the
    /// fixed set of registered assignment ranges (disjoint, inside the owned
    /// range), this cap yields a hard buffer bound:
    /// `cap × Σ registered range lengths ≤ cap × owned_range.len()` buffered
    /// f32s (ADR-0003 memory bound).
    #[error(
        "worker cap exceeded for range: {cap} workers already buffered (ADR-0003 memory bound)"
    )]
    WorkerCapExceeded { cap: usize },

    #[error("aggregation failed: {0}")]
    Aggregation(String),

    #[error("storage error: {0}")]
    Storage(String),
}

/// Protocol constants from the Feature-A ADRs, in one place (ADR-0008).
/// A5 mirrors these on-chain as governance parameters.
#[derive(Clone, Debug)]
pub struct PsConfig {
    /// Trim fraction per side for coordinate-wise trimmed mean (ADR-0003)
    pub trim_beta: f64,
    /// SSP staleness bound in steps (ADR-0006)
    pub staleness_bound: u64,
    /// Sampled re-execution rate (ADR-0005)
    pub spot_check_rate: f64,
    /// Slash = this multiple of the per-step reward (ADR-0008)
    pub slash_multiple: u64,
    /// Epoch snapshots retained for the audit window (ADR-0007)
    pub snapshot_retention: u64,
    /// Max buffered updates per registered assignment range. Pushes are
    /// admitted only for the fixed, disjoint registered range set, so total
    /// buffer memory is bounded by
    /// `max_workers_per_range × Σ registered range lengths ≤
    /// max_workers_per_range × owned_range.len()` f32s — the
    /// O(workers × shard) aggregation memory bound (ADR-0003); enforced at
    /// push, mirrored at assignment (A3)
    pub max_workers_per_range: usize,
}

impl Default for PsConfig {
    fn default() -> Self {
        Self {
            trim_beta: 0.2,
            staleness_bound: 3,
            spot_check_rate: 0.05,
            slash_multiple: 40,
            snapshot_retention: 4,
            max_workers_per_range: 64,
        }
    }
}
