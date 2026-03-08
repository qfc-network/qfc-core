//! Pending task queue and assignment

use std::collections::{HashMap, VecDeque};

use qfc_inference::{GpuTier, InferenceTask};
use qfc_types::{Address, Hash};

use crate::task_types::{synthetic_task_for_tier, task_requirements};

/// How the result data is stored
#[derive(Clone, Debug)]
pub enum ResultStorage {
    /// Result stored inline (small results)
    Inline(Vec<u8>),
    /// Result stored on IPFS (large results)
    Ipfs {
        cid: String,
        size: usize,
        /// First 1KB preview
        preview: Vec<u8>,
    },
}

/// Status of a publicly submitted inference task
#[derive(Clone, Debug)]
pub enum PublicTaskStatus {
    Pending,
    Assigned,
    Completed {
        result: ResultStorage,
        miner: Address,
        execution_time_ms: u64,
    },
    Failed,
    Expired,
}

/// A publicly submitted inference task (paid)
#[derive(Clone, Debug)]
pub struct PublicTask {
    pub task_id: Hash,
    pub submitter: Address,
    pub inner_task: InferenceTask,
    pub max_fee: u128,
    pub status: PublicTaskStatus,
    pub submitted_at: u64,
}

/// Tracks a task that was fetched by a miner but not yet completed
#[derive(Clone, Debug)]
#[allow(dead_code)]
struct AssignedTask {
    task: InferenceTask,
    miner: Address,
    assigned_at: u64,
}

/// Default timeout for assigned tasks before reassignment (30 seconds)
const ASSIGNMENT_TIMEOUT_MS: u64 = 30_000;

/// Pool of pending inference tasks to be assigned to miners
pub struct TaskPool {
    /// Pending tasks, ordered by creation time
    pending: VecDeque<InferenceTask>,
    /// Current epoch
    current_epoch: u64,
    /// Counter for generating task IDs
    task_counter: u64,
    /// Public tasks (paid inference requests), keyed by task_id
    public_tasks: HashMap<Hash, PublicTask>,
    /// Reverse index: input_hash -> task_id (for proof-to-task matching)
    input_hash_index: HashMap<Hash, Hash>,
    /// Tasks assigned to miners but not yet completed (task_id -> assignment)
    assigned: HashMap<Hash, AssignedTask>,
    /// Redundant assignment tracking: task_id -> assigned miners
    redundant_assignments: HashMap<Hash, Vec<Address>>,
    /// How many miners to assign for redundant tasks
    redundancy_count: usize,
}

impl TaskPool {
    pub fn new() -> Self {
        Self {
            pending: VecDeque::new(),
            current_epoch: 0,
            task_counter: 0,
            public_tasks: HashMap::new(),
            input_hash_index: HashMap::new(),
            assigned: HashMap::new(),
            redundant_assignments: HashMap::new(),
            redundancy_count: 2,
        }
    }

    /// Set the current epoch
    pub fn set_epoch(&mut self, epoch: u64) {
        self.current_epoch = epoch;
    }

    /// Submit a new task to the pool
    pub fn submit_task(&mut self, task: InferenceTask) {
        self.pending.push_back(task);
    }

    /// Generate synthetic tasks for an epoch (when no real demand exists)
    pub fn generate_synthetic_tasks(&mut self, epoch: u64, epoch_seed: u64, deadline: u64) {
        self.current_epoch = epoch;

        // Generate one task per tier
        for tier in [GpuTier::Cold, GpuTier::Warm, GpuTier::Hot] {
            let task_type = synthetic_task_for_tier(tier, epoch, epoch_seed);
            let task_id = self.next_task_id(epoch);
            let task = InferenceTask::new(
                task_id,
                epoch,
                task_type,
                Vec::new(), // synthetic tasks have no input data
                now_ms(),
                deadline,
            );
            self.pending.push_back(task);
        }
    }

    /// Fetch a task suitable for a miner with the given tier and memory.
    /// For redundant tasks, the task stays in the queue until `redundancy_count` fetches.
    pub fn fetch_task(&mut self, tier: GpuTier, available_memory_mb: u64) -> Option<InferenceTask> {
        self.fetch_task_for(tier, available_memory_mb, None)
    }

