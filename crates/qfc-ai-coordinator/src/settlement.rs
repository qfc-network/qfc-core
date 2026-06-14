//! Pure, deterministic epoch settlement (ROADMAP-AI-V3 milestone A5; design
//! in `docs/adr/0009-chain-integration.md`, Decisions 1-4).
//!
//! [`settle_epoch`] is a **pure function** of one closed training epoch's
//! result: it computes *what* changes on-chain — a version commitment
//! ([`TrainingEpochCommit`]), pro-rata contribution rewards
//! ([`RewardCredit`]), and slashes ([`SlashResult`], offense
//! `InvalidTraining`) — but never *applies* them. Purity is the
//! cross-operator-agreement primitive (ADR-0009 Decision 1): two operators
//! that received the same accepted-update set produce a byte-identical
//! [`EpochSettlement`] (and therefore the same `commit_hash()`), regardless of
//! the order in which records or penalties arrive.
//!
//! # Deferred to A6 / node integration (ADR-0009 Decision 6)
//!
//! These are NOT done here; each is grep-able as `A6`:
//!
//! - **A6: state mutation at the epoch boundary** — actually crediting
//!   [`RewardCredit`] amounts to balances and applying [`SlashResult`] to
//!   validator/worker stake. Settlement produces the deltas; the node applies
//!   them against its State, validator set, and clock.
//! - **A6: absolute-amount slash entry point** in consensus. The existing
//!   `consensus::slash_validator(percent, jail_ms)` is percent-of-stake;
//!   training slashing is an absolute amount in
//!   [`SlashResult::slashed_amount`] (ADR-0009 Decision 4). Capping the slash
//!   at the worker's locked training stake also happens at application time
//!   (settlement has no stake view).
//! - **A6: P2P broadcast** of commitments and slashing evidence.
//! - **A6: N-of-M operator agreement** that selects the canonical
//!   `EpochOutcome` — settlement gives the deterministic-equality primitive
//!   only, not the voting that picks the winner. Note the agreement boundary:
//!   `commit_hash()` equality across operators certifies agreement on the
//!   COMMITMENT only (`params_hash` + `records_root` + counts) — it does NOT
//!   certify `rewards`/`slashes` agreement, since those depend on each
//!   operator's observed penalty set and are not inputs to `commit_hash`. A6's
//!   N-of-M must therefore agree on the outcome AND the penalty set before
//!   applying any reward/slash.
//! - **A6: VRF sampling entropy**, **A6: per-job stake-locking enforcement**,
//!   and **A6: signed `ParamUpdate`** are likewise out of scope.

use std::collections::{BTreeMap, BTreeSet};

use borsh::{BorshDeserialize, BorshSerialize};
use qfc_ps::{AcceptanceRecord, EpochOutcome, PsConfig};
use qfc_types::{Address, Hash, SlashResult, SlashableOffense, U256};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::training::TrainingJobSpec;
use crate::training_verification::TrainingPenalty;

/// Errors from epoch settlement. Settlement only fails on inconsistent
/// inputs — a well-formed epoch result always settles.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SettlementError {
    /// An accepted record names an epoch other than the outcome's epoch.
    #[error("accepted record for worker {worker} has epoch {record_epoch}, expected {epoch}")]
    RecordEpochMismatch {
        worker: Address,
        record_epoch: u64,
        epoch: u64,
    },

    /// A penalty names an epoch other than the outcome's epoch.
    #[error("penalty for worker {worker} has epoch {penalty_epoch}, expected {epoch}")]
    PenaltyEpochMismatch {
        worker: Address,
        penalty_epoch: u64,
        epoch: u64,
    },

    /// A penalty names a job other than the spec's job.
    #[error("penalty for worker {worker} has job {penalty_job}, expected {job_id}")]
    PenaltyJobMismatch {
        worker: Address,
        penalty_job: Hash,
        job_id: Hash,
    },

    /// Commitment persistence failed (used by [`crate::chain_store`]).
    #[error("storage error: {0}")]
    Storage(String),
}

/// The on-chain version-commitment anchor for one closed training epoch
/// (ADR-0009 Decision 2). The full `Vec<AcceptanceRecord>` stays off-chain;
/// only [`Self::records_root`] is committed, so any party holding the records
/// can recompute and verify the root, and two operators with the same set get
/// the same root.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub struct TrainingEpochCommit {
    /// Job this epoch belongs to.
    pub job_id: Hash,
    /// The closed epoch.
    pub epoch: u64,
    /// New model version: `EpochOutcome::params_hash` (duplicated here so the
    /// commit is self-contained — ADR-0009 D2).
    pub params_hash: Hash,
    /// blake3 over the canonically-sorted ACCEPTED records (the full accepted
    /// set; voiding is a payout concept and does not change the record set —
    /// see [`settle_epoch`]). Computed by [`records_root`].
    pub records_root: Hash,
    /// Number of accepted records this epoch (the full accepted set).
    pub accepted_count: u64,
    /// Sum of `flops_estimated` over ALL accepted records (including any
    /// later voided for payout — the commitment records what was accepted).
    pub total_flops_accepted: u64,
}

