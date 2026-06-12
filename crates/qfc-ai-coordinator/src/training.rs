//! Training task type: job pool + assignment for decentralized PS training
//! (ROADMAP-AI-V3 milestone A3; design in `docs/adr/0003`–`0008`).
//!
//! # Data-parallel layout
//!
//! PS training is **data-parallel**: every assigned worker trains the FULL
//! model on its own data shards and pushes per-range update segments to the
//! PS shards. A [`TrainingAssignment`] therefore assigns **data shards** to a
//! worker, never a parameter range. The job's `range_partition` is the
//! PS-shard layout — what `qfc-ps` `ShardService`s register — and every
//! worker may push to every range.
//!
//! Because each worker pushes to each range, ADR-0003's
//! `max_workers_per_range` caps the **worker count per job** — and this
//! equivalence is now exact: `qfc-ps`'s `UpdateBuffer` keeps ONE accumulated
//! entry per (range, worker) (gradient accumulation), so its per-range cap
//! counts distinct workers, the same unit this module caps at assignment.
//! The minimum viable worker count is `TrimmedMean::min_updates()` — below
//! that the epoch barrier would skip every range, so assignment refuses and
//! the job suspends instead of training pointlessly (ADR-0007 fail-safe).
//!
//! Payout note (A5): `per_step_reward` is the floor currency here
//! (ADR-0008); reconciling it with FLOPs-pro-rata payout (ADR-0004) is an
//! A5 deliverable.
//!
//! # Determinism
//!
//! Assignment is a pure function of `(job spec, epoch, registry state,
//! current_time)` — `current_time` is an explicit input: it drives the
//! miner-liveness filter and is recorded as `assigned_at`. Two operators
//! with the same registry view AND the same time input produce
//! byte-identical assignments. The A5 cross-operator agreement artifact must
//! therefore exclude `assigned_at` (or derive the time from epoch metadata)
//! so that wall-clock skew between operators cannot break agreement.
//!
//! Consensus-visible types (`ComputeTaskType`, proofs) are untouched here;
//! that is A5. Everything in this module is coordinator-local.

use std::collections::HashMap;

use qfc_inference::{BackendType, GpuTier, ModelId, ShardManifest};
use qfc_ps::{AggregationRule, ParamRange, PsConfig, TrimmedMean};
use qfc_types::{Address, Hash};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::assignment::{tier_matches, MinerRegistry};

/// Errors from training-job submission and assignment
#[derive(Debug, Error)]
pub enum TrainingError {
    #[error("invalid training job spec: {0}")]
    InvalidSpec(String),

    #[error("duplicate training job {0}")]
    DuplicateJob(Hash),

    #[error("training job {0} not found")]
    JobNotFound(Hash),

    #[error("job {job_id}: cannot assign epoch {got}, next expected epoch is {expected}")]
    EpochOutOfOrder {
        job_id: Hash,
        expected: u64,
        got: u64,
    },

    #[error("job {job_id} is {status} and cannot be assigned")]
    JobNotAssignable { job_id: Hash, status: String },

    /// All of the job's epochs have been assigned (`spec.epochs` enforced):
    /// the job awaits `complete_job`, never another assignment.
    #[error("job {job_id} exhausted: all {epochs} epochs already assigned")]
    JobExhausted { job_id: Hash, epochs: u64 },

    /// Illegal lifecycle transition (e.g. completing a job that has not
    /// assigned all its epochs, or touching a terminal job).
    #[error("job {job_id}: invalid transition from {from} to {to}: {detail}")]
    InvalidTransition {
        job_id: Hash,
        from: String,
        to: String,
        detail: String,
    },

    /// The pool holds `MAX_JOBS` jobs; new submissions are refused.
    #[error("training pool full: {max} jobs (MAX_JOBS)")]
    PoolFull { max: usize },

    /// ADR-0007 fail-safe: with fewer workers than the aggregation rule's
    /// minimum, the epoch barrier would skip every range — the job suspends
    /// rather than training on insufficient participation.
    #[error(
        "job {job_id} epoch {epoch}: insufficient eligible workers, need {need} \
         (TrimmedMean::min_updates for trim_beta, ADR-0003), have {have}; job suspended \
         (ADR-0007 fail-safe)"
    )]
    InsufficientWorkers {
        job_id: Hash,
        epoch: u64,
        need: usize,
        have: usize,
    },

    #[error("parameter-server config error: {0}")]
    Config(String),
}

/// Submission/spec bounds — all attacker-facing inputs are capped so a
/// single hostile spec cannot balloon coordinator memory or CPU.
///
/// Max ranges in a `range_partition`: each range becomes a registered
/// `qfc-ps` buffer key and a per-epoch aggregation pass, so this bounds
/// PS-side bookkeeping per job. 4096 shards comfortably covers
/// `PARAM_COUNT_MAX` at sensible shard sizes.
pub const MAX_RANGE_PARTITION_LEN: usize = 4096;

/// Max data shards in a job's manifest: bounds the round-robin distribution
/// work and per-assignment index lists (u32 indices stay well in range).
pub const MAX_DATA_SHARDS: usize = 65536;

/// Max concurrent jobs in a [`TrainingPool`]: bounds pool memory against
/// submission spam until the ADR-0007 submitter pinning deposit (A5) makes
/// submissions costly.
pub const MAX_JOBS: usize = 1024;

/// Sanity ceiling on `param_count` (1e12 ≈ 1T dense parameters, ~4 TB of
/// fp32 — beyond anything a data-parallel CPU pilot worker could hold).
/// Also keeps `param_count * 4` (memory floor math) far from u64 overflow.
pub const PARAM_COUNT_MAX: u64 = 1_000_000_000_000;

/// Specification of a PS training job (submitted once, assigned per epoch).
///
/// PS wiring note (A5): the `qfc-ps` `ShardService::new` for this job must
/// receive `steps_per_epoch` as its `max_steps_per_epoch` — the push-enforced
/// per-epoch step ceiling that bounds both per-worker accumulation and the
/// SSP admission floor (ADR-0006).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrainingJobSpec {
    /// Unique job identifier
    pub job_id: Hash,
    /// Model being trained
    pub model_id: ModelId,
    /// Training data, sharded and CID-addressed via IPFS — the B-1
    /// `ShardManifest` format reused for training DATA (ADR-0007: the
    /// submitter produces and pays to pin it; assigned miners pin their
    /// shards for the job duration)
    pub data_manifest: ShardManifest,
    /// Total dense parameter count of the model
    pub param_count: u64,
    /// PS-shard layout: the ranges `qfc-ps` ShardServices register. This is
    /// NOT a worker assignment — every worker pushes to every range
    /// (data-parallel training).
    pub range_partition: Vec<ParamRange>,
    /// Number of training epochs (SSP epoch barriers, ADR-0006)
    pub epochs: u64,
    /// Steps per epoch per worker
    pub steps_per_epoch: u64,
    /// Tokens processed per training step (FLOPs metering input, ADR-0004)
    pub tokens_per_step: u64,
    /// Reward per accepted step update in wei (ADR-0004)
    pub per_step_reward: u128,
    /// Execution backend constraint. Pilot determinism (ADR-0005 stage 1):
    /// only `BackendType::Cpu` validates today; GPU jobs are gated on the
    /// A4b tolerance-band calibration.
    pub required_backend: BackendType,
    /// Minimum miner tier
    pub min_tier: GpuTier,
    /// Minimum miner memory in MB (full model + optimizer state — workers
    /// hold the whole model in data-parallel training)
    pub min_memory_mb: u64,
}

