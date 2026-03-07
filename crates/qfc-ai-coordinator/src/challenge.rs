//! Challenge task system for verifying miner honesty
//!
//! Pre-computed tasks with known answers, injected at adaptive rates.
//! Indistinguishable from real tasks to miners.

use std::collections::HashMap;

use qfc_inference::task::InferenceTask;
use qfc_inference::InferenceEngine;
use qfc_types::{Address, Hash};

use crate::task_types::synthetic_task_for_tier;

/// A challenge task with a known expected output
#[derive(Clone, Debug)]
pub struct ChallengeTask {
    pub task: InferenceTask,
    pub expected_output_hash: Hash,
    pub tolerance: f64,
}

/// Result of verifying a challenge
#[derive(Clone, Debug, PartialEq)]
pub enum ChallengeVerdict {
    Passed,
    Suspicious { similarity: f64 },
    Failed { similarity: f64 },
}

/// Track a miner's challenge history
#[derive(Clone, Debug, Default)]
pub struct MinerChallengeRecord {
    pub total_challenges: u64,
    pub passed: u64,
    pub failed: u64,
    pub consecutive_failures: u32,
}

/// Penalty escalation from challenge failures
#[derive(Clone, Debug)]
pub struct ChallengePenalty {
    /// Reputation reduction in basis points
    pub reputation_reduction: u32,
    /// Percentage of stake to slash (0 for none)
    pub slash_percent: u8,
    /// Jail duration in milliseconds (0 for none)
    pub jail_duration_ms: u64,
}

/// Generates and tracks challenge tasks
#[derive(Clone)]
pub struct ChallengeGenerator {
    challenge_pool: Vec<ChallengeTask>,
    active_challenges: HashMap<Hash, ChallengeTask>,
    miner_records: HashMap<Address, MinerChallengeRecord>,
    base_ratio: f64,
}

impl ChallengeGenerator {
    pub fn new() -> Self {
        Self {
            challenge_pool: Vec::new(),
            active_challenges: HashMap::new(),
            miner_records: HashMap::new(),
            base_ratio: 0.05,
        }
    }

    /// Pre-compute challenge tasks for an epoch using a CpuEngine
    pub async fn generate_challenges(
        &mut self,
        engine: &dyn InferenceEngine,
        epoch: u64,
        seed: u64,
    ) {
        self.challenge_pool.clear();

        // Generate one challenge per tier
        for tier in [
            qfc_inference::GpuTier::Cold,
            qfc_inference::GpuTier::Warm,
            qfc_inference::GpuTier::Hot,
        ] {
            let task_type = synthetic_task_for_tier(tier, epoch, seed.wrapping_add(0xCAFE));
            let task_id = {
                let mut data = Vec::with_capacity(24);
                data.extend_from_slice(&epoch.to_le_bytes());
                data.extend_from_slice(&seed.to_le_bytes());
                data.extend_from_slice(&[tier as u8; 8]);
                qfc_crypto::blake3_hash(&data)
            };

            let task = InferenceTask::new(task_id, epoch, task_type, Vec::new(), 0, u64::MAX);

            // Run through engine to get expected output
            let expected_output_hash = match engine.run_inference(&task).await {
                Ok(result) => result.output_hash,
                Err(_) => continue,
            };

            self.challenge_pool.push(ChallengeTask {
                task,
                expected_output_hash,
                tolerance: 0.0, // exact match for deterministic outputs
            });
        }
    }

    /// Determine if a challenge should be injected for this miner
    pub fn should_inject_challenge(
        &self,
        miner: &Address,
        tasks_completed: u64,
        reputation: u32,
    ) -> bool {
        let rate = if tasks_completed < 100 {
            0.10 // 10% for new miners
        } else if reputation < 8000 {
            0.08 // 8% for low reputation
        } else {
            self.base_ratio // 5% standard
        };

        // Deterministic pseudo-random based on miner + tasks_completed
        let mut data = Vec::with_capacity(28);
        data.extend_from_slice(miner.as_bytes());
        data.extend_from_slice(&tasks_completed.to_le_bytes());
        let hash = qfc_crypto::blake3_hash(&data);
        let val = u32::from_le_bytes(hash.as_bytes()[..4].try_into().unwrap());
        let threshold = (rate * u32::MAX as f64) as u32;
        val < threshold
    }

