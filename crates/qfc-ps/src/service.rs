//! Shard service: push/pull + epoch barrier, tying together kv / clock /
//! aggregate (the component a PS operator runs, ADR-0002).
//!
//! Implemented in milestone A1, hardened after adversarial review. API:
//!
//! - `ShardService::new(store, config, epoch, ranges, max_steps_per_epoch)
//!   -> Result<Self, PsError>`
//!   — validates `config` (`PsConfig::validate`). `ranges` is the fixed set
//!   of registered assignment ranges for this shard (the assignment layer,
//!   A3, feeds them): non-empty, pairwise disjoint, each inside the owned
//!   range. Workers do NOT choose ranges; pushes are admitted only for
//!   exactly-registered ranges, which bounds buffer memory and makes overlap
//!   double-apply impossible (ADR-0003). `max_steps_per_epoch` is the job's
//!   `steps_per_epoch` (A3/A5 wiring): the per-epoch step ceiling. `0` means
//!   unlimited — tests only; production always passes the job's value.
//! - `push(&mut self, update: ParamUpdate) -> Result<AcceptanceRecord, PsError>`
//!   — validates epoch match + structure, range registration, the per-epoch
//!   step ceiling (`update.clock < max_steps_per_epoch` when non-zero; this
//!   bounds a worker's per-epoch contribution AND caps `fastest_clock`, so
//!   the SSP admission floor can never exceed the ceiling — ADR-0006),
//!   step-clock sequence (a worker's clock advances by exactly one per
//!   accepted push; first push must carry clock 0 — ADR-0006), SSP admission
//!   via `SspClock`, buffers via `UpdateBuffer` (per-worker gradient
//!   accumulation; the per-range cap counts distinct workers — ADR-0003),
//!   records acceptance (ADR-0004). Rejected-as-stale is an error, not a
//!   slash (ADR-0006).
//! - `pull(&self, range: ParamRange) -> Result<Vec<f32>, PsError>` — current
//!   shard parameters for any sub-range of the owned range.
//! - `end_epoch(&mut self) -> Result<EpochOutcome, PsError>` — the barrier
//!   (ADR-0006) with the production rule, `TrimmedMean` at the configured
//!   `trim_beta` (ADR-0003): for each buffered range with at least
//!   `rule.min_updates()` per-worker entries (= distinct workers), aggregate
//!   (entries sorted by worker for determinism) and `apply_delta`; thinner
//!   ranges are skipped with a warning (deterministic no-op, acceptance
//!   records kept — ADR-0004).
//!   Then clear buffer, bump epoch, and return
//!   `EpochOutcome { epoch, params_hash, accepted: Vec<AcceptanceRecord> }`
//!   — the inputs of the on-chain version commit (A5).
//!   `end_epoch_with_rule` exists for tests and trusted-cluster experiments.
//! - Pushes for a closed/mismatched epoch → `PsError::EpochClosed` /
//!   `EpochMismatch`.

use qfc_types::Hash;
use tracing::{debug, info, warn};

use crate::{
    aggregate::{AggregationRule, TrimmedMean, UpdateBuffer},
    clock::SspClock,
    kv::ShardStore,
    types::{AcceptanceRecord, Epoch, ParamRange, ParamUpdate},
    PsConfig, PsError,
};

/// Result of closing an epoch at the barrier — the inputs of the on-chain
/// version commit (A5): the epoch just closed, the post-aggregation shard
/// parameter hash, and every acceptance record issued during the epoch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EpochOutcome {
    /// The epoch that was just closed (the service is now at `epoch + 1`).
    pub epoch: Epoch,
    /// `ShardStore::params_hash` after applying the aggregated deltas.
    pub params_hash: Hash,
    /// All acceptance records issued by `push` during the closed epoch
    /// (ADR-0004 — the unit reward distribution reads).
    pub accepted: Vec<AcceptanceRecord>,
}

