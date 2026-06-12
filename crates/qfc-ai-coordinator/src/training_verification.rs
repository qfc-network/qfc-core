//! Training-step verification: sampled re-execution of accepted training
//! steps (ROADMAP-AI-V3 milestone A4; spec in ADR-0005, economics in
//! ADR-0008).
//!
//! The verification subject is one **accepted training step**: the claim is
//! an [`qfc_ps::AcceptanceRecord`] plus its job context and the params
//! version the worker trained against. A verifier rebuilds the ADR-0005
//! replay tuple — `(params version hash, data shards, seed, worker, epoch,
//! clock, range)`, packaged here as [`ReplayContext`] — re-runs the step,
//! and compares the recomputed update against the claim.
//!
//! Two comparison modes, staged per ADR-0005:
//!
//! 1. [`CompareMode::Exact`] — the pilot. CPU-only determinism
//!    (`SafetensorsFp32`-class) makes the recomputed update bit-exact, so
//!    the check is `ParamUpdate::update_hash()` equality against the
//!    record's `update_hash`.
//! 2. [`CompareMode::ToleranceBand`] — A4b, for GPU jobs whose kernel
//!    nondeterminism breaks exact equality. Per-coordinate check:
//!    `|claimed_i − replayed_i| <= max(ε × |replayed_i|, abs_floor)`.
//!
//! Sampling is deterministic at `PsConfig::spot_check_rate` (5% nominal,
//! ADR-0005) from `blake3(epoch_entropy ‖ update_hash)`, where
//! `epoch_entropy` is the epoch's committed post-barrier `params_hash`
//! (`qfc_ps::EpochOutcome::params_hash`) — entropy that does not exist when
//! the worker pushes its update, so the worker cannot grind its way out of
//! the sample (see [`TrainingVerifier::should_verify`]).
//!
//! # Scope notes (deferred work)
//!
//! - **No real training engine exists yet** — re-execution goes through the
//!   pluggable [`TrainingExecutor`] trait; the real executor lands with A6.
//! - **Challenge traps for training** (pre-computed steps with known
//!   updates, indistinguishable from real assignments — `challenge.rs`'s
//!   pattern, ADR-0005) are deferred to A6 alongside the real executor.
//! - **Verifier-side economics** (ADR-0008: verification is metered work,
//!   per-epoch verifier budget = p × accepted FLOPs paid from the epoch
//!   reward pool) are wired in A5.
//! - **Penalty application** is A5: this module only *produces* the
//!   [`TrainingPenalty`] recommendation; turning it into a
//!   `SlashResult`/`SlashingEvidence` and anchoring it on-chain is A5 scope.

use async_trait::async_trait;
use qfc_inference::{ModelId, ShardManifest};
use qfc_ps::{AcceptanceRecord, ParamRange, ParamUpdate, PsConfig};
use qfc_types::{Address, Hash};
use thiserror::Error;

use crate::training::{derive_seed, TrainingAssignment, TrainingJobSpec};

/// Jail duration recommended for a failed training-step verification:
/// 6 hours, the v2.x `InvalidInference` convention carried over per
/// ADR-0005/ADR-0008.
pub const INVALID_TRAINING_JAIL_MS: u64 = 21_600_000;

/// Errors from training-step verification.
///
/// NOTE: a *mismatch between claim and replay* is NOT an error — it is an
/// `Ok(TrainingVerdict { passed: false, .. })`. Errors are reserved for
/// replay infrastructure failures and misuse of the API.
#[derive(Debug, Error)]
pub enum TrainingVerificationError {
    /// The claim, assignment and job context disagree (wrong worker, wrong
    /// epoch, clock out of range, ...) — the verifier was handed
    /// inconsistent inputs.
    #[error("claim/context mismatch: {0}")]
    ContextMismatch(String),

    /// Tolerance-band mode requires the full claimed `ParamUpdate` (values
    /// included) to compute the distance; the caller supplied none.
    #[error("tolerance-band compare requires the claimed ParamUpdate, got None")]
    MissingClaimedUpdate,

    /// The claimed update supplied for tolerance-band compare does not hash
    /// to the acceptance record's `update_hash` — the operator (or whoever
    /// served the update body) provided data that is NOT what was accepted.
    /// This implicates the data source, not the worker.
    #[error("claimed update hash {got} does not match acceptance record hash {expected} (tampered or mis-served update body)")]
    ClaimedUpdateHashMismatch { expected: Hash, got: Hash },

    /// The claimed update supplied for tolerance-band compare is
    /// structurally invalid (`ParamUpdate::validate` failed).
    #[error("claimed update invalid: {0}")]
    InvalidClaimedUpdate(String),

    /// The executor failed to replay the step (data unavailable, engine
    /// error, wrong output shape, ...) — verification infrastructure
    /// failure, not a verdict about the worker.
    #[error("replay failed: {0}")]
    ReplayFailed(String),

    /// `PsConfig::validate` rejected the verifier's config.
    #[error("invalid ps config: {0}")]
    InvalidConfig(String),

    /// Tolerance-band epsilon outside `(0, 1)`.
    #[error("invalid tolerance epsilon {0}: must satisfy 0 < epsilon < 1")]
    InvalidEpsilon(f64),

    /// Tolerance-band absolute floor negative or non-finite.
    #[error("invalid tolerance abs_floor {0}: must be finite and >= 0")]
    InvalidAbsFloor(f64),
}

/// Everything a verifier needs to deterministically re-run one accepted
/// training step — the ADR-0005 replay tuple:
/// `(params version hash, data shards, seed, worker, epoch, clock, range)`.
///
/// Carries `per_step_reward` too: it is job context, and the ADR-0008
/// penalty (`slash_multiple × per_step_reward`) is computed at verdict time.
#[derive(Clone, Debug, PartialEq)]
pub struct ReplayContext {
    /// Job this step belongs to
    pub job_id: Hash,
    /// Model being trained
    pub model_id: ModelId,
    /// Training epoch of the audited step
    pub epoch: u64,
    /// Worker that produced the claimed update
    pub worker: Address,
    /// Worker's SSP step clock at the audited step
    pub clock: u64,
    /// Parameter range of the claimed update segment
    pub range: ParamRange,
    /// Per-(job, epoch, worker) training seed. MUST equal the output of
    /// `training::derive_seed(job_id, epoch, worker)` — [`Self::for_claim`]
    /// recomputes it from public data and rejects assignments that disagree.
    pub seed: u64,
    /// Data shards (indices into `data_manifest.shards`) the worker was
    /// assigned for this epoch
    pub data_shard_indices: Vec<u32>,
    /// The job's training-data manifest (ADR-0007)
    pub data_manifest: ShardManifest,
    /// Hash of the epoch-start parameter version the worker pulled before
    /// training. Served from PS snapshot retention (ADR-0007:
    /// `snapshot_retention >= audit window`), so the verifier can fetch the
    /// exact `(version, range)` bytes the worker started from.
    pub params_version_hash: Hash,
    /// The job's per-step reward in wei (ADR-0004) — input to the ADR-0008
    /// slash computation (`slash_multiple × per_step_reward`)
    pub per_step_reward: u128,
}