    /// Inject a challenge task, returning it if available
    pub fn inject_challenge(&mut self, task_id: Hash) -> Option<&ChallengeTask> {
        if self.challenge_pool.is_empty() {
            return None;
        }

        // Pick a challenge from the pool (round-robin based on task_id)
        let idx_bytes: [u8; 4] = task_id.as_bytes()[..4].try_into().unwrap();
        let idx = u32::from_le_bytes(idx_bytes) as usize % self.challenge_pool.len();
        let challenge = self.challenge_pool[idx].clone();
        self.active_challenges.insert(task_id, challenge);
        self.active_challenges.get(&task_id)
    }

    /// Check if a task_id is an active challenge
    pub fn is_challenge(&self, task_id: &Hash) -> bool {
        self.active_challenges.contains_key(task_id)
    }

    /// Verify a challenge result
    pub fn verify_challenge(
        &mut self,
        task_id: &Hash,
        actual_output_hash: &Hash,
    ) -> Option<ChallengeVerdict> {
        let challenge = self.active_challenges.remove(task_id)?;

        if *actual_output_hash == challenge.expected_output_hash {
            Some(ChallengeVerdict::Passed)
        } else {
            // Compute similarity via matching prefix bytes
            let expected = challenge.expected_output_hash.as_bytes();
            let actual = actual_output_hash.as_bytes();
            let matching = expected
                .iter()
                .zip(actual.iter())
                .take_while(|(a, b)| a == b)
                .count();
            let similarity = matching as f64 / expected.len() as f64;

            if similarity > 0.5 {
                Some(ChallengeVerdict::Suspicious { similarity })
            } else {
                Some(ChallengeVerdict::Failed { similarity })
            }
        }
    }

    /// Record a challenge result for a miner and return penalty if warranted
    pub fn record_result(
        &mut self,
        miner: &Address,
        verdict: &ChallengeVerdict,
    ) -> Option<ChallengePenalty> {
        let record = self.miner_records.entry(*miner).or_default();
        record.total_challenges += 1;

        match verdict {
            ChallengeVerdict::Passed => {
                record.passed += 1;
                record.consecutive_failures = 0;
                None
            }
            ChallengeVerdict::Suspicious { .. } => {
                record.failed += 1;
                record.consecutive_failures += 1;
                // Suspicious = reputation reduction only
                Some(ChallengePenalty {
                    reputation_reduction: 500,
                    slash_percent: 0,
                    jail_duration_ms: 0,
                })
            }
            ChallengeVerdict::Failed { .. } => {
                record.failed += 1;
                record.consecutive_failures += 1;

                if record.consecutive_failures >= 3 {
                    // Escalated: slash + jail
                    Some(ChallengePenalty {
                        reputation_reduction: 500,
                        slash_percent: 5,
                        jail_duration_ms: 3 * 24 * 60 * 60 * 1000, // 3 days
                    })
                } else {
                    // Single failure: reputation only
                    Some(ChallengePenalty {
                        reputation_reduction: 500,
                        slash_percent: 0,
                        jail_duration_ms: 0,
                    })
                }
            }
        }
    }

    /// Get a miner's challenge record
    pub fn get_record(&self, miner: &Address) -> Option<&MinerChallengeRecord> {
        self.miner_records.get(miner)
    }
}

impl Default for ChallengeGenerator {
    fn default() -> Self {
        Self::new()
    }
}

// ── E3: Multi-validator arbitration ──────────────────────────────────────

/// Minimum validators needed for a valid arbitration panel
pub const MIN_ARBITRATORS: usize = 3;

/// A vote from a single arbitrator (validator re-executing the task)
#[derive(Clone, Debug)]
pub struct ArbitrationVote {
    pub validator: Address,
    pub output_hash: Hash,
    pub execution_time_ms: u64,
}

/// Outcome of a multi-validator arbitration
#[derive(Clone, Debug, PartialEq)]
pub enum ArbitrationOutcome {
    /// Miner's result matches majority — no fault
    MinerVindicated,
    /// Miner's result disagrees with majority — penalize
    MinerFaulted {
        majority_hash: Hash,
        agree_count: usize,
        total_count: usize,
    },
    /// Not enough votes to decide (< MIN_ARBITRATORS)
    Inconclusive,
}

/// Manages a dispute for a single task
#[derive(Clone, Debug)]
pub struct ArbitrationPanel {
    pub task_id: Hash,
    pub miner: Address,
    pub miner_output_hash: Hash,
    pub votes: Vec<ArbitrationVote>,
    pub created_at: u64,
}

