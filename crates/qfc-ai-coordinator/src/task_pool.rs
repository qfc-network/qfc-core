//! Pending task queue and assignment

use std::collections::{HashMap, VecDeque};

use borsh::BorshDeserialize;
use qfc_inference::{GpuTier, InferenceTask};
use qfc_types::{Address, Hash};

use crate::cost::{CostMeter, CostReport, LoggingTreasuryHook, TreasuryHook};
use crate::quota::{QuotaConfig, QuotaEnforcer, QuotaError, PRIORITY_TIERS, REJECT_REASONS};
use crate::task_types::{synthetic_task_for_tier, task_requirements};

/// How the result data is stored
#[derive(Clone, Debug, borsh::BorshSerialize, borsh::BorshDeserialize)]
pub enum ResultStorage {
    /// Result stored inline (small results)
    Inline(Vec<u8>),
    /// Result stored on IPFS (large results)
    Ipfs {
        cid: String,
        size: u64,
        /// First 1KB preview
        preview: Vec<u8>,
    },
}

/// Status of a publicly submitted inference task
#[derive(Clone, Debug, borsh::BorshSerialize, borsh::BorshDeserialize)]
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

/// Filter for listing public tasks
#[derive(Clone, Debug, Default)]
pub struct PublicTaskFilter {
    /// Filter by submitter address
    pub submitter: Option<Address>,
    /// Filter by status name (e.g. "Pending", "Completed")
    pub status: Option<String>,
    /// Max number of results (default 50, max 200)
    pub limit: usize,
    /// Offset for pagination
    pub offset: usize,
}

/// A publicly submitted inference task (paid)
#[derive(Clone, Debug, borsh::BorshSerialize, borsh::BorshDeserialize)]
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

/// How long completed/expired/failed tasks are retained for querying (1 hour)
const COMPLETED_RETENTION_MS: u64 = 3_600_000;

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
    /// Completed/expired/failed tasks retained for querying, with expiry timestamp (ms)
    completed_tasks: HashMap<Hash, (PublicTask, u64)>,
    /// T5: per-tenant admission control (quotas off until configured)
    quota: QuotaEnforcer,
    /// T5: FLOPs/fee cost attribution per tenant and per miner
    cost: CostMeter,
    /// T5: treasury integration hook (default: structured logging no-op)
    treasury_hook: Box<dyn TreasuryHook>,
    /// T5: round-robin cursor for fair scheduling across tenants
    rr_last_tenant: Option<Address>,
    /// T5 metrics: public-task submissions per priority tier
    submitted_by_tier: [u64; PRIORITY_TIERS],
    /// T5 metrics: quota rejections per reason (indexed like REJECT_REASONS)
    rejected_by_reason: [u64; REJECT_REASONS.len()],
    /// T5 metrics: estimated_flops metered on completion, per priority tier
    flops_metered_by_tier: [u128; PRIORITY_TIERS],
}

/// T5: bounded-cardinality snapshot of quota/cost activity for the
/// Prometheus exporter. Labeled by priority **tier**, never by tenant
/// address (tenant cardinality is unbounded); per-tenant detail lives in
/// the cost report (structured log + `last_cost_report`).
#[derive(Clone, Debug)]
pub struct AiQuotaMetrics {
    pub quotas_enabled: bool,
    pub pending_tasks: usize,
    pub submitted_by_tier: [u64; PRIORITY_TIERS],
    pub rejected_by_reason: [(&'static str, u64); REJECT_REASONS.len()],
    pub flops_metered_by_tier: [u128; PRIORITY_TIERS],
    pub inflight_by_tier: [u64; PRIORITY_TIERS],
    /// Unix ms of the last cost report (0 = never since start).
    pub cost_report_unix_ms: u64,
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
            completed_tasks: HashMap::new(),
            quota: QuotaEnforcer::new(),
            cost: CostMeter::new(),
            treasury_hook: Box::new(LoggingTreasuryHook),
            rr_last_tenant: None,
            submitted_by_tier: [0; PRIORITY_TIERS],
            rejected_by_reason: [0; REJECT_REASONS.len()],
            flops_metered_by_tier: [0; PRIORITY_TIERS],
        }
    }

    /// T5: install (or clear) the per-tenant quota configuration.
    pub fn set_quota_config(&mut self, config: Option<QuotaConfig>) {
        self.quota.set_config(config);
    }

    /// T5: whether quota enforcement is active.
    pub fn quotas_enabled(&self) -> bool {
        self.quota.enabled()
    }