impl ReplayContext {
    /// Build the replay context for one accepted step, cross-checking that
    /// the spec, assignment and acceptance record actually describe the
    /// same step:
    ///
    /// - `assignment.job_id == spec.job_id`
    /// - `assignment.worker == record.worker`
    /// - `assignment.epoch == record.epoch`
    /// - `record.clock < spec.steps_per_epoch` (a worker cannot have taken
    ///   more steps than the epoch allows)
    /// - `assignment.seed == training::derive_seed(job, epoch, worker)` —
    ///   the seed is RECOMPUTED, never trusted: a forged assignment with a
    ///   wrong seed would replay a different trajectory and manufacture a
    ///   false penalty against an honest worker. Recomputing closes that
    ///   vector (the seed is a pure function of public data).
    /// - `record.range` is a member of `spec.range_partition` — an accepted
    ///   record can only ever name a registered PS-shard range.
    ///
    /// # Honesty note: `data_shard_indices` are NOT re-derivable here
    ///
    /// The shard indices come from `assign_epoch`, which depends on the
    /// miner-registry snapshot at assignment time — state this function does
    /// not have. Assignment IS deterministic (`training.rs`), so any auditor
    /// holding the same registry snapshot can recompute `assign_epoch` and
    /// check the indices; A5 anchors assignments on-chain, making that check
    /// universal. Until then, a verifier supplying a wrong assignment is
    /// caught by the seed assertion above only if the seed differs — the
    /// seed covers (job, epoch, worker) but NOT the shard indices, so a
    /// forged assignment that keeps those three fields and lies only about
    /// `data_shard_indices` passes these checks, the replay diverges, and
    /// the worker is wrongly penalized. That residual false-penalty vector
    /// closes when A5 makes assignments verifiable.
    pub fn for_claim(
        spec: &TrainingJobSpec,
        assignment: &TrainingAssignment,
        record: &AcceptanceRecord,
        params_version_hash: Hash,
    ) -> Result<Self, TrainingVerificationError> {
        if assignment.job_id != spec.job_id {
            return Err(TrainingVerificationError::ContextMismatch(format!(
                "assignment job {} != spec job {}",
                assignment.job_id, spec.job_id
            )));
        }
        if assignment.worker != record.worker {
            return Err(TrainingVerificationError::ContextMismatch(format!(
                "assignment worker {} != record worker {}",
                assignment.worker, record.worker
            )));
        }
        if assignment.epoch != record.epoch {
            return Err(TrainingVerificationError::ContextMismatch(format!(
                "assignment epoch {} != record epoch {}",
                assignment.epoch, record.epoch
            )));
        }
        if record.clock >= spec.steps_per_epoch {
            return Err(TrainingVerificationError::ContextMismatch(format!(
                "record clock {} >= steps_per_epoch {}",
                record.clock, spec.steps_per_epoch
            )));
        }
        // Recompute the seed from public data — do NOT trust the assignment
        // (a forged seed replays a different trajectory and would falsely
        // penalize an honest worker).
        let expected_seed = derive_seed(&spec.job_id, assignment.epoch, &assignment.worker);
        if assignment.seed != expected_seed {
            return Err(TrainingVerificationError::ContextMismatch(format!(
                "assignment seed {} != derived seed {} for (job {}, epoch {}, worker {}) \
                 — forged or corrupted assignment",
                assignment.seed, expected_seed, spec.job_id, assignment.epoch, assignment.worker
            )));
        }
        // The accepted range must be one of the job's registered PS-shard
        // ranges. range_partition is validated sorted (TrainingJobSpec::
        // validate), so binary search applies.
        if spec.range_partition.binary_search(&record.range).is_err() {
            return Err(TrainingVerificationError::ContextMismatch(format!(
                "record range {} is not a member of the job's range_partition",
                record.range
            )));
        }
        Ok(Self {
            job_id: spec.job_id,
            model_id: spec.model_id.clone(),
            epoch: record.epoch,
            worker: record.worker,
            clock: record.clock,
            range: record.range,
            seed: assignment.seed,
            data_shard_indices: assignment.data_shard_indices.clone(),
            data_manifest: spec.data_manifest.clone(),
            params_version_hash,
            per_step_reward: spec.per_step_reward,
        })
    }
}

/// Pluggable re-execution backend (the real training engine lands with A6;
/// tests use mocks).
///
/// `replay_step` must deterministically re-run the worker's step described
/// by `ctx` and return the recomputed update values for `ctx.range`
/// (`ctx.range.len()` f32s, in key order).
#[async_trait]
pub trait TrainingExecutor: Send + Sync {
    async fn replay_step(&self, ctx: &ReplayContext)
        -> Result<Vec<f32>, TrainingVerificationError>;
}

/// How a replayed update is compared against the claim (ADR-0005).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CompareMode {
    /// ADR-0005 stage 1 (pilot): bit-exact `update_hash` equality. Requires
    /// CPU determinism — this is why pilot training jobs are CPU-only
    /// (`TrainingJobSpec::validate` enforces the backend gate).
    Exact,

    /// A4b: PER-COORDINATE check — accept iff for every coordinate `i`
    /// `|claimed_i − replayed_i| <= max(epsilon × |replayed_i|, abs_floor)`.
    ///
    /// Per-coordinate, NOT a global ∞-norm: a global `ε × max|replayed|`
    /// band would let one large honest coordinate widen the band for every
    /// other coordinate, hiding a targeted poison in a small one. Each
    /// coordinate gets a band scaled to ITS OWN replayed magnitude.
    ///
    /// `abs_floor` exists because a purely relative per-coordinate band
    /// blows up on near-zero gradient coordinates: `ε × |replayed_i|` → 0
    /// there, and benign kernel noise (the entire reason this mode exists)
    /// would fail honest workers. BOTH knobs — `epsilon` AND `abs_floor` —
    /// must come from the A4b calibration report before GPU jobs enable
    /// this mode; neither has a defensible a-priori value.
    ///
    /// WARNING: enabling this for GPU jobs ALSO requires a re-derivation of
    /// the ADR-0008 slash/reward ratio — tolerance bands weaken the slash
    /// guarantee (a cheater can move band-far undetected). Construct via
    /// [`CompareMode::tolerance_band`] to get the domain checks.
    ToleranceBand { epsilon: f64, abs_floor: f64 },
}

impl CompareMode {
    /// Validated constructor for [`CompareMode::ToleranceBand`]:
    /// `0 < epsilon < 1` and `abs_floor` finite with `abs_floor >= 0`
    /// (NaN rejected by both checks).
    pub fn tolerance_band(epsilon: f64, abs_floor: f64) -> Result<Self, TrainingVerificationError> {
        if !(epsilon > 0.0 && epsilon < 1.0) {
            return Err(TrainingVerificationError::InvalidEpsilon(epsilon));
        }
        if !(abs_floor.is_finite() && abs_floor >= 0.0) {
            return Err(TrainingVerificationError::InvalidAbsFloor(abs_floor));
        }
        Ok(Self::ToleranceBand { epsilon, abs_floor })
    }

