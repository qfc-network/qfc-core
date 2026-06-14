//! A6 integration-test gates (ADR-0010 Decision 1).
//!
//! 1. honest-only run: loss converges (final << initial).
//! 2. with-cheater run: every cheater caught; cheater slashed and unrewarded;
//!    honest miners paid.
//! 3. cross-operator determinism: the SAME accepted-update set pushed through
//!    two independent `ShardService` instances in DIFFERENT order, settled,
//!    yields an identical `commit_hash`.
//! 4. commit persistence: `put_commit` then `get_commit`/`commits_for_job`
//!    round-trip.

use qfc_ai_coordinator::{settle_epoch, TrainingChainStore, TrainingJobSpec, TrainingPenalty};
use qfc_inference::{BackendType, GpuTier, ModelId};
use qfc_ps::{ParamRange, ParamUpdate, PsConfig, ShardService, ShardStore, TrimmedMean};
use qfc_types::{Address, Hash};

use qfc_training_pilot::{build_manifest, compute_delta, run_pilot, synth_datastore, PilotConfig};

const NOW: u64 = 1_000_000;

fn addr(i: usize) -> Address {
    Address::new([(i as u8) + 1; 20])
}

// ---------------------------------------------------------------------------
// Gate 1: honest-only convergence
// ---------------------------------------------------------------------------
#[test]
fn gate1_honest_only_converges() {
    let report = run_pilot(PilotConfig {
        num_cheaters: 0,
        ..PilotConfig::default()
    });
    assert_eq!(report.cheaters_caught, 0);
    let final_loss = *report.losses.last().unwrap();
    assert!(
        final_loss < report.initial_loss,
        "loss must decrease: {} -> {}",
        report.initial_loss,
        final_loss
    );
    // Clear convergence margin on the noiseless linear target.
    assert!(
        final_loss < report.initial_loss * 0.25,
        "loss should drop by >=4x: {} -> {}",
        report.initial_loss,
        final_loss
    );
    // Losses are (weakly) decreasing across epochs.
    for w in report.losses.windows(2) {
        assert!(
            w[1] <= w[0] + 1e-9,
            "loss should not increase across epochs: {:?}",
            report.losses
        );
    }
    assert!(report.final_balances.values().any(|&b| b > 0));
}

// ---------------------------------------------------------------------------
// Gate 2: cheater caught, slashed, unrewarded; honest paid
// ---------------------------------------------------------------------------
#[test]
fn gate2_cheater_caught_slashed_and_honest_paid() {
    let cfg = PilotConfig::default();
    let report = run_pilot(cfg.clone());

    assert_eq!(
        report.cheaters_caught, cfg.num_cheaters,
        "all planted cheaters must be caught by sampled re-execution"
    );

    for cheater in &report.cheater_addresses {
        // Slashed: stake reduced below the comfortable starting margin.
        let final_stake = report.final_stakes.get(cheater).copied().unwrap_or(0);
        // Honest starting stake = stake_floor * 4; a slash of slash_multiple *
        // per_step_reward must have been deducted, so the cheater's stake is
        // strictly below any honest miner's untouched stake.
        let honest_stake = report
            .final_stakes
            .iter()
            .find(|(a, _)| !report.cheater_addresses.contains(a))
            .map(|(_, &s)| s)
            .expect("an honest miner exists");
        assert!(
            final_stake < honest_stake,
            "cheater stake {final_stake} must be slashed below honest {honest_stake}"
        );
        // Unrewarded: records voided.
        let bal = report.final_balances.get(cheater).copied().unwrap_or(0);
        assert_eq!(bal, 0, "cheater must earn no rewards (records voided)");
    }

    // Honest miners receive rewards.
    let honest_paid = report
        .final_balances
        .iter()
        .any(|(a, &b)| !report.cheater_addresses.contains(a) && b > 0);
    assert!(honest_paid, "honest miners must be rewarded");
}

// ---------------------------------------------------------------------------
// Gate 3: cross-operator determinism on REAL training data
// ---------------------------------------------------------------------------

/// Build a deterministic set of honest per-worker updates for one epoch, using
/// the SAME compute_delta the harness uses, over real synthetic training data.
/// Returns (config, spec, updates-grouped-by-worker, ranges, init_params).
fn build_epoch_updates() -> (
    PsConfig,
    TrainingJobSpec,
    Vec<Vec<ParamUpdate>>,
    Vec<ParamRange>,
    Vec<f32>,
) {
    let num_miners = 4usize;
    let dim = 8u64;
    let steps_per_epoch = 2u64;
    let lr = 0.05;

    let config = PsConfig {
        max_workers_per_range: num_miners,
        ..PsConfig::default()
    };
    let store = synth_datastore(dim as usize, 4, 24, 0.0);
    let manifest = build_manifest(&store);
    let ranges = vec![
        ParamRange::new(0, 4).unwrap(),
        ParamRange::new(4, 8).unwrap(),
    ];
    let spec = TrainingJobSpec {
        job_id: Hash::new([0x42; 32]),
        model_id: ModelId::new("pilot-linreg", "v1"),
        data_manifest: manifest,
        param_count: dim,
        range_partition: ranges.clone(),
        epochs: 1,
        steps_per_epoch,
        tokens_per_step: 128,
        per_step_reward: 1_000_000,
        required_backend: BackendType::Cpu,
        min_tier: GpuTier::Cold,
        min_memory_mb: 1024,
    };
    spec.validate(&config).unwrap();

    let init_params = vec![0.0f32; dim as usize];

    // Mirror the harness assignment: round-robin shards over workers at epoch 0.
    let n = num_miners;
    let mut shards_per_worker: Vec<Vec<u32>> = vec![Vec::new(); n];
    for i in 0..store.shards.len() {
        shards_per_worker[i % n].push(i as u32);
    }

    let mut by_worker: Vec<Vec<ParamUpdate>> = Vec::new();
    for (w, shards) in shards_per_worker.iter().enumerate() {
        let worker = addr(w);
        // A fixed per-worker seed: gate 3 only requires the two operators to
        // receive the IDENTICAL update set, not that the seed equals
        // assign_epoch's derived value (which is pub(crate)). Determinism of
        // the commit is what's under test.
        let seed = 0xA5A5_0000u64 + w as u64;
        let data = store.assemble(shards);
        let mut updates = Vec::new();
        for clock in 0..steps_per_epoch {
            let range = ranges[(clock as usize) % ranges.len()];
            let values = compute_delta(&init_params, &data, seed, clock, range, lr);
            updates.push(ParamUpdate {
                worker,
                epoch: 0,
                clock,
                range,
                values,
                flops_estimated: spec.flops_per_step(),
            });
        }
        by_worker.push(updates);
    }

    (config, spec, by_worker, ranges, init_params)
}

