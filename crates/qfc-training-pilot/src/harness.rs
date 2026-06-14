//! The in-process ≥3-miner pilot loop + settlement application (ADR-0010
//! Decisions 1 & 4).
//!
//! [`run_pilot`] drives N simulated miners through the REAL A1–A5 APIs in one
//! process: real `MinerRegistry`/`TrainingPool` assignment, a real
//! `ShardService` doing trimmed-mean aggregation, the real `TrainingVerifier`,
//! and the real `settle_epoch`. The only thing simulated is the
//! network/process boundary between miners — not any protocol logic.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use qfc_ai_coordinator::{
    settle_epoch, CompareMode, EpochSettlement, MinerCapability, MinerRegistry, ReplayContext,
    TrainingChainStore, TrainingEpochCommit, TrainingJobSpec, TrainingPenalty, TrainingPool,
    TrainingVerifier,
};
use qfc_inference::{BackendType, GpuTier, ModelId};
use qfc_ps::{
    AcceptanceRecord, ParamRange, ParamUpdate, PsConfig, ShardService, ShardStore, TrimmedMean,
};
use qfc_types::{Address, Hash, SlashResult};

use crate::data::{build_manifest, synth_datastore};
use crate::executor::{PilotExecutor, VersionMap};
use crate::model::{compute_delta, mse_loss};

/// Wall-clock-ish base time for the (deterministic) pilot. Anything >= 60_000
/// is fine as long as miner `last_seen` is set within the liveness window.
const NOW_MS: u64 = 1_000_000;

/// Configuration for a pilot run.
#[derive(Clone, Debug)]
pub struct PilotConfig {
    pub num_miners: usize,
    pub num_cheaters: usize,
    pub dim: usize,
    pub num_shards: usize,
    pub epochs: u64,
    pub steps_per_epoch: u64,
    pub reward_pool_per_epoch: u128,
    pub lr: f64,
}

impl Default for PilotConfig {
    fn default() -> Self {
        Self {
            num_miners: 4,
            num_cheaters: 1,
            dim: 8,
            num_shards: 4,
            epochs: 10,
            steps_per_epoch: 4,
            reward_pool_per_epoch: 1_000_000_000_000_000_000, // 1e18 wei
            lr: 0.3,
        }
    }
}

/// What one pilot run produced.
#[derive(Clone, Debug)]
pub struct PilotReport {
    /// Per-epoch global MSE loss of the model AFTER that epoch's barrier.
    pub losses: Vec<f64>,
    /// The loss BEFORE any training (epoch-0 start).
    pub initial_loss: f64,
    /// How many planted cheaters were caught by sampled re-execution.
    pub cheaters_caught: usize,
    /// Final balances (rewards credited) keyed by address.
    pub final_balances: HashMap<Address, u128>,
    /// Final stakes (after slashing) keyed by address.
    pub final_stakes: HashMap<Address, u128>,
    /// Per-epoch persisted commitments.
    pub commits: Vec<TrainingEpochCommit>,
    /// Addresses planted as cheaters (for assertions/printing).
    pub cheater_addresses: Vec<Address>,
}

/// Apply an [`EpochSettlement`] to plain in-memory balance/stake maps — the
/// reference implementation for real node wiring (ADR-0010 Decision 4).
///
/// - Each [`qfc_ai_coordinator::RewardCredit`] is credited to `balances`.
/// - Each [`SlashResult`] deducts `min(slashed_amount, locked stake)` from
///   `stakes` (the slash is capped at the worker's locked training stake —
///   settlement has no stake view, so the cap is applied here, ADR-0009 D4).
pub fn apply_settlement(
    s: &EpochSettlement,
    balances: &mut HashMap<Address, u128>,
    stakes: &mut HashMap<Address, u128>,
) {
    for credit in &s.rewards {
        *balances.entry(credit.worker).or_insert(0) += credit.amount;
    }
    for slash in &s.slashes {
        apply_slash(slash, stakes);
    }
}

fn apply_slash(slash: &SlashResult, stakes: &mut HashMap<Address, u128>) {
    let locked = stakes.get(&slash.validator).copied().unwrap_or(0);
    let amount = slash.slashed_amount.low_u128().min(locked);
    stakes.insert(slash.validator, locked - amount);
}

/// Deterministic miner address: byte `i+1` repeated (avoids the all-zero
/// address). Cheaters are the LAST `num_cheaters` miners.
fn miner_addr(i: usize) -> Address {
    Address::new([(i as u8) + 1; 20])
}