    /// Fetch a task, optionally recording the miner for redundant assignment.
    /// Selects the highest-fee matching task (C3: priority by fee).
    pub fn fetch_task_for(
        &mut self,
        tier: GpuTier,
        available_memory_mb: u64,
        miner: Option<Address>,
    ) -> Option<InferenceTask> {
        // Find all matching candidates, pick the one with highest fee (C3)
        let mut best_idx: Option<usize> = None;
        let mut best_fee: u128 = 0;

        for (i, task) in self.pending.iter().enumerate() {
            let reqs = task_requirements(&task.task_type);
            if !tier_can_run(tier, reqs.min_tier) || available_memory_mb < reqs.min_memory_mb {
                continue;
            }
            // For redundant tasks, check miner hasn't already been assigned
            if let Some(ref m) = miner {
                if let Some(assigned) = self.redundant_assignments.get(&task.task_id) {
                    if assigned.contains(m) {
                        continue;
                    }
                }
            }
            // Check fee from public_tasks; synthetic tasks have fee=0
            let fee = self
                .public_tasks
                .get(&task.task_id)
                .map(|pt| pt.max_fee)
                .unwrap_or(0);
            if best_idx.is_none() || fee > best_fee {
                best_idx = Some(i);
                best_fee = fee;
            }
        }

        let idx = best_idx?;
        let task = self.pending[idx].clone();
        let task_id = task.task_id;

        // Check if this is a redundant task
        if let Some(assigned) = self.redundant_assignments.get_mut(&task_id) {
            if let Some(m) = miner {
                assigned.push(m);
            }
            if assigned.len() >= self.redundancy_count {
                self.pending.remove(idx);
            }
        } else {
            self.pending.remove(idx);
        }

        // C1: Track assignment and update PublicTask status
        let miner_addr = miner.unwrap_or(Address::ZERO);
        self.assigned.insert(
            task_id,
            AssignedTask {
                task: task.clone(),
                miner: miner_addr,
                assigned_at: now_ms(),
            },
        );
        if let Some(pt) = self.public_tasks.get_mut(&task_id) {
            pt.status = PublicTaskStatus::Assigned;
        }

        Some(task)
    }

    /// Mark a task as requiring redundant assignment
    pub fn mark_redundant(&mut self, task_id: Hash) {
        self.redundant_assignments.entry(task_id).or_default();
    }

    /// Set the redundancy count
    pub fn set_redundancy_count(&mut self, count: usize) {
        self.redundancy_count = count;
    }

    /// Number of pending tasks
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Remove expired tasks
    pub fn prune_expired(&mut self) {
        let now = now_ms();
        self.pending.retain(|t| t.deadline > now);
    }

    /// Submit a public inference task
    pub fn submit_public_task(
        &mut self,
        submitter: Address,
        task: InferenceTask,
        max_fee: u128,
    ) -> Hash {
        let task_id = task.task_id;
        let input_hash = task.task_type.input_hash();
        let now = now_ms();
        let public = PublicTask {
            task_id,
            submitter,
            inner_task: task.clone(),
            max_fee,
            status: PublicTaskStatus::Pending,
            submitted_at: now,
        };
        self.public_tasks.insert(task_id, public);
        // Index by input_hash so proofs can be matched to tasks
        self.input_hash_index.insert(input_hash, task_id);
        // Also add to the pending queue so miners can pick it up
        self.pending.push_back(task);
        task_id
    }

    /// Get a public task by task ID
    pub fn get_public_task(&self, task_id: &Hash) -> Option<&PublicTask> {
        self.public_tasks.get(task_id)
    }

    /// Get a public task by input_hash (used by settlement to match proofs to tasks)
    pub fn get_public_task_by_input_hash(&self, input_hash: &Hash) -> Option<&PublicTask> {
        self.input_hash_index
            .get(input_hash)
            .and_then(|task_id| self.public_tasks.get(task_id))
    }

    /// Mark a public task as completed by task_id
    pub fn complete_public_task(
        &mut self,
        task_id: &Hash,
        result: ResultStorage,
        miner: Address,
        execution_time_ms: u64,
    ) -> bool {
        self.assigned.remove(task_id);
        if let Some(task) = self.public_tasks.get_mut(task_id) {
            task.status = PublicTaskStatus::Completed {
                result,
                miner,
                execution_time_ms,
            };
            true
        } else {
            false
        }
    }

    /// Mark a public task as completed by input_hash (used by settlement)
    pub fn complete_public_task_by_input_hash(
        &mut self,
        input_hash: &Hash,
        result: ResultStorage,
        miner: Address,
        execution_time_ms: u64,
    ) -> bool {
        if let Some(task_id) = self.input_hash_index.get(input_hash).copied() {
            self.complete_public_task(&task_id, result, miner, execution_time_ms)
        } else {
            false
        }
    }