/// Push a worker-grouped update set into a fresh ShardService following the
/// given worker order (each worker's own clocks stay monotonic — required by
/// the A3 step-increment rule), end the epoch, settle it, and return the
/// commit hash.
fn settle_in_order(
    config: &PsConfig,
    spec: &TrainingJobSpec,
    ranges: &[ParamRange],
    init_params: &[f32],
    by_worker: &[Vec<ParamUpdate>],
    worker_order: &[usize],
) -> Hash {
    let dim = spec.param_count;
    let store = ShardStore::open_temp(ParamRange::new(0, dim).unwrap()).unwrap();
    store
        .set_range(ParamRange::new(0, dim).unwrap(), init_params)
        .unwrap();
    let mut service = ShardService::new(
        store,
        config.clone(),
        0,
        ranges.to_vec(),
        spec.steps_per_epoch,
    )
    .unwrap();

    for &w in worker_order {
        for update in &by_worker[w] {
            service.push(update.clone()).unwrap();
        }
    }
    let outcome = service
        .end_epoch_with_rule(&TrimmedMean::new(config.trim_beta).unwrap())
        .unwrap();

    let penalties: Vec<TrainingPenalty> = Vec::new();
    let settlement = settle_epoch(
        &outcome,
        spec,
        &penalties,
        1_000_000_000_000u128,
        config,
        NOW,
    )
    .unwrap();
    settlement.commit.commit_hash()
}

#[test]
fn gate3_cross_operator_determinism() {
    let (config, spec, by_worker, ranges, init_params) = build_epoch_updates();

    // Operator A: workers in order 0,1,2,3.
    let hash_a = settle_in_order(
        &config,
        &spec,
        &ranges,
        &init_params,
        &by_worker,
        &[0, 1, 2, 3],
    );
    // Operator B: workers in a DIFFERENT order 3,1,0,2.
    let hash_b = settle_in_order(
        &config,
        &spec,
        &ranges,
        &init_params,
        &by_worker,
        &[3, 1, 0, 2],
    );

    assert_eq!(
        hash_a, hash_b,
        "two operators fed the same updates in different order must produce \
         a byte-identical commit_hash (ADR-0009 cross-operator determinism)"
    );
}

// ---------------------------------------------------------------------------
// Gate 4: commit persistence round-trip
// ---------------------------------------------------------------------------
#[test]
fn gate4_commit_persistence_roundtrip() {
    let (config, spec, by_worker, ranges, init_params) = build_epoch_updates();

    // Settle one epoch to get a real commit.
    let dim = spec.param_count;
    let store = ShardStore::open_temp(ParamRange::new(0, dim).unwrap()).unwrap();
    store
        .set_range(ParamRange::new(0, dim).unwrap(), &init_params)
        .unwrap();
    let mut service = ShardService::new(
        store,
        config.clone(),
        0,
        ranges.clone(),
        spec.steps_per_epoch,
    )
    .unwrap();
    for updates in &by_worker {
        for u in updates {
            service.push(u.clone()).unwrap();
        }
    }
    let outcome = service
        .end_epoch_with_rule(&TrimmedMean::new(config.trim_beta).unwrap())
        .unwrap();
    let settlement =
        settle_epoch(&outcome, &spec, &[], 1_000_000_000_000u128, &config, NOW).unwrap();

    let chain = TrainingChainStore::open_temp().unwrap();
    chain.put_commit(&settlement.commit).unwrap();

    // get_commit round-trips.
    let got = chain
        .get_commit(&spec.job_id, settlement.commit.epoch)
        .unwrap()
        .expect("commit must persist");
    assert_eq!(got, settlement.commit);
    assert_eq!(got.commit_hash(), settlement.commit.commit_hash());

    // commits_for_job returns it.
    let all = chain.commits_for_job(&spec.job_id).unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0], settlement.commit);

    // Missing epoch / job → None / empty.
    assert!(chain.get_commit(&spec.job_id, 999).unwrap().is_none());
    assert!(chain
        .commits_for_job(&Hash::new([0x99; 32]))
        .unwrap()
        .is_empty());
}
