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
//!
//! Step-increment policy (ADR-0006): a worker's clock advances by exactly
//! one per *accepted push* — there is no out-of-band clock report. This
//! bounds `fastest_clock` growth (and hence the admission floor that can
//! starve other workers) to accepted, metered, audit-eligible work — a
//! cost-free clock-ratchet DoS is impossible. `SspClock` itself only checks
//! monotonicity; the step-increment policy is enforced by `ShardService`.

use std::collections::HashMap;

use qfc_types::Address;
use tracing::debug;

use crate::{types::StepClock, PsError};

/// Per-epoch SSP clock state: one logical clock (step count) per worker,
/// plus the staleness bound that gates push admission.
#[derive(Debug)]
pub struct SspClock {
    staleness_bound: u64,
    clocks: HashMap<Address, StepClock>,
}

impl SspClock {
    pub fn new(staleness_bound: u64) -> Self {
        Self {
            staleness_bound,
            clocks: HashMap::new(),
        }
    }

    pub fn staleness_bound(&self) -> u64 {
        self.staleness_bound
    }

    /// Register a worker at clock 0 (no-op if already registered).
    pub fn register_worker(&mut self, worker: Address) {
        self.clocks.entry(worker).or_insert(0);
    }

    /// The worker's current clock, if registered.
    pub fn worker_clock(&self, worker: Address) -> Option<StepClock> {
        self.clocks.get(&worker).copied()
    }

    /// Worker reports step completion. Auto-registers unknown workers.
    /// Clocks are monotonic per worker: regression is an error
    /// (`PsError::InvalidUpdate`); re-reporting the current clock is a no-op.
    ///
    /// This is the internal advancement mechanism — it only checks
    /// monotonicity. Callers must enforce the step-increment policy (exactly
    /// one step per accepted push, ADR-0006); `ShardService` does.
    pub fn advance(&mut self, worker: Address, clock: StepClock) -> Result<(), PsError> {
        let entry = self.clocks.entry(worker).or_insert(0);
        if clock < *entry {
            return Err(PsError::InvalidUpdate(format!(
                "clock regression for worker {}: {} < {}",
                worker, clock, entry
            )));
        }
        *entry = clock;
        Ok(())
    }

    /// Slowest registered worker (0 when none are registered).
    pub fn min_clock(&self) -> StepClock {
        self.clocks.values().copied().min().unwrap_or(0)
    }

    /// Fastest registered worker (0 when none are registered).
    pub fn fastest_clock(&self) -> StepClock {
        self.clocks.values().copied().max().unwrap_or(0)
    }

    /// SSP admission: a push at `worker_clock` is admitted iff
    /// `worker_clock + staleness_bound >= fastest_clock`, i.e. it is no
    /// further than the staleness bound behind the fastest worker. Rejection
    /// is `PsError::StaleUpdate` — an error, never a slash (ADR-0006).
    pub fn admit(&self, worker_clock: StepClock) -> Result<(), PsError> {
        let floor = self.fastest_clock().saturating_sub(self.staleness_bound);
        if worker_clock < floor {
            return Err(PsError::StaleUpdate {
                worker_clock,
                floor,
                bound: self.staleness_bound,
            });
        }
        Ok(())
    }

    /// Epoch barrier: returns the final per-worker clock map for acceptance
    /// accounting and resets all clocks for the next epoch (workers
    /// re-register as they participate).
    pub fn barrier(&mut self) -> HashMap<Address, StepClock> {
        debug!(workers = self.clocks.len(), "ssp clock barrier");
        std::mem::take(&mut self.clocks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(b: u8) -> Address {
        Address::new([b; 20])
    }

    #[test]
    fn test_admission_within_and_outside_bound() {
        let mut clock = SspClock::new(3);
        // No workers yet: floor is 0, everything admits.
        assert!(clock.admit(0).is_ok());

        clock.advance(addr(1), 10).unwrap();
        clock.advance(addr(2), 4).unwrap();
        assert_eq!(clock.fastest_clock(), 10);
        assert_eq!(clock.min_clock(), 4);

        // Floor = 10 - 3 = 7.
        assert!(clock.admit(7).is_ok());
        assert!(clock.admit(10).is_ok());
        assert!(matches!(
            clock.admit(6),
            Err(PsError::StaleUpdate {
                worker_clock: 6,
                floor: 7,
                bound: 3
            })
        ));
    }

    #[test]
    fn test_monotonicity_violation() {
        let mut clock = SspClock::new(3);
        clock.advance(addr(1), 5).unwrap();
        // Equal clock: idempotent no-op.
        clock.advance(addr(1), 5).unwrap();
        // Regression: error, clock unchanged.
        assert!(matches!(
            clock.advance(addr(1), 4),
            Err(PsError::InvalidUpdate(_))
        ));
        assert_eq!(clock.worker_clock(addr(1)), Some(5));
    }

    #[test]
    fn test_register_and_worker_clock() {
        let mut clock = SspClock::new(3);
        assert_eq!(clock.worker_clock(addr(1)), None);
        clock.register_worker(addr(1));
        assert_eq!(clock.worker_clock(addr(1)), Some(0));
        // Re-register does not reset.
        clock.advance(addr(1), 2).unwrap();
        clock.register_worker(addr(1));
        assert_eq!(clock.worker_clock(addr(1)), Some(2));
    }

    #[test]
    fn test_barrier_returns_final_map_and_resets() {
        let mut clock = SspClock::new(3);
        clock.advance(addr(1), 7).unwrap();
        clock.advance(addr(2), 9).unwrap();

        let finals = clock.barrier();
        assert_eq!(finals.len(), 2);
        assert_eq!(finals[&addr(1)], 7);
        assert_eq!(finals[&addr(2)], 9);

        // Reset: clocks cleared, admission floor back to 0.
        assert_eq!(clock.worker_clock(addr(1)), None);
        assert_eq!(clock.fastest_clock(), 0);
        assert!(clock.admit(0).is_ok());
        assert!(clock.barrier().is_empty());
    }
}