    /// C2: Re-queue tasks that were assigned but not completed within timeout.
    /// Returns the number of tasks reassigned.
    pub fn reassign_stale_tasks(&mut self) -> usize {
        let now = now_ms();
        let stale_ids: Vec<Hash> = self
            .assigned
            .iter()
            .filter(|(_, a)| now.saturating_sub(a.assigned_at) > ASSIGNMENT_TIMEOUT_MS)
            .map(|(id, _)| *id)
            .collect();

        let mut count = 0;
        for task_id in stale_ids {
            if let Some(assignment) = self.assigned.remove(&task_id) {
                // Only re-queue if the task hasn't expired and isn't completed
                if assignment.task.deadline > now {
                    if let Some(pt) = self.public_tasks.get_mut(&task_id) {
                        if matches!(pt.status, PublicTaskStatus::Assigned) {
                            pt.status = PublicTaskStatus::Pending;
                        }
                    }
                    self.pending.push_back(assignment.task);
                    count += 1;
                }
            }
        }
        count
    }

    /// Prune expired public tasks and return them (for refund)
    /// A task is expired if now > submitted_at + 60_000ms (deadline)
    pub fn prune_expired_public(&mut self, now: u64) -> Vec<PublicTask> {
        let mut expired = Vec::new();
        let expired_ids: Vec<Hash> = self
            .public_tasks
            .iter()
            .filter(|(_, t)| {
                matches!(
                    t.status,
                    PublicTaskStatus::Pending | PublicTaskStatus::Assigned
                ) && now > t.inner_task.deadline
            })
            .map(|(id, _)| *id)
            .collect();

        for id in expired_ids {
            if let Some(mut task) = self.public_tasks.remove(&id) {
                // Clean up indices
                let input_hash = task.inner_task.task_type.input_hash();
                self.input_hash_index.remove(&input_hash);
                self.assigned.remove(&id);
                task.status = PublicTaskStatus::Expired;
                expired.push(task);
            }
        }
        expired
    }

    /// Generate a unique task ID
    fn next_task_id(&mut self, epoch: u64) -> Hash {
        self.task_counter += 1;
        let mut data = Vec::with_capacity(16);
        data.extend_from_slice(&epoch.to_le_bytes());
        data.extend_from_slice(&self.task_counter.to_le_bytes());
        qfc_crypto::blake3_hash(&data)
    }
}

impl Default for TaskPool {
    fn default() -> Self {
        Self::new()
    }
}