/// The shard service a staked PS operator runs (ADR-0002): SSP-gated
/// push/pull within an epoch, byzantine-robust aggregation at the barrier.
pub struct ShardService {
    store: ShardStore,
    config: PsConfig,
    epoch: Epoch,
    clock: SspClock,
    buffer: UpdateBuffer,
    /// The fixed set of registered assignment ranges (sorted, pairwise
    /// disjoint, each inside the owned range). Pushes must target exactly
    /// one of these — workers never choose ranges (ADR-0003, A3).
    ranges: Vec<ParamRange>,
    /// Per-epoch step ceiling: pushes with `clock >= max_steps_per_epoch`
    /// are rejected (when non-zero). The job's `steps_per_epoch` (A3/A5
    /// wiring). Bounds both per-worker row minting and the clock-ratchet
    /// admission floor: `fastest_clock <= ceiling` (ADR-0006). `0` =
    /// unlimited, tests only.
    max_steps_per_epoch: u64,
    /// Acceptance records issued this epoch. Survives `buffer.clear()` at the
    /// barrier — records are accounting, the buffer is working memory.
    accepted: Vec<AcceptanceRecord>,
}

impl ShardService {
    /// `ranges` are the assignment-layer ranges for this shard (A3 supplies
    /// them; tests and the pilot use a partition of the owned range). They
    /// must be non-empty, pairwise disjoint, and each inside the owned
    /// range — otherwise `InvalidRange`. `config` must pass
    /// `PsConfig::validate` — otherwise `Config`.
    ///
    /// `max_steps_per_epoch` is the per-epoch step ceiling, i.e. the job's
    /// `steps_per_epoch` (`TrainingJobSpec`, A3; wired on-chain in A5).
    /// `0` = unlimited — tests only; production always caps.
    pub fn new(
        store: ShardStore,
        config: PsConfig,
        epoch: Epoch,
        ranges: Vec<ParamRange>,
        max_steps_per_epoch: u64,
    ) -> Result<Self, PsError> {
        config.validate()?;
        if ranges.is_empty() {
            return Err(PsError::InvalidRange(
                "registered assignment range set must be non-empty".to_string(),
            ));
        }
        let mut ranges = ranges;
        ranges.sort();
        ranges.dedup();
        for r in &ranges {
            if !store.owned_range().covers(r) {
                return Err(PsError::InvalidRange(format!(
                    "registered range {} outside owned range {}",
                    r,
                    store.owned_range()
                )));
            }
        }
        for pair in ranges.windows(2) {
            if pair[0].overlaps(&pair[1]) {
                return Err(PsError::InvalidRange(format!(
                    "registered ranges {} and {} overlap; assignment ranges \
                     must be pairwise disjoint (ADR-0003)",
                    pair[0], pair[1]
                )));
            }
        }
        let clock = SspClock::new(config.staleness_bound);
        let buffer = UpdateBuffer::new(config.max_workers_per_range);
        Ok(Self {
            store,
            config,
            epoch,
            clock,
            buffer,
            ranges,
            max_steps_per_epoch,
            accepted: Vec::new(),
        })
    }

    /// Current (open) training epoch.
    pub fn epoch(&self) -> Epoch {
        self.epoch
    }

    /// The underlying parameter store (e.g. for snapshot export, A4).
    pub fn store(&self) -> &ShardStore {
        &self.store
    }

    /// The registered assignment ranges (sorted, pairwise disjoint).
    pub fn registered_ranges(&self) -> &[ParamRange] {
        &self.ranges
    }