/// Build the job's range partition: split `[0, dim)` into `num_ranges`
/// near-equal contiguous ranges covering the whole space (A3 requires a
/// sorted, disjoint, contiguous cover of `[0, param_count)`).
fn partition(dim: u64, num_ranges: u64) -> Vec<ParamRange> {
    let num_ranges = num_ranges.clamp(1, dim);
    let base = dim / num_ranges;
    let rem = dim % num_ranges;
    let mut out = Vec::new();
    let mut start = 0u64;
    for r in 0..num_ranges {
        let extra = if r < rem { 1 } else { 0 };
        let end = start + base + extra;
        out.push(ParamRange::new(start, end).expect("non-empty range"));
        start = end;
    }
    out
}

/// The range a worker pushes to at a given step `clock` (round-robin over the
/// partition). With `steps_per_epoch >= num_ranges` every worker touches every
/// range at least once per epoch, so each range collects all workers'
/// contributions (≥ `min_updates`).
fn range_for_clock(ranges: &[ParamRange], clock: u64) -> ParamRange {
    ranges[(clock as usize) % ranges.len()]
}

/// Run the in-process pilot. Returns a [`PilotReport`].
///
/// # Panics on internal protocol errors
///
/// The pilot drives the real APIs and treats any `Err` from them as a pilot
/// bug (unwrap), since the harness constructs all inputs to be valid.
pub fn run_pilot(cfg: PilotConfig) -> PilotReport {
    assert!(cfg.num_miners >= 3, "pilot needs >= 3 miners (ADR-0010)");
    assert!(
        cfg.num_cheaters < cfg.num_miners,
        "cheaters must be a minority"
    );

    let config = ps_config_for(cfg.num_miners);
    let dim = cfg.dim as u64;

    // --- Data + manifest (synthetic; real-but-local manifest, ADR-0010 D3) ---
    let rows_per_shard = 24;
    let store = Arc::new(synth_datastore(
        cfg.dim,
        cfg.num_shards,
        rows_per_shard,
        0.0, // noiseless so honest convergence is crisp
    ));
    let manifest = build_manifest(&store);
    let full_data = store.full();

    // --- Range partition: a few contiguous ranges covering [0, dim) ---
    let num_ranges = (cfg.num_miners as u64)
        .min(dim)
        .min(cfg.steps_per_epoch.max(1));
    let ranges = partition(dim, num_ranges);

    // --- Job spec (CPU-only for exact-match verification, ADR-0005) ---
    let per_step_reward: u128 = 1_000_000;
    let spec = TrainingJobSpec {
        job_id: Hash::new([0x42; 32]),
        model_id: ModelId::new("pilot-linreg", "v1"),
        data_manifest: manifest,
        param_count: dim,
        range_partition: ranges.clone(),
        epochs: cfg.epochs,
        steps_per_epoch: cfg.steps_per_epoch,
        tokens_per_step: 128,
        per_step_reward,
        required_backend: BackendType::Cpu,
        min_tier: GpuTier::Cold,
        min_memory_mb: 1024,
    };
    spec.validate(&config)
        .expect("pilot job spec must be valid");
    let stake_floor = spec.stake_floor(&config);

    // --- Registry: N miners, all CPU/Cold, staked above the floor ---
    let mut registry = MinerRegistry::new();
    let mut initial_stake = HashMap::new();
    let starting_stake = stake_floor.saturating_mul(4); // comfortable margin
    for i in 0..cfg.num_miners {
        let mut cap = MinerCapability::new(
            miner_addr(i),
            BackendType::Cpu,
            GpuTier::Cold,
            2048,
            starting_stake,
        );
        cap.last_seen = NOW_MS; // keep miners active within the liveness window
        registry.register(cap);
        initial_stake.insert(miner_addr(i), starting_stake);
    }

    // Cheaters are the LAST num_cheaters miners.
    let cheater_set: Vec<Address> = (cfg.num_miners - cfg.num_cheaters..cfg.num_miners)
        .map(miner_addr)
        .collect();
    let is_cheater = |a: &Address| cheater_set.contains(a);

    // --- Pool ---
    let mut pool = TrainingPool::new(config.clone()).expect("valid pool config");
    pool.submit(spec.clone(), NOW_MS).expect("submit job");

    // --- PS shard service over the whole [0, dim) space ---
    let ps_store = ShardStore::open_temp(ParamRange::new(0, dim).unwrap()).unwrap();
    // Deterministic small initial params (zeros).
    let init_params = vec![0.0f32; cfg.dim];
    ps_store
        .set_range(ParamRange::new(0, dim).unwrap(), &init_params)
        .unwrap();
    let mut service = ShardService::new(
        ps_store,
        config.clone(),
        0,
        ranges.clone(),
        cfg.steps_per_epoch,
    )
    .unwrap();

    // --- Executor + version map (epoch-start params keyed by params_hash) ---
    let versions: VersionMap = Arc::new(Mutex::new(HashMap::new()));
    let executor = PilotExecutor::new(Arc::clone(&store), Arc::clone(&versions), cfg.lr);

    let verifier =
        TrainingVerifier::new(config.clone(), CompareMode::Exact).expect("valid verifier");
    let chain = TrainingChainStore::open_temp().expect("open chain store");

    // --- Settlement application state ---
    let mut balances: HashMap<Address, u128> = HashMap::new();
    let mut stakes: HashMap<Address, u128> = initial_stake.clone();

    let initial_loss = mse_loss(&init_params, &full_data);
    let mut report = PilotReport {
        losses: Vec::new(),
        initial_loss,
        cheaters_caught: 0,
        final_balances: HashMap::new(),
        final_stakes: HashMap::new(),
        commits: Vec::new(),
        cheater_addresses: cheater_set.clone(),
    };

    let mut total_cheaters_caught = 0usize;
    let mut any_honest_passed = false;

    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("tokio runtime");

    for epoch in 0..cfg.epochs {
        // 1. Epoch-start params version: the version all this epoch's steps pull
        //    (SSP — no mid-epoch apply, one version per epoch). Record it in the
        //    executor's map BEFORE the steps run, keyed by the store's
        //    params_hash at epoch start (= the version a verifier replays at).
        let epoch_start_params = service
            .pull(ParamRange::new(0, dim).unwrap())
            .expect("pull full params");
        let version_hash = service.store().params_hash().expect("params hash");
        executor.record_version(version_hash, epoch_start_params.clone());

        // 2. Assign the epoch.
        let assignments = pool
            .assign_epoch(&spec.job_id, epoch, &registry, NOW_MS)
            .expect("assign epoch");

        // 3. Each worker takes steps_per_epoch pushes (clock 0..steps), each to
        //    one range (round-robin). Honest workers push the real delta;
        //    cheaters push a corrupted delta. Track each pushed update so
        //    verification can supply the claimed update and replay matches.
        let mut pushed: HashMap<(Address, u64, ParamRange), ParamUpdate> = HashMap::new();
        for assignment in &assignments {
            let worker = assignment.worker;
            let data = store.assemble(&assignment.data_shard_indices);
            for clock in 0..cfg.steps_per_epoch {
                let range = range_for_clock(&ranges, clock);
                let honest_delta = compute_delta(
                    &epoch_start_params,
                    &data,
                    assignment.seed,
                    clock,
                    range,
                    cfg.lr,
                );
                let values = if is_cheater(&worker) {
                    // A corrupted delta: a large fixed poison vector, NOT what
                    // honest compute_delta produces → fails exact-match replay.
                    vec![1e3f32; range.len() as usize]
                } else {
                    honest_delta
                };
                let update = ParamUpdate {
                    worker,
                    epoch,
                    clock,
                    range,
                    values,
                    flops_estimated: spec.flops_per_step(),
                };
                service.push(update.clone()).expect("push accepted");
                pushed.insert((worker, clock, range), update);
            }
        }

        // 4. Barrier: trimmed-mean aggregation (the production rule).
        let outcome = service
            .end_epoch_with_rule(&TrimmedMean::new(config.trim_beta).unwrap())
            .expect("end epoch");

        // 5. Verification: re-execute the cheaters' records, the 5%-sampled
        //    set, AND (deterministically) the first honest record so honest
        //    coverage is guaranteed every epoch. epoch_entropy is the committed
        //    post-barrier params_hash (ADR-0005). The cheaters are audited
        //    explicitly because the pilot proves DETECTION, not the sampling
        //    lottery (which at 5% would rarely pick the planted cheater).
        let sampled = verifier.sample(&outcome.accepted, &outcome.params_hash);
        let first_honest = outcome.accepted.iter().position(|r| !is_cheater(&r.worker));
        let mut to_verify: Vec<&AcceptanceRecord> = Vec::new();
        for (idx, r) in outcome.accepted.iter().enumerate() {
            let in_sample = sampled.iter().any(|s| std::ptr::eq(*s, r));
            if is_cheater(&r.worker) || in_sample || Some(idx) == first_honest {
                to_verify.push(r);
            }
        }

        let mut penalties: Vec<TrainingPenalty> = Vec::new();
        let mut honest_passed = false;
        for record in to_verify {
            let assignment = assignments
                .iter()
                .find(|a| a.worker == record.worker && a.epoch == record.epoch)
                .expect("assignment for record");
            let ctx = ReplayContext::for_claim(&spec, assignment, record, version_hash)
                .expect("build replay context");
            let claimed = pushed
                .get(&(record.worker, record.clock, record.range))
                .expect("pushed update for record");
            let verdict = rt
                .block_on(verifier.verify_step(record, &ctx, Some(claimed), &executor))
                .expect("verify_step infra ok");
            if verdict.passed {
                if !is_cheater(&record.worker) {
                    honest_passed = true;
                }
            } else if let Some(p) = verdict.penalty {
                penalties.push(p);
            }
        }

        // De-dup cheaters caught this epoch (a cheater may produce multiple
        // failing records).
        let caught_this_epoch: std::collections::HashSet<Address> =
            penalties.iter().map(|p| p.worker).collect();
        total_cheaters_caught = total_cheaters_caught.max(caught_this_epoch.len());

        // The pilot's invariant: honest deterministic work passes exact-match.
        // honest-only runs must never produce a penalty; with cheaters present,
        // the explicitly-verified first honest record must pass (proving exact
        // match holds for the honest majority while the cheater is caught).
        if cfg.num_cheaters == 0 {
            assert!(penalties.is_empty(), "honest-only run produced a penalty");
        } else if first_honest.is_some() {
            assert!(
                honest_passed,
                "expected the audited honest record to pass exact-match (epoch {epoch})"
            );
            any_honest_passed = true;
        }

        // 6. Settle + apply + persist.
        let settlement = settle_epoch(
            &outcome,
            &spec,
            &penalties,
            cfg.reward_pool_per_epoch,
            &config,
            NOW_MS,
        )
        .expect("settle epoch");
        apply_settlement(&settlement, &mut balances, &mut stakes);
        chain
            .put_commit(&settlement.commit)
            .expect("persist commit");
        report.commits.push(settlement.commit.clone());

        // 7. Report loss after this epoch's barrier.
        let params_now = service
            .pull(ParamRange::new(0, dim).unwrap())
            .expect("pull params after epoch");
        report.losses.push(mse_loss(&params_now, &full_data));
    }

    // Across the run, with cheaters present, at least one honest verdict passed
    // (exact-match holds for the honest majority — ADR-0010 D1 gate 2).
    if cfg.num_cheaters > 0 {
        assert!(
            any_honest_passed,
            "expected at least one honest verdict to pass over the run"
        );
    }

    report.cheaters_caught = total_cheaters_caught;
    report.final_balances = balances;
    report.final_stakes = stakes;
    report
}

