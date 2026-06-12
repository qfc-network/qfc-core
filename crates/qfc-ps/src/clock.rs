//! SSP (stale-synchronous parallel) clock tracking (ADR-0006).
//!
//! Implemented in milestone A1. Required API:
//!
//! - `SspClock::new(staleness_bound)` — per-epoch state.
//! - `register_worker(&mut self, worker)` / `worker_clock(&self, worker)`.
//! - `advance(&mut self, worker, clock)` — worker reports step completion;
//!   clocks are monotonic per worker.
//! - `min_clock(&self) -> StepClock` — slowest registered worker.
//! - `admit(&self, worker_clock) -> Result<(), PsError>` — SSP admission:
//!   a push at `worker_clock` is admitted iff
//!   `worker_clock + staleness_bound >= fastest_clock`; rejected pushes
//!   return `PsError::StaleUpdate` (ADR-0006: stale ≠ slashable).
//! - `barrier(&mut self)` — epoch barrier: resets clocks for the next epoch,
//!   returns the final per-worker clock map (feeds acceptance accounting).

#[allow(unused_imports)]
use crate::{types::StepClock, PsError};