    /// Push a worker update: epoch check → structural validation → range
    /// registration → step-clock sequence → SSP admission → buffer
    /// (cap/dedup) → clock advance → acceptance record (ADR-0004).
    pub fn push(&mut self, update: ParamUpdate) -> Result<AcceptanceRecord, PsError> {
        if update.epoch != self.epoch {
            return Err(if update.epoch < self.epoch {
                PsError::EpochClosed {
                    epoch: update.epoch,
                }
            } else {
                PsError::EpochMismatch {
                    expected: self.epoch,
                    got: update.epoch,
                }
            });
        }
        update.validate()?;
        // Range admission: only exactly-registered assignment ranges. This is
        // what bounds buffer memory and rules out overlap double-apply —
        // workers must not choose their own (sub-)ranges (ADR-0003).
        if self.ranges.binary_search(&update.range).is_err() {
            return Err(PsError::InvalidRange(format!(
                "update range {} is not a registered assignment range \
                 (ADR-0003: pushes are admitted only for assignment-layer ranges)",
                update.range
            )));
        }
        // Per-epoch step ceiling: a worker may contribute at most
        // max_steps_per_epoch steps (the job's steps_per_epoch). This bounds
        // both row-minting (a worker's accumulated contribution is at most
        // the ceiling's worth of steps) and the clock-ratchet floor:
        // fastest_clock <= ceiling, so the SSP admission floor can never
        // exceed it (ADR-0006).
        if self.max_steps_per_epoch > 0 && update.clock >= self.max_steps_per_epoch {
            return Err(PsError::InvalidUpdate(format!(
                "clock {} at or above the per-epoch step ceiling {} \
                 (steps_per_epoch — ADR-0006)",
                update.clock, self.max_steps_per_epoch
            )));
        }
        // Step-clock sequence (ADR-0006): exactly one step per accepted push.
        // First push from an unseen worker carries clock 0; each subsequent
        // accepted push carries prev + 1. This bounds fastest_clock growth to
        // accepted (metered, audit-eligible) work, and subsumes both the
        // regression check and (worker, clock) duplicates at the service
        // level.
        let expected = self.clock.worker_clock(update.worker).map_or(0, |c| c + 1);
        if update.clock != expected {
            return Err(PsError::InvalidUpdate(format!(
                "clock step violation for worker {}: expected {}, got {} \
                 (clocks advance exactly one per accepted push, ADR-0006)",
                update.worker, expected, update.clock
            )));
        }
        self.clock.admit(update.clock)?;
        let record = AcceptanceRecord::for_update(&update);
        let (worker, clock) = (update.worker, update.clock);
        self.buffer.add(update)?;
        self.clock.advance(worker, clock)?;
        debug!(%worker, clock, epoch = self.epoch, "accepted push");
        self.accepted.push(record.clone());
        Ok(record)
    }

    /// Current shard parameters for any sub-range of the owned range.
    pub fn pull(&self, range: ParamRange) -> Result<Vec<f32>, PsError> {
        self.store.get_range(range)
    }

    /// The epoch barrier (ADR-0006) with the production rule:
    /// `TrimmedMean` at the configured `trim_beta` (ADR-0003).
    pub fn end_epoch(&mut self) -> Result<EpochOutcome, PsError> {
        let rule = TrimmedMean::new(self.config.trim_beta)?;
        self.end_epoch_with_rule(&rule)
    }

    /// The epoch barrier (ADR-0006) with an explicit rule — for tests and
    /// trusted-cluster experiments; production uses [`Self::end_epoch`].
    ///
    /// For each buffered range: ranges with fewer than `rule.min_updates()`
    /// per-worker entries (= distinct workers; the buffer accumulates each
    /// worker's steps into one entry) are skipped with a warning — a
    /// deterministic no-op for that range this epoch. Thin participation is
    /// an assignment-layer shortfall, not worker misbehavior, so the workers'
    /// acceptance records are KEPT (the work was done and assigned —
    /// ADR-0004); only the buffered deltas drop at clear. Ranges at or above
    /// the minimum aggregate (entries pre-sorted by worker for determinism)
    /// and any aggregation error still propagates — that is a real bug, not
    /// thin participation. Then: clear buffer, barrier the clock, open the
    /// next epoch.
    pub fn end_epoch_with_rule(
        &mut self,
        rule: &dyn AggregationRule,
    ) -> Result<EpochOutcome, PsError> {
        let closed = self.epoch;
        for range in self.buffer.ranges() {
            let updates = self.buffer.updates_for(&range);
            if updates.len() < rule.min_updates() {
                warn!(
                    %range,
                    count = updates.len(),
                    min = rule.min_updates(),
                    epoch = closed,
                    "thin range: below rule minimum, skipping aggregation \
                     (no delta this epoch; acceptance records kept)"
                );
                continue;
            }
            let delta = rule.aggregate(&updates, &range)?;
            self.store.apply_delta(range, &delta)?;
        }
        self.buffer.clear();
        let _final_clocks = self.clock.barrier();
        self.epoch += 1;
        let accepted = std::mem::take(&mut self.accepted);
        let params_hash = self.store.params_hash()?;
        info!(
            epoch = closed,
            accepted = accepted.len(),
            params_hash = %params_hash,
            "epoch barrier complete"
        );
        Ok(EpochOutcome {
            epoch: closed,
            params_hash,
            accepted,
        })
    }