/// A [`PsConfig`] sized for `num_miners`: the per-range worker cap must be at
/// least `num_miners` (every worker may push to every range, ADR-0003), and
/// must also satisfy `PsConfig::validate`'s minimum-viable-worker check.
fn ps_config_for(num_miners: usize) -> PsConfig {
    PsConfig {
        max_workers_per_range: num_miners.max(PsConfig::default().max_workers_per_range),
        ..PsConfig::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn honest_only_converges() {
        let cfg = PilotConfig {
            num_cheaters: 0,
            ..PilotConfig::default()
        };
        let report = run_pilot(cfg);
        assert!(report.cheaters_caught == 0);
        let last = *report.losses.last().unwrap();
        assert!(
            last < report.initial_loss,
            "loss must decrease: {} -> {}",
            report.initial_loss,
            last
        );
        // Convergence: clear margin (noiseless target).
        assert!(
            last < report.initial_loss * 0.5,
            "loss should drop by >2x: {} -> {}",
            report.initial_loss,
            last
        );
        // Honest miners earn rewards.
        assert!(report.final_balances.values().any(|&b| b > 0));
    }

    #[test]
    fn cheater_is_caught_and_slashed() {
        let cfg = PilotConfig::default();
        let report = run_pilot(cfg.clone());
        assert_eq!(
            report.cheaters_caught, cfg.num_cheaters,
            "all planted cheaters must be caught"
        );
        for cheater in &report.cheater_addresses {
            // Cheater earns NO reward (records voided).
            let bal = report.final_balances.get(cheater).copied().unwrap_or(0);
            assert_eq!(bal, 0, "cheater must not be rewarded");
        }
        // At least one honest miner got paid.
        let honest_paid = report
            .final_balances
            .iter()
            .any(|(a, &b)| !report.cheater_addresses.contains(a) && b > 0);
        assert!(honest_paid, "honest miners must receive rewards");
    }

    #[test]
    fn cheater_stake_is_slashed_below_honest() {
        let cfg = PilotConfig::default();
        let report = run_pilot(cfg.clone());
        let honest_stake = report
            .final_stakes
            .iter()
            .find(|(a, _)| !report.cheater_addresses.contains(a))
            .map(|(_, &s)| s)
            .expect("an honest miner exists");
        for cheater in &report.cheater_addresses {
            let final_stake = report.final_stakes.get(cheater).copied().unwrap_or(0);
            assert!(
                final_stake < honest_stake,
                "cheater stake {final_stake} must be slashed below honest {honest_stake}"
            );
        }
        // Honest stakes are untouched (no slashing).
        for i in 0..(cfg.num_miners - cfg.num_cheaters) {
            assert!(
                report
                    .final_stakes
                    .get(&miner_addr(i))
                    .copied()
                    .unwrap_or(0)
                    > 0
            );
        }
    }
}