impl TrainingJobSpec {
    /// Validate the spec against the PS protocol config.
    ///
    /// Checks the data manifest (B-1 invariants, plus: every shard cid
    /// non-empty — training data must be fetchable; at least
    /// `TrimmedMean::min_updates()` shards — otherwise epochs are
    /// guaranteed-dead; at most [`MAX_DATA_SHARDS`]), that `range_partition`
    /// is a sorted, disjoint, contiguous cover of `[0, param_count)` with at
    /// most [`MAX_RANGE_PARTITION_LEN`] ranges, that all work/reward
    /// quantities are non-zero, that `param_count` is sane
    /// ([`PARAM_COUNT_MAX`]) and `min_memory_mb` covers the fp32 model
    /// floor, that the backend satisfies the ADR-0005 pilot determinism
    /// gate, and that the ADR-0008 stake floor is computable without
    /// overflow (so [`Self::stake_floor`] is exact for every validated
    /// spec).
    pub fn validate(&self, ps_config: &PsConfig) -> Result<(), TrainingError> {
        if self.param_count == 0 {
            return Err(TrainingError::InvalidSpec(
                "param_count must be > 0".to_string(),
            ));
        }
        if self.param_count > PARAM_COUNT_MAX {
            return Err(TrainingError::InvalidSpec(format!(
                "param_count {} exceeds PARAM_COUNT_MAX {} (sanity ceiling)",
                self.param_count, PARAM_COUNT_MAX
            )));
        }
        // fp32 floor: 4 bytes per parameter just for the weights — a worker
        // claiming less memory than the raw model can never train it
        // (data-parallel workers hold the full model; optimizer state only
        // makes the real requirement larger). Saturating is safe: param_count
        // <= PARAM_COUNT_MAX keeps param_count * 4 far below u64::MAX.
        let memory_floor_mb = self.param_count.saturating_mul(4) / (1024 * 1024);
        if self.min_memory_mb < memory_floor_mb {
            return Err(TrainingError::InvalidSpec(format!(
                "min_memory_mb {} below the fp32 model floor {} MB \
                 (param_count {} x 4 bytes)",
                self.min_memory_mb, memory_floor_mb, self.param_count
            )));
        }
        if self.epochs == 0 {
            return Err(TrainingError::InvalidSpec("epochs must be > 0".to_string()));
        }
        if self.steps_per_epoch == 0 {
            return Err(TrainingError::InvalidSpec(
                "steps_per_epoch must be > 0".to_string(),
            ));
        }
        if self.tokens_per_step == 0 {
            return Err(TrainingError::InvalidSpec(
                "tokens_per_step must be > 0".to_string(),
            ));
        }
        if self.per_step_reward == 0 {
            return Err(TrainingError::InvalidSpec(
                "per_step_reward must be > 0".to_string(),
            ));
        }

        // ADR-0005 stage 1 pilot gate: exact-match verification requires
        // CPU determinism (SafetensorsFp32-class). A constraint, not a
        // hardcode — the field stays, only the accepted set is narrow.
        if self.required_backend != BackendType::Cpu {
            return Err(TrainingError::InvalidSpec(format!(
                "required_backend {} not allowed: pilot training jobs are CPU-only for \
                 exact-match verification (ADR-0005 stage 1); GPU backends are gated on \
                 the A4b tolerance-band calibration",
                self.required_backend
            )));
        }

        self.data_manifest
            .validate()
            .map_err(|e| TrainingError::InvalidSpec(format!("data manifest: {}", e)))?;
        if self.data_manifest.shards.len() > MAX_DATA_SHARDS {
            return Err(TrainingError::InvalidSpec(format!(
                "data_manifest has {} shards, exceeding MAX_DATA_SHARDS {}",
                self.data_manifest.shards.len(),
                MAX_DATA_SHARDS
            )));
        }
        // Training data must be fetchable NOW: B-1's empty-cid pre-upload
        // state is legal in a manifest in general, but not for a training
        // job — assigned workers would have nothing to train on.
        if let Some(i) = self
            .data_manifest
            .shards
            .iter()
            .position(|s| s.cid.is_empty())
        {
            return Err(TrainingError::InvalidSpec(format!(
                "data_manifest shard {} has an empty cid: training jobs require \
                 uploaded data (the B-1 pre-upload state is not allowed here)",
                i
            )));
        }
        // With fewer shards than the aggregation rule's minimum worker
        // count, some selected workers receive zero shards, push nothing,
        // and every range thin-skips — guaranteed-dead epochs.
        let min_workers = TrimmedMean::new(ps_config.trim_beta)
            .map_err(|e| TrainingError::Config(e.to_string()))?
            .min_updates();
        if self.data_manifest.shards.len() < min_workers {
            return Err(TrainingError::InvalidSpec(format!(
                "data_manifest has {} shards but the aggregation rule needs at \
                 least {} workers with data (TrimmedMean::min_updates for \
                 trim_beta {}); every epoch would thin-skip (ADR-0003)",
                self.data_manifest.shards.len(),
                min_workers,
                ps_config.trim_beta
            )));
        }

        // range_partition must be a sorted, disjoint, contiguous cover of
        // [0, param_count). Contiguity (next.start == prev.end) subsumes
        // sortedness and disjointness in one pass.
        if self.range_partition.is_empty() {
            return Err(TrainingError::InvalidSpec(
                "range_partition is empty".to_string(),
            ));
        }
        if self.range_partition.len() > MAX_RANGE_PARTITION_LEN {
            return Err(TrainingError::InvalidSpec(format!(
                "range_partition has {} ranges, exceeding MAX_RANGE_PARTITION_LEN {}",
                self.range_partition.len(),
                MAX_RANGE_PARTITION_LEN
            )));
        }
        let first = &self.range_partition[0];
        if first.start != 0 {
            return Err(TrainingError::InvalidSpec(format!(
                "range_partition must start at 0, first range is {}",
                first
            )));
        }
        let mut prev_end = 0u64;
        for (i, range) in self.range_partition.iter().enumerate() {
            // ParamRange fields are public, so re-check non-emptiness here.
            if range.start >= range.end {
                return Err(TrainingError::InvalidSpec(format!(
                    "range_partition[{}] = {} is empty",
                    i, range
                )));
            }
            if range.start != prev_end {
                return Err(TrainingError::InvalidSpec(format!(
                    "range_partition[{}] starts at {} but previous range ends at {} \
                     (ranges must be sorted, disjoint and contiguous)",
                    i, range.start, prev_end
                )));
            }
            prev_end = range.end;
        }
        if prev_end != self.param_count {
            return Err(TrainingError::InvalidSpec(format!(
                "range_partition covers [0, {}) but param_count is {}",
                prev_end, self.param_count
            )));
        }

        // ADR-0008: the stake floor must be representable; computing it once
        // here means stake_floor() can never silently saturate post-validate.
        self.checked_stake_floor(ps_config)
            .ok_or_else(|| {
                TrainingError::InvalidSpec(format!(
                    "stake floor overflows u128: slash_multiple {} x per_step_reward {} x \
                     steps_per_epoch {} (ADR-0008)",
                    ps_config.slash_multiple, self.per_step_reward, self.steps_per_epoch
                ))
            })
            .map(|_| ())
    }