    /// Protocol constants this service runs with.
    pub fn config(&self) -> &PsConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::StepClock;
    use qfc_types::Address;

    /// Test-only rule: coordinate-wise sum. NOT byzantine-robust — exists so
    /// A1 service tests do not depend on `TrimmedMean` (implemented in A2).
    struct SumRule;

    impl AggregationRule for SumRule {
        fn aggregate(
            &self,
            updates: &[&ParamUpdate],
            range: &ParamRange,
        ) -> Result<Vec<f32>, PsError> {
            let mut out = vec![0.0f32; range.len() as usize];
            for u in updates {
                for (slot, v) in out.iter_mut().zip(&u.values) {
                    *slot += v;
                }
            }
            Ok(out)
        }
    }

    fn addr(b: u8) -> Address {
        Address::new([b; 20])
    }

    fn range(start: u64, end: u64) -> ParamRange {
        ParamRange::new(start, end).unwrap()
    }

    fn update(worker: u8, epoch: Epoch, clock: StepClock, r: ParamRange, val: f32) -> ParamUpdate {
        ParamUpdate {
            worker: addr(worker),
            epoch,
            clock,
            range: r,
            values: vec![val; r.len() as usize],
            flops_estimated: 1000,
        }
    }

    /// Service over `owned` with an explicit registered-range set and no
    /// step ceiling (0 = unlimited, tests only).
    fn service_with(owned: ParamRange, epoch: Epoch, ranges: Vec<ParamRange>) -> ShardService {
        let store = ShardStore::open_temp(owned).unwrap();
        ShardService::new(store, PsConfig::default(), epoch, ranges, 0).unwrap()
    }

    /// Service over [0, 100) with the registered partition used by most
    /// tests: [0, 10), [10, 20), [50, 60).
    fn service(epoch: Epoch) -> ShardService {
        service_with(
            range(0, 100),
            epoch,
            vec![range(0, 10), range(10, 20), range(50, 60)],
        )
    }

    #[test]
    fn test_new_rejects_bad_registered_range_sets() {
        let owned = range(0, 100);
        // Empty set.
        let store = ShardStore::open_temp(owned).unwrap();
        assert!(matches!(
            ShardService::new(store, PsConfig::default(), 0, vec![], 0),
            Err(PsError::InvalidRange(_))
        ));
        // Overlapping ranges.
        let store = ShardStore::open_temp(owned).unwrap();
        assert!(matches!(
            ShardService::new(
                store,
                PsConfig::default(),
                0,
                vec![range(0, 10), range(5, 15)],
                0
            ),
            Err(PsError::InvalidRange(_))
        ));
        // Range outside the owned range.
        let store = ShardStore::open_temp(owned).unwrap();
        assert!(matches!(
            ShardService::new(
                store,
                PsConfig::default(),
                0,
                vec![range(0, 10), range(90, 110)],
                0
            ),
            Err(PsError::InvalidRange(_))
        ));
        // A valid partition is accepted and reported sorted.
        let store = ShardStore::open_temp(owned).unwrap();
        let svc = ShardService::new(
            store,
            PsConfig::default(),
            0,
            vec![range(50, 100), range(0, 50)],
            0,
        )
        .unwrap();
        assert_eq!(svc.registered_ranges(), &[range(0, 50), range(50, 100)]);
    }