impl ArbitrationPanel {
    /// Create a new arbitration panel for a disputed task
    pub fn new(task_id: Hash, miner: Address, miner_output_hash: Hash) -> Self {
        let created_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        Self {
            task_id,
            miner,
            miner_output_hash,
            votes: Vec::new(),
            created_at,
        }
    }

    /// Add a validator's re-execution vote
    pub fn add_vote(&mut self, vote: ArbitrationVote) {
        // Prevent duplicate votes from the same validator
        if !self.votes.iter().any(|v| v.validator == vote.validator) {
            self.votes.push(vote);
        }
    }

    /// Check if enough votes have been collected
    pub fn has_quorum(&self) -> bool {
        self.votes.len() >= MIN_ARBITRATORS
    }

    /// Resolve the dispute by majority vote
    pub fn resolve(&self) -> ArbitrationOutcome {
        if self.votes.len() < MIN_ARBITRATORS {
            return ArbitrationOutcome::Inconclusive;
        }

        // Count occurrences of each output hash
        let mut hash_counts: HashMap<Hash, usize> = HashMap::new();
        for vote in &self.votes {
            *hash_counts.entry(vote.output_hash).or_default() += 1;
        }

        // Find the majority hash (most common)
        let (majority_hash, majority_count) = hash_counts
            .into_iter()
            .max_by_key(|(_, count)| *count)
            .unwrap(); // safe: votes.len() >= MIN_ARBITRATORS

        // Majority requires > 50% of votes
        let total = self.votes.len();
        if majority_count * 2 <= total {
            // No clear majority — inconclusive
            return ArbitrationOutcome::Inconclusive;
        }

        if self.miner_output_hash == majority_hash {
            ArbitrationOutcome::MinerVindicated
        } else {
            ArbitrationOutcome::MinerFaulted {
                majority_hash,
                agree_count: majority_count,
                total_count: total,
            }
        }
    }
}

/// Manages all active arbitration panels
pub struct ArbitrationManager {
    /// Active panels keyed by task_id
    panels: HashMap<Hash, ArbitrationPanel>,
    /// Timeout for collecting votes (ms)
    vote_timeout_ms: u64,
}

impl ArbitrationManager {
    pub fn new() -> Self {
        Self {
            panels: HashMap::new(),
            vote_timeout_ms: 60_000, // 60 seconds to collect votes
        }
    }

    /// Open a new arbitration panel for a disputed task
    pub fn open_dispute(&mut self, task_id: Hash, miner: Address, miner_output_hash: Hash) -> bool {
        if self.panels.contains_key(&task_id) {
            return false; // already open
        }
        self.panels.insert(
            task_id,
            ArbitrationPanel::new(task_id, miner, miner_output_hash),
        );
        true
    }

    /// Submit a validator's re-execution vote
    pub fn submit_vote(&mut self, task_id: &Hash, vote: ArbitrationVote) -> bool {
        if let Some(panel) = self.panels.get_mut(task_id) {
            panel.add_vote(vote);
            true
        } else {
            false
        }
    }

    /// Try to resolve panels that have quorum, returning outcomes
    pub fn resolve_ready(&mut self) -> Vec<(Hash, ArbitrationOutcome, Address)> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let mut resolved = Vec::new();
        let mut to_remove = Vec::new();

        for (task_id, panel) in &self.panels {
            if panel.has_quorum() {
                let outcome = panel.resolve();
                resolved.push((*task_id, outcome, panel.miner));
                to_remove.push(*task_id);
            } else if now.saturating_sub(panel.created_at) > self.vote_timeout_ms {
                // Timed out without quorum — inconclusive
                resolved.push((*task_id, ArbitrationOutcome::Inconclusive, panel.miner));
                to_remove.push(*task_id);
            }
        }

        for id in to_remove {
            self.panels.remove(&id);
        }

        resolved
    }

    /// Get a panel for a task
    pub fn get_panel(&self, task_id: &Hash) -> Option<&ArbitrationPanel> {
        self.panels.get(task_id)
    }

    /// Number of active panels
    pub fn active_count(&self) -> usize {
        self.panels.len()
    }
}