    /// T5: replace the treasury hook (default is [`LoggingTreasuryHook`]).
    pub fn set_treasury_hook(&mut self, hook: Box<dyn TreasuryHook>) {
        self.treasury_hook = hook;
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
    ///
    /// T5 fair scheduling, in order:
    /// 1. **Priority tier first** — only candidates from the highest
    ///    priority tier present are considered (tier 2 before 1 before 0).
    /// 2. **Round-robin across tenants** within that tier — tenants are
    ///    ordered by address and served cyclically from a single cursor, so
    ///    no tenant is served twice before every other tenant with matching
    ///    pending work is served once (starvation-free).
    /// 3. **Highest fee within a tenant** (pre-T5 C3 behavior, preserved).
    ///
    /// Synthetic tasks have no submitter and are attributed to
    /// `Address::ZERO` at the default tier's priority.
    pub fn fetch_task_for(
        &mut self,
        tier: GpuTier,
        available_memory_mb: u64,
        miner: Option<Address>,
    ) -> Option<InferenceTask> {
        // Collect matching candidates: (index, tenant, priority, fee).
        let mut candidates: Vec<(usize, Address, u8, u128)> = Vec::new();
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
            // Tenant + fee from public_tasks; synthetic tasks are the
            // zero-address tenant with fee 0.
            let (tenant, fee) = self
                .public_tasks
                .get(&task.task_id)
                .map(|pt| (pt.submitter, pt.max_fee))
                .unwrap_or((Address::ZERO, 0));
            let priority = self.quota.priority_for(&tenant);
            candidates.push((i, tenant, priority, fee));
        }
        if candidates.is_empty() {
            return None;
        }

        // 1. Highest priority tier only.
        let max_priority = candidates.iter().map(|c| c.2).max().unwrap_or(0);
        candidates.retain(|c| c.2 == max_priority);

        // 2. Round-robin across tenants: pick the smallest tenant address
        //    strictly greater than the cursor, wrapping to the smallest.
        let mut tenants: Vec<Address> = candidates.iter().map(|c| c.1).collect();
        tenants.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        tenants.dedup();
        let chosen_tenant = match self.rr_last_tenant {
            Some(last) => tenants
                .iter()
                .find(|t| t.as_bytes() > last.as_bytes())
                .copied()
                .unwrap_or(tenants[0]),
            None => tenants[0],
        };
        self.rr_last_tenant = Some(chosen_tenant);

        // 3. Highest fee within the chosen tenant.
        let idx = candidates
            .iter()
            .filter(|c| c.1 == chosen_tenant)
            .max_by_key(|c| c.3)
            .map(|c| c.0)?;
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

    /// T5: submit a public inference task with per-tenant quota admission.
    ///
    /// Checks (in order): pool-pressure shedding (lowest priority first),
    /// QPS token bucket, in-flight limit, FLOPs budget (charged with the
    /// task's `estimated_flops`). With no quota config installed every
    /// submission is admitted. On rejection the pool is unchanged and the
    /// typed [`QuotaError`] carries the violated limit + retry-after hint.
    pub fn try_submit_public_task(
        &mut self,
        submitter: Address,
        task: InferenceTask,
        max_fee: u128,
        now_ms: u64,
    ) -> Result<Hash, QuotaError> {
        let estimated_flops = task_requirements(&task.task_type).estimated_flops;
        if let Err(err) = self
            .quota
            .admit(&submitter, estimated_flops, self.pending.len(), now_ms)
        {
            if let Some(i) = REJECT_REASONS.iter().position(|r| *r == err.reason()) {
                self.rejected_by_reason[i] += 1;
            }
            return Err(err);
        }
        Ok(self.submit_public_task(submitter, task, max_fee))
    }

    /// Submit a public inference task (no quota admission — prefer
    /// [`TaskPool::try_submit_public_task`] on externally driven paths).
    pub fn submit_public_task(
        &mut self,
        submitter: Address,
        task: InferenceTask,
        max_fee: u128,
    ) -> Hash {
        let task_id = task.task_id;
        let input_hash = task.task_type.input_hash();
        let now = now_ms();
        // T5: in-flight + submission accounting.
        self.quota.task_started(&submitter, now);
        let tier = self.quota.priority_for(&submitter) as usize;
        self.submitted_by_tier[tier] += 1;
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

    /// List public tasks with optional filtering and pagination
    pub fn list_public_tasks(&self, filter: &PublicTaskFilter) -> Vec<&PublicTask> {
        let limit = filter.limit.min(200).max(1);
        let mut tasks: Vec<&PublicTask> = self
            .public_tasks
            .values()
            .filter(|t| {
                if let Some(ref submitter) = filter.submitter {
                    if &t.submitter != submitter {
                        return false;
                    }
                }
                if let Some(ref status) = filter.status {
                    let task_status = match &t.status {
                        PublicTaskStatus::Pending => "Pending",
                        PublicTaskStatus::Assigned => "Assigned",
                        PublicTaskStatus::Completed { .. } => "Completed",
                        PublicTaskStatus::Failed => "Failed",
                        PublicTaskStatus::Expired => "Expired",
                    };
                    if task_status != status.as_str() {
                        return false;
                    }
                }
                true
            })
            .collect();
        // Sort by submission time descending (newest first)
        tasks.sort_by(|a, b| b.submitted_at.cmp(&a.submitted_at));
        tasks.into_iter().skip(filter.offset).take(limit).collect()
    }

    /// Get a public task by task ID (checks active tasks, then retained completed tasks)
    pub fn get_public_task(&self, task_id: &Hash) -> Option<&PublicTask> {
        self.public_tasks
            .get(task_id)
            .or_else(|| self.completed_tasks.get(task_id).map(|(t, _)| t))
    }

    /// Get a public task by input_hash (used by settlement to match proofs to tasks)
    pub fn get_public_task_by_input_hash(&self, input_hash: &Hash) -> Option<&PublicTask> {
        self.input_hash_index
            .get(input_hash)
            .and_then(|task_id| self.get_public_task(task_id))
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
        if let Some(mut task) = self.public_tasks.remove(task_id) {
            // T5: meter the completed task — estimated_flops attributed to
            // both the tenant (submitter) and the executing miner — and
            // notify the treasury hook.
            let flops = task_requirements(&task.inner_task.task_type).estimated_flops;
            let tier = self.quota.priority_for(&task.submitter) as usize;
            self.quota.task_finished(&task.submitter);
            self.cost
                .record(&task.submitter, &miner, flops, task.max_fee);
            self.flops_metered_by_tier[tier] =
                self.flops_metered_by_tier[tier].saturating_add(flops as u128);
            self.treasury_hook
                .on_task_charged(&task.submitter, &miner, flops, task.max_fee);

            task.status = PublicTaskStatus::Completed {
                result,
                miner,
                execution_time_ms,
            };
            let retain_until = now_ms() + COMPLETED_RETENTION_MS;
            self.completed_tasks.insert(*task_id, (task, retain_until));
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
                // T5: expired tasks leave the tenant's in-flight count
                // (no cost metering — the task was never executed).
                self.quota.task_finished(&task.submitter);
                task.status = PublicTaskStatus::Expired;
                // Retain for querying before returning for refund
                let retain_until = now + COMPLETED_RETENTION_MS;
                self.completed_tasks
                    .insert(id, (task.clone(), retain_until));
                expired.push(task);
            }
        }
        expired
    }

    /// Remove entries from completed_tasks whose retention period has elapsed.
    /// Returns the pruned tasks so the caller can persist them to storage if needed.
    pub fn prune_retained_completed(&mut self) -> Vec<PublicTask> {
        let now = now_ms();
        let expired_ids: Vec<Hash> = self
            .completed_tasks
            .iter()
            .filter(|(_, (_, expiry))| now >= *expiry)
            .map(|(id, _)| *id)
            .collect();

        let mut pruned = Vec::new();
        for id in expired_ids {
            if let Some((task, _)) = self.completed_tasks.remove(&id) {
                pruned.push(task);
            }
        }
        pruned
    }

    /// T5: generate a periodic cost report (per-tenant + per-miner FLOPs and
    /// fees over the interval since the previous report), notify the
    /// treasury hook, and retain it for querying.
    pub fn generate_cost_report(&mut self, now_ms: u64) -> CostReport {
        let report = self.cost.generate_report(now_ms);
        self.treasury_hook.on_cost_report(&report);
        report
    }

    /// T5: the most recent cost report (queryable accessor for RPC/exporter).
    pub fn last_cost_report(&self) -> Option<&CostReport> {
        self.cost.last_report()
    }

    /// T5: read access to the cost meter (cumulative per-tenant/per-miner
    /// totals).
    pub fn cost_meter(&self) -> &CostMeter {
        &self.cost
    }

    /// T5: bounded-cardinality metrics snapshot for the Prometheus exporter.
    pub fn quota_metrics(&self) -> AiQuotaMetrics {
        let mut rejected = [("", 0u64); REJECT_REASONS.len()];
        for (i, reason) in REJECT_REASONS.iter().enumerate() {
            rejected[i] = (*reason, self.rejected_by_reason[i]);
        }
        AiQuotaMetrics {
            quotas_enabled: self.quota.enabled(),
            pending_tasks: self.pending.len(),
            submitted_by_tier: self.submitted_by_tier,
            rejected_by_reason: rejected,
            flops_metered_by_tier: self.flops_metered_by_tier,
            inflight_by_tier: self.quota.inflight_by_priority(),
            cost_report_unix_ms: self.cost.last_report_unix_ms(),
        }
    }

    /// Serialize a PublicTask to bytes for storage
    pub fn serialize_task(task: &PublicTask) -> Option<Vec<u8>> {
        borsh::to_vec(task).ok()
    }

    /// Deserialize a PublicTask from bytes
    pub fn deserialize_task(bytes: &[u8]) -> Option<PublicTask> {
        PublicTask::try_from_slice(bytes).ok()
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

    #[test]
    fn test_serialize_deserialize_pending_task() {
        let task = PublicTask {
            task_id: qfc_crypto::blake3_hash(b"task-ser-1"),
            submitter: Address::new([0xAB; 20]),
            inner_task: InferenceTask::new(
                qfc_crypto::blake3_hash(b"task-ser-1"),
                5,
                qfc_inference::task::ComputeTaskType::Embedding {
                    model_id: qfc_inference::task::ModelId::new("bert", "v1"),
                    input_hash: qfc_crypto::blake3_hash(b"input"),
                },
                vec![1, 2, 3],
                1000,
                u64::MAX,
            ),
            max_fee: 5000,
            status: PublicTaskStatus::Pending,
            submitted_at: 1000,
        };

        let bytes = TaskPool::serialize_task(&task).expect("serialize");
        let restored = TaskPool::deserialize_task(&bytes).expect("deserialize");

        assert_eq!(restored.task_id, task.task_id);
        assert_eq!(restored.submitter, task.submitter);
        assert_eq!(restored.max_fee, task.max_fee);
        assert_eq!(restored.submitted_at, task.submitted_at);
        assert!(matches!(restored.status, PublicTaskStatus::Pending));
    }

    #[test]
    fn test_serialize_deserialize_completed_inline() {
        let task = PublicTask {
            task_id: qfc_crypto::blake3_hash(b"task-ser-2"),
            submitter: Address::new([0x01; 20]),
            inner_task: InferenceTask::new(
                qfc_crypto::blake3_hash(b"task-ser-2"),
                3,
                qfc_inference::task::ComputeTaskType::Embedding {
                    model_id: qfc_inference::task::ModelId::new("bert", "v1"),
                    input_hash: qfc_crypto::blake3_hash(b"data2"),
                },
                vec![],
                500,
                u64::MAX,
            ),
            max_fee: 1000,
            status: PublicTaskStatus::Completed {
                result: ResultStorage::Inline(vec![10, 20, 30, 40]),
                miner: Address::new([0xCC; 20]),
                execution_time_ms: 250,
            },
            submitted_at: 500,
        };

        let bytes = TaskPool::serialize_task(&task).expect("serialize");
        let restored = TaskPool::deserialize_task(&bytes).expect("deserialize");

        match &restored.status {
            PublicTaskStatus::Completed {
                result,
                miner,
                execution_time_ms,
            } => {
                match result {
                    ResultStorage::Inline(data) => assert_eq!(data, &[10, 20, 30, 40]),
                    _ => panic!("expected Inline result"),
                }
                assert_eq!(*miner, Address::new([0xCC; 20]));
                assert_eq!(*execution_time_ms, 250);
            }
            _ => panic!("expected Completed status"),
        }
    }

    #[test]
    fn test_serialize_deserialize_completed_ipfs() {
        let task = PublicTask {
            task_id: qfc_crypto::blake3_hash(b"task-ser-3"),
            submitter: Address::ZERO,
            inner_task: InferenceTask::new(
                qfc_crypto::blake3_hash(b"task-ser-3"),
                1,
                qfc_inference::task::ComputeTaskType::Embedding {
                    model_id: qfc_inference::task::ModelId::new("llama", "v2"),
                    input_hash: qfc_crypto::blake3_hash(b"large-input"),
                },
                vec![],
                100,
                u64::MAX,
            ),
            max_fee: 50000,
            status: PublicTaskStatus::Completed {
                result: ResultStorage::Ipfs {
                    cid: "QmTestCid123456".to_string(),
                    size: 1_048_576,
                    preview: vec![0xFF; 128],
                },
                miner: Address::new([0x42; 20]),
                execution_time_ms: 5000,
            },
            submitted_at: 100,
        };

        let bytes = TaskPool::serialize_task(&task).expect("serialize");
        let restored = TaskPool::deserialize_task(&bytes).expect("deserialize");

        match &restored.status {
            PublicTaskStatus::Completed { result, .. } => match result {
                ResultStorage::Ipfs { cid, size, preview } => {
                    assert_eq!(cid, "QmTestCid123456");
                    assert_eq!(*size, 1_048_576);
                    assert_eq!(preview.len(), 128);
                }
                _ => panic!("expected Ipfs result"),
            },
            _ => panic!("expected Completed status"),
        }
    }

    #[test]
    fn test_serialize_deserialize_expired() {
        let task = PublicTask {
            task_id: qfc_crypto::blake3_hash(b"task-ser-4"),
            submitter: Address::new([0x55; 20]),
            inner_task: InferenceTask::new(
                qfc_crypto::blake3_hash(b"task-ser-4"),
                2,
                qfc_inference::task::ComputeTaskType::Embedding {
                    model_id: qfc_inference::task::ModelId::new("bert", "v1"),
                    input_hash: qfc_crypto::blake3_hash(b"expired-input"),
                },
                vec![],
                100,
                200, // short deadline
            ),
            max_fee: 100,
            status: PublicTaskStatus::Expired,
            submitted_at: 100,
        };

        let bytes = TaskPool::serialize_task(&task).expect("serialize");
        let restored = TaskPool::deserialize_task(&bytes).expect("deserialize");

        assert!(matches!(restored.status, PublicTaskStatus::Expired));
        assert_eq!(restored.task_id, task.task_id);
        assert_eq!(restored.submitter, Address::new([0x55; 20]));
    }

    #[test]
    fn test_serialize_deserialize_failed() {
        let task = PublicTask {
            task_id: qfc_crypto::blake3_hash(b"task-ser-5"),
            submitter: Address::ZERO,
            inner_task: InferenceTask::new(
                qfc_crypto::blake3_hash(b"task-ser-5"),
                1,
                qfc_inference::task::ComputeTaskType::Embedding {
                    model_id: qfc_inference::task::ModelId::new("bert", "v1"),
                    input_hash: qfc_crypto::blake3_hash(b"failed-input"),
                },
                vec![],
                100,
                u64::MAX,
            ),
            max_fee: 200,
            status: PublicTaskStatus::Failed,
            submitted_at: 100,
        };

        let bytes = TaskPool::serialize_task(&task).expect("serialize");
        let restored = TaskPool::deserialize_task(&bytes).expect("deserialize");
        assert!(matches!(restored.status, PublicTaskStatus::Failed));
    }

    #[test]
    fn test_prune_retained_completed_returns_tasks() {
        let mut pool = TaskPool::new();
        let input_hash = qfc_crypto::blake3_hash(b"prune-test");
        let task_type = qfc_inference::task::ComputeTaskType::Embedding {
            model_id: qfc_inference::task::ModelId::new("bert", "v1"),
            input_hash,
        };
        let task_id = qfc_crypto::blake3_hash(b"task-prune");
        let task = InferenceTask::new(task_id, 1, task_type, vec![], now_ms(), u64::MAX);

        pool.submit_public_task(Address::ZERO, task, 500);
        pool.complete_public_task(
            &task_id,
            ResultStorage::Inline(vec![1, 2, 3]),
            Address::new([1; 20]),
            100,
        );

        // Task is in completed_tasks but retention hasn't expired yet
        let pruned = pool.prune_retained_completed();
        assert!(pruned.is_empty());
        assert!(pool.get_public_task(&task_id).is_some());

        // Manually expire the retention by backdating
        if let Some((_, expiry)) = pool.completed_tasks.get_mut(&task_id) {
            *expiry = 0; // already expired
        }

        // Now prune should return the task
        let pruned = pool.prune_retained_completed();
        assert_eq!(pruned.len(), 1);
        assert_eq!(pruned[0].task_id, task_id);

        // Task should no longer be in memory
        assert!(pool.get_public_task(&task_id).is_none());
    }

    // ---------- T5: quotas, fairness, metering, treasury hook ----------

    fn make_public_task(pool: &mut TaskPool, submitter: Address, seed: &[u8], fee: u128) -> Hash {
        let input_hash = qfc_crypto::blake3_hash(seed);
        let task_type = qfc_inference::task::ComputeTaskType::Embedding {
            model_id: qfc_inference::task::ModelId::new("test", "v1"),
            input_hash,
        };
        let task_id = qfc_crypto::blake3_hash(&[seed, b"id"].concat());
        let task = InferenceTask::new(task_id, 1, task_type, vec![], now_ms(), u64::MAX);
        pool.submit_public_task(submitter, task, fee);
        task_id
    }

    fn try_submit(
        pool: &mut TaskPool,
        submitter: Address,
        seed: &[u8],
        now: u64,
    ) -> Result<Hash, crate::quota::QuotaError> {
        let input_hash = qfc_crypto::blake3_hash(seed);
        let task_type = qfc_inference::task::ComputeTaskType::Embedding {
            model_id: qfc_inference::task::ModelId::new("test", "v1"),
            input_hash,
        };
        let task_id = qfc_crypto::blake3_hash(&[seed, b"id"].concat());
        let task = InferenceTask::new(task_id, 1, task_type, vec![], now, u64::MAX);
        pool.try_submit_public_task(submitter, task, 1_000, now)
    }

    fn quota_cfg(json: &str) -> crate::quota::QuotaConfig {
        crate::quota::QuotaConfig::from_json_str(json).unwrap()
    }

    #[test]
    fn test_try_submit_quota_rejection_and_counters() {
        let mut pool = TaskPool::new();
        pool.set_quota_config(Some(quota_cfg(
            r#"{ "default_tier": { "max_qps": 1.0, "burst": 1 } }"#,
        )));

        assert!(try_submit(&mut pool, Address::new([1; 20]), b"a", 0).is_ok());
        let err = try_submit(&mut pool, Address::new([1; 20]), b"b", 0).unwrap_err();
        assert_eq!(err.reason(), "qps");
        assert!(err.retry_after_ms() > 0);

        let m = pool.quota_metrics();
        assert!(m.quotas_enabled);
        assert_eq!(m.pending_tasks, 1);
        assert_eq!(m.submitted_by_tier[1], 1); // default tier priority = 1
        assert_eq!(
            m.rejected_by_reason
                .iter()
                .find(|(r, _)| *r == "qps")
                .unwrap()
                .1,
            1
        );
    }

    #[test]
    fn test_quotas_off_admits_everything() {
        let mut pool = TaskPool::new();
        for i in 0..50u64 {
            assert!(try_submit(&mut pool, Address::new([1; 20]), &i.to_le_bytes(), 0).is_ok());
        }
        assert!(!pool.quotas_enabled());
        assert_eq!(pool.quota_metrics().submitted_by_tier[1], 50);
    }

    #[test]
    fn test_fair_scheduling_two_tenants_starvation_free() {
        let mut pool = TaskPool::new();
        let (a, b) = (Address::new([0xAA; 20]), Address::new([0xBB; 20]));

        // Tenant A floods the pool with high-fee tasks; B submits cheap ones.
        for i in 0..4u8 {
            make_public_task(&mut pool, a, &[b'a', i], 1_000_000);
        }
        for i in 0..4u8 {
            make_public_task(&mut pool, b, &[b'b', i], 1);
        }

        // Round-robin must alternate tenants despite the fee gap.
        let mut order = Vec::new();
        for _ in 0..8 {
            let task = pool.fetch_task(GpuTier::Hot, 100_000).unwrap();
            let tenant = pool.get_public_task(&task.task_id).unwrap().submitter;
            order.push(tenant);
        }
        let a_count_first_half = order[..4].iter().filter(|t| **t == a).count();
        let b_count_first_half = order[..4].iter().filter(|t| **t == b).count();
        assert_eq!(
            a_count_first_half, 2,
            "tenant A must not starve B: {order:?}"
        );
        assert_eq!(
            b_count_first_half, 2,
            "tenant B must be served fairly: {order:?}"
        );
        // No two consecutive fetches serve the same tenant while both have work.
        for w in order[..7].windows(2) {
            assert_ne!(w[0], w[1], "round-robin must alternate: {order:?}");
        }
    }

    #[test]
    fn test_priority_tier_served_before_lower() {
        let mut pool = TaskPool::new();
        let (lo, hi) = (Address::new([0x01; 20]), Address::new([0x02; 20]));
        pool.set_quota_config(Some(quota_cfg(&format!(
            r#"{{
                "default_tier": {{ "max_qps": 0.0 }},
                "tenants": {{
                    "0x{}": {{ "max_qps": 0.0, "priority": 0 }},
                    "0x{}": {{ "max_qps": 0.0, "priority": 2 }}
                }}
            }}"#,
            hex::encode(lo.as_bytes()),
            hex::encode(hi.as_bytes()),
        ))));

        // Low-priority task submitted first and with a far higher fee.
        make_public_task(&mut pool, lo, b"low-prio", 1_000_000);
        make_public_task(&mut pool, hi, b"high-prio", 1);

        let first = pool.fetch_task(GpuTier::Hot, 100_000).unwrap();
        assert_eq!(
            pool.get_public_task(&first.task_id).unwrap().submitter,
            hi,
            "highest priority tier must be served first"
        );
        let second = pool.fetch_task(GpuTier::Hot, 100_000).unwrap();
        assert_eq!(pool.get_public_task(&second.task_id).unwrap().submitter, lo);
    }