    /// Domain check (re-run by [`TrainingVerifier::new`] since the variant
    /// fields are public).
    fn validate(&self) -> Result<(), TrainingVerificationError> {
        match *self {
            Self::Exact => Ok(()),
            Self::ToleranceBand { epsilon, abs_floor } => {
                Self::tolerance_band(epsilon, abs_floor).map(|_| ())
            }
        }
    }
}

/// ADR-0008 penalty recommendation for a failed verification.
///
/// This module only *recommends*; applying the penalty
/// (`SlashResult`/`SlashingEvidence`, on-chain anchoring) is A5. The
/// penalty is self-identifying — `worker`/`job_id`/`epoch` name who is
/// slashed for what — so A5 can apply it standalone without re-deriving
/// the claim context.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrainingPenalty {
    /// Worker being penalized (from the failed claim)
    pub worker: Address,
    /// Job the failed step belonged to
    pub job_id: Hash,
    /// Training epoch of the failed step (the epoch whose records
    /// `void_epoch_records` voids)
    pub epoch: u64,
    /// `slash_multiple × per_step_reward` (ADR-0008: S = 40r at the default
    /// config), assessed against the miner's training stake
    pub slash_amount: u128,
    /// Retroactively void the worker's acceptance records for the epoch
    /// (ADR-0004) — always true on a failed verification
    pub void_epoch_records: bool,
    /// Jail duration: [`INVALID_TRAINING_JAIL_MS`] (6h, the v2.x
    /// InvalidInference convention per ADR-0005)
    pub jail_duration_ms: u64,
}

/// Outcome of verifying one accepted training step.
#[derive(Clone, Debug, PartialEq)]
pub struct TrainingVerdict {
    /// Did the replayed update match the claim?
    pub passed: bool,
    /// Comparison mode that produced this verdict
    pub mode_used: CompareMode,
    /// Human-readable detail
    pub details: String,
    /// ADR-0008 penalty recommendation — populated iff `!passed`
    pub penalty: Option<TrainingPenalty>,
}

/// Sampled re-execution verifier for accepted training steps (ADR-0005).
#[derive(Clone, Debug)]
pub struct TrainingVerifier {
    config: PsConfig,
    mode: CompareMode,
}

impl TrainingVerifier {
    /// Validates both the protocol config (`PsConfig::validate`) and the
    /// compare mode's epsilon domain.
    pub fn new(config: PsConfig, mode: CompareMode) -> Result<Self, TrainingVerificationError> {
        config
            .validate()
            .map_err(|e| TrainingVerificationError::InvalidConfig(e.to_string()))?;
        mode.validate()?;
        Ok(Self { config, mode })
    }

    /// Deterministic sampling decision for one acceptance record.
    ///
    /// Entropy: the first 4 big-endian bytes of
    /// `blake3(epoch_entropy ‖ record.update_hash)` as a u32, compared
    /// against `spot_check_rate × u32::MAX` (within ~2⁻³² of the configured
    /// rate — finer than `verification.rs::should_spot_check`'s single-byte
    /// 1/256 quantization).
    ///
    /// # Why the entropy is mixed in (grinding resistance)
    ///
    /// `update_hash` alone is WORKER-CHOSEN entropy: the worker computes the
    /// hash itself before pushing, and `flops_estimated` is a free grinding
    /// knob inside it — at a 5% rate, ~1.05 hash attempts per step suffice
    /// to find an update that is never sampled. `epoch_entropy` must be the
    /// epoch's COMMITTED post-barrier params hash
    /// (`qfc_ps::EpochOutcome::params_hash`): it is fixed only after every
    /// record of the epoch is already bound, so it DOES NOT EXIST when the
    /// worker pushes — there is nothing to grind against.
    ///
    /// # Residual caveat
    ///
    /// A colluding worker *majority* has some influence over the aggregate
    /// `params_hash` (their accepted updates are inputs to it), so a
    /// majority cartel retains a weak grinding handle. A5 replaces
    /// `epoch_entropy` with on-chain VRF randomness, closing that too
    /// (ADR-0005/ADR-0008).
    pub fn should_verify(&self, record: &AcceptanceRecord, epoch_entropy: &Hash) -> bool {
        let mut bytes = Vec::with_capacity(64);
        bytes.extend_from_slice(epoch_entropy.as_bytes());
        bytes.extend_from_slice(record.update_hash.as_bytes());
        let mixed = qfc_crypto::blake3_hash(&bytes);
        let val = u32::from_be_bytes(mixed.as_bytes()[..4].try_into().expect("hash >= 4 bytes"));
        let threshold = (self.config.spot_check_rate * u32::MAX as f64) as u32;
        val < threshold
    }