    /// `ShardService::new` validates the config (NIT: PsConfig::validate).
    #[test]
    fn test_new_rejects_invalid_config() {
        let owned = range(0, 100);
        let store = ShardStore::open_temp(owned).unwrap();
        let bad = PsConfig {
            trim_beta: 0.7,
            ..PsConfig::default()
        };
        assert!(matches!(
            ShardService::new(store, bad, 0, vec![owned], 0),
            Err(PsError::Config(_))
        ));
    }

    #[test]
    fn test_push_happy_path_returns_record() {
        let mut svc = service(0);
        let u = update(1, 0, 0, range(10, 20), 1.0);
        let record = svc.push(u.clone()).unwrap();
        assert_eq!(record, AcceptanceRecord::for_update(&u));
        assert_eq!(record.worker, addr(1));
        assert_eq!(record.epoch, 0);
        assert_eq!(record.range, range(10, 20));
    }

    #[test]
    fn test_epoch_mismatch_and_closed_rejection() {
        let mut svc = service(5);
        // Future epoch: mismatch.
        assert!(matches!(
            svc.push(update(1, 6, 0, range(0, 10), 1.0)),
            Err(PsError::EpochMismatch {
                expected: 5,
                got: 6
            })
        ));
        // Past epoch: closed.
        assert!(matches!(
            svc.push(update(1, 4, 0, range(0, 10), 1.0)),
            Err(PsError::EpochClosed { epoch: 4 })
        ));
    }

    /// Range admission (post-review hardening): a push is accepted only for
    /// EXACTLY a registered assignment range — unregistered ranges,
    /// attacker-chosen sub-ranges of a registered range, and ranges that
    /// straddle two registered ranges are all refused. This is what bounds
    /// buffer memory (workers cannot mint fresh buffer keys) and rules out
    /// overlap double-apply.
    #[test]
    fn test_push_unregistered_sub_or_overlapping_range_rejected() {
        let mut svc = service(0);
        for bad in [
            range(20, 30),  // inside owned, but not registered
            range(12, 18),  // strict sub-range of registered [10, 20)
            range(10, 15),  // shares a start with registered [10, 20)
            range(5, 15),   // straddles [0, 10) and [10, 20)
            range(90, 110), // outside the owned range entirely
        ] {
            match svc.push(update(1, 0, 0, bad, 1.0)) {
                Err(PsError::InvalidRange(msg)) => {
                    assert!(
                        msg.contains(&bad.to_string()),
                        "message must name the range: {}",
                        msg
                    );
                    assert!(
                        msg.contains("not a registered assignment range"),
                        "message must say why: {}",
                        msg
                    );
                }
                other => panic!("expected InvalidRange for {}, got {:?}", bad, other),
            }
        }
        // Nothing was buffered or recorded.
        assert!(svc.end_epoch().unwrap().accepted.is_empty());
    }

    /// Step-clock sequence (post-review hardening, ADR-0006): the first push
    /// from a worker must carry clock 0; every subsequent accepted push
    /// exactly prev + 1. Jumps cannot ratchet the admission floor for free.
    #[test]
    fn test_clock_jump_rejected_in_order_accepted() {
        let mut svc = service(0);
        let r = range(0, 10);
        // First push must be clock 0.
        match svc.push(update(1, 0, 5, r, 1.0)) {
            Err(PsError::InvalidUpdate(msg)) => {
                assert!(msg.contains("expected 0"), "{}", msg);
                assert!(msg.contains("got 5"), "{}", msg);
            }
            other => panic!("expected InvalidUpdate, got {:?}", other),
        }
        // In-order 0, 1, 2: accepted.
        svc.push(update(1, 0, 0, r, 1.0)).unwrap();
        svc.push(update(1, 0, 1, r, 1.0)).unwrap();
        svc.push(update(1, 0, 2, r, 1.0)).unwrap();
        // Jump after progress (2 -> 5): rejected, clock unchanged.
        match svc.push(update(1, 0, 5, r, 1.0)) {
            Err(PsError::InvalidUpdate(msg)) => {
                assert!(msg.contains("expected 3"), "{}", msg);
                assert!(msg.contains("got 5"), "{}", msg);
            }
            other => panic!("expected InvalidUpdate, got {:?}", other),
        }
        svc.push(update(1, 0, 3, r, 1.0)).unwrap();
    }