impl TrainingEpochCommit {
    /// blake3 over a canonical byte encoding of every field (big-endian
    /// integers). Two operators with identical fields produce the same hash —
    /// the cross-operator equality primitive (ADR-0009 Decision 1).
    pub fn commit_hash(&self) -> Hash {
        let mut hasher = blake3::Hasher::new();
        hasher.update(self.job_id.as_bytes());
        hasher.update(&self.epoch.to_be_bytes());
        hasher.update(self.params_hash.as_bytes());
        hasher.update(self.records_root.as_bytes());
        hasher.update(&self.accepted_count.to_be_bytes());
        hasher.update(&self.total_flops_accepted.to_be_bytes());
        Hash::new(*hasher.finalize().as_bytes())
    }
}

/// A pro-rata reward to credit to one worker (ADR-0009 Decision 3). Settlement
/// produces these; the node credits balances at the epoch boundary (A6).
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub struct RewardCredit {
    /// Worker to credit.
    pub worker: Address,
    /// Amount in wei.
    pub amount: u128,
}

/// The complete deterministic output of settling one epoch (ADR-0009
/// Decision 1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EpochSettlement {
    /// The on-chain version-commitment anchor.
    pub commit: TrainingEpochCommit,
    /// Pro-rata rewards, one per non-voided worker, sorted by worker bytes.
    pub rewards: Vec<RewardCredit>,
    /// Slashes, one per penalty, sorted by worker bytes (offense
    /// `InvalidTraining`).
    pub slashes: Vec<SlashResult>,
    /// Wei reserved off the top of the gross `reward_pool` to pay verification
    /// work (ADR-0008: verification is metered work "paid from the epoch reward
    /// pool"). Computed as `reward_pool × p` (p = `spot_check_rate` in basis
    /// points) and carved out BEFORE workers are pro-rated, so workers split
    /// only the remainder (`reward_pool − verifier_reserve_wei`). A6 pays
    /// verifiers from this reserve.
    pub verifier_reserve_wei: u128,
    /// FLOPs reserved for verification work (ADR-0008: `p × accepted FLOPs`).
    /// Informational for A6 (the work budget); the actual pay is
    /// [`Self::verifier_reserve_wei`].
    pub verifier_budget_flops: u64,
}

/// Canonical blake3 root over an acceptance-record set (ADR-0009 Decision 2).
///
/// Records are sorted by `(worker bytes, epoch, clock, range.start, range.end,
/// update_hash bytes)`, then each record's canonical bytes
/// (`worker ‖ epoch BE ‖ clock BE ‖ range.start BE ‖ range.end BE ‖
/// update_hash ‖ flops BE`) are streamed into one hasher. The result is
/// order-independent: any permutation of the same set yields the same root.
/// An empty set yields the well-defined hash of empty input.
pub fn records_root(records: &[AcceptanceRecord]) -> Hash {
    let mut sorted: Vec<&AcceptanceRecord> = records.iter().collect();
    sorted.sort_by(|a, b| {
        a.worker
            .as_bytes()
            .cmp(b.worker.as_bytes())
            .then(a.epoch.cmp(&b.epoch))
            .then(a.clock.cmp(&b.clock))
            .then(a.range.start.cmp(&b.range.start))
            .then(a.range.end.cmp(&b.range.end))
            .then(a.update_hash.as_bytes().cmp(b.update_hash.as_bytes()))
    });
    let mut hasher = blake3::Hasher::new();
    for r in sorted {
        hasher.update(r.worker.as_bytes());
        hasher.update(&r.epoch.to_be_bytes());
        hasher.update(&r.clock.to_be_bytes());
        hasher.update(&r.range.start.to_be_bytes());
        hasher.update(&r.range.end.to_be_bytes());
        hasher.update(r.update_hash.as_bytes());
        hasher.update(&r.flops_estimated.to_be_bytes());
    }
    Hash::new(*hasher.finalize().as_bytes())
}

/// Convert a [`TrainingPenalty`] into a [`SlashResult`] (ADR-0009 Decision 4).
///
/// The slash is an ABSOLUTE amount (`penalty.slash_amount`, =
/// `slash_multiple × per_step_reward` per ADR-0008), carried in
/// [`SlashResult::slashed_amount`] — NOT a percent of stake. Jail until
/// `now_ms + penalty.jail_duration_ms` (saturating).
///
/// A6: applying this against the worker's locked training stake (and capping
/// at it) is node work — settlement has no stake view.
pub fn penalty_to_slash(penalty: &TrainingPenalty, now_ms: u64) -> SlashResult {
    SlashResult {
        validator: penalty.worker,
        offense: SlashableOffense::InvalidTraining,
        slashed_amount: U256::from_u128(penalty.slash_amount),
        jail_until: now_ms.saturating_add(penalty.jail_duration_ms),
    }
}

