//! Byzantine-robust aggregation (ADR-0003).
//!
//! Updates buffer per range during the epoch; at the SSP barrier
//! (ADR-0006) the configured [`AggregationRule`] reduces each range's
//! buffered updates to one delta vector. Plain averaging is forbidden as a
//! production rule — workers are untrusted.

use std::collections::HashMap;

use crate::types::{ParamRange, ParamUpdate};
use crate::PsError;

/// Reduces one range's buffered updates to a single delta vector.
pub trait AggregationRule {
    /// `updates` are structurally valid (see `ParamUpdate::validate`) and all
    /// have ranges equal to `range`. Must be deterministic: callers sort
    /// updates by worker address before invoking (ADR-0003).
    fn aggregate(&self, updates: &[&ParamUpdate], range: &ParamRange) -> Result<Vec<f32>, PsError>;
}

/// Coordinate-wise trimmed mean with trim fraction `beta` per side
/// (ADR-0003). Tolerates `f < beta * n` malicious updates per range.
#[derive(Clone, Copy, Debug)]
pub struct TrimmedMean {
    pub beta: f64,
}

impl TrimmedMean {
    pub fn new(beta: f64) -> Result<Self, PsError> {
        if !(0.0..0.5).contains(&beta) {
            return Err(PsError::Aggregation(format!(
                "trim beta {} outside [0, 0.5)",
                beta
            )));
        }
        Ok(Self { beta })
    }
}

impl AggregationRule for TrimmedMean {
    fn aggregate(&self, updates: &[&ParamUpdate], range: &ParamRange) -> Result<Vec<f32>, PsError> {
        // Implemented in milestone A2.
        let _ = (updates, range);
        todo!("A2: coordinate-wise trimmed mean")
    }
}

/// Per-epoch buffer of pushed updates, keyed by exact range.
///
/// Enforces the per-range worker cap — the O(workers × shard_size)
/// aggregation memory bound from ADR-0003 — and one-update-per-(worker, clock)
/// dedup. Cleared at the epoch barrier.
#[derive(Debug, Default)]
pub struct UpdateBuffer {
    max_per_range: usize,
    buffered: HashMap<ParamRange, Vec<ParamUpdate>>,
}

impl UpdateBuffer {
    /// `max_per_range = 0` means uncapped (tests only; production always caps).
    pub fn new(max_per_range: usize) -> Self {
        Self {
            max_per_range,
            buffered: HashMap::new(),
        }
    }

    /// Buffer a validated update. Rejects duplicates (same worker + clock on
    /// the same range) and pushes beyond the worker cap.
    pub fn add(&mut self, update: ParamUpdate) -> Result<(), PsError> {
        update.validate()?;
        let entry = self.buffered.entry(update.range).or_default();
        if entry
            .iter()
            .any(|u| u.worker == update.worker && u.clock == update.clock)
        {
            return Err(PsError::InvalidUpdate(format!(
                "duplicate update from {} at clock {} for {}",
                update.worker, update.clock, update.range
            )));
        }
        if self.max_per_range > 0 && entry.len() >= self.max_per_range {
            return Err(PsError::WorkerCapExceeded {
                cap: self.max_per_range,
            });
        }
        entry.push(update);
        Ok(())
    }

    /// Updates for a range, sorted by (worker, clock) for deterministic
    /// aggregation order (ADR-0003).
    pub fn updates_for(&self, range: &ParamRange) -> Vec<&ParamUpdate> {
        let mut v: Vec<&ParamUpdate> = self
            .buffered
            .get(range)
            .map(|u| u.iter().collect())
            .unwrap_or_default();
        v.sort_by_key(|u| (*u.worker.as_bytes(), u.clock));
        v
    }

    /// All ranges with at least one buffered update, sorted.
    pub fn ranges(&self) -> Vec<ParamRange> {
        let mut r: Vec<ParamRange> = self.buffered.keys().copied().collect();
        r.sort();
        r
    }

    pub fn len(&self) -> usize {
        self.buffered.values().map(|v| v.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Drain everything at the epoch barrier.
    pub fn clear(&mut self) {
        self.buffered.clear();
    }
}

#[cfg(test)]
mod buffer_tests {
    use super::*;
    use qfc_types::Address;

    fn update(worker: u8, clock: u64, range: ParamRange) -> ParamUpdate {
        ParamUpdate {
            worker: Address::new([worker; 20]),
            epoch: 1,
            clock,
            range,
            values: vec![1.0; range.len() as usize],
            flops_estimated: 1,
        }
    }

    #[test]
    fn test_buffer_dedup_and_cap() {
        let r = ParamRange::new(0, 4).unwrap();
        let mut buf = UpdateBuffer::new(2);
        buf.add(update(1, 0, r)).unwrap();
        // Same worker, new clock: allowed
        buf.add(update(1, 1, r)).unwrap();
        // Duplicate (worker, clock): rejected
        assert!(matches!(
            buf.add(update(1, 0, r)),
            Err(PsError::InvalidUpdate(_))
        ));
        // Cap reached
        assert!(matches!(
            buf.add(update(2, 0, r)),
            Err(PsError::WorkerCapExceeded { cap: 2 })
        ));
        assert_eq!(buf.len(), 2);
        buf.clear();
        assert!(buf.is_empty());
    }

    #[test]
    fn test_buffer_deterministic_order() {
        let r = ParamRange::new(0, 2).unwrap();
        let mut buf = UpdateBuffer::new(0);
        buf.add(update(3, 0, r)).unwrap();
        buf.add(update(1, 1, r)).unwrap();
        buf.add(update(1, 0, r)).unwrap();
        let order: Vec<(u8, u64)> = buf
            .updates_for(&r)
            .iter()
            .map(|u| (u.worker.as_bytes()[0], u.clock))
            .collect();
        assert_eq!(order, vec![(1, 0), (1, 1), (3, 0)]);
    }
}