    /// Duplicate (worker, clock) is rejected at the step check — replaying
    /// an already-accepted clock can never earn a second acceptance record.
    #[test]
    fn test_duplicate_worker_clock_rejected() {
        let mut svc = service(0);
        let r = range(0, 10);
        svc.push(update(1, 0, 0, r, 1.0)).unwrap();
        assert!(matches!(
            svc.push(update(1, 0, 0, r, 2.0)),
            Err(PsError::InvalidUpdate(_))
        ));
        // Same clock on a different registered range is still a replay.
        assert!(matches!(
            svc.push(update(1, 0, 0, range(10, 20), 2.0)),
            Err(PsError::InvalidUpdate(_))
        ));
        let outcome = svc.end_epoch().unwrap();
        assert_eq!(outcome.accepted.len(), 1);
    }

    #[test]
    fn test_stale_push_rejected_after_fast_worker_advances() {
        let mut svc = service(0);
        let r = range(0, 10);
        // Fast worker 9 reaches clock 10 the only way possible: 11 accepted
        // pushes (clocks 0..=10). Staleness bound is 3 → admission floor 7.
        for c in 0..=10 {
            svc.push(update(9, 0, c, r, 0.0)).unwrap();
        }
        // Worker 1's first push must carry clock 0, which is below the floor:
        // refused as stale (an error, never a slash — ADR-0006).
        assert!(matches!(
            svc.push(update(1, 0, 0, range(10, 20), 1.0)),
            Err(PsError::StaleUpdate {
                worker_clock: 0,
                floor: 7,
                bound: 3
            })
        ));
    }

    #[test]
    fn test_pull_sub_range() {
        let owned = range(0, 100);
        let store = ShardStore::open_temp(owned).unwrap();
        store
            .set_range(owned, &(0..100).map(|i| i as f32).collect::<Vec<_>>())
            .unwrap();
        let svc = ShardService::new(store, PsConfig::default(), 0, vec![owned], 0).unwrap();

        assert_eq!(
            svc.pull(range(40, 44)).unwrap(),
            vec![40.0, 41.0, 42.0, 43.0]
        );
        assert!(matches!(
            svc.pull(range(90, 101)),
            Err(PsError::InvalidRange(_))
        ));
    }

    #[test]
    fn test_full_epoch_cycle_and_second_epoch() {
        let mut svc = service(0);
        let r = range(10, 20);

        // Two workers push deltas on the same range, one on another range.
        // Each worker's first push carries clock 0 (step semantics).
        svc.push(update(1, 0, 0, r, 1.0)).unwrap();
        svc.push(update(2, 0, 0, r, 2.5)).unwrap();
        svc.push(update(3, 0, 0, range(50, 60), -1.0)).unwrap();

        let outcome = svc.end_epoch_with_rule(&SumRule).unwrap();
        assert_eq!(outcome.epoch, 0);
        assert_eq!(outcome.accepted.len(), 3);
        assert_eq!(outcome.params_hash, svc.store().params_hash().unwrap());
        assert_eq!(svc.epoch(), 1);

        // Deltas applied coordinate-wise: 1.0 + 2.5 on [10,20), -1.0 on [50,60).
        assert_eq!(svc.pull(r).unwrap(), vec![3.5; 10]);
        assert_eq!(svc.pull(range(50, 60)).unwrap(), vec![-1.0; 10]);
        assert_eq!(svc.pull(range(0, 10)).unwrap(), vec![0.0; 10]);

        // Old epoch is closed; new epoch accepts (clocks were barrier-reset,
        // so worker 1 starts again at clock 0).
        assert!(matches!(
            svc.push(update(1, 0, 5, r, 1.0)),
            Err(PsError::EpochClosed { epoch: 0 })
        ));
        svc.push(update(1, 1, 0, r, 0.5)).unwrap();
        let outcome2 = svc.end_epoch_with_rule(&SumRule).unwrap();
        assert_eq!(outcome2.epoch, 1);
        assert_eq!(outcome2.accepted.len(), 1);
        assert_ne!(outcome2.params_hash, outcome.params_hash);
        assert_eq!(svc.pull(r).unwrap(), vec![4.0; 10]);
        assert_eq!(svc.epoch(), 2);
    }