    #[test]
    fn test_degradation_order_under_pool_pressure() {
        let mut pool = TaskPool::new();
        let (t0, t1, t2) = (
            Address::new([0x10; 20]),
            Address::new([0x11; 20]),
            Address::new([0x12; 20]),
        );
        pool.set_quota_config(Some(quota_cfg(&format!(
            r#"{{
                "max_pending": 8,
                "default_tier": {{ "max_qps": 0.0 }},
                "tenants": {{
                    "0x{}": {{ "max_qps": 0.0, "priority": 0 }},
                    "0x{}": {{ "max_qps": 0.0, "priority": 1 }},
                    "0x{}": {{ "max_qps": 0.0, "priority": 2 }}
                }}
            }}"#,
            hex::encode(t0.as_bytes()),
            hex::encode(t1.as_bytes()),
            hex::encode(t2.as_bytes()),
        ))));

        // Fill the pool to 4 pending (50% of 8): tier 0 sheds first.
        for i in 0..4u8 {
            assert!(try_submit(&mut pool, t2, &[b'f', i], 0).is_ok());
        }
        assert_eq!(pool.pending_count(), 4);
        assert_eq!(
            try_submit(&mut pool, t0, b"shed0", 0).unwrap_err().reason(),
            "pool_pressure"
        );
        assert!(try_submit(&mut pool, t1, b"ok1", 0).is_ok());
        assert!(try_submit(&mut pool, t2, b"ok2", 0).is_ok());

        // At 6 pending (75%): tier 1 sheds too; tier 2 still admitted.
        assert_eq!(pool.pending_count(), 6);
        assert_eq!(
            try_submit(&mut pool, t1, b"shed1", 0).unwrap_err().reason(),
            "pool_pressure"
        );
        assert!(try_submit(&mut pool, t2, b"ok3", 0).is_ok());
        assert!(try_submit(&mut pool, t2, b"ok4", 0).is_ok());

        // At the hard cap (8): even tier 2 sheds.
        assert_eq!(pool.pending_count(), 8);
        assert_eq!(
            try_submit(&mut pool, t2, b"shed2", 0).unwrap_err().reason(),
            "pool_pressure"
        );
    }

    #[test]
    fn test_metering_per_tenant_and_miner_and_inflight() {
        let mut pool = TaskPool::new();
        let (tenant_a, tenant_b) = (Address::new([0xA1; 20]), Address::new([0xB1; 20]));
        let (miner_x, miner_y) = (Address::new([0x71; 20]), Address::new([0x72; 20]));

        let id1 = make_public_task(&mut pool, tenant_a, b"m1", 500);
        let id2 = make_public_task(&mut pool, tenant_a, b"m2", 700);
        let id3 = make_public_task(&mut pool, tenant_b, b"m3", 900);

        // In-flight is tracked per tenant (default tier = priority 1).
        assert_eq!(pool.quota_metrics().inflight_by_tier, [0, 3, 0]);

        // Embedding tasks meter 1 GFLOP each (task_requirements).
        pool.complete_public_task(&id1, ResultStorage::Inline(vec![]), miner_x, 10);
        pool.complete_public_task(&id2, ResultStorage::Inline(vec![]), miner_y, 10);
        pool.complete_public_task(&id3, ResultStorage::Inline(vec![]), miner_x, 10);

        let meter = pool.cost_meter();
        let a = meter.tenant_total(&tenant_a);
        assert_eq!(a.tasks, 2);
        assert_eq!(a.flops, 2_000_000_000);
        assert_eq!(a.fees_wei, 1_200);
        let b = meter.tenant_total(&tenant_b);
        assert_eq!(b.tasks, 1);
        assert_eq!(b.flops, 1_000_000_000);
        let x = meter.miner_total(&miner_x);
        assert_eq!(x.tasks, 2);
        assert_eq!(x.flops, 2_000_000_000);
        assert_eq!(x.fees_wei, 1_400);
        let y = meter.miner_total(&miner_y);
        assert_eq!(y.tasks, 1);

        let m = pool.quota_metrics();
        assert_eq!(m.inflight_by_tier, [0, 0, 0]);
        assert_eq!(m.flops_metered_by_tier[1], 3_000_000_000);

        // Cost report: queryable, sorted, freshness timestamp set.
        let report = pool.generate_cost_report(123_456);
        assert_eq!(report.tenants.len(), 2);
        assert_eq!(report.miners.len(), 2);
        assert_eq!(report.interval_total.tasks, 3);
        assert_eq!(pool.last_cost_report().unwrap().generated_at_ms, 123_456);
        assert_eq!(pool.quota_metrics().cost_report_unix_ms, 123_456);
    }

    #[test]
    fn test_expired_task_frees_inflight_without_metering() {
        let mut pool = TaskPool::new();
        let tenant = Address::new([0xC1; 20]);
        let input_hash = qfc_crypto::blake3_hash(b"exp");
        let task_type = qfc_inference::task::ComputeTaskType::Embedding {
            model_id: qfc_inference::task::ModelId::new("test", "v1"),
            input_hash,
        };
        let task_id = qfc_crypto::blake3_hash(b"exp-id");
        let now = now_ms();
        let task = InferenceTask::new(task_id, 1, task_type, vec![], now, now + 10);
        pool.submit_public_task(tenant, task, 100);
        assert_eq!(pool.quota_metrics().inflight_by_tier[1], 1);

        let expired = pool.prune_expired_public(now + 1_000);
        assert_eq!(expired.len(), 1);
        assert_eq!(pool.quota_metrics().inflight_by_tier[1], 0);
        // Never executed → nothing metered.
        assert_eq!(pool.cost_meter().cumulative_flops(), 0);
    }

    #[test]
    fn test_treasury_hook_invoked() {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::Arc;

        #[derive(Default)]
        struct RecordingHook {
            charges: AtomicU64,
            charged_flops: AtomicU64,
            reports: AtomicU64,
        }
        impl crate::cost::TreasuryHook for RecordingHook {
            fn on_task_charged(
                &self,
                _tenant: &Address,
                _miner: &Address,
                flops: u64,
                _fee_wei: u128,
            ) {
                self.charges.fetch_add(1, Ordering::Relaxed);
                self.charged_flops.fetch_add(flops, Ordering::Relaxed);
            }
            fn on_cost_report(&self, _report: &crate::cost::CostReport) {
                self.reports.fetch_add(1, Ordering::Relaxed);
            }
        }

        struct SharedHook(Arc<RecordingHook>);
        impl crate::cost::TreasuryHook for SharedHook {
            fn on_task_charged(&self, t: &Address, m: &Address, f: u64, w: u128) {
                self.0.on_task_charged(t, m, f, w)
            }
            fn on_cost_report(&self, r: &crate::cost::CostReport) {
                self.0.on_cost_report(r)
            }
        }

        let hook = Arc::new(RecordingHook::default());
        let mut pool = TaskPool::new();
        pool.set_treasury_hook(Box::new(SharedHook(hook.clone())));

        let tenant = Address::new([0xD1; 20]);
        let miner = Address::new([0xD2; 20]);
        let id = make_public_task(&mut pool, tenant, b"hook", 500);
        pool.complete_public_task(&id, ResultStorage::Inline(vec![]), miner, 5);
        pool.generate_cost_report(1);

        assert_eq!(hook.charges.load(Ordering::Relaxed), 1);
        assert_eq!(hook.charged_flops.load(Ordering::Relaxed), 1_000_000_000);
        assert_eq!(hook.reports.load(Ordering::Relaxed), 1);
    }
}
