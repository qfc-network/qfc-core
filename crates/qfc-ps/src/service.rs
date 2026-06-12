//! Shard service: push/pull + epoch barrier, tying together kv / clock /
//! aggregate (the component a PS operator runs, ADR-0002).
//!
//! Implemented in milestone A1. Required API:
//!
//! - `ShardService::new(store: ShardStore, config: PsConfig, epoch: Epoch)`.
//! - `push(&mut self, update: ParamUpdate) -> Result<AcceptanceRecord, PsError>`
//!   — validates epoch match + structure, SSP admission via `SspClock`,
//!   buffers via `UpdateBuffer` (worker cap), records acceptance (ADR-0004).
//!   Rejected-as-stale is an error, not a slash (ADR-0006).
//! - `pull(&self, range: ParamRange) -> Result<Vec<f32>, PsError>` — current
//!   shard parameters for any sub-range of the owned range.
//! - `advance_clock(&mut self, worker, clock)` — worker step report.
//! - `end_epoch(&mut self, rule: &dyn AggregationRule)
//!      -> Result<EpochOutcome, PsError>` — the barrier (ADR-0006): for each
//!   buffered range, aggregate (updates sorted by worker for determinism),
//!   `apply_delta`, then clear buffer, bump epoch, and return
//!   `EpochOutcome { epoch, params_hash, accepted: Vec<AcceptanceRecord> }`
//!   — the inputs of the on-chain version commit (A5).
//! - Pushes for a closed/mismatched epoch → `PsError::EpochClosed` /
//!   `EpochMismatch`.

#[allow(unused_imports)]
use crate::{
    aggregate::{AggregationRule, UpdateBuffer},
    types::{AcceptanceRecord, Epoch, ParamRange, ParamUpdate},
    PsConfig, PsError,
};