    /// The production barrier: `end_epoch()` aggregates with `TrimmedMean`
    /// at the configured `trim_beta`. Three updates on one range with
    /// beta = 0.2 trim one per side, so the extreme is dropped and the
    /// median's value lands in the parameters.
    #[test]
    fn test_end_epoch_default_rule_is_trimmed_mean() {
        let mut svc = service(0);
        let r = range(10, 20);
        svc.push(update(1, 0, 0, r, 1.0)).unwrap();
        svc.push(update(2, 0, 0, r, 2.0)).unwrap();
        svc.push(update(3, 0, 0, r, 1000.0)).unwrap(); // outlier, trimmed
        let outcome = svc.end_epoch().unwrap();
        assert_eq!(outcome.accepted.len(), 3);
        // n = 3, beta = 0.2 → trim ceil(0.6) = 1 per side, keep the median.
        assert_eq!(svc.pull(r).unwrap(), vec![2.0; 10]);
    }

    /// Thin range (post-review hardening): a range with fewer updates than
    /// the rule's minimum must NOT abort the barrier — it is skipped (no
    /// delta this epoch), while other ranges aggregate and the thin range's
    /// acceptance records are kept (the work was assigned and done,
    /// ADR-0004; thin participation is not the worker's fault).
    #[test]
    fn test_thin_range_skipped_not_fatal() {
        let mut svc = service(0);
        let healthy = range(10, 20);
        let thin = range(50, 60);
        // Healthy range: 3 updates (>= TrimmedMean(0.2).min_updates() = 3).
        svc.push(update(1, 0, 0, healthy, 1.0)).unwrap();
        svc.push(update(2, 0, 0, healthy, 2.0)).unwrap();
        svc.push(update(3, 0, 0, healthy, 3.0)).unwrap();
        // Thin range: a single update — would make TrimmedMean error.
        svc.push(update(4, 0, 0, thin, 7.0)).unwrap();

        let outcome = svc.end_epoch().unwrap();
        // All four acceptance records survive, including the thin range's.
        assert_eq!(outcome.accepted.len(), 4);
        assert!(outcome
            .accepted
            .iter()
            .any(|rec| rec.worker == addr(4) && rec.range == thin));
        // Healthy range aggregated (trim 1.0 and 3.0, keep 2.0); thin range
        // untouched (deterministic no-op).
        assert_eq!(svc.pull(healthy).unwrap(), vec![2.0; 10]);
        assert_eq!(svc.pull(thin).unwrap(), vec![0.0; 10]);
        // The buffered thin update dropped at the barrier: epoch 1 starts
        // clean and a fresh end_epoch is a pure no-op.
        let outcome2 = svc.end_epoch().unwrap();
        assert!(outcome2.accepted.is_empty());
        assert_eq!(outcome2.params_hash, outcome.params_hash);
    }