    /// ADR-0008 per-epoch assignment stake floor:
    /// `slash_multiple × per_step_reward × steps_per_epoch`.
    ///
    /// A miner's training stake must cover the slash for every step it could
    /// fabricate in one epoch (`S = slash_multiple × r` per detected step,
    /// `steps_per_epoch` steps assigned), so the audit lottery stays
    /// unprofitable to cheat. This is also what makes ADR-0003's
    /// "stake-bounded minority" real: buying update slots means buying
    /// slashable stake.
    ///
    /// HONESTY NOTE: stake is not yet locked per job — a miner's stake can
    /// satisfy the floors of concurrent jobs simultaneously; per-job stake
    /// locking is A5 scope (ADR-0008).
    ///
    /// Saturating arithmetic — [`Self::validate`] rejects specs whose floor
    /// would overflow, so for any validated spec this is exact.
    pub fn stake_floor(&self, ps_config: &PsConfig) -> u128 {
        u128::from(ps_config.slash_multiple)
            .saturating_mul(self.per_step_reward)
            .saturating_mul(u128::from(self.steps_per_epoch))
    }

    fn checked_stake_floor(&self, ps_config: &PsConfig) -> Option<u128> {
        u128::from(ps_config.slash_multiple)
            .checked_mul(self.per_step_reward)?
            .checked_mul(u128::from(self.steps_per_epoch))
    }

    /// Estimated FLOPs per training step (ADR-0004 metering, same estimator
    /// family as `task_types::task_requirements`):
    /// forward ≈ `2 × param_count × tokens_per_step`, backward ≈ 2 × forward,
    /// total ≈ `6 × param_count × tokens_per_step`. Saturating.
    pub fn flops_per_step(&self) -> u64 {
        6u64.saturating_mul(self.param_count)
            .saturating_mul(self.tokens_per_step)
    }

    /// Estimated FLOPs per epoch per worker: `flops_per_step × steps_per_epoch`.
    pub fn flops_per_epoch(&self) -> u64 {
        self.flops_per_step().saturating_mul(self.steps_per_epoch)
    }
}

/// Lifecycle status of a training job
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrainingJobStatus {
    /// Submitted, no epoch assigned yet
    Pending,
    /// Epoch `epoch` is the most recently assigned epoch
    Active { epoch: u64 },
    /// Assignment refused (e.g. insufficient workers, ADR-0007 fail-safe).
    /// Re-assignable: a later attempt at the job's `next_epoch` transitions
    /// back to `Active` if workers have appeared.
    Suspended { reason: String },
    /// All epochs done
    Completed,
    /// Terminally failed
    Failed { reason: String },
}

impl std::fmt::Display for TrainingJobStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => write!(f, "Pending"),
            Self::Active { epoch } => write!(f, "Active(epoch {})", epoch),
            Self::Suspended { reason } => write!(f, "Suspended({})", reason),
            Self::Completed => write!(f, "Completed"),
            Self::Failed { reason } => write!(f, "Failed({})", reason),
        }
    }
}

/// A training job tracked by the pool
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrainingJob {
    pub spec: TrainingJobSpec,
    pub status: TrainingJobStatus,
    /// The next epoch eligible for assignment. Tracked explicitly (rather
    /// than derived from `status`) so a `Suspended` job retries the same
    /// epoch it failed to staff, and out-of-order assigns are rejected.
    pub next_epoch: u64,
    pub created_at: u64,
}

/// One worker's assignment for one training epoch: which data shards to
/// train on. The worker trains the full model on these shards and pushes
/// per-range update segments to every range in the job's `range_partition`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrainingAssignment {
    pub job_id: Hash,
    pub epoch: u64,
    pub worker: Address,
    /// Indices into `TrainingJobSpec::data_manifest.shards`
    pub data_shard_indices: Vec<u32>,
    /// Deterministic per-(job, epoch, worker) training seed — together with
    /// the committed params and the data shards, this is the
    /// `(params, data-shard, seed)` replay tuple a verifier needs to
    /// re-execute the worker's steps (ADR-0005)
    pub seed: u64,
    pub assigned_at: u64,
}

/// First 8 bytes (big-endian) of `blake3(job_id ‖ epoch_be ‖ worker)`.
/// Deterministic and unique per (job, epoch, worker), so replays (ADR-0005)
/// can reconstruct the seed from public data alone.
///
/// `pub(crate)` so `training_verification::ReplayContext::for_claim` can
/// RECOMPUTE the seed instead of trusting the assignment it was handed — a
/// forged assignment with a wrong seed would otherwise replay garbage and
/// manufacture a false penalty against an honest worker.
pub(crate) fn derive_seed(job_id: &Hash, epoch: u64, worker: &Address) -> u64 {
    let mut bytes = Vec::with_capacity(32 + 8 + 20);
    bytes.extend_from_slice(job_id.as_bytes());
    bytes.extend_from_slice(&epoch.to_be_bytes());
    bytes.extend_from_slice(worker.as_bytes());
    let hash = qfc_crypto::blake3_hash(&bytes);
    u64::from_be_bytes(hash.as_bytes()[..8].try_into().expect("hash >= 8 bytes"))
}

/// Pool of training jobs with per-epoch worker assignment.
#[derive(Clone, Debug)]
pub struct TrainingPool {
    jobs: HashMap<Hash, TrainingJob>,
    ps_config: PsConfig,
}

impl TrainingPool {
    /// Validates `ps_config` (`PsConfig::validate`: trim-beta domain, worker
    /// cap vs. minimum viable count, slash multiple, staleness bound) so the
    /// pool can never run on a jointly-broken config.
    pub fn new(ps_config: PsConfig) -> Result<Self, TrainingError> {
        ps_config
            .validate()
            .map_err(|e| TrainingError::Config(e.to_string()))?;
        Ok(Self {
            jobs: HashMap::new(),
            ps_config,
        })
    }

