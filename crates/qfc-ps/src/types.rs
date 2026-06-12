//! Core parameter-server types.
//!
//! Keys are dense parameter indices (u64); a shard owns a contiguous
//! `ParamRange` (ps-lite-style range sharding — dense tensors have key
//! locality, unlike hashed sparse-embedding IDs; see ADR-0001 discussion
//! and ROADMAP-AI-V3 Feature A).

use borsh::{BorshDeserialize, BorshSerialize};
use qfc_types::{Address, Hash};
use serde::{Deserialize, Serialize};

use crate::PsError;

/// Training-epoch identifier. Opaque to this crate — the coordinator owns
/// epoch progression (ADR-0006: no dependency on qfc-consensus).
pub type Epoch = u64;

/// A worker's logical clock = its step count within the current epoch (SSP).
pub type StepClock = u64;

/// Contiguous parameter key range `[start, end)`.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    std::hash::Hash,
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
)]
pub struct ParamRange {
    pub start: u64,
    pub end: u64,
}

impl ParamRange {
    pub fn new(start: u64, end: u64) -> Result<Self, PsError> {
        if start >= end {
            return Err(PsError::InvalidRange(format!(
                "start {} >= end {}",
                start, end
            )));
        }
        Ok(Self { start, end })
    }

    pub fn len(&self) -> u64 {
        self.end - self.start
    }

    pub fn is_empty(&self) -> bool {
        self.start >= self.end
    }

    pub fn contains(&self, key: u64) -> bool {
        key >= self.start && key < self.end
    }

    /// True if `other` lies entirely within this range.
    pub fn covers(&self, other: &ParamRange) -> bool {
        other.start >= self.start && other.end <= self.end
    }

    pub fn overlaps(&self, other: &ParamRange) -> bool {
        self.start < other.end && other.start < self.end
    }
}

impl std::fmt::Display for ParamRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}, {})", self.start, self.end)
    }
}

/// A gradient/delta update pushed by a training worker for one range.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ParamUpdate {
    pub worker: Address,
    pub epoch: Epoch,
    /// Worker's SSP clock at the step that produced this update
    pub clock: StepClock,
    pub range: ParamRange,
    /// Delta values for `[range.start, range.end)`, in key order
    pub values: Vec<f32>,
    /// Metering claim (ADR-0004), audited by sampled re-execution (ADR-0005)
    pub flops_estimated: u64,
}

impl ParamUpdate {
    /// Structural validity: value count matches the range, all values finite.
    pub fn validate(&self) -> Result<(), PsError> {
        if self.range.is_empty() {
            return Err(PsError::InvalidRange(format!("empty range {}", self.range)));
        }
        if self.values.len() as u64 != self.range.len() {
            return Err(PsError::InvalidUpdate(format!(
                "value count {} != range length {} for {}",
                self.values.len(),
                self.range.len(),
                self.range
            )));
        }
        if !self.values.iter().all(|v| v.is_finite()) {
            return Err(PsError::InvalidUpdate(
                "non-finite value in update".to_string(),
            ));
        }
        Ok(())
    }

    /// Blake3 over a canonical encoding — this is the `update_hash` that
    /// acceptance records carry and that verifiers recompute (ADR-0005).
    pub fn update_hash(&self) -> Hash {
        let mut bytes = Vec::with_capacity(20 + 8 * 4 + self.values.len() * 4);
        bytes.extend_from_slice(self.worker.as_bytes());
        bytes.extend_from_slice(&self.epoch.to_be_bytes());
        bytes.extend_from_slice(&self.clock.to_be_bytes());
        bytes.extend_from_slice(&self.range.start.to_be_bytes());
        bytes.extend_from_slice(&self.range.end.to_be_bytes());
        for v in &self.values {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        bytes.extend_from_slice(&self.flops_estimated.to_be_bytes());
        qfc_crypto::blake3_hash(&bytes)
    }
}

/// Per-epoch acceptance record (ADR-0004) — the unit reward distribution
/// reads. A5 anchors the epoch's record-set hash on-chain.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub struct AcceptanceRecord {
    pub worker: Address,
    pub epoch: Epoch,
    pub clock: StepClock,
    pub range: ParamRange,
    pub update_hash: Hash,
    pub flops_estimated: u64,
}

impl AcceptanceRecord {
    pub fn for_update(update: &ParamUpdate) -> Self {
        Self {
            worker: update.worker,
            epoch: update.epoch,
            clock: update.clock,
            range: update.range,
            update_hash: update.update_hash(),
            flops_estimated: update.flops_estimated,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(b: u8) -> Address {
        Address::new([b; 20])
    }

    fn update(worker: u8, range: ParamRange, values: Vec<f32>) -> ParamUpdate {
        ParamUpdate {
            worker: addr(worker),
            epoch: 1,
            clock: 0,
            range,
            values,
            flops_estimated: 1000,
        }
    }

    #[test]
    fn test_range_basics() {
        assert!(ParamRange::new(5, 5).is_err());
        assert!(ParamRange::new(6, 5).is_err());
        let r = ParamRange::new(10, 20).unwrap();
        assert_eq!(r.len(), 10);
        assert!(r.contains(10) && r.contains(19) && !r.contains(20));
        let inner = ParamRange::new(12, 18).unwrap();
        assert!(r.covers(&inner) && !inner.covers(&r));
        let disjoint = ParamRange::new(20, 30).unwrap();
        assert!(!r.overlaps(&disjoint));
        assert!(r.overlaps(&ParamRange::new(19, 25).unwrap()));
    }

    #[test]
    fn test_update_validation() {
        let r = ParamRange::new(0, 4).unwrap();
        assert!(update(1, r, vec![1.0; 4]).validate().is_ok());
        assert!(update(1, r, vec![1.0; 3]).validate().is_err());
        assert!(update(1, r, vec![1.0, 2.0, f32::NAN, 4.0])
            .validate()
            .is_err());
        assert!(update(1, r, vec![1.0, 2.0, f32::INFINITY, 4.0])
            .validate()
            .is_err());
    }

    #[test]
    fn test_update_hash_sensitivity() {
        let r = ParamRange::new(0, 2).unwrap();
        let base = update(1, r, vec![1.0, 2.0]);
        assert_eq!(base.update_hash(), base.update_hash());
        let mut other = base.clone();
        other.values[1] = 2.0001;
        assert_ne!(base.update_hash(), other.update_hash());
        let mut other = base.clone();
        other.clock = 1;
        assert_ne!(base.update_hash(), other.update_hash());
        assert_ne!(
            base.update_hash(),
            update(2, r, vec![1.0, 2.0]).update_hash()
        );
    }
}
