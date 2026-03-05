//! Pending task queue and assignment

use std::collections::VecDeque;

use qfc_inference::{GpuTier, InferenceTask};
use qfc_types::Hash;

use crate::task_types::{synthetic_task_for_tier, task_requirements};

/// Pool of pending inference tasks to be assigned to miners
pub struct TaskPool {
    /// Pending tasks, ordered by creation time
    pending: VecDeque<InferenceTask>,
    /// Current epoch
    current_epoch: u64,
    /// Counter for generating task IDs
    task_counter: u64,
}

impl TaskPool {
    pub fn new() -> Self {
        Self {
            pending: VecDeque::new(),
            current_epoch: 0,
            task_counter: 0,
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

    /// Fetch a task suitable for a miner with the given tier and memory
    pub fn fetch_task(&mut self, tier: GpuTier, available_memory_mb: u64) -> Option<InferenceTask> {
        let idx = self.pending.iter().position(|task| {
            let reqs = task_requirements(&task.task_type);
            tier_can_run(tier, reqs.min_tier) && available_memory_mb >= reqs.min_memory_mb
        })?;

        self.pending.remove(idx)
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