impl Default for ArbitrationManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_addr(byte: u8) -> Address {
        Address::new([byte; 20])
    }

    #[test]
    fn test_should_inject_challenge_new_miner() {
        let gen = ChallengeGenerator::new();
        // Use diverse miner addresses to get better distribution
        let mut injected = 0;
        for i in 0..1000u64 {
            let mut addr_bytes = [0u8; 20];
            addr_bytes[..8].copy_from_slice(&i.to_le_bytes());
            let miner = Address::new(addr_bytes);
            if gen.should_inject_challenge(&miner, 50, 10000) {
                injected += 1;
            }
        }
        // Should be roughly 100 out of 1000 (10%), allow wide margin
        assert!(injected > 50 && injected < 200, "injected = {}", injected);
    }

    #[test]
    fn test_should_inject_challenge_standard() {
        let gen = ChallengeGenerator::new();
        let miner = test_addr(0x02);
        let mut injected = 0;
        for i in 0..1000u64 {
            if gen.should_inject_challenge(&miner, 1000 + i, 10000) {
                injected += 1;
            }
        }
        // 5% standard rate
        assert!(injected > 10 && injected < 120, "injected = {}", injected);
    }

    #[test]
    fn test_should_inject_challenge_low_rep() {
        let gen = ChallengeGenerator::new();
        let miner = test_addr(0x03);
        let mut injected = 0;
        for i in 0..1000u64 {
            if gen.should_inject_challenge(&miner, 500 + i, 5000) {
                injected += 1;
            }
        }
        // 8% for low reputation
        assert!(injected > 20 && injected < 160, "injected = {}", injected);
    }

    #[test]
    fn test_verify_challenge_exact_match() {
        let mut gen = ChallengeGenerator::new();
        let task_id = Hash::new([0x42; 32]);
        let expected_hash = Hash::new([0xAA; 32]);

        // Manually insert a challenge
        gen.active_challenges.insert(
            task_id,
            ChallengeTask {
                task: InferenceTask::new(
                    task_id,
                    1,
                    qfc_inference::task::ComputeTaskType::Embedding {
                        model_id: qfc_inference::task::ModelId::new("test", "v1"),
                        input_hash: Hash::ZERO,
                    },
                    Vec::new(),
                    0,
                    u64::MAX,
                ),
                expected_output_hash: expected_hash,
                tolerance: 0.0,
            },
        );

        let verdict = gen.verify_challenge(&task_id, &expected_hash).unwrap();
        assert_eq!(verdict, ChallengeVerdict::Passed);
    }

    #[test]
    fn test_verify_challenge_failed() {
        let mut gen = ChallengeGenerator::new();
        let task_id = Hash::new([0x42; 32]);
        let expected_hash = Hash::new([0xAA; 32]);
        let wrong_hash = Hash::new([0xBB; 32]);

        gen.active_challenges.insert(
            task_id,
            ChallengeTask {
                task: InferenceTask::new(
                    task_id,
                    1,
                    qfc_inference::task::ComputeTaskType::Embedding {
                        model_id: qfc_inference::task::ModelId::new("test", "v1"),
                        input_hash: Hash::ZERO,
                    },
                    Vec::new(),
                    0,
                    u64::MAX,
                ),
                expected_output_hash: expected_hash,
                tolerance: 0.0,
            },
        );

        let verdict = gen.verify_challenge(&task_id, &wrong_hash).unwrap();
        assert!(matches!(verdict, ChallengeVerdict::Failed { .. }));
    }

    #[test]
    fn test_record_result_escalation() {
        let mut gen = ChallengeGenerator::new();
        let miner = test_addr(0x01);
        let failed = ChallengeVerdict::Failed { similarity: 0.0 };

        // First failure: rep only
        let penalty1 = gen.record_result(&miner, &failed).unwrap();
        assert_eq!(penalty1.slash_percent, 0);
        assert_eq!(penalty1.reputation_reduction, 500);

        // Second failure: still rep only
        let penalty2 = gen.record_result(&miner, &failed).unwrap();
        assert_eq!(penalty2.slash_percent, 0);

        // Third consecutive failure: escalated
        let penalty3 = gen.record_result(&miner, &failed).unwrap();
        assert_eq!(penalty3.slash_percent, 5);
        assert!(penalty3.jail_duration_ms > 0);
    }

    #[test]
    fn test_arbitration_miner_vindicated() {
        let task_id = Hash::new([0x10; 32]);
        let miner = test_addr(0x01);
        let correct_hash = Hash::new([0xAA; 32]);

        let mut panel = ArbitrationPanel::new(task_id, miner, correct_hash);

        // 3 validators agree with miner
        for i in 0..3 {
            panel.add_vote(ArbitrationVote {
                validator: test_addr(0x10 + i),
                output_hash: correct_hash,
                execution_time_ms: 100,
            });
        }

        assert!(panel.has_quorum());
        assert_eq!(panel.resolve(), ArbitrationOutcome::MinerVindicated);
    }

    #[test]
    fn test_arbitration_miner_faulted() {
        let task_id = Hash::new([0x20; 32]);
        let miner = test_addr(0x01);
        let miner_hash = Hash::new([0xBB; 32]);
        let correct_hash = Hash::new([0xAA; 32]);

        let mut panel = ArbitrationPanel::new(task_id, miner, miner_hash);

        // 3 validators disagree with miner
        for i in 0..3 {
            panel.add_vote(ArbitrationVote {
                validator: test_addr(0x10 + i),
                output_hash: correct_hash,
                execution_time_ms: 100,
            });
        }

        assert!(panel.has_quorum());
        match panel.resolve() {
            ArbitrationOutcome::MinerFaulted {
                majority_hash,
                agree_count,
                total_count,
            } => {
                assert_eq!(majority_hash, correct_hash);
                assert_eq!(agree_count, 3);
                assert_eq!(total_count, 3);
            }
            other => panic!("Expected MinerFaulted, got {:?}", other),
        }
    }

    #[test]
    fn test_arbitration_no_quorum() {
        let task_id = Hash::new([0x30; 32]);
        let panel = ArbitrationPanel::new(task_id, test_addr(0x01), Hash::new([0xAA; 32]));
        assert!(!panel.has_quorum());
        assert_eq!(panel.resolve(), ArbitrationOutcome::Inconclusive);
    }

    #[test]
    fn test_arbitration_no_majority() {
        let task_id = Hash::new([0x40; 32]);
        let miner = test_addr(0x01);
        let miner_hash = Hash::new([0xAA; 32]);

        let mut panel = ArbitrationPanel::new(task_id, miner, miner_hash);

        // 3 validators each give different hashes — no majority
        panel.add_vote(ArbitrationVote {
            validator: test_addr(0x10),
            output_hash: Hash::new([0x01; 32]),
            execution_time_ms: 100,
        });
        panel.add_vote(ArbitrationVote {
            validator: test_addr(0x11),
            output_hash: Hash::new([0x02; 32]),
            execution_time_ms: 100,
        });
        panel.add_vote(ArbitrationVote {
            validator: test_addr(0x12),
            output_hash: Hash::new([0x03; 32]),
            execution_time_ms: 100,
        });

        assert!(panel.has_quorum());
        assert_eq!(panel.resolve(), ArbitrationOutcome::Inconclusive);
    }

    #[test]
    fn test_arbitration_duplicate_votes() {
        let task_id = Hash::new([0x50; 32]);
        let mut panel = ArbitrationPanel::new(task_id, test_addr(0x01), Hash::new([0xAA; 32]));

        let validator = test_addr(0x10);
        panel.add_vote(ArbitrationVote {
            validator,
            output_hash: Hash::new([0xAA; 32]),
            execution_time_ms: 100,
        });
        panel.add_vote(ArbitrationVote {
            validator, // duplicate
            output_hash: Hash::new([0xBB; 32]),
            execution_time_ms: 100,
        });

        assert_eq!(panel.votes.len(), 1); // duplicate rejected
    }

    #[test]
    fn test_arbitration_manager() {
        let mut mgr = ArbitrationManager::new();
        let task_id = Hash::new([0x60; 32]);
        let miner = test_addr(0x01);
        let correct_hash = Hash::new([0xAA; 32]);

        assert!(mgr.open_dispute(task_id, miner, correct_hash));
        assert!(!mgr.open_dispute(task_id, miner, correct_hash)); // duplicate
        assert_eq!(mgr.active_count(), 1);

        // Submit 3 votes
        for i in 0..3 {
            mgr.submit_vote(
                &task_id,
                ArbitrationVote {
                    validator: test_addr(0x10 + i),
                    output_hash: correct_hash,
                    execution_time_ms: 100,
                },
            );
        }

        let outcomes = mgr.resolve_ready();
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].1, ArbitrationOutcome::MinerVindicated);
        assert_eq!(mgr.active_count(), 0);
    }

    #[test]
    fn test_record_result_pass_resets_consecutive() {
        let mut gen = ChallengeGenerator::new();
        let miner = test_addr(0x01);
        let failed = ChallengeVerdict::Failed { similarity: 0.0 };
        let passed = ChallengeVerdict::Passed;

        gen.record_result(&miner, &failed);
        gen.record_result(&miner, &failed);
        gen.record_result(&miner, &passed); // resets consecutive

        // Next failure should not be escalated
        let penalty = gen.record_result(&miner, &failed).unwrap();
        assert_eq!(penalty.slash_percent, 0);
    }
}