    /// The step clock is per worker, not per (worker, range): one worker
    /// covering several registered ranges spends one clock step per push,
    /// in sequence across ranges.
    #[test]
    fn test_one_worker_multiple_ranges_sequential_clocks() {
        let mut svc = service(0);
        svc.push(update(1, 0, 0, range(0, 10), 1.0)).unwrap();
        svc.push(update(1, 0, 1, range(10, 20), 2.0)).unwrap();
        svc.push(update(1, 0, 2, range(50, 60), 3.0)).unwrap();
        // Reusing clock 2 on yet another range is a replay.
        assert!(matches!(
            svc.push(update(1, 0, 2, range(0, 10), 4.0)),
            Err(PsError::InvalidUpdate(_))
        ));
        assert_eq!(svc.end_epoch().unwrap().accepted.len(), 3);
    }

    /// FINDING 1b: pushes with clock >= max_steps_per_epoch are rejected —
    /// the per-epoch step ceiling bounds both per-worker accumulation and
    /// the SSP clock-ratchet floor (fastest_clock <= ceiling).
    #[test]
    fn test_step_ceiling_rejected_at_push() {
        let store = ShardStore::open_temp(range(0, 100)).unwrap();
        let mut svc =
            ShardService::new(store, PsConfig::default(), 0, vec![range(0, 10)], 3).unwrap();
        let r = range(0, 10);
        // Clocks 0, 1, 2 accepted (ceiling 3).
        for c in 0..3 {
            svc.push(update(1, 0, c, r, 1.0)).unwrap();
        }
        // Clock 3 = ceiling: rejected with a message naming clock + ceiling.
        match svc.push(update(1, 0, 3, r, 1.0)) {
            Err(PsError::InvalidUpdate(msg)) => {
                assert!(msg.contains("clock 3"), "{}", msg);
                assert!(msg.contains("ceiling 3"), "{}", msg);
            }
            other => panic!("expected InvalidUpdate, got {:?}", other),
        }
        // The ceiling also bounds the admission floor: fastest_clock <= 2,
        // so a fresh worker's clock-0 push is never stale.
        svc.push(update(2, 0, 0, r, 1.0)).unwrap();
        assert_eq!(svc.end_epoch().unwrap().accepted.len(), 4);
    }

    /// FINDING 1a end-to-end: 3 workers × 4 steps each. The barrier sees 3
    /// per-worker accumulated contributions (n = 3 workers, not 12 rows);
    /// the applied delta is the trimmed mean of the accumulated sums, and
    /// acceptance records remain one per accepted push (12).
    #[test]
    fn test_end_epoch_aggregates_per_worker_accumulated_deltas() {
        let mut svc = service(0);
        let r = range(10, 20);
        // Per-step deltas: worker 1 → 0.5, worker 2 → 1.0, worker 3 → 100.0.
        // Accumulated over 4 steps: 2.0, 4.0, 400.0.
        for (worker, val) in [(1u8, 0.5f32), (2, 1.0), (3, 100.0)] {
            for c in 0..4 {
                svc.push(update(worker, 0, c, r, val)).unwrap();
            }
        }
        let outcome = svc.end_epoch().unwrap();
        // One acceptance record per accepted push (ADR-0004), not per entry.
        assert_eq!(outcome.accepted.len(), 12);
        // TrimmedMean(0.2) over n = 3 accumulated sums {2, 4, 400} trims one
        // per side and keeps the median: 4.0. A row-denominated buffer would
        // instead have n = 12 and let worker 3's repeated 100s survive.
        assert_eq!(svc.pull(r).unwrap(), vec![4.0; 10]);
    }

    #[test]
    fn test_records_survive_buffer_clear_and_reset_per_epoch() {
        let mut svc = service(0);
        svc.push(update(1, 0, 0, range(0, 10), 1.0)).unwrap();
        let outcome = svc.end_epoch_with_rule(&SumRule).unwrap();
        assert_eq!(outcome.accepted.len(), 1);

        // Next epoch starts with an empty record set.
        let outcome2 = svc.end_epoch_with_rule(&SumRule).unwrap();
        assert_eq!(outcome2.epoch, 1);
        assert!(outcome2.accepted.is_empty());
    }
}
