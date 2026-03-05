//! Redundant verification for high-value inference tasks
//!
//! Tasks with fees above a threshold are sent to multiple miners.
//! The majority-consistent output wins; inconsistent miners are penalized.

use std::collections::HashMap;

use qfc_types::{Address, Hash};

/// Configuration for redundant verification
#[derive(Clone, Debug)]
pub struct RedundantConfig {
    /// Minimum fee (in base units) to trigger redundant verification
    pub fee_threshold: u128,
    /// Number of miners to assign per redundant task
    pub redundancy_count: usize,
}

impl Default for RedundantConfig {
    fn default() -> Self {
        Self {
            fee_threshold: 1_000_000_000_000_000_000, // 1 QFC
            redundancy_count: 3,
        }
    }
}

/// Result of redundant verification when all submissions are in
#[derive(Clone, Debug)]
pub struct RedundantResult {
    /// The consensus output hash (majority)
    pub consensus_hash: Hash,
    /// Miners whose output matched the consensus
    pub consistent_miners: Vec<Address>,
    /// Miners whose output did NOT match the consensus
    pub inconsistent_miners: Vec<Address>,
}

/// Manages redundant verification for high-value tasks
pub struct RedundantVerifier {
    config: RedundantConfig,
    /// task_id -> list of (miner, output_hash) submissions
    pending: HashMap<Hash, Vec<(Address, Hash)>>,
}

impl RedundantVerifier {
    pub fn new(config: RedundantConfig) -> Self {
        Self {
            config,
            pending: HashMap::new(),
        }
    }

    /// Check if a task fee warrants redundant verification
    pub fn requires_redundant(&self, task_fee: u128) -> bool {
        task_fee >= self.config.fee_threshold
    }

    /// Register a task for redundant verification
    pub fn register_task(&mut self, task_id: Hash) {
        self.pending.entry(task_id).or_default();
    }

    /// Check if a task is pending redundant verification (waiting for more submissions)
    pub fn is_pending(&self, task_id: &Hash) -> bool {
        self.pending.contains_key(task_id)
    }

    /// Record a submission for a redundant task.
    /// Returns `Some(RedundantResult)` when all N miners have submitted.
    pub fn record_submission(
        &mut self,
        task_id: Hash,
        miner: Address,
        output_hash: Hash,
    ) -> Option<RedundantResult> {
        let submissions = self.pending.entry(task_id).or_default();

        // Don't allow duplicate submissions from the same miner
        if submissions.iter().any(|(m, _)| *m == miner) {
            return None;
        }

        submissions.push((miner, output_hash));

        if submissions.len() < self.config.redundancy_count {
            return None; // still waiting
        }

        // All submissions in — find consensus (majority hash)
        let submissions = self.pending.remove(&task_id)?;
        let mut hash_counts: HashMap<Hash, Vec<Address>> = HashMap::new();
        for (m, h) in &submissions {
            hash_counts.entry(*h).or_default().push(*m);
        }

        // Find the hash with most votes
        let (consensus_hash, consistent_miners) = hash_counts
            .into_iter()
            .max_by_key(|(_, miners)| miners.len())?;

        let inconsistent_miners: Vec<Address> = submissions
            .iter()
            .filter(|(_, h)| *h != consensus_hash)
            .map(|(m, _)| *m)
            .collect();

        Some(RedundantResult {
            consensus_hash,
            consistent_miners,
            inconsistent_miners,
        })
    }

    /// Get the redundancy count
    pub fn redundancy_count(&self) -> usize {
        self.config.redundancy_count
    }
}

impl Default for RedundantVerifier {
    fn default() -> Self {
        Self::new(RedundantConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(b: u8) -> Address {
        Address::new([b; 20])
    }

    #[test]
    fn test_requires_redundant() {
        let rv = RedundantVerifier::default();
        assert!(!rv.requires_redundant(0));
        assert!(!rv.requires_redundant(999_999_999_999_999_999));
        assert!(rv.requires_redundant(1_000_000_000_000_000_000));
        assert!(rv.requires_redundant(2_000_000_000_000_000_000));
    }

    #[test]
    fn test_record_submission_all_agree() {
        let mut rv = RedundantVerifier::new(RedundantConfig {
            fee_threshold: 100,
            redundancy_count: 3,
        });

        let task_id = Hash::new([0x42; 32]);
        let output = Hash::new([0xAA; 32]);

        rv.register_task(task_id);

        // First two submissions return None (waiting)
        assert!(rv.record_submission(task_id, addr(1), output).is_none());
        assert!(rv.record_submission(task_id, addr(2), output).is_none());

        // Third submission triggers result
        let result = rv.record_submission(task_id, addr(3), output).unwrap();
        assert_eq!(result.consensus_hash, output);
        assert_eq!(result.consistent_miners.len(), 3);
        assert!(result.inconsistent_miners.is_empty());
    }

    #[test]
    fn test_record_submission_one_disagrees() {
        let mut rv = RedundantVerifier::new(RedundantConfig {
            fee_threshold: 100,
            redundancy_count: 3,
        });

        let task_id = Hash::new([0x42; 32]);
        let good_output = Hash::new([0xAA; 32]);
        let bad_output = Hash::new([0xBB; 32]);

        rv.register_task(task_id);

        assert!(rv
            .record_submission(task_id, addr(1), good_output)
            .is_none());
        assert!(rv.record_submission(task_id, addr(2), bad_output).is_none());
        let result = rv.record_submission(task_id, addr(3), good_output).unwrap();

        assert_eq!(result.consensus_hash, good_output);
        assert_eq!(result.consistent_miners.len(), 2);
        assert_eq!(result.inconsistent_miners.len(), 1);
        assert_eq!(result.inconsistent_miners[0], addr(2));
    }

    #[test]
    fn test_duplicate_submission_ignored() {
        let mut rv = RedundantVerifier::new(RedundantConfig {
            fee_threshold: 100,
            redundancy_count: 3,
        });

        let task_id = Hash::new([0x42; 32]);
        let output = Hash::new([0xAA; 32]);

        rv.register_task(task_id);
        assert!(rv.record_submission(task_id, addr(1), output).is_none());
        // Duplicate from same miner
        assert!(rv.record_submission(task_id, addr(1), output).is_none());
        // Still need 2 more unique miners
        assert!(rv.record_submission(task_id, addr(2), output).is_none());
        assert!(rv.record_submission(task_id, addr(3), output).is_some());
    }

    #[test]
    fn test_is_pending() {
        let mut rv = RedundantVerifier::default();
        let task_id = Hash::new([0x42; 32]);

        assert!(!rv.is_pending(&task_id));
        rv.register_task(task_id);
        assert!(rv.is_pending(&task_id));
    }
}