    /// Submit a new training job. Validates the spec (ADR gates), rejects
    /// duplicate job ids, and refuses submissions once the pool holds
    /// [`MAX_JOBS`] jobs.
    ///
    /// NOTE: submission is currently free for the submitter; the ADR-0007
    /// pinning deposit (submitter pays to pin the data for the job duration)
    /// is A5 scope — until then [`MAX_JOBS`] is the only spam bound.
    pub fn submit(&mut self, spec: TrainingJobSpec, created_at: u64) -> Result<(), TrainingError> {
        if self.jobs.len() >= MAX_JOBS {
            return Err(TrainingError::PoolFull { max: MAX_JOBS });
        }
        spec.validate(&self.ps_config)?;
        if self.jobs.contains_key(&spec.job_id) {
            return Err(TrainingError::DuplicateJob(spec.job_id));
        }
        let job_id = spec.job_id;
        self.jobs.insert(
            job_id,
            TrainingJob {
                spec,
                status: TrainingJobStatus::Pending,
                next_epoch: 0,
                created_at,
            },
        );
        Ok(())
    }

    /// Assign workers and data shards for one epoch of a job.
    ///
    /// Eligibility: active, backend == `required_backend`, tier and memory
    /// sufficient, and stake >= the ADR-0008 floor. Selected workers are the
    /// lexicographically smallest eligible addresses, capped at
    /// `max_workers_per_range` (ADR-0003: every worker pushes to every
    /// range, so the per-range buffer cap is a per-job worker cap). If fewer
    /// than `TrimmedMean::min_updates()` workers can be selected, the job
    /// suspends (ADR-0007 fail-safe) and the same epoch may be retried later.
    ///
    /// All manifest shard indices are distributed round-robin with epoch
    /// rotation (`shard i → workers[(i + epoch) % n]`): every shard is
    /// assigned exactly once per epoch, and the shard→worker mapping changes
    /// across epochs. The whole function is deterministic in
    /// `(spec, epoch, registry state)` — identical inputs yield identical
    /// assignments on every operator (forward ref: A5 cross-operator
    /// agreement).
    pub fn assign_epoch(
        &mut self,
        job_id: &Hash,
        epoch: u64,
        registry: &MinerRegistry,
        current_time: u64,
    ) -> Result<Vec<TrainingAssignment>, TrainingError> {
        let job = self
            .jobs
            .get_mut(job_id)
            .ok_or(TrainingError::JobNotFound(*job_id))?;

        match &job.status {
            TrainingJobStatus::Completed | TrainingJobStatus::Failed { .. } => {
                return Err(TrainingError::JobNotAssignable {
                    job_id: *job_id,
                    status: job.status.to_string(),
                });
            }
            // Pending, Active, Suspended: assignable at next_epoch.
            _ => {}
        }
        // spec.epochs is enforced here: once every epoch has been assigned,
        // the job is exhausted and only complete_job / fail_job are legal.
        if job.next_epoch >= job.spec.epochs {
            return Err(TrainingError::JobExhausted {
                job_id: *job_id,
                epochs: job.spec.epochs,
            });
        }
        if epoch != job.next_epoch {
            return Err(TrainingError::EpochOutOfOrder {
                job_id: *job_id,
                expected: job.next_epoch,
                got: epoch,
            });
        }

        let stake_floor = job.spec.stake_floor(&self.ps_config);
        let mut eligible: Vec<Address> = registry
            .iter()
            .filter(|m| {
                m.is_active(current_time)
                    && m.backend == job.spec.required_backend
                    && tier_matches(m.tier, job.spec.min_tier)
                    && m.memory_mb >= job.spec.min_memory_mb
                    && m.stake >= stake_floor
            })
            .map(|m| m.address)
            .collect();
        // Sort by address bytes (lexicographic), then take the smallest up
        // to the cap: both steps are deterministic given the same registry
        // state.
        eligible.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        eligible.truncate(self.ps_config.max_workers_per_range);

        let need = TrimmedMean::new(self.ps_config.trim_beta)
            .map_err(|e| TrainingError::Config(e.to_string()))?
            .min_updates();
        if eligible.len() < need {
            let have = eligible.len();
            let reason = format!(
                "insufficient eligible workers for epoch {}: need {}, have {} \
                 (ADR-0007 fail-safe)",
                epoch, need, have
            );
            job.status = TrainingJobStatus::Suspended {
                reason: reason.clone(),
            };
            return Err(TrainingError::InsufficientWorkers {
                job_id: *job_id,
                epoch,
                need,
                have,
            });
        }

        // Round-robin with epoch rotation over the sorted worker list:
        // every shard index assigned exactly once per epoch.
        let n = eligible.len();
        let mut shards_per_worker: Vec<Vec<u32>> = vec![Vec::new(); n];
        for i in 0..job.spec.data_manifest.shards.len() {
            // u64 arithmetic: `i + epoch as usize` could overflow usize on
            // 32-bit targets for large epochs.
            let w = ((i as u64 + epoch) % n as u64) as usize;
            shards_per_worker[w].push(i as u32);
        }

        let assignments: Vec<TrainingAssignment> = eligible
            .into_iter()
            .zip(shards_per_worker)
            .map(|(worker, data_shard_indices)| TrainingAssignment {
                job_id: *job_id,
                epoch,
                worker,
                data_shard_indices,
                seed: derive_seed(job_id, epoch, &worker),
                assigned_at: current_time,
            })
            .collect();

        job.status = TrainingJobStatus::Active { epoch };
        job.next_epoch = epoch + 1;
        // `eligible` was sorted, so assignments are already sorted by worker.
        Ok(assignments)
    }

    /// Mark a job completed. Legal only from `Active` with every epoch
    /// assigned (`next_epoch == spec.epochs`) — completing early or from
    /// `Pending`/`Suspended`/terminal states is an `InvalidTransition`.
    pub fn complete_job(&mut self, job_id: &Hash) -> Result<(), TrainingError> {
        let job = self
            .jobs
            .get_mut(job_id)
            .ok_or(TrainingError::JobNotFound(*job_id))?;
        if !matches!(job.status, TrainingJobStatus::Active { .. }) {
            return Err(TrainingError::InvalidTransition {
                job_id: *job_id,
                from: job.status.to_string(),
                to: "Completed".to_string(),
                detail: "only Active jobs can complete".to_string(),
            });
        }
        if job.next_epoch != job.spec.epochs {
            return Err(TrainingError::InvalidTransition {
                job_id: *job_id,
                from: job.status.to_string(),
                to: "Completed".to_string(),
                detail: format!(
                    "only {} of {} epochs assigned",
                    job.next_epoch, job.spec.epochs
                ),
            });
        }
        job.status = TrainingJobStatus::Completed;
        Ok(())
    }