/// Check if a node's tier can run a task requiring min_tier
fn tier_can_run(node_tier: GpuTier, min_tier: GpuTier) -> bool {
    match (node_tier, min_tier) {
        (GpuTier::Hot, _) => true,
        (GpuTier::Warm, GpuTier::Hot) => false,
        (GpuTier::Warm, _) => true,
        (GpuTier::Cold, GpuTier::Cold) => true,
        (GpuTier::Cold, _) => false,
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_task_pool_basic() {
        let mut pool = TaskPool::new();
        assert_eq!(pool.pending_count(), 0);

        pool.generate_synthetic_tasks(1, 42, u64::MAX);
        assert_eq!(pool.pending_count(), 3); // one per tier
    }

    #[test]
    fn test_fetch_task_by_tier() {
        let mut pool = TaskPool::new();
        pool.generate_synthetic_tasks(1, 42, u64::MAX);

        // Cold tier should only get cold tasks
        let cold_task = pool.fetch_task(GpuTier::Cold, 10_000);
        assert!(cold_task.is_some());
        assert_eq!(pool.pending_count(), 2);

        // Hot tier should be able to get any remaining task
        let hot_task = pool.fetch_task(GpuTier::Hot, 100_000);
        assert!(hot_task.is_some());
    }

    #[test]
    fn test_public_task_input_hash_index() {
        let mut pool = TaskPool::new();
        let input_hash = qfc_crypto::blake3_hash(b"test input data");
        let task_type = qfc_inference::task::ComputeTaskType::Embedding {
            model_id: qfc_inference::task::ModelId::new("test", "v1"),
            input_hash,
        };
        let task_id = qfc_crypto::blake3_hash(b"task1");
        let task = InferenceTask::new(task_id, 1, task_type, vec![], now_ms(), u64::MAX);
        let submitter = Address::ZERO;

        let returned_id = pool.submit_public_task(submitter, task, 1000);
        assert_eq!(returned_id, task_id);

        // Should be findable by input_hash
        let found = pool.get_public_task_by_input_hash(&input_hash);
        assert!(found.is_some());
        assert_eq!(found.unwrap().task_id, task_id);

        // Complete by input_hash
        assert!(pool.complete_public_task_by_input_hash(
            &input_hash,
            ResultStorage::Inline(vec![1, 2, 3]),
            Address::ZERO,
            100,
        ));
    }

    #[test]
    fn test_fetch_highest_fee_first() {
        let mut pool = TaskPool::new();

        // Submit two public tasks with different fees
        let mut make_task = |seed: &[u8], fee: u128| {
            let input_hash = qfc_crypto::blake3_hash(seed);
            let task_type = qfc_inference::task::ComputeTaskType::Embedding {
                model_id: qfc_inference::task::ModelId::new("test", "v1"),
                input_hash,
            };
            let task_id = qfc_crypto::blake3_hash(&[seed, b"id"].concat());
            let task = InferenceTask::new(task_id, 1, task_type, vec![], now_ms(), u64::MAX);
            pool.submit_public_task(Address::ZERO, task, fee);
            task_id
        };

        let low_fee_id = make_task(b"low", 100);
        let high_fee_id = make_task(b"high", 10_000);
        assert_eq!(pool.pending_count(), 2);

        // Fetch should return high-fee task first
        let fetched = pool.fetch_task(GpuTier::Hot, 100_000).unwrap();
        assert_eq!(fetched.task_id, high_fee_id);

        let fetched2 = pool.fetch_task(GpuTier::Hot, 100_000).unwrap();
        assert_eq!(fetched2.task_id, low_fee_id);
    }

    #[test]
    fn test_assignment_tracking() {
        let mut pool = TaskPool::new();
        let input_hash = qfc_crypto::blake3_hash(b"data");
        let task_type = qfc_inference::task::ComputeTaskType::Embedding {
            model_id: qfc_inference::task::ModelId::new("test", "v1"),
            input_hash,
        };
        let task_id = qfc_crypto::blake3_hash(b"task-assign");
        let task = InferenceTask::new(task_id, 1, task_type, vec![], now_ms(), u64::MAX);
        let miner = Address::new([1; 20]);

        pool.submit_public_task(Address::ZERO, task, 500);

        // Fetch with miner identity
        let fetched = pool.fetch_task_for(GpuTier::Hot, 100_000, Some(miner));
        assert!(fetched.is_some());

        // PublicTask should be Assigned
        let pt = pool.get_public_task(&task_id).unwrap();
        assert!(matches!(pt.status, PublicTaskStatus::Assigned));

        // Assignment should be tracked
        assert!(pool.assigned.contains_key(&task_id));
    }

    #[test]
    fn test_reassign_stale_tasks() {
        let mut pool = TaskPool::new();
        let input_hash = qfc_crypto::blake3_hash(b"stale");
        let task_type = qfc_inference::task::ComputeTaskType::Embedding {
            model_id: qfc_inference::task::ModelId::new("test", "v1"),
            input_hash,
        };
        let task_id = qfc_crypto::blake3_hash(b"task-stale");
        let task = InferenceTask::new(task_id, 1, task_type, vec![], now_ms(), u64::MAX);

        pool.submit_public_task(Address::ZERO, task, 500);
        assert_eq!(pool.pending_count(), 1);

        // Fetch the task
        let _ = pool.fetch_task_for(GpuTier::Hot, 100_000, Some(Address::new([1; 20])));
        assert_eq!(pool.pending_count(), 0);

        // Not stale yet — should reassign nothing
        assert_eq!(pool.reassign_stale_tasks(), 0);

        // Simulate staleness by backdating the assignment
        if let Some(a) = pool.assigned.get_mut(&task_id) {
            a.assigned_at = now_ms().saturating_sub(ASSIGNMENT_TIMEOUT_MS + 1000);
        }

        // Now should reassign
        assert_eq!(pool.reassign_stale_tasks(), 1);
        assert_eq!(pool.pending_count(), 1);

        // PublicTask should be back to Pending
        let pt = pool.get_public_task(&task_id).unwrap();
        assert!(matches!(pt.status, PublicTaskStatus::Pending));
    }

    #[test]
    fn test_fetch_task_insufficient_memory() {
        let mut pool = TaskPool::new();
        pool.generate_synthetic_tasks(1, 42, u64::MAX);

        // Very low memory should not match any task
        let task = pool.fetch_task(GpuTier::Hot, 0);
        assert!(task.is_none());
    }
}