/// Settle one closed training epoch into an [`EpochSettlement`] (ADR-0009
/// Decisions 1-4). Pure and deterministic: identical inputs in ANY order of
/// `outcome.accepted` / `penalties` yield a byte-identical result.
///
/// # Validation
///
/// - Every record in `outcome.accepted` must have `epoch == outcome.epoch`.
/// - Every penalty must have `epoch == outcome.epoch` and
///   `job_id == spec.job_id`.
/// - A penalty naming a worker that is not among the accepted set is still
///   slashed (it just earns no reward to begin with).
///
/// # Pool split (ADR-0008 + Decision 3)
///
/// `reward_pool` is the GROSS epoch pool. Settlement first reserves
/// `verifier_reserve_wei = reward_pool × p` off the top (p = `spot_check_rate`
/// in basis points; ADR-0008: verification is metered work paid from the same
/// pool), then distributes the remainder `worker_pool = reward_pool −
/// verifier_reserve_wei` to workers. A6 pays verifiers from the reserve.
///
/// # Rewards (Decision 3)
///
/// Penalized workers with `void_epoch_records == true` have ALL their records
/// excluded from the reward computation (ADR-0004 retroactive voiding). The
/// remaining records' `flops_estimated` are summed per worker; each
/// non-voided worker's reward is `worker_pool × worker_flops / total_flops`
/// (computed through a U256 intermediate so the product never overflows u128;
/// integer division — the rounding remainder is carried forward, left in the
/// pool, ADR-0009 D3). Zero-amount credits are omitted; rewards are sorted by
/// worker bytes.
///
/// # Commitment (Decision 2)
///
/// `accepted_count`, `total_flops_accepted` and `records_root` cover the FULL
/// accepted set, INCLUDING voided records: the commitment records what was
/// *accepted* during the epoch; voiding only affects payout (ADR-0004 voids
/// records "for that epoch" with respect to reward distribution). This keeps
/// the commitment a faithful, operator-agreeable account of the epoch's work,
/// independent of which penalties a given operator has observed at commit
/// time.
///
/// # Verifier budget (ADR-0008)
///
/// `verifier_budget_flops = round_down(total_flops_accepted × p)` (p =
/// `spot_check_rate` in basis points) — the metered verification WORK budget,
/// informational for A6. The verification PAY is [`EpochSettlement::verifier_reserve_wei`].
pub fn settle_epoch(
    outcome: &EpochOutcome,
    spec: &TrainingJobSpec,
    penalties: &[TrainingPenalty],
    reward_pool: u128,
    config: &PsConfig,
    now_ms: u64,
) -> Result<EpochSettlement, SettlementError> {
    // --- Validation ---
    for record in &outcome.accepted {
        if record.epoch != outcome.epoch {
            return Err(SettlementError::RecordEpochMismatch {
                worker: record.worker,
                record_epoch: record.epoch,
                epoch: outcome.epoch,
            });
        }
    }
    for penalty in penalties {
        if penalty.epoch != outcome.epoch {
            return Err(SettlementError::PenaltyEpochMismatch {
                worker: penalty.worker,
                penalty_epoch: penalty.epoch,
                epoch: outcome.epoch,
            });
        }
        if penalty.job_id != spec.job_id {
            return Err(SettlementError::PenaltyJobMismatch {
                worker: penalty.worker,
                penalty_job: penalty.job_id,
                job_id: spec.job_id,
            });
        }
    }

    // --- Void set (ADR-0004): workers whose records are excluded from payout ---
    // Keyed by raw address bytes since `Address` is not `Ord`.
    let voided: BTreeSet<[u8; 20]> = penalties
        .iter()
        .filter(|p| p.void_epoch_records)
        .map(|p| *p.worker.as_bytes())
        .collect();

    // --- Commitment covers the FULL accepted set (voiding is payout-only) ---
    let records_root = records_root(&outcome.accepted);
    let accepted_count = outcome.accepted.len() as u64;
    let total_flops_accepted: u64 = outcome
        .accepted
        .iter()
        .fold(0u64, |acc, r| acc.saturating_add(r.flops_estimated));

    let commit = TrainingEpochCommit {
        job_id: spec.job_id,
        epoch: outcome.epoch,
        params_hash: outcome.params_hash,
        records_root,
        accepted_count,
        total_flops_accepted,
    };

    // --- Verifier reserve (ADR-0008): carve p × pool off the GROSS pool ---
    // Convert spot_check_rate (f64, e.g. 0.05) to deterministic integer basis
    // points ONCE. This is the ONLY f64->int step in settlement; for a fixed
    // config value it is deterministic (same input -> same bps on every
    // operator). Clamp to a valid probability in [0, 10_000] bps.
    let p_bps: u128 = {
        let raw = (config.spot_check_rate * 10_000.0).round();
        raw.clamp(0.0, 10_000.0) as u128
    };
    // verifier_reserve_wei = reward_pool × p_bps / 10_000, via U256 so the
    // product never overflows u128. The quotient <= reward_pool <= u128::MAX,
    // so the narrowing back to u128 is always exact.
    let verifier_reserve_wei: u128 = (U256::from_u128(reward_pool) * U256::from_u128(p_bps)
        / U256::from_u128(10_000))
    .low_u128();
    // worker_pool is the remainder distributed to workers (reserve <= pool, so
    // this never underflows).
    let worker_pool: u128 = reward_pool - verifier_reserve_wei;

    // --- Rewards: pro-rata over non-voided FLOPs (ADR-0009 D3) ---
    // Aggregate per-worker flops (one credit per worker, not per record).
    // Keyed by raw address bytes (`Address` is not `Ord`); the BTreeMap keeps
    // iteration sorted by worker bytes deterministically.
    let mut worker_flops: BTreeMap<[u8; 20], u128> = BTreeMap::new();
    let mut total_flops: u128 = 0;
    for record in &outcome.accepted {
        let bytes = *record.worker.as_bytes();
        if voided.contains(&bytes) {
            continue;
        }
        let f = u128::from(record.flops_estimated);
        *worker_flops.entry(bytes).or_insert(0) += f;
        total_flops += f;
    }

    let mut rewards: Vec<RewardCredit> = Vec::new();
    for (worker_bytes, flops) in &worker_flops {
        // amount = worker_pool × flops / total_flops (remainder carried forward
        // / left in pool per ADR-0009 D3). Computed through a U256 intermediate
        // so `worker_pool × flops` never overflows u128 (realistic pools ~2^90
        // × epoch flops ~2^50 exceed u128). `total_flops` is a sum of the same
        // non-voided records' flops, so it is >= every `flops` here and
        // strictly positive whenever the map is non-empty — `checked_div`
        // makes the no-division-by-zero contract explicit (the all-zero-flops
        // edge yields total_flops == 0 -> 0). The quotient is bounded by
        // worker_pool <= u128::MAX, so `low_u128()` is an exact narrowing.
        let amount: u128 = if total_flops == 0 {
            0
        } else {
            let scaled = U256::from_u128(worker_pool) * U256::from_u128(*flops);
            let quotient = scaled
                .checked_div(U256::from_u128(total_flops))
                .expect("total_flops != 0 checked above");
            debug_assert!(
                quotient <= U256::from_u128(worker_pool),
                "pro-rata share must not exceed the worker pool"
            );
            quotient.low_u128()
        };
        if amount > 0 {
            rewards.push(RewardCredit {
                worker: Address::new(*worker_bytes),
                amount,
            });
        }
    }
    // worker_flops is a BTreeMap keyed by address bytes, so `rewards` is
    // already sorted by worker bytes; this keeps the guarantee local.

    // --- Slashes (ADR-0009 D4) ---
    // Sort by a TOTAL order over the slash content, not by validator bytes
    // alone: a single worker can incur multiple penalties in one epoch (one
    // per failed step), and a validator-only key is a partial order whose
    // stable-sort tie-break would leak the input order — breaking
    // cross-operator determinism (Decision 1). Tie-break on slashed_amount
    // then jail_until so equal-worker slashes have a deterministic order
    // regardless of how penalties were observed.
    let mut slashes: Vec<SlashResult> = penalties
        .iter()
        .map(|p| penalty_to_slash(p, now_ms))
        .collect();
    slashes.sort_by(|a, b| {
        a.validator
            .as_bytes()
            .cmp(b.validator.as_bytes())
            .then(a.slashed_amount.cmp(&b.slashed_amount))
            .then(a.jail_until.cmp(&b.jail_until))
    });

    // --- Verifier budget (ADR-0008): p × accepted FLOPs (informational) ---
    // Reuse the deterministic basis-points conversion for the FLOPs budget too,
    // via U256 (no overflow, exact narrowing of a value <= total_flops_accepted).
    let verifier_budget_flops: u64 = (U256::from_u128(u128::from(total_flops_accepted))
        * U256::from_u128(p_bps)
        / U256::from_u128(10_000))
    .low_u64();

    Ok(EpochSettlement {
        commit,
        rewards,
        slashes,
        verifier_reserve_wei,
        verifier_budget_flops,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use qfc_inference::{BackendType, GpuTier, ModelId, ShardEntry, ShardManifest};
    use qfc_ps::ParamRange;
    use rand::rngs::StdRng;
    use rand::{Rng, SeedableRng};

    fn addr(b: u8) -> Address {
        Address::new([b; 20])
    }

    fn hash(b: u8) -> Hash {
        Hash::new([b; 32])
    }

    fn range(start: u64, end: u64) -> ParamRange {
        ParamRange::new(start, end).unwrap()
    }

    /// A spec whose job_id is `hash(1)` — only `job_id` matters to settlement.
    fn spec() -> TrainingJobSpec {
        TrainingJobSpec {
            job_id: hash(1),
            model_id: ModelId::new("bert-tiny", "v1"),
            data_manifest: ShardManifest {
                shards: vec![ShardEntry {
                    cid: "QmShard0".to_string(),
                    hash: hash(9),
                    size_bytes: 1024,
                    layer_range: None,
                }],
                total_size_bytes: 1024,
                assembled_hash: hash(0xAA),
            },
            param_count: 1000,
            range_partition: vec![range(0, 1000)],
            epochs: 4,
            steps_per_epoch: 10,
            tokens_per_step: 128,
            per_step_reward: 1_000_000,
            required_backend: BackendType::Cpu,
            min_tier: GpuTier::Cold,
            min_memory_mb: 1024,
        }
    }

    fn record(worker: u8, epoch: u64, clock: u64, flops: u64) -> AcceptanceRecord {
        AcceptanceRecord {
            worker: addr(worker),
            epoch,
            clock,
            range: range(0, 1000),
            // A distinct hash per (worker, clock) so root sensitivity is real.
            update_hash: qfc_crypto::blake3_hash(&[worker, clock as u8]),
            flops_estimated: flops,
        }
    }

    fn penalty(worker: u8, epoch: u64, slash: u128, void: bool) -> TrainingPenalty {
        TrainingPenalty {
            worker: addr(worker),
            job_id: hash(1),
            epoch,
            slash_amount: slash,
            void_epoch_records: void,
            jail_duration_ms: crate::training_verification::INVALID_TRAINING_JAIL_MS,
        }
    }

    fn outcome(epoch: u64, accepted: Vec<AcceptanceRecord>) -> EpochOutcome {
        EpochOutcome {
            epoch,
            params_hash: hash(0x55),
            accepted,
        }
    }

    // ---- records_root ----

    #[test]
    fn test_records_root_order_independent() {
        let recs = vec![
            record(1, 0, 0, 100),
            record(2, 0, 0, 200),
            record(3, 0, 1, 300),
        ];
        let root = records_root(&recs);
        // Shuffle: same set, different order -> identical root.
        let shuffled = vec![recs[2].clone(), recs[0].clone(), recs[1].clone()];
        assert_eq!(records_root(&shuffled), root);
    }

    #[test]
    fn test_records_root_sensitive_to_changes() {
        let base = vec![record(1, 0, 0, 100), record(2, 0, 0, 200)];
        let root = records_root(&base);

        // Changed flops -> different root.
        let mut changed_flops = base.clone();
        changed_flops[0].flops_estimated = 101;
        assert_ne!(records_root(&changed_flops), root);

        // Changed worker -> different root.
        let mut changed_worker = base.clone();
        changed_worker[0].worker = addr(9);
        assert_ne!(records_root(&changed_worker), root);
    }

    #[test]
    fn test_records_root_empty_is_stable_known_value() {
        // Empty input -> hash of empty input, the blake3 IV digest. Stable.
        let expected = Hash::new(*blake3::Hasher::new().finalize().as_bytes());
        assert_eq!(records_root(&[]), expected);
        assert_eq!(records_root(&[]), records_root(&[]));
    }

    // ---- settle_epoch happy path ----

    #[test]
    fn test_settle_happy_path_pro_rata() {
        // 3 workers, flops 100 / 200 / 700 = total 1000; gross pool 10_000.
        // Verifier reserve = 10_000 × 0.05 = 500; worker_pool = 9_500.
        // Pro-rata over 9_500: 950, 1_900, 6_650 exactly (no remainder).
        let recs = vec![
            record(1, 0, 0, 100),
            record(2, 0, 0, 200),
            record(3, 0, 0, 700),
        ];
        let out = outcome(0, recs);
        let cfg = PsConfig::default();
        let s = settle_epoch(&out, &spec(), &[], 10_000, &cfg, 1_000).unwrap();

        assert_eq!(s.verifier_reserve_wei, 500);
        assert_eq!(
            s.rewards,
            vec![
                RewardCredit {
                    worker: addr(1),
                    amount: 950
                },
                RewardCredit {
                    worker: addr(2),
                    amount: 1_900
                },
                RewardCredit {
                    worker: addr(3),
                    amount: 6_650
                },
            ]
        );
        // Sorted by worker bytes.
        assert!(s
            .rewards
            .windows(2)
            .all(|w| w[0].worker.as_bytes() <= w[1].worker.as_bytes()));
        // Pool conservation (ADR-0008): sum(rewards) + reserve <= gross pool.
        let paid: u128 = s.rewards.iter().map(|r| r.amount).sum();
        assert!(paid + s.verifier_reserve_wei <= 10_000);

        // Commit reflects the full accepted set.
        assert_eq!(s.commit.accepted_count, 3);
        assert_eq!(s.commit.total_flops_accepted, 1000);
        assert_eq!(s.commit.params_hash, hash(0x55));
        assert_eq!(s.commit.job_id, hash(1));
        assert_eq!(s.commit.records_root, records_root(&out.accepted));
        assert!(s.slashes.is_empty());
    }

    #[test]
    fn test_settle_pro_rata_remainder_carried() {
        // flops 1 / 1 / 1 = 3; gross pool 10. Reserve = 10×500/10_000 = 0
        // (floor), so worker_pool = 10. Each gets 10×1/3 = 3 (floor); 9 paid,
        // remainder 1 carried (left in pool, not distributed).
        let recs = vec![record(1, 0, 0, 1), record(2, 0, 0, 1), record(3, 0, 0, 1)];
        let out = outcome(0, recs);
        let cfg = PsConfig::default();
        let s = settle_epoch(&out, &spec(), &[], 10, &cfg, 0).unwrap();
        assert_eq!(s.verifier_reserve_wei, 0);
        assert!(s.rewards.iter().all(|r| r.amount == 3));
        let paid: u128 = s.rewards.iter().map(|r| r.amount).sum();
        assert_eq!(paid, 9);
    }

    /// One reward per worker even when a worker has multiple accepted records.
    #[test]
    fn test_settle_aggregates_per_worker() {
        let recs = vec![
            record(1, 0, 0, 100),
            record(1, 0, 1, 100), // worker 1 again -> aggregate to 200
            record(2, 0, 0, 200),
        ];
        let out = outcome(0, recs);
        let cfg = PsConfig::default();
        let s = settle_epoch(&out, &spec(), &[], 4_000, &cfg, 0).unwrap();
        // gross 4_000; reserve = 200; worker_pool = 3_800. total flops 400;
        // worker1 200/400, worker2 200/400 -> 1_900 each.
        assert_eq!(s.verifier_reserve_wei, 200);
        assert_eq!(s.rewards.len(), 2);
        assert_eq!(
            s.rewards[0],
            RewardCredit {
                worker: addr(1),
                amount: 1_900
            }
        );
        assert_eq!(
            s.rewards[1],
            RewardCredit {
                worker: addr(2),
                amount: 1_900
            }
        );
        assert_eq!(s.commit.accepted_count, 3);
    }

    // ---- voiding ----

    #[test]
    fn test_settle_voiding_excludes_from_rewards_only() {
        // 3 workers, flops 100 / 200 / 700. Worker 3 is penalized with
        // void=true -> excluded from rewards; pool re-pro-rated over 100+200.
        let recs = vec![
            record(1, 0, 0, 100),
            record(2, 0, 0, 200),
            record(3, 0, 0, 700),
        ];
        let out = outcome(0, recs);
        let cfg = PsConfig::default();
        let pen = vec![penalty(3, 0, 40_000_000, true)];
        let s = settle_epoch(&out, &spec(), &pen, 3_000, &cfg, 5_000).unwrap();

        // gross 3_000; reserve = 150; worker_pool = 2_850. Worker 3 absent from
        // rewards; 1 and 2 split over remaining 300 flops.
        // 2_850×100/300 = 950, 2_850×200/300 = 1_900.
        assert_eq!(s.verifier_reserve_wei, 150);
        assert_eq!(
            s.rewards,
            vec![
                RewardCredit {
                    worker: addr(1),
                    amount: 950
                },
                RewardCredit {
                    worker: addr(2),
                    amount: 1_900
                },
            ]
        );
        assert!(s.rewards.iter().all(|r| r.worker != addr(3)));

        // CHOSEN SEMANTICS: voided records STILL count toward the commitment.
        assert_eq!(s.commit.accepted_count, 3);
        assert_eq!(s.commit.total_flops_accepted, 1000); // includes voided 700
        assert_eq!(s.commit.records_root, records_root(&out.accepted));

        // Slash present for worker 3.
        assert_eq!(s.slashes.len(), 1);
        assert_eq!(s.slashes[0].validator, addr(3));
        assert_eq!(s.slashes[0].offense, SlashableOffense::InvalidTraining);
        assert_eq!(s.slashes[0].slashed_amount, U256::from_u128(40_000_000));
        assert_eq!(
            s.slashes[0].jail_until,
            5_000 + crate::training_verification::INVALID_TRAINING_JAIL_MS
        );
    }

    /// A penalty for an unknown worker (not in accepted) is still slashed.
    #[test]
    fn test_settle_penalty_unknown_worker_still_slashed() {
        let recs = vec![record(1, 0, 0, 100), record(2, 0, 0, 100)];
        let out = outcome(0, recs);
        let cfg = PsConfig::default();
        let pen = vec![penalty(99, 0, 1234, true)];
        let s = settle_epoch(&out, &spec(), &pen, 1_000, &cfg, 0).unwrap();
        assert_eq!(s.slashes.len(), 1);
        assert_eq!(s.slashes[0].validator, addr(99));
        // Rewards unaffected (voiding an absent worker removes nothing).
        assert_eq!(s.rewards.len(), 2);
    }

    // ---- penalty_to_slash ----

    #[test]
    fn test_penalty_to_slash_fields() {
        let p = penalty(7, 3, 40_000_000, true);
        let slash = penalty_to_slash(&p, 1_000);
        assert_eq!(slash.validator, addr(7));
        assert_eq!(slash.offense, SlashableOffense::InvalidTraining);
        assert_eq!(slash.slashed_amount, U256::from_u128(40_000_000));
        assert_eq!(
            slash.jail_until,
            1_000 + crate::training_verification::INVALID_TRAINING_JAIL_MS
        );
    }

    #[test]
    fn test_penalty_to_slash_jail_saturates() {
        let mut p = penalty(7, 0, 1, true);
        p.jail_duration_ms = u64::MAX;
        let slash = penalty_to_slash(&p, 100);
        assert_eq!(slash.jail_until, u64::MAX);
    }

    // ---- validation errors ----

    #[test]
    fn test_settle_record_epoch_mismatch() {
        let recs = vec![record(1, 0, 0, 100), record(2, 5, 0, 100)]; // wrong epoch
        let out = outcome(0, recs);
        let cfg = PsConfig::default();
        assert_eq!(
            settle_epoch(&out, &spec(), &[], 1_000, &cfg, 0),
            Err(SettlementError::RecordEpochMismatch {
                worker: addr(2),
                record_epoch: 5,
                epoch: 0,
            })
        );
    }

    #[test]
    fn test_settle_penalty_epoch_mismatch() {
        let out = outcome(0, vec![record(1, 0, 0, 100)]);
        let cfg = PsConfig::default();
        let pen = vec![penalty(1, 7, 1, true)];
        assert_eq!(
            settle_epoch(&out, &spec(), &pen, 1_000, &cfg, 0),
            Err(SettlementError::PenaltyEpochMismatch {
                worker: addr(1),
                penalty_epoch: 7,
                epoch: 0,
            })
        );
    }

    #[test]
    fn test_settle_penalty_job_mismatch() {
        let out = outcome(0, vec![record(1, 0, 0, 100)]);
        let cfg = PsConfig::default();
        let mut p = penalty(1, 0, 1, true);
        p.job_id = hash(0xEE);
        assert_eq!(
            settle_epoch(&out, &spec(), &[p], 1_000, &cfg, 0),
            Err(SettlementError::PenaltyJobMismatch {
                worker: addr(1),
                penalty_job: hash(0xEE),
                job_id: hash(1),
            })
        );
    }

    // ---- verifier budget ----

    #[test]
    fn test_verifier_budget_is_rate_times_flops() {
        // total flops 1000, default spot_check_rate 0.05 -> 50.
        let recs = vec![record(1, 0, 0, 400), record(2, 0, 0, 600)];
        let out = outcome(0, recs);
        let cfg = PsConfig::default();
        let s = settle_epoch(&out, &spec(), &[], 1_000, &cfg, 0).unwrap();
        assert_eq!(s.verifier_budget_flops, 50);
    }

    #[test]
    fn test_verifier_reserve_carved_from_pool() {
        // total flops 1000, default rate 0.05. gross 100_000.
        // reserve = 100_000 × 0.05 = 5_000; worker_pool = 95_000.
        let recs = vec![record(1, 0, 0, 400), record(2, 0, 0, 600)];
        let out = outcome(0, recs);
        let cfg = PsConfig::default();
        let s = settle_epoch(&out, &spec(), &[], 100_000, &cfg, 0).unwrap();
        assert_eq!(s.verifier_reserve_wei, 5_000);
        // 95_000 × 400/1000 = 38_000; 95_000 × 600/1000 = 57_000.
        let paid: u128 = s.rewards.iter().map(|r| r.amount).sum();
        assert_eq!(paid, 95_000);
        // INVARIANT (pool conservation): sum(rewards) + reserve <= gross pool.
        assert!(paid + s.verifier_reserve_wei <= 100_000);
    }

    /// Finding 2: large pool × large epoch flops overflows u128 in a naive
    /// `pool × worker_flops` mul, but the U256 intermediate must settle cleanly.
    #[test]
    fn test_settle_large_pool_no_overflow() {
        // pool ~2^90 wei, two workers with ~2^49 flops each (u64-range). A naive
        // u128 `pool × flops` (~2^139) overflows; via U256 it must not.
        let big_pool: u128 = 1u128 << 90;
        let flops: u64 = 1u64 << 49;
        let recs = vec![record(1, 0, 0, flops), record(2, 0, 0, flops)];
        let out = outcome(0, recs);
        let cfg = PsConfig::default();
        let s = settle_epoch(&out, &spec(), &[], big_pool, &cfg, 0).unwrap();
        // worker_pool = big_pool - reserve; split 50/50 -> each = worker_pool/2.
        let worker_pool = big_pool - s.verifier_reserve_wei;
        assert_eq!(s.rewards.len(), 2);
        for r in &s.rewards {
            assert_eq!(r.amount, worker_pool / 2);
        }
        let paid: u128 = s.rewards.iter().map(|r| r.amount).sum();
        assert!(paid + s.verifier_reserve_wei <= big_pool);
    }

    /// Finding 5: non-empty accepted set where ALL non-voided records have
    /// flops_estimated == 0 and pool > 0. No panic (checked_div(0) guard),
    /// all rewards omitted, entire worker_pool carried forward, and the
    /// verifier reserve is still computed from the gross pool.
    #[test]
    fn test_settle_all_zero_flops_carries_whole_pool() {
        let recs = vec![record(1, 0, 0, 0), record(2, 0, 0, 0), record(3, 0, 1, 0)];
        let out = outcome(0, recs);
        let cfg = PsConfig::default();
        let s = settle_epoch(&out, &spec(), &[], 1_000_000, &cfg, 0).unwrap();
        // No reward credits (all amounts would be 0 -> omitted).
        assert!(s.rewards.is_empty());
        // Reserve still carved from the gross pool: 1_000_000 × 0.05 = 50_000.
        assert_eq!(s.verifier_reserve_wei, 50_000);
        // total_flops_accepted == 0 -> verifier_budget_flops == 0.
        assert_eq!(s.commit.total_flops_accepted, 0);
        assert_eq!(s.verifier_budget_flops, 0);
        // Entire worker_pool carried forward (nothing distributed).
        let paid: u128 = s.rewards.iter().map(|r| r.amount).sum();
        assert_eq!(paid, 0);
        assert_eq!(s.commit.accepted_count, 3);
    }

    // ---- determinism property test (seeded, no proptest dep) ----

    #[test]
    fn test_settle_deterministic_under_shuffle() {
        let mut rng = StdRng::seed_from_u64(0xA5_C0FFEE);
        let cfg = PsConfig::default();

        for _iter in 0..100 {
            let epoch = 0u64;
            let n = rng.gen_range(3..=12);
            // Build N random accepted records. Use a small worker pool so
            // per-worker aggregation and voiding actually bite.
            let mut recs: Vec<AcceptanceRecord> = Vec::with_capacity(n);
            for i in 0..n {
                let worker = rng.gen_range(1..=6u8);
                let flops = rng.gen_range(1..=10_000u64);
                recs.push(record(worker, epoch, i as u64, flops));
            }
            // M random penalties on workers in the pool.
            let m = rng.gen_range(0..=3);
            let mut pens: Vec<TrainingPenalty> = Vec::with_capacity(m);
            for _ in 0..m {
                let worker = rng.gen_range(1..=6u8);
                let void = rng.gen_bool(0.5);
                let slash = rng.gen_range(1..=u128::from(u64::MAX));
                pens.push(penalty(worker, epoch, slash, void));
            }
            let pool: u128 = rng.gen_range(0..=1_000_000_000u128);
            let now = rng.gen_range(0..=1_000_000u64);
            let out = outcome(epoch, recs.clone());

            let a = settle_epoch(&out, &spec(), &pens, pool, &cfg, now).unwrap();

            // Independently shuffle both input vectors.
            let mut recs_shuf = recs.clone();
            let mut pens_shuf = pens.clone();
            for i in (1..recs_shuf.len()).rev() {
                recs_shuf.swap(i, rng.gen_range(0..=i));
            }
            if !pens_shuf.is_empty() {
                for i in (1..pens_shuf.len()).rev() {
                    pens_shuf.swap(i, rng.gen_range(0..=i));
                }
            }
            let out_shuf = outcome(epoch, recs_shuf);
            let b = settle_epoch(&out_shuf, &spec(), &pens_shuf, pool, &cfg, now).unwrap();

            assert_eq!(a.commit.commit_hash(), b.commit.commit_hash());
            assert_eq!(a.commit, b.commit);
            assert_eq!(a.rewards, b.rewards);
            assert_eq!(a.slashes, b.slashes);
            assert_eq!(a.verifier_reserve_wei, b.verifier_reserve_wei);
            assert_eq!(a.verifier_budget_flops, b.verifier_budget_flops);

            // Pool conservation (ADR-0008): sum(rewards) + reserve <= gross pool.
            let paid: u128 = a.rewards.iter().map(|r| r.amount).sum();
            assert!(paid + a.verifier_reserve_wei <= pool);
        }
    }
}