    /// Mark a job terminally failed. Legal from any non-terminal state
    /// (`Pending`/`Active`/`Suspended`); terminal jobs (`Completed`/`Failed`)
    /// reject with `InvalidTransition`.
    pub fn fail_job(
        &mut self,
        job_id: &Hash,
        reason: impl Into<String>,
    ) -> Result<(), TrainingError> {
        let job = self
            .jobs
            .get_mut(job_id)
            .ok_or(TrainingError::JobNotFound(*job_id))?;
        if matches!(
            job.status,
            TrainingJobStatus::Completed | TrainingJobStatus::Failed { .. }
        ) {
            return Err(TrainingError::InvalidTransition {
                job_id: *job_id,
                from: job.status.to_string(),
                to: "Failed".to_string(),
                detail: "job is already terminal".to_string(),
            });
        }
        job.status = TrainingJobStatus::Failed {
            reason: reason.into(),
        };
        Ok(())
    }

    /// Get a job by id.
    pub fn get(&self, job_id: &Hash) -> Option<&TrainingJob> {
        self.jobs.get(job_id)
    }

    /// Jobs whose status matches the predicate, sorted by job id bytes for
    /// deterministic iteration order (the backing map is unordered).
    pub fn jobs_by_status(&self, pred: impl Fn(&TrainingJobStatus) -> bool) -> Vec<&TrainingJob> {
        let mut jobs: Vec<&TrainingJob> = self.jobs.values().filter(|j| pred(&j.status)).collect();
        jobs.sort_by_key(|j| *j.spec.job_id.as_bytes());
        jobs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assignment::MinerCapability;
    use qfc_inference::ShardEntry;

    const NOW: u64 = 1000;

    fn addr(b: u8) -> Address {
        Address::new([b; 20])
    }

    fn job_hash(b: u8) -> Hash {
        Hash::new([b; 32])
    }

    fn manifest(n_shards: usize) -> ShardManifest {
        let shards: Vec<ShardEntry> = (0..n_shards)
            .map(|i| ShardEntry {
                cid: format!("QmShard{}", i),
                hash: Hash::new([i as u8; 32]),
                size_bytes: 1024,
                layer_range: None,
            })
            .collect();
        ShardManifest {
            shards,
            total_size_bytes: 1024 * n_shards as u64,
            assembled_hash: Hash::new([0xAA; 32]),
        }
    }

    fn range(start: u64, end: u64) -> ParamRange {
        ParamRange::new(start, end).unwrap()
    }

    fn spec(n_shards: usize) -> TrainingJobSpec {
        TrainingJobSpec {
            job_id: job_hash(1),
            model_id: ModelId::new("bert-tiny", "v1"),
            data_manifest: manifest(n_shards),
            param_count: 1000,
            range_partition: vec![range(0, 500), range(500, 1000)],
            epochs: 4,
            steps_per_epoch: 10,
            tokens_per_step: 128,
            per_step_reward: 1_000_000,
            required_backend: BackendType::Cpu,
            min_tier: GpuTier::Cold,
            min_memory_mb: 1024,
        }
    }

    /// Default-config stake floor for `spec()`: 40 * 1e6 * 10
    const FLOOR: u128 = 400_000_000;

    fn miner(b: u8, stake: u128) -> MinerCapability {
        MinerCapability {
            address: addr(b),
            backend: BackendType::Cpu,
            tier: GpuTier::Cold,
            memory_mb: 4096,
            loaded_models: vec![],
            last_seen: NOW,
            stake,
        }
    }

    fn registry_of(miners: impl IntoIterator<Item = MinerCapability>) -> MinerRegistry {
        let mut registry = MinerRegistry::new();
        for m in miners {
            registry.register(m);
        }
        registry
    }

    fn pool() -> TrainingPool {
        TrainingPool::new(PsConfig::default()).unwrap()
    }

    // ---- spec validation ----

    #[test]
    fn test_validate_ok() {
        assert!(spec(4).validate(&PsConfig::default()).is_ok());
    }

    #[test]
    fn test_validate_partition_rejected() {
        let cfg = PsConfig::default();
        let cases: Vec<(&str, Vec<ParamRange>)> = vec![
            ("empty", vec![]),
            ("gapped", vec![range(0, 400), range(500, 1000)]),
            ("overlapping", vec![range(0, 600), range(500, 1000)]),
            ("unsorted", vec![range(500, 1000), range(0, 500)]),
            (
                "not covering param_count",
                vec![range(0, 500), range(500, 900)],
            ),
            ("not starting at 0", vec![range(100, 1000)]),
        ];
        for (name, partition) in cases {
            let mut s = spec(4);
            s.range_partition = partition;
            assert!(
                matches!(s.validate(&cfg), Err(TrainingError::InvalidSpec(_))),
                "partition case {:?} should be rejected",
                name
            );
        }
    }

    #[test]
    fn test_validate_non_cpu_backend_rejected() {
        let mut s = spec(4);
        s.required_backend = BackendType::Cuda;
        let err = s.validate(&PsConfig::default()).unwrap_err();
        assert!(
            err.to_string().contains("ADR-0005"),
            "error should reference the ADR-0005 pilot gate: {}",
            err
        );
    }

    #[test]
    fn test_validate_zero_fields_rejected() {
        let cfg = PsConfig::default();
        for f in [
            (&mut |s: &mut TrainingJobSpec| s.param_count = 0)
                as &mut dyn FnMut(&mut TrainingJobSpec),
            &mut |s| s.epochs = 0,
            &mut |s| s.steps_per_epoch = 0,
            &mut |s| s.tokens_per_step = 0,
            &mut |s| s.per_step_reward = 0,
        ] {
            let mut s = spec(4);
            f(&mut s);
            assert!(matches!(
                s.validate(&cfg),
                Err(TrainingError::InvalidSpec(_))
            ));
        }
    }

    #[test]
    fn test_validate_bad_manifest_rejected() {
        let mut s = spec(4);
        s.data_manifest.shards.clear();
        let err = s.validate(&PsConfig::default()).unwrap_err();
        assert!(err.to_string().contains("data manifest"));
    }

    #[test]
    fn test_validate_stake_floor_overflow_rejected() {
        let mut s = spec(4);
        s.per_step_reward = u128::MAX / 2;
        let err = s.validate(&PsConfig::default()).unwrap_err();
        assert!(err.to_string().contains("stake floor overflows"));
    }

    #[test]
    fn test_stake_floor_and_flops() {
        let s = spec(4);
        let cfg = PsConfig::default();
        // ADR-0008: 40 (slash_multiple) * 1_000_000 (per-step reward)
        //           * 10 (steps_per_epoch)
        assert_eq!(s.stake_floor(&cfg), FLOOR);
        // 6 * 1000 params * 128 tokens
        assert_eq!(s.flops_per_step(), 768_000);
        assert_eq!(s.flops_per_epoch(), 7_680_000);
    }

    // ---- submission ----

    #[test]
    fn test_submit_duplicate_rejected() {
        let mut pool = pool();
        pool.submit(spec(4), NOW).unwrap();
        assert!(matches!(
            pool.submit(spec(4), NOW),
            Err(TrainingError::DuplicateJob(_))
        ));
        assert_eq!(
            pool.get(&job_hash(1)).unwrap().status,
            TrainingJobStatus::Pending
        );
    }

    // ---- assignment eligibility ----

    #[test]
    fn test_stake_floor_filtering() {
        let mut pool = pool();
        pool.submit(spec(4), NOW).unwrap();
        // Three miners at exactly the floor (included), one a single unit
        // below (excluded).
        let registry = registry_of([
            miner(1, FLOOR),
            miner(2, FLOOR),
            miner(3, FLOOR),
            miner(4, FLOOR - 1),
        ]);
        let assignments = pool.assign_epoch(&job_hash(1), 0, &registry, NOW).unwrap();
        let workers: Vec<Address> = assignments.iter().map(|a| a.worker).collect();
        assert_eq!(workers, vec![addr(1), addr(2), addr(3)]);
    }

    #[test]
    fn test_capability_filtering() {
        let mut pool = pool();
        let mut s = spec(4);
        s.min_tier = GpuTier::Warm;
        s.min_memory_mb = 4096;
        pool.submit(s, NOW).unwrap();

        let warm = |b: u8| {
            let mut m = miner(b, FLOOR);
            m.tier = GpuTier::Warm;
            m
        };
        let mut wrong_backend = warm(4);
        wrong_backend.backend = BackendType::Cuda;
        let cold_tier = miner(5, FLOOR); // Cold < required Warm
        let mut low_memory = warm(6);
        low_memory.memory_mb = 4095;
        let mut inactive = warm(7);
        inactive.last_seen = 0; // NOW would be fine, but use a stale time below

        let registry = registry_of([
            warm(1),
            warm(2),
            warm(3),
            wrong_backend,
            cold_tier,
            low_memory,
            inactive,
        ]);
        // current_time chosen so miner 7 (last_seen 0) is inactive but the
        // others (last_seen NOW=1000) are active.
        let current_time = 60_500;
        let assignments = pool
            .assign_epoch(&job_hash(1), 0, &registry, current_time)
            .unwrap();
        let workers: Vec<Address> = assignments.iter().map(|a| a.worker).collect();
        assert_eq!(workers, vec![addr(1), addr(2), addr(3)]);
    }

    #[test]
    fn test_worker_cap_enforced() {
        let cfg = PsConfig {
            max_workers_per_range: 4,
            ..PsConfig::default()
        };
        let mut pool = TrainingPool::new(cfg).unwrap();
        pool.submit(spec(8), NOW).unwrap();
        // 6 eligible, cap 4 -> the 4 smallest addresses (ADR-0003 cap).
        let registry = registry_of([6, 2, 5, 1, 4, 3].map(|b| miner(b, FLOOR)));
        let assignments = pool.assign_epoch(&job_hash(1), 0, &registry, NOW).unwrap();
        let workers: Vec<Address> = assignments.iter().map(|a| a.worker).collect();
        assert_eq!(workers, vec![addr(1), addr(2), addr(3), addr(4)]);
    }

    #[test]
    fn test_insufficient_workers_suspends() {
        let mut pool = pool();
        pool.submit(spec(4), NOW).unwrap();
        // beta = 0.2 -> TrimmedMean::min_updates() = 3; only 2 eligible.
        let registry = registry_of([miner(1, FLOOR), miner(2, FLOOR)]);
        let err = pool
            .assign_epoch(&job_hash(1), 0, &registry, NOW)
            .unwrap_err();
        match err {
            TrainingError::InsufficientWorkers { need, have, .. } => {
                assert_eq!(need, 3);
                assert_eq!(have, 2);
            }
            other => panic!("expected InsufficientWorkers, got {:?}", other),
        }
        assert!(matches!(
            pool.get(&job_hash(1)).unwrap().status,
            TrainingJobStatus::Suspended { .. }
        ));
    }

    #[test]
    fn test_suspended_job_recovers() {
        let mut pool = pool();
        pool.submit(spec(4), NOW).unwrap();
        let registry = registry_of([miner(1, FLOOR), miner(2, FLOOR)]);
        assert!(pool.assign_epoch(&job_hash(1), 0, &registry, NOW).is_err());
        assert!(matches!(
            pool.get(&job_hash(1)).unwrap().status,
            TrainingJobStatus::Suspended { .. }
        ));
        // A third worker appears: the SAME epoch is retried and succeeds.
        let registry = registry_of([miner(1, FLOOR), miner(2, FLOOR), miner(3, FLOOR)]);
        let assignments = pool.assign_epoch(&job_hash(1), 0, &registry, NOW).unwrap();
        assert_eq!(assignments.len(), 3);
        assert_eq!(
            pool.get(&job_hash(1)).unwrap().status,
            TrainingJobStatus::Active { epoch: 0 }
        );
    }

    // ---- data-shard distribution ----

    fn shard_map(assignments: &[TrainingAssignment]) -> HashMap<Address, Vec<u32>> {
        assignments
            .iter()
            .map(|a| (a.worker, a.data_shard_indices.clone()))
            .collect()
    }

    #[test]
    fn test_every_shard_assigned_exactly_once() {
        let n_shards = 7;
        let mut pool = pool();
        pool.submit(spec(n_shards), NOW).unwrap();
        let registry = registry_of([miner(1, FLOOR), miner(2, FLOOR), miner(3, FLOOR)]);
        let assignments = pool.assign_epoch(&job_hash(1), 0, &registry, NOW).unwrap();
        let mut all: Vec<u32> = assignments
            .iter()
            .flat_map(|a| a.data_shard_indices.iter().copied())
            .collect();
        all.sort_unstable();
        assert_eq!(all, (0..n_shards as u32).collect::<Vec<u32>>());
    }

    #[test]
    fn test_epoch_rotation_changes_mapping() {
        let mut pool = pool();
        pool.submit(spec(7), NOW).unwrap();
        let registry = registry_of([miner(1, FLOOR), miner(2, FLOOR), miner(3, FLOOR)]);
        let e0 = pool.assign_epoch(&job_hash(1), 0, &registry, NOW).unwrap();
        let e1 = pool.assign_epoch(&job_hash(1), 1, &registry, NOW).unwrap();
        assert_ne!(shard_map(&e0), shard_map(&e1));
        // Coverage holds in the rotated epoch too.
        let mut all: Vec<u32> = e1
            .iter()
            .flat_map(|a| a.data_shard_indices.iter().copied())
            .collect();
        all.sort_unstable();
        assert_eq!(all, (0..7).collect::<Vec<u32>>());
    }

    #[test]
    fn test_assignment_deterministic() {
        let make = || {
            let mut pool = pool();
            pool.submit(spec(7), NOW).unwrap();
            let registry = registry_of([miner(3, FLOOR), miner(1, FLOOR), miner(2, FLOOR)]);
            pool.assign_epoch(&job_hash(1), 0, &registry, NOW).unwrap()
        };
        assert_eq!(make(), make());
        // Cloned pool with an identically-rebuilt registry agrees too.
        let mut pool_a = pool();
        pool_a.submit(spec(7), NOW).unwrap();
        let mut pool_b = pool_a.clone();
        let registry = registry_of([miner(1, FLOOR), miner(2, FLOOR), miner(3, FLOOR)]);
        assert_eq!(
            pool_a
                .assign_epoch(&job_hash(1), 0, &registry, NOW)
                .unwrap(),
            pool_b
                .assign_epoch(&job_hash(1), 0, &registry, NOW)
                .unwrap()
        );
    }

    // ---- seeds ----

    #[test]
    fn test_seed_deterministic_and_distinct() {
        // Deterministic per (job, epoch, worker)...
        assert_eq!(
            derive_seed(&job_hash(1), 0, &addr(1)),
            derive_seed(&job_hash(1), 0, &addr(1))
        );
        // ...differs per worker, per epoch, per job.
        assert_ne!(
            derive_seed(&job_hash(1), 0, &addr(1)),
            derive_seed(&job_hash(1), 0, &addr(2))
        );
        assert_ne!(
            derive_seed(&job_hash(1), 0, &addr(1)),
            derive_seed(&job_hash(1), 1, &addr(1))
        );
        assert_ne!(
            derive_seed(&job_hash(1), 0, &addr(1)),
            derive_seed(&job_hash(2), 0, &addr(1))
        );

        // And the assignments carry exactly the derived seeds.
        let mut pool = pool();
        pool.submit(spec(4), NOW).unwrap();
        let registry = registry_of([miner(1, FLOOR), miner(2, FLOOR), miner(3, FLOOR)]);
        let assignments = pool.assign_epoch(&job_hash(1), 0, &registry, NOW).unwrap();
        for a in &assignments {
            assert_eq!(a.seed, derive_seed(&a.job_id, a.epoch, &a.worker));
        }
        let mut seeds: Vec<u64> = assignments.iter().map(|a| a.seed).collect();
        seeds.sort_unstable();
        seeds.dedup();
        assert_eq!(seeds.len(), assignments.len());
    }

    // ---- epoch ordering & lifecycle ----

    #[test]
    fn test_epoch_ordering_enforced() {
        let mut pool = pool();
        pool.submit(spec(4), NOW).unwrap();
        let registry = registry_of([miner(1, FLOOR), miner(2, FLOOR), miner(3, FLOOR)]);

        // Epoch 2 before any assignment: rejected.
        assert!(matches!(
            pool.assign_epoch(&job_hash(1), 2, &registry, NOW),
            Err(TrainingError::EpochOutOfOrder {
                expected: 0,
                got: 2,
                ..
            })
        ));
        pool.assign_epoch(&job_hash(1), 0, &registry, NOW).unwrap();
        // Double-assigning epoch 0: rejected.
        assert!(matches!(
            pool.assign_epoch(&job_hash(1), 0, &registry, NOW),
            Err(TrainingError::EpochOutOfOrder {
                expected: 1,
                got: 0,
                ..
            })
        ));
        // Epoch 2 before epoch 1: rejected; epoch 1 then succeeds.
        assert!(matches!(
            pool.assign_epoch(&job_hash(1), 2, &registry, NOW),
            Err(TrainingError::EpochOutOfOrder { .. })
        ));
        pool.assign_epoch(&job_hash(1), 1, &registry, NOW).unwrap();
        assert_eq!(
            pool.get(&job_hash(1)).unwrap().status,
            TrainingJobStatus::Active { epoch: 1 }
        );
    }

    #[test]
    fn test_terminal_jobs_not_assignable() {
        let mut pool = pool();
        pool.submit(spec(4), NOW).unwrap(); // epochs = 4
        let mut failed = spec(4);
        failed.job_id = job_hash(2);
        pool.submit(failed, NOW).unwrap();
        let registry = registry_of([miner(1, FLOOR), miner(2, FLOOR), miner(3, FLOOR)]);

        // Job 1: run all epochs, complete legitimately, then assignment is
        // refused.
        for e in 0..4 {
            pool.assign_epoch(&job_hash(1), e, &registry, NOW).unwrap();
        }
        pool.complete_job(&job_hash(1)).unwrap();
        assert!(matches!(
            pool.assign_epoch(&job_hash(1), 4, &registry, NOW),
            Err(TrainingError::JobNotAssignable { .. })
        ));

        // Job 2: failed terminally, assignment refused.
        pool.fail_job(&job_hash(2), "data unavailable").unwrap();
        let err = pool
            .assign_epoch(&job_hash(2), 0, &registry, NOW)
            .unwrap_err();
        assert!(err.to_string().contains("Failed"));

        // Unknown job.
        assert!(matches!(
            pool.assign_epoch(&job_hash(9), 0, &registry, NOW),
            Err(TrainingError::JobNotFound(_))
        ));
        assert_eq!(
            pool.jobs_by_status(|s| matches!(s, TrainingJobStatus::Failed { .. }))
                .len(),
            1
        );
    }

    // ---- spec.epochs enforcement & lifecycle state machine (FINDINGS 2, 9) ----

    /// FINDING 2: assigning past `spec.epochs` errors with `JobExhausted`.
    #[test]
    fn test_job_exhausted_after_all_epochs() {
        let mut pool = pool();
        pool.submit(spec(4), NOW).unwrap(); // epochs = 4
        let registry = registry_of([miner(1, FLOOR), miner(2, FLOOR), miner(3, FLOOR)]);
        for e in 0..4 {
            pool.assign_epoch(&job_hash(1), e, &registry, NOW).unwrap();
        }
        match pool
            .assign_epoch(&job_hash(1), 4, &registry, NOW)
            .unwrap_err()
        {
            TrainingError::JobExhausted { epochs, .. } => assert_eq!(epochs, 4),
            other => panic!("expected JobExhausted, got {:?}", other),
        }
        // Exhaustion fires even for an out-of-order epoch request.
        assert!(matches!(
            pool.assign_epoch(&job_hash(1), 0, &registry, NOW),
            Err(TrainingError::JobExhausted { .. })
        ));
    }

    /// FINDINGS 2 + 9: complete only from Active with all epochs assigned;
    /// fail from any non-terminal state; terminal states reject both.
    #[test]
    fn test_complete_and_fail_state_machine() {
        let mut pool = pool();
        pool.submit(spec(4), NOW).unwrap(); // epochs = 4
        let registry = registry_of([miner(1, FLOOR), miner(2, FLOOR), miner(3, FLOOR)]);

        // Pending: complete illegal.
        assert!(matches!(
            pool.complete_job(&job_hash(1)),
            Err(TrainingError::InvalidTransition { .. })
        ));
        // Active but epochs remain: complete illegal.
        pool.assign_epoch(&job_hash(1), 0, &registry, NOW).unwrap();
        let err = pool.complete_job(&job_hash(1)).unwrap_err();
        assert!(
            err.to_string().contains("1 of 4 epochs"),
            "error should name progress: {}",
            err
        );
        // All epochs assigned: complete legal.
        for e in 1..4 {
            pool.assign_epoch(&job_hash(1), e, &registry, NOW).unwrap();
        }
        pool.complete_job(&job_hash(1)).unwrap();
        // Terminal (Completed): both complete and fail rejected.
        assert!(matches!(
            pool.complete_job(&job_hash(1)),
            Err(TrainingError::InvalidTransition { .. })
        ));
        assert!(matches!(
            pool.fail_job(&job_hash(1), "nope"),
            Err(TrainingError::InvalidTransition { .. })
        ));

        // fail is legal from Pending...
        let mut s = spec(4);
        s.job_id = job_hash(2);
        pool.submit(s, NOW).unwrap();
        pool.fail_job(&job_hash(2), "submitter cancelled").unwrap();
        // ...and terminal (Failed) rejects both.
        assert!(matches!(
            pool.fail_job(&job_hash(2), "again"),
            Err(TrainingError::InvalidTransition { .. })
        ));
        assert!(matches!(
            pool.complete_job(&job_hash(2)),
            Err(TrainingError::InvalidTransition { .. })
        ));

        // fail is legal from Suspended.
        let mut s = spec(4);
        s.job_id = job_hash(3);
        pool.submit(s, NOW).unwrap();
        let thin = registry_of([miner(1, FLOOR)]);
        assert!(pool.assign_epoch(&job_hash(3), 0, &thin, NOW).is_err());
        assert!(matches!(
            pool.get(&job_hash(3)).unwrap().status,
            TrainingJobStatus::Suspended { .. }
        ));
        pool.fail_job(&job_hash(3), "gave up staffing").unwrap();
    }

    // ---- spec bounds & sanity (FINDINGS 5, 6, 7, 8) ----

    /// FINDING 6: every shard cid must be non-empty (uploaded data).
    #[test]
    fn test_empty_cid_rejected() {
        let mut s = spec(4);
        s.data_manifest.shards[2].cid = String::new();
        let err = s.validate(&PsConfig::default()).unwrap_err();
        assert!(
            err.to_string()
                .contains("training jobs require uploaded data"),
            "{}",
            err
        );
    }

    /// FINDING 7: fewer shards than the rule's minimum worker count means
    /// guaranteed-dead epochs — rejected, naming both numbers.
    #[test]
    fn test_too_few_shards_rejected() {
        // beta = 0.2 -> min_updates = 3; 2 shards can feed only 2 workers.
        let s = spec(2);
        let err = s.validate(&PsConfig::default()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("2 shards"), "{}", msg);
        assert!(msg.contains("at least 3"), "{}", msg);
    }

    /// FINDING 5: partition / shard-count / job-count caps.
    #[test]
    fn test_partition_and_shard_caps() {
        let cfg = PsConfig::default();

        // range_partition cap: 4097 unit ranges.
        let mut s = spec(4);
        s.param_count = (MAX_RANGE_PARTITION_LEN + 1) as u64;
        s.range_partition = (0..=MAX_RANGE_PARTITION_LEN as u64)
            .map(|i| range(i, i + 1))
            .collect();
        let err = s.validate(&cfg).unwrap_err();
        assert!(
            err.to_string().contains("MAX_RANGE_PARTITION_LEN"),
            "{}",
            err
        );

        // data shard cap: 65537 shards.
        let s = spec(MAX_DATA_SHARDS + 1);
        let err = s.validate(&cfg).unwrap_err();
        assert!(err.to_string().contains("MAX_DATA_SHARDS"), "{}", err);
    }

    /// FINDING 5: the pool refuses submissions at MAX_JOBS.
    #[test]
    fn test_pool_full() {
        let mut pool = pool();
        for i in 0..MAX_JOBS {
            let mut s = spec(4);
            let mut id = [0u8; 32];
            id[..8].copy_from_slice(&(i as u64).to_be_bytes());
            s.job_id = Hash::new(id);
            pool.submit(s, NOW).unwrap();
        }
        let mut s = spec(4);
        s.job_id = Hash::new([0xFF; 32]);
        assert!(matches!(
            pool.submit(s, NOW),
            Err(TrainingError::PoolFull { max: MAX_JOBS })
        ));
    }

    /// FINDING 8: param_count sanity ceiling and the fp32 memory floor.
    #[test]
    fn test_param_count_and_memory_sanity() {
        let cfg = PsConfig::default();

        // param_count above the 1e12 ceiling.
        let mut s = spec(4);
        s.param_count = PARAM_COUNT_MAX + 1;
        s.range_partition = vec![range(0, PARAM_COUNT_MAX + 1)];
        s.min_memory_mb = u64::MAX;
        let err = s.validate(&cfg).unwrap_err();
        assert!(err.to_string().contains("PARAM_COUNT_MAX"), "{}", err);

        // fp32 floor: 2^30 params * 4 bytes = 4096 MB.
        let mut s = spec(4);
        s.param_count = 1 << 30;
        s.range_partition = vec![range(0, 1 << 30)];
        s.min_memory_mb = 4095;
        let err = s.validate(&cfg).unwrap_err();
        assert!(err.to_string().contains("fp32 model floor"), "{}", err);
        s.min_memory_mb = 4096;
        assert!(s.validate(&cfg).is_ok());
    }

    // ---- config & determinism nits ----

    /// NIT: `TrainingPool::new` validates the PsConfig.
    #[test]
    fn test_pool_rejects_invalid_config() {
        let bad = PsConfig {
            trim_beta: 0.6,
            ..PsConfig::default()
        };
        assert!(matches!(
            TrainingPool::new(bad),
            Err(TrainingError::Config(_))
        ));
    }

    /// NIT: assignment is invariant to miner registration order — the same
    /// miners registered in two different orders yield identical
    /// assignments.
    #[test]
    fn test_registration_order_invariance() {
        let assign = |order: [u8; 4]| {
            let mut pool = pool();
            pool.submit(spec(7), NOW).unwrap();
            let registry = registry_of(order.map(|b| miner(b, FLOOR)));
            pool.assign_epoch(&job_hash(1), 0, &registry, NOW).unwrap()
        };
        assert_eq!(assign([1, 2, 3, 4]), assign([4, 2, 1, 3]));
    }
}