    /// Filter a record set down to the sampled subset. `epoch_entropy` is
    /// the epoch's committed post-barrier params hash — see
    /// [`Self::should_verify`] for why it must be worker-independent.
    pub fn sample<'a>(
        &self,
        records: &'a [AcceptanceRecord],
        epoch_entropy: &Hash,
    ) -> Vec<&'a AcceptanceRecord> {
        records
            .iter()
            .filter(|r| self.should_verify(r, epoch_entropy))
            .collect()
    }

    /// Re-execute one accepted step via `executor` and compare against the
    /// claim.
    ///
    /// - [`CompareMode::Exact`]: reconstructs the `ParamUpdate` from the
    ///   claim's metadata plus the replayed values and requires
    ///   `update_hash()` equality (bit-exact — the reason the pilot is
    ///   CPU-only). `claimed_update` is not needed and may be `None`.
    /// - [`CompareMode::ToleranceBand`]: REQUIRES `claimed_update` (the
    ///   record only carries a hash; the distance needs the values). The
    ///   supplied update must be structurally valid and must hash to the
    ///   record's `update_hash` — otherwise whoever served it tampered with
    ///   the accepted data ([`TrainingVerificationError::ClaimedUpdateHashMismatch`]).
    ///   Accepts iff EVERY coordinate satisfies
    ///   `|claimed_i − replayed_i| <= max(epsilon × |replayed_i|, abs_floor)`
    ///   (per-coordinate band — see [`CompareMode::ToleranceBand`] for why
    ///   a global ∞-norm band is unsound). On failure the details name the
    ///   worst-offending coordinate, its claimed/replayed values and the
    ///   band it was allowed.
    ///
    /// A mismatch is NOT an error: it returns
    /// `Ok(TrainingVerdict { passed: false, penalty: Some(..) })`. Errors
    /// are replay infrastructure failures or inconsistent inputs.
    pub async fn verify_step(
        &self,
        claim: &AcceptanceRecord,
        ctx: &ReplayContext,
        claimed_update: Option<&ParamUpdate>,
        executor: &dyn TrainingExecutor,
    ) -> Result<TrainingVerdict, TrainingVerificationError> {
        // The context must describe the claimed step — otherwise an exact
        // compare would manufacture a false failure (and a penalty) out of
        // verifier-side inconsistency.
        if ctx.worker != claim.worker
            || ctx.epoch != claim.epoch
            || ctx.clock != claim.clock
            || ctx.range != claim.range
        {
            return Err(TrainingVerificationError::ContextMismatch(format!(
                "replay context (worker {}, epoch {}, clock {}, range {}) does not describe \
                 claim (worker {}, epoch {}, clock {}, range {})",
                ctx.worker,
                ctx.epoch,
                ctx.clock,
                ctx.range,
                claim.worker,
                claim.epoch,
                claim.clock,
                claim.range
            )));
        }

        // Tolerance mode validates the claimed update BEFORE doing the
        // (expensive) replay: a tampered/missing body is an operator-side
        // fault and must not be confused with a worker mismatch.
        let claimed = match self.mode {
            CompareMode::Exact => None,
            CompareMode::ToleranceBand { .. } => {
                let claimed =
                    claimed_update.ok_or(TrainingVerificationError::MissingClaimedUpdate)?;
                claimed
                    .validate()
                    .map_err(|e| TrainingVerificationError::InvalidClaimedUpdate(e.to_string()))?;
                let got = claimed.update_hash();
                if got != claim.update_hash {
                    return Err(TrainingVerificationError::ClaimedUpdateHashMismatch {
                        expected: claim.update_hash,
                        got,
                    });
                }
                Some(claimed)
            }
        };

        let replayed = executor.replay_step(ctx).await?;
        if replayed.len() as u64 != ctx.range.len() {
            return Err(TrainingVerificationError::ReplayFailed(format!(
                "executor returned {} values for range {} (len {})",
                replayed.len(),
                ctx.range,
                ctx.range.len()
            )));
        }

        let (passed, details) = match self.mode {
            CompareMode::Exact => {
                // Reconstruct the update the worker SHOULD have produced and
                // require bit-exact hash equality (ADR-0005 stage 1).
                let recomputed = ParamUpdate {
                    worker: claim.worker,
                    epoch: claim.epoch,
                    clock: claim.clock,
                    range: claim.range,
                    values: replayed,
                    flops_estimated: claim.flops_estimated,
                }
                .update_hash();
                if recomputed == claim.update_hash {
                    (
                        true,
                        "exact: replayed update_hash matches claim".to_string(),
                    )
                } else {
                    (
                        false,
                        format!(
                            "exact: replayed update_hash {} != claimed {}",
                            recomputed, claim.update_hash
                        ),
                    )
                }
            }
            CompareMode::ToleranceBand { epsilon, abs_floor } => {
                let claimed = claimed.expect("tolerance mode validated claimed_update above");
                // Per-coordinate: each coordinate's band is scaled to ITS
                // OWN replayed magnitude (with the calibrated absolute floor
                // for near-zero coordinates) — a large honest coordinate
                // cannot widen the band of any other coordinate.
                // Track the worst offender: the coordinate maximizing
                // |diff| − band (largest excess over its allowed band).
                let mut worst: Option<(usize, f64, f64, f64)> = None; // (i, claimed, replayed, band)
                let mut worst_excess = f64::NEG_INFINITY;
                for (i, (c, r)) in claimed.values.iter().zip(replayed.iter()).enumerate() {
                    let (c, r) = (f64::from(*c), f64::from(*r));
                    let band = (epsilon * r.abs()).max(abs_floor);
                    let excess = (c - r).abs() - band;
                    if excess > worst_excess {
                        worst_excess = excess;
                        worst = Some((i, c, r, band));
                    }
                }
                match worst {
                    Some((i, c, r, band)) if worst_excess > 0.0 => (
                        false,
                        format!(
                            "tolerance: coordinate {} out of band: claimed {:.6e}, \
                             replayed {:.6e}, |diff| {:.6e} > allowed band {:.6e} \
                             (epsilon {}, abs_floor {:.6e})",
                            i,
                            c,
                            r,
                            (c - r).abs(),
                            band,
                            epsilon,
                            abs_floor
                        ),
                    ),
                    _ => (
                        true,
                        format!(
                            "tolerance: all {} coordinates within \
                             max(epsilon × |replayed_i|, abs_floor) \
                             (epsilon {}, abs_floor {:.6e})",
                            replayed.len(),
                            epsilon,
                            abs_floor
                        ),
                    ),
                }
            }
        };

        let penalty = if passed {
            None
        } else {
            Some(TrainingPenalty {
                worker: claim.worker,
                job_id: ctx.job_id,
                epoch: claim.epoch,
                slash_amount: u128::from(self.config.slash_multiple)
                    .saturating_mul(ctx.per_step_reward),
                void_epoch_records: true,
                jail_duration_ms: INVALID_TRAINING_JAIL_MS,
            })
        };

        Ok(TrainingVerdict {
            passed,
            mode_used: self.mode,
            details,
            penalty,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assignment::{MinerCapability, MinerRegistry};
    use crate::training::TrainingPool;
    use qfc_inference::{BackendType, GpuTier, ShardEntry};

    const NOW: u64 = 1000;
    const PER_STEP_REWARD: u128 = 1_000_000;

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
            param_count: 8,
            range_partition: vec![range(0, 4), range(4, 8)],
            epochs: 4,
            steps_per_epoch: 10,
            tokens_per_step: 128,
            per_step_reward: PER_STEP_REWARD,
            required_backend: BackendType::Cpu,
            min_tier: GpuTier::Cold,
            min_memory_mb: 1024,
        }
    }

    fn assignment(worker: u8, epoch: u64) -> TrainingAssignment {
        TrainingAssignment {
            job_id: job_hash(1),
            epoch,
            worker: addr(worker),
            data_shard_indices: vec![0, 3],
            // for_claim recomputes the seed (FINDING N1) — the helper must
            // carry the genuine derived value.
            seed: derive_seed(&job_hash(1), epoch, &addr(worker)),
            assigned_at: NOW,
        }
    }

    /// Epoch-entropy fixture for sampling tests: stands in for the epoch's
    /// committed post-barrier params hash (`EpochOutcome::params_hash`).
    fn entropy() -> Hash {
        qfc_crypto::blake3_hash(b"post-barrier params hash fixture")
    }

    fn update(worker: u8, epoch: u64, clock: u64, values: Vec<f32>) -> ParamUpdate {
        ParamUpdate {
            worker: addr(worker),
            epoch,
            clock,
            range: range(0, values.len() as u64),
            values,
            flops_estimated: 768_000,
        }
    }

    fn ctx_for(
        spec: &TrainingJobSpec,
        assignment: &TrainingAssignment,
        record: &AcceptanceRecord,
    ) -> ReplayContext {
        ReplayContext::for_claim(spec, assignment, record, Hash::new([0x77; 32])).unwrap()
    }

    fn verifier(mode: CompareMode) -> TrainingVerifier {
        TrainingVerifier::new(PsConfig::default(), mode).unwrap()
    }

    /// ADR-0008 slash at the default config for the test spec:
    /// 40 × per_step_reward.
    const SLASH: u128 = 40 * PER_STEP_REWARD;

    /// Closure-driven mock executor (the real executor is A6).
    struct MockExecutor<F>(F)
    where
        F: Fn(&ReplayContext) -> Result<Vec<f32>, TrainingVerificationError> + Send + Sync;

    #[async_trait]
    impl<F> TrainingExecutor for MockExecutor<F>
    where
        F: Fn(&ReplayContext) -> Result<Vec<f32>, TrainingVerificationError> + Send + Sync,
    {
        async fn replay_step(
            &self,
            ctx: &ReplayContext,
        ) -> Result<Vec<f32>, TrainingVerificationError> {
            (self.0)(ctx)
        }
    }

    fn returning(
        values: Vec<f32>,
    ) -> MockExecutor<
        impl Fn(&ReplayContext) -> Result<Vec<f32>, TrainingVerificationError> + Send + Sync,
    > {
        MockExecutor(move |_ctx: &ReplayContext| Ok(values.clone()))
    }

    // ---- sampling ----

    fn synthetic_record(i: u64) -> AcceptanceRecord {
        AcceptanceRecord {
            worker: addr((i % 251) as u8),
            epoch: 0,
            clock: i % 10,
            range: range(0, 4),
            // Uniformly-spread synthetic update hashes
            update_hash: qfc_crypto::blake3_hash(&i.to_be_bytes()),
            flops_estimated: 768_000,
        }
    }

    #[test]
    fn test_should_verify_deterministic() {
        let v = verifier(CompareMode::Exact);
        for i in 0..100 {
            let r = synthetic_record(i);
            assert_eq!(
                v.should_verify(&r, &entropy()),
                v.should_verify(&r, &entropy())
            );
        }
        // A different epoch entropy yields a different sample set — the
        // decision is NOT a function of the record alone.
        let other = qfc_crypto::blake3_hash(b"a different epoch's params hash");
        assert!((0..1000)
            .map(synthetic_record)
            .any(|r| v.should_verify(&r, &entropy()) != v.should_verify(&r, &other)));
    }

    #[test]
    fn test_should_verify_rate_near_5_percent() {
        let v = verifier(CompareMode::Exact);
        let n = 10_000u64;
        let sampled = (0..n)
            .filter(|i| v.should_verify(&synthetic_record(*i), &entropy()))
            .count();
        let rate = sampled as f64 / n as f64;
        assert!(
            (0.03..=0.07).contains(&rate),
            "sampling rate {} outside the 3%-7% band ({} of {})",
            rate,
            sampled,
            n
        );
    }

    #[test]
    fn test_sample_filters_correctly() {
        let v = verifier(CompareMode::Exact);
        let records: Vec<AcceptanceRecord> = (0..1000).map(synthetic_record).collect();
        let sampled = v.sample(&records, &entropy());
        assert!(!sampled.is_empty());
        assert!(sampled.len() < records.len());
        for r in &sampled {
            assert!(v.should_verify(r, &entropy()));
        }
        let sampled_count = records
            .iter()
            .filter(|r| v.should_verify(r, &entropy()))
            .count();
        assert_eq!(sampled.len(), sampled_count);
    }

    /// FINDING B1 (+ review N3): the sampling entropy must be
    /// worker-independent — the grinding attack and its fix, end to end.
    ///
    /// # Attack narrative
    ///
    /// Under the OLD scheme, `should_verify` derived its coin from
    /// `record.update_hash` ALONE — but the worker computes that hash
    /// itself, before pushing, and `flops_estimated` is a free grinding
    /// knob inside it (changing it changes the hash but not the training
    /// output; value bit-tweaks are a second knob). At a 5% sampling rate
    /// the attacker needs ~1.05 hash attempts per step to find a variant
    /// whose first 4 BE bytes land above the threshold — a step that is
    /// NEVER audited. Every step can then be fabricated with zero
    /// detection risk, which voids the ADR-0008 `p·S > (1−p)·r` economics
    /// entirely (effective p = 0).
    ///
    /// # Fix
    ///
    /// The NEW scheme mixes in `epoch_entropy` — the epoch's COMMITTED
    /// post-barrier params hash, which does not exist when the update is
    /// pushed — so the coin is `blake3(epoch_entropy ‖ update_hash)` and
    /// there is nothing for the worker to grind against at push time.
    #[test]
    fn test_grinding_attack_defeated_by_epoch_entropy() {
        let v = verifier(CompareMode::Exact);
        let threshold = (PsConfig::default().spot_check_rate * u32::MAX as f64) as u32;
        // The OLD scheme's sampling coin: first 4 BE bytes of update_hash —
        // fully computable by the worker at push time.
        let old_scheme_sampled = |r: &AcceptanceRecord| {
            u32::from_be_bytes(r.update_hash.as_bytes()[..4].try_into().unwrap()) < threshold
        };

        // Fixed but UNKNOWN to the "worker" while grinding: the grinding
        // loop below never consults it (it cannot — at push time the
        // post-barrier params hash has not been computed yet).
        let epoch_entropy = entropy();

        // The attacker fabricates 1000 steps, grinding flops_estimated
        // (primary knob) and value bit-tweaks (secondary knob) until the
        // OLD scheme's coin says "not sampled".
        let mut ground: Vec<AcceptanceRecord> = Vec::with_capacity(1000);
        for i in 0..1000u64 {
            let mut upd = update(
                ((i % 250) + 1) as u8,
                0,
                i % 10,
                vec![i as f32, -1.0, 2.0, 0.5],
            );
            let mut attempts = 0u32;
            let rec = loop {
                let rec = AcceptanceRecord::for_update(&upd);
                if !old_scheme_sampled(&rec) {
                    break rec;
                }
                attempts += 1;
                upd.flops_estimated += 1;
                if attempts % 4 == 0 {
                    // Mix in the second knob: flip the LSB of a value.
                    upd.values[0] = f32::from_bits(upd.values[0].to_bits() ^ 1);
                }
            };
            ground.push(rec);
        }

        // The vulnerability, documented: the attacker drove OLD-scheme
        // sampling to exactly zero over 1000 fabricated steps.
        assert_eq!(
            ground.iter().filter(|r| old_scheme_sampled(r)).count(),
            0,
            "old scheme: a grinding worker fully evades sampling"
        );

        // The fix: mixing the post-hoc epoch entropy re-randomizes the
        // coin, so the SAME ground records still sample at ~5%.
        let sampled = ground
            .iter()
            .filter(|r| v.should_verify(r, &epoch_entropy))
            .count();
        let rate = sampled as f64 / ground.len() as f64;
        assert!(
            (0.03..=0.07).contains(&rate),
            "new scheme: ground records must still sample at ~5%, got {} ({} of {})",
            rate,
            sampled,
            ground.len()
        );
    }

    // ---- constructors ----

    #[test]
    fn test_tolerance_band_epsilon_domain() {
        assert!(CompareMode::tolerance_band(0.01, 1e-6).is_ok());
        assert!(CompareMode::tolerance_band(0.01, 0.0).is_ok());
        for bad in [0.0, -0.5, 1.0, 1.5, f64::NAN] {
            assert!(matches!(
                CompareMode::tolerance_band(bad, 1e-6),
                Err(TrainingVerificationError::InvalidEpsilon(_))
            ));
        }
        for bad_floor in [-1e-9, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(matches!(
                CompareMode::tolerance_band(0.01, bad_floor),
                Err(TrainingVerificationError::InvalidAbsFloor(_))
            ));
        }
        // Verifier::new re-checks a directly-constructed mode...
        assert!(matches!(
            TrainingVerifier::new(
                PsConfig::default(),
                CompareMode::ToleranceBand {
                    epsilon: 2.0,
                    abs_floor: 1e-6
                }
            ),
            Err(TrainingVerificationError::InvalidEpsilon(_))
        ));
        assert!(matches!(
            TrainingVerifier::new(
                PsConfig::default(),
                CompareMode::ToleranceBand {
                    epsilon: 0.01,
                    abs_floor: -1.0
                }
            ),
            Err(TrainingVerificationError::InvalidAbsFloor(_))
        ));
        // ...and the PsConfig.
        let bad_cfg = PsConfig {
            trim_beta: 0.6,
            ..PsConfig::default()
        };
        assert!(matches!(
            TrainingVerifier::new(bad_cfg, CompareMode::Exact),
            Err(TrainingVerificationError::InvalidConfig(_))
        ));
    }

    // ---- exact mode ----

    #[tokio::test]
    async fn test_exact_honest_replay_passes() {
        let values = vec![0.5f32, -1.25, 2.0, 0.0];
        let upd = update(1, 0, 3, values.clone());
        let claim = AcceptanceRecord::for_update(&upd);
        let ctx = ctx_for(&spec(4), &assignment(1, 0), &claim);

        let v = verifier(CompareMode::Exact);
        let verdict = v
            .verify_step(&claim, &ctx, None, &returning(values))
            .await
            .unwrap();
        assert!(verdict.passed);
        assert_eq!(verdict.mode_used, CompareMode::Exact);
        assert!(verdict.penalty.is_none());
    }

    #[tokio::test]
    async fn test_exact_one_bit_difference_fails_with_penalty() {
        let values = vec![0.5f32, -1.25, 2.0, 0.0];
        let upd = update(1, 0, 3, values.clone());
        let claim = AcceptanceRecord::for_update(&upd);
        let ctx = ctx_for(&spec(4), &assignment(1, 0), &claim);

        // Flip one bit of one value.
        let mut tampered = values.clone();
        tampered[2] = f32::from_bits(tampered[2].to_bits() ^ 1);
        assert_ne!(values, tampered);

        let v = verifier(CompareMode::Exact);
        let verdict = v
            .verify_step(&claim, &ctx, None, &returning(tampered))
            .await
            .unwrap();
        assert!(!verdict.passed);
        let penalty = verdict.penalty.expect("failed verdict carries a penalty");
        assert_eq!(penalty.slash_amount, SLASH); // 40 × per_step_reward
        assert!(penalty.void_epoch_records);
        assert_eq!(penalty.jail_duration_ms, INVALID_TRAINING_JAIL_MS); // 6h
                                                                        // FINDING N2: the penalty is self-identifying for standalone A5
                                                                        // application.
        assert_eq!(penalty.worker, addr(1));
        assert_eq!(penalty.job_id, job_hash(1));
        assert_eq!(penalty.epoch, 0);
    }

    #[tokio::test]
    async fn test_replay_infra_error_is_err_not_verdict() {
        let upd = update(1, 0, 3, vec![1.0; 4]);
        let claim = AcceptanceRecord::for_update(&upd);
        let ctx = ctx_for(&spec(4), &assignment(1, 0), &claim);

        let failing = MockExecutor(|_: &ReplayContext| {
            Err(TrainingVerificationError::ReplayFailed(
                "data shard unavailable".to_string(),
            ))
        });
        let v = verifier(CompareMode::Exact);
        assert!(matches!(
            v.verify_step(&claim, &ctx, None, &failing).await,
            Err(TrainingVerificationError::ReplayFailed(_))
        ));

        // Wrong-shape executor output is also an infra error, never a
        // false-failure verdict against the worker.
        let wrong_len = returning(vec![1.0; 3]);
        assert!(matches!(
            v.verify_step(&claim, &ctx, None, &wrong_len).await,
            Err(TrainingVerificationError::ReplayFailed(_))
        ));
    }

    // ---- tolerance mode ----

    #[tokio::test]
    async fn test_tolerance_within_band_passes() {
        let claimed_values = vec![1.0f32, -2.0, 0.5, 4.0];
        let upd = update(1, 0, 3, claimed_values.clone());
        let claim = AcceptanceRecord::for_update(&upd);
        let ctx = ctx_for(&spec(4), &assignment(1, 0), &claim);

        // Per-coordinate diffs (0.02, 0.01, 0, 0.01) vs bands
        // ε × |replayed_i| = (0.051, 0.0995, 0.025, 0.1995) at ε = 0.05.
        let replayed = vec![1.02f32, -1.99, 0.5, 3.99];
        let v = verifier(CompareMode::tolerance_band(0.05, 1e-6).unwrap());
        let verdict = v
            .verify_step(&claim, &ctx, Some(&upd), &returning(replayed))
            .await
            .unwrap();
        assert!(verdict.passed, "{}", verdict.details);
        assert!(verdict.penalty.is_none());
    }

    #[tokio::test]
    async fn test_tolerance_outside_band_fails_with_penalty() {
        let claimed_values = vec![1.0f32, -2.0, 0.5, 4.0];
        let upd = update(1, 0, 3, claimed_values.clone());
        let claim = AcceptanceRecord::for_update(&upd);
        let ctx = ctx_for(&spec(4), &assignment(1, 0), &claim);

        // Coordinate 2 off by 1.0; its band is max(0.01 × 1.5, 1e-6) = 0.015.
        let replayed = vec![1.0f32, -2.0, 1.5, 4.0];
        let v = verifier(CompareMode::tolerance_band(0.01, 1e-6).unwrap());
        let verdict = v
            .verify_step(&claim, &ctx, Some(&upd), &returning(replayed))
            .await
            .unwrap();
        assert!(!verdict.passed);
        // Failure details name the worst coordinate and its band.
        assert!(
            verdict.details.contains("coordinate 2"),
            "details must name the worst coordinate: {}",
            verdict.details
        );
        assert!(
            verdict.details.contains("band"),
            "details must state the allowed band: {}",
            verdict.details
        );
        let penalty = verdict.penalty.expect("failed verdict carries a penalty");
        assert_eq!(penalty.slash_amount, SLASH);
        assert!(penalty.void_epoch_records);
        assert_eq!(penalty.jail_duration_ms, INVALID_TRAINING_JAIL_MS);
        assert_eq!(penalty.worker, addr(1));
        assert_eq!(penalty.job_id, job_hash(1));
        assert_eq!(penalty.epoch, 0);
    }

    /// FINDING S1 regression: under the OLD global ∞-norm band
    /// (`‖diff‖∞ / max|replayed| <= ε`), one large honest coordinate
    /// widened the band for EVERY coordinate — a targeted poison on a
    /// near-zero coordinate sailed through. The per-coordinate band
    /// catches it.
    #[tokio::test]
    async fn test_tolerance_large_coordinate_cannot_hide_targeted_poison() {
        // Honest replay: one big gradient coordinate (100.0), one tiny
        // (0.001). Claim poisons the tiny coordinate by +0.499.
        let claimed_values = vec![100.0f32, 0.5, 0.0, 0.0];
        let upd = update(1, 0, 3, claimed_values);
        let claim = AcceptanceRecord::for_update(&upd);
        let ctx = ctx_for(&spec(4), &assignment(1, 0), &claim);
        let replayed = vec![100.0f32, 0.001, 0.0, 0.0];

        // OLD scheme would have PASSED this: ‖diff‖∞ = 0.499,
        // scale = 100 → ratio 0.005 <= ε = 0.01. The per-coordinate band
        // for coordinate 1 is max(0.01 × 0.001, 1e-4) ≈ 1e-4 ≪ 0.499.
        let v = verifier(CompareMode::tolerance_band(0.01, 1e-4).unwrap());
        let verdict = v
            .verify_step(&claim, &ctx, Some(&upd), &returning(replayed))
            .await
            .unwrap();
        assert!(
            !verdict.passed,
            "poison must be caught: {}",
            verdict.details
        );
        assert!(
            verdict.details.contains("coordinate 1"),
            "details must name the poisoned coordinate: {}",
            verdict.details
        );
        assert!(verdict.penalty.is_some());
    }

    #[tokio::test]
    async fn test_tolerance_missing_claimed_update() {
        let upd = update(1, 0, 3, vec![1.0; 4]);
        let claim = AcceptanceRecord::for_update(&upd);
        let ctx = ctx_for(&spec(4), &assignment(1, 0), &claim);

        let v = verifier(CompareMode::tolerance_band(0.01, 1e-6).unwrap());
        assert!(matches!(
            v.verify_step(&claim, &ctx, None, &returning(vec![1.0; 4]))
                .await,
            Err(TrainingVerificationError::MissingClaimedUpdate)
        ));
    }

    #[tokio::test]
    async fn test_tolerance_tampered_claimed_update() {
        let upd = update(1, 0, 3, vec![1.0; 4]);
        let claim = AcceptanceRecord::for_update(&upd);
        let ctx = ctx_for(&spec(4), &assignment(1, 0), &claim);

        // Operator serves an update body that does NOT hash to the accepted
        // record's update_hash.
        let mut tampered = upd.clone();
        tampered.values[0] = 9.0;
        let v = verifier(CompareMode::tolerance_band(0.01, 1e-6).unwrap());
        assert!(matches!(
            v.verify_step(&claim, &ctx, Some(&tampered), &returning(vec![1.0; 4]))
                .await,
            Err(TrainingVerificationError::ClaimedUpdateHashMismatch { .. })
        ));
    }

    /// The abs_floor exists for near-zero replayed coordinates, where the
    /// relative band `ε × |replayed_i|` collapses to ~0 and benign kernel
    /// noise would fail honest workers.
    #[tokio::test]
    async fn test_tolerance_abs_floor_near_zero_coordinates() {
        // Claimed identical to the zero replay: in band even at floor 0.
        let v0 = verifier(CompareMode::tolerance_band(0.01, 0.0).unwrap());
        let zeros = update(1, 0, 3, vec![0.0; 4]);
        let claim = AcceptanceRecord::for_update(&zeros);
        let ctx = ctx_for(&spec(4), &assignment(1, 0), &claim);
        let verdict = v0
            .verify_step(&claim, &ctx, Some(&zeros), &returning(vec![0.0; 4]))
            .await
            .unwrap();
        assert!(verdict.passed);

        // Tiny noise (1e-7) on a zero replayed coordinate:
        // - floor 1e-6 absorbs it (honest GPU noise passes),
        // - floor 0 means band 0 — any nonzero diff fails.
        let noisy = update(1, 0, 3, vec![1e-7, 0.0, 0.0, 0.0]);
        let claim = AcceptanceRecord::for_update(&noisy);
        let ctx = ctx_for(&spec(4), &assignment(1, 0), &claim);

        let v_floor = verifier(CompareMode::tolerance_band(0.01, 1e-6).unwrap());
        let verdict = v_floor
            .verify_step(&claim, &ctx, Some(&noisy), &returning(vec![0.0; 4]))
            .await
            .unwrap();
        assert!(verdict.passed, "{}", verdict.details);

        let verdict = v0
            .verify_step(&claim, &ctx, Some(&noisy), &returning(vec![0.0; 4]))
            .await
            .unwrap();
        assert!(!verdict.passed);
        assert!(verdict.penalty.is_some());
    }

    // ---- ReplayContext::for_claim ----

    #[test]
    fn test_for_claim_ok_and_cross_checks() {
        let s = spec(4);
        let a = assignment(1, 0);
        let upd = update(1, 0, 3, vec![1.0; 4]);
        let record = AcceptanceRecord::for_update(&upd);

        let ctx = ReplayContext::for_claim(&s, &a, &record, Hash::new([0x77; 32])).unwrap();
        assert_eq!(ctx.job_id, s.job_id);
        assert_eq!(ctx.worker, addr(1));
        assert_eq!(ctx.epoch, 0);
        assert_eq!(ctx.clock, 3);
        assert_eq!(ctx.range, record.range);
        assert_eq!(ctx.seed, a.seed);
        assert_eq!(ctx.data_shard_indices, a.data_shard_indices);
        assert_eq!(ctx.per_step_reward, s.per_step_reward);
        assert_eq!(ctx.params_version_hash, Hash::new([0x77; 32]));

        // Wrong worker.
        let wrong_worker = assignment(2, 0);
        assert!(matches!(
            ReplayContext::for_claim(&s, &wrong_worker, &record, Hash::ZERO),
            Err(TrainingVerificationError::ContextMismatch(_))
        ));

        // Wrong epoch.
        let wrong_epoch = assignment(1, 1);
        assert!(matches!(
            ReplayContext::for_claim(&s, &wrong_epoch, &record, Hash::ZERO),
            Err(TrainingVerificationError::ContextMismatch(_))
        ));

        // Wrong job id.
        let mut wrong_job = assignment(1, 0);
        wrong_job.job_id = job_hash(9);
        assert!(matches!(
            ReplayContext::for_claim(&s, &wrong_job, &record, Hash::ZERO),
            Err(TrainingVerificationError::ContextMismatch(_))
        ));

        // Clock at/over steps_per_epoch (10).
        let over_clock = AcceptanceRecord::for_update(&update(1, 0, 10, vec![1.0; 4]));
        assert!(matches!(
            ReplayContext::for_claim(&s, &a, &over_clock, Hash::ZERO),
            Err(TrainingVerificationError::ContextMismatch(_))
        ));
    }

    /// FINDING N1(a): the seed is recomputed, not trusted — a forged
    /// assignment with a wrong seed (which would replay a different
    /// trajectory and falsely penalize an honest worker) is rejected.
    #[test]
    fn test_for_claim_rejects_forged_seed() {
        let s = spec(4);
        let record = AcceptanceRecord::for_update(&update(1, 0, 3, vec![1.0; 4]));
        let mut forged = assignment(1, 0);
        forged.seed ^= 1;
        let err = ReplayContext::for_claim(&s, &forged, &record, Hash::ZERO).unwrap_err();
        assert!(
            matches!(err, TrainingVerificationError::ContextMismatch(_)),
            "{}",
            err
        );
        assert!(err.to_string().contains("seed"), "{}", err);
    }

    /// FINDING N1(b): the record's range must be a member of the job's
    /// range_partition — an accepted record can only name a registered
    /// PS-shard range.
    #[test]
    fn test_for_claim_rejects_range_outside_partition() {
        let s = spec(4); // partition: [0,4), [4,8)
        let a = assignment(1, 0);
        // Sub-range [0,3): inside the parameter space but not a partition
        // member.
        let record = AcceptanceRecord::for_update(&update(1, 0, 3, vec![1.0; 3]));
        let err = ReplayContext::for_claim(&s, &a, &record, Hash::ZERO).unwrap_err();
        assert!(
            matches!(err, TrainingVerificationError::ContextMismatch(_)),
            "{}",
            err
        );
        assert!(err.to_string().contains("range_partition"), "{}", err);
        // A straddling range [2,6) is rejected too.
        let mut straddling = AcceptanceRecord::for_update(&update(1, 0, 3, vec![1.0; 4]));
        straddling.range = range(2, 6);
        assert!(matches!(
            ReplayContext::for_claim(&s, &a, &straddling, Hash::ZERO),
            Err(TrainingVerificationError::ContextMismatch(_))
        ));
        // The second partition member [4,8) is accepted.
        let mut second = AcceptanceRecord::for_update(&update(1, 0, 3, vec![1.0; 4]));
        second.range = range(4, 8);
        assert!(ReplayContext::for_claim(&s, &a, &second, Hash::ZERO).is_ok());
    }

    #[tokio::test]
    async fn test_verify_step_rejects_mismatched_context() {
        let upd = update(1, 0, 3, vec![1.0; 4]);
        let claim = AcceptanceRecord::for_update(&upd);
        let mut ctx = ctx_for(&spec(4), &assignment(1, 0), &claim);
        ctx.clock = 4; // no longer describes the claim

        let v = verifier(CompareMode::Exact);
        assert!(matches!(
            v.verify_step(&claim, &ctx, None, &returning(vec![1.0; 4]))
                .await,
            Err(TrainingVerificationError::ContextMismatch(_))
        ));
    }

    // ---- end-to-end shape ----

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

    /// Full A4 pipeline shape: pool-assigned workers produce updates, the
    /// PS accepts them ([`AcceptanceRecord::for_update`]), the verifier
    /// samples the record set and re-executes the sampled claims through a
    /// mock executor — honest workers pass, the fabricator fails with the
    /// ADR-0008 penalty.
    #[tokio::test]
    async fn test_end_to_end_sample_and_verify() {
        let s = spec(4);
        let stake_floor = s.stake_floor(&PsConfig::default());
        let mut pool = TrainingPool::new(PsConfig::default()).unwrap();
        pool.submit(s.clone(), NOW).unwrap();
        let mut registry = MinerRegistry::new();
        for b in 1..=3 {
            registry.register(miner(b, stake_floor));
        }
        let assignments = pool.assign_epoch(&s.job_id, 0, &registry, NOW).unwrap();
        assert_eq!(assignments.len(), 3);

        // Each assigned worker pushes one accepted step per clock; worker 3
        // (the last assignment) fabricates — its claimed values were never
        // computed, so the honest replay diverges.
        let cheater = assignments[2].worker;
        let honest_values = |worker: &Address, clock: u64| -> Vec<f32> {
            (0..4)
                .map(|i| (worker.as_bytes()[0] as f32) + (clock as f32) * 0.5 + i as f32)
                .collect()
        };
        let mut updates: Vec<ParamUpdate> = Vec::new();
        for a in &assignments {
            for clock in 0..s.steps_per_epoch {
                let mut values = honest_values(&a.worker, clock);
                if a.worker == cheater {
                    values[0] += 1.0; // fabricated
                }
                updates.push(ParamUpdate {
                    worker: a.worker,
                    epoch: a.epoch,
                    clock,
                    range: range(0, 4),
                    values,
                    flops_estimated: s.flops_per_step(),
                });
            }
        }
        let records: Vec<AcceptanceRecord> =
            updates.iter().map(AcceptanceRecord::for_update).collect();

        // The mock executor replays the HONEST computation for everyone.
        let executor = MockExecutor(move |ctx: &ReplayContext| {
            (0..4)
                .map(|i| {
                    Ok((ctx.worker.as_bytes()[0] as f32) + (ctx.clock as f32) * 0.5 + i as f32)
                })
                .collect()
        });

        let v = verifier(CompareMode::Exact);
        // Epoch entropy: in production this is the epoch's committed
        // post-barrier params_hash (EpochOutcome.params_hash).
        let sampled = v.sample(&records, &entropy());
        // Verify EVERY record here (the sampled subset of 30 records may
        // be empty); the sample() path is exercised above and its rate in
        // its own test.
        let assignment_of = |worker: &Address| {
            assignments
                .iter()
                .find(|a| a.worker == *worker)
                .unwrap()
                .clone()
        };
        for claim in records.iter().chain(sampled.iter().copied()) {
            let ctx = ReplayContext::for_claim(
                &s,
                &assignment_of(&claim.worker),
                claim,
                Hash::new([0x77; 32]),
            )
            .unwrap();
            let verdict = v.verify_step(claim, &ctx, None, &executor).await.unwrap();
            if claim.worker == cheater {
                assert!(!verdict.passed, "fabricated step must fail");
                let p = verdict.penalty.unwrap();
                assert_eq!(p.worker, cheater);
                assert_eq!(p.job_id, s.job_id);
                assert_eq!(p.epoch, 0);
                assert_eq!(p.slash_amount, SLASH);
                assert!(p.void_epoch_records);
                assert_eq!(p.jail_duration_ms, INVALID_TRAINING_JAIL_MS);
            } else {
                assert!(verdict.passed, "honest step must pass: {}", verdict.details);
                assert!(verdict.penalty.is_none());
            }
        }
    }
}
