//! Pending task queue and assignment

use std::collections::{HashMap, VecDeque};

use qfc_inference::{GpuTier, InferenceTask};
use qfc_types::{Address, Hash};

use crate::task_types::{synthetic_task_for_tier, task_requirements};

/// Status of a publicly submitted inference task
#[derive(Clone, Debug)]
pub enum PublicTaskStatus {
    Pending,
    Assigned,
    Completed {
        result_data: Vec<u8>,
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

/// Pool of pending inference tasks to be assigned to miners
pub struct TaskPool {
    /// Pending tasks, ordered by creation time
    pending: VecDeque<InferenceTask>,
    /// Current epoch
    current_epoch: u64,
    /// Counter for generating task IDs
    task_counter: u64,
    /// Public tasks (paid inference requests)
    public_tasks: HashMap<Hash, PublicTask>,
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
            redundant_assignments: HashMap::new(),
            redundancy_count: 3,
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

    /// Fetch a task, optionally recording the miner for redundant assignment
    pub fn fetch_task_for(
        &mut self,
        tier: GpuTier,
        available_memory_mb: u64,
        miner: Option<Address>,
    ) -> Option<InferenceTask> {
        let idx = self.pending.iter().position(|task| {
            let reqs = task_requirements(&task.task_type);
            if !tier_can_run(tier, reqs.min_tier) || available_memory_mb < reqs.min_memory_mb {
                return false;
            }
            // For redundant tasks, check miner hasn't already been assigned
            if let Some(ref m) = miner {
                if let Some(assigned) = self.redundant_assignments.get(&task.task_id) {
                    if assigned.contains(m) {
                        return false;
                    }
                }
            }
            true
        })?;

        let task = self.pending[idx].clone();
        let task_id = task.task_id;

        // Check if this is a redundant task
        if let Some(assigned) = self.redundant_assignments.get_mut(&task_id) {
            if let Some(m) = miner {
                assigned.push(m);
            }
            if assigned.len() >= self.redundancy_count {
                // All slots filled — remove from queue
                self.pending.remove(idx);
            }
        } else {
            // Not a redundant task — remove immediately
            self.pending.remove(idx);
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
        // Also add to the pending queue so miners can pick it up
        self.pending.push_back(task);
        task_id
    }

    /// Get a public task by ID
    pub fn get_public_task(&self, task_id: &Hash) -> Option<&PublicTask> {
        self.public_tasks.get(task_id)
    }

    /// Mark a public task as completed
    pub fn complete_public_task(
        &mut self,
        task_id: &Hash,
        result_data: Vec<u8>,
        miner: Address,
        execution_time_ms: u64,
    ) -> bool {
        if let Some(task) = self.public_tasks.get_mut(task_id) {
            task.status = PublicTaskStatus::Completed {
                result_data,
                miner,
                execution_time_ms,
            };
            true
        } else {
            false
        }
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
    fn test_fetch_task_insufficient_memory() {
        let mut pool = TaskPool::new();
        pool.generate_synthetic_tasks(1, 42, u64::MAX);

        // Very low memory should not match any task
        let task = pool.fetch_task(GpuTier::Hot, 0);
        assert!(task.is_none());
    }
}
