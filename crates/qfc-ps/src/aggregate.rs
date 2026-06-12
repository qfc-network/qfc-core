//! Byzantine-robust aggregation (ADR-0003).
//!
//! Updates buffer per range during the epoch; at the SSP barrier
//! (ADR-0006) the configured [`AggregationRule`] reduces each range's
//! buffered updates to one delta vector. Plain averaging is forbidden as a
//! production rule — workers are untrusted.
//!
//! Buffer keys are the *registered assignment ranges* admitted by
//! `ShardService` (fixed per epoch, pairwise disjoint, each inside the owned
//! range). Disjointness means no parameter coordinate can receive two
//! aggregated deltas in one epoch (no overlap double-apply), and the fixed
//! key set bounds buffer memory (see [`UpdateBuffer`]).

use std::collections::HashMap;

use crate::types::{ParamRange, ParamUpdate};
use crate::PsError;

/// Reduces one range's buffered updates to a single delta vector.
pub trait AggregationRule {
    /// `updates` are structurally valid (see `ParamUpdate::validate`) and all
    /// have ranges equal to `range`. Must be deterministic: callers sort
    /// updates by worker address before invoking (ADR-0003).
    fn aggregate(&self, updates: &[&ParamUpdate], range: &ParamRange) -> Result<Vec<f32>, PsError>;

    /// Minimum number of buffered updates this rule needs to produce a
    /// meaningful delta. The epoch barrier skips (deterministic no-op, with a
    /// warning) any range with fewer updates instead of aborting — thin
    /// participation on one range must not block the whole epoch.
    fn min_updates(&self) -> usize {
        1
    }
}

/// Coordinate-wise trimmed mean with trim fraction `beta` per side
/// (ADR-0003). Tolerates `f < beta * n` malicious updates per range.
#[derive(Clone, Copy, Debug)]
pub struct TrimmedMean {
    pub beta: f64,
}

impl TrimmedMean {
    pub fn new(beta: f64) -> Result<Self, PsError> {
        if !(0.0..0.5).contains(&beta) {
            return Err(PsError::Aggregation(format!(
                "trim beta {} outside [0, 0.5)",
                beta
            )));
        }
        Ok(Self { beta })
    }
}

/// Defensive input checks shared by aggregation rules. The trait contract
/// already guarantees these (updates validated, ranges equal), so this only
/// re-checks what is cheap: non-empty input, range equality, value length.
/// It does NOT re-run full `ParamUpdate::validate` (finiteness etc.).
fn check_inputs(updates: &[&ParamUpdate], range: &ParamRange) -> Result<usize, PsError> {
    if updates.is_empty() {
        return Err(PsError::Aggregation(format!(
            "no updates to aggregate for {}",
            range
        )));
    }
    let dim = range.len() as usize;
    for u in updates {
        if u.range != *range {
            return Err(PsError::Aggregation(format!(
                "update range {} != aggregation range {}",
                u.range, range
            )));
        }
        if u.values.len() != dim {
            return Err(PsError::Aggregation(format!(
                "value count {} != range length {} for {}",
                u.values.len(),
                dim,
                range
            )));
        }
    }
    Ok(dim)
}

impl AggregationRule for TrimmedMean {
    fn aggregate(&self, updates: &[&ParamUpdate], range: &ParamRange) -> Result<Vec<f32>, PsError> {
        let dim = check_inputs(updates, range)?;
        let n = updates.len();
        // ADR-0003: drop the top and bottom ceil(beta * n) per coordinate.
        let trim = (self.beta * n as f64).ceil() as usize;
        let kept = n.saturating_sub(2 * trim);
        if kept == 0 {
            return Err(PsError::Aggregation(format!(
                "trimmed mean over n = {} updates with beta = {} trims {} per \
                 side and leaves nothing; caller must assign enough workers \
                 per range (ADR-0003)",
                n, self.beta, trim
            )));
        }
        // Determinism note: per-coordinate sorting makes the result invariant
        // to the order of `updates`, so the trait's "callers pre-sort by
        // worker address" requirement is not load-bearing for TrimmedMean.
        // It exists for rules that are NOT permutation-invariant.
        let mut out = Vec::with_capacity(dim);
        // One column buffer reused across coordinates (clear + refill), so the
        // whole aggregation allocates O(n) scratch, not O(n * dim).
        let mut column: Vec<f32> = Vec::with_capacity(n);
        for j in 0..dim {
            column.clear();
            column.extend(updates.iter().map(|u| u.values[j]));
            // Values are finite per the input contract, but total_cmp gives a
            // total order on f32 regardless, so sorting can never misbehave.
            column.sort_unstable_by(f32::total_cmp);
            // Accumulate in f64 (ADR-0003); the sorted column also fixes the
            // summation order independent of worker permutation.
            let sum: f64 = column[trim..n - trim].iter().map(|&v| f64::from(v)).sum();
            out.push((sum / kept as f64) as f32);
        }
        Ok(out)
    }

    /// Smallest `n` with `n - 2·⌈β·n⌉ >= 1` — below this, trimming leaves
    /// nothing and the barrier skips the range instead of aborting.
    fn min_updates(&self) -> usize {
        (1usize..)
            .find(|&n| n.saturating_sub(2 * ((self.beta * n as f64).ceil() as usize)) >= 1)
            .expect("beta < 0.5 guarantees a finite minimum")
    }
}

#[doc = "FedAvg-style plain mean. POISONABLE — exists only as a test baseline and for trusted-cluster experiments; never the production rule (ADR-0003)."]
#[derive(Clone, Copy, Debug, Default)]
pub struct Mean;

impl AggregationRule for Mean {
    fn aggregate(&self, updates: &[&ParamUpdate], range: &ParamRange) -> Result<Vec<f32>, PsError> {
        let dim = check_inputs(updates, range)?;
        let n = updates.len() as f64;
        let mut out = Vec::with_capacity(dim);
        for j in 0..dim {
            // f64 accumulation, same as TrimmedMean (ADR-0003).
            let sum: f64 = updates.iter().map(|u| f64::from(u.values[j])).sum();
            out.push((sum / n) as f32);
        }
        Ok(out)
    }
}

/// Per-epoch buffer of pushed updates, keyed by exact range.
///
/// `ShardService` only feeds this buffer updates whose range is exactly one
/// of the registered assignment ranges (fixed per epoch, pairwise disjoint,
/// each inside the owned range), so the key set is bounded and
/// attacker-independent. Together with the per-range worker cap this gives a
/// hard memory bound: `max_per_range × Σ registered range lengths
/// ≤ cap × owned_range.len()` buffered f32s — the O(workers × shard_size)
/// aggregation memory bound from ADR-0003. Also enforces
/// one-update-per-(worker, clock) dedup. Cleared at the epoch barrier.
#[derive(Debug, Default)]
pub struct UpdateBuffer {
    max_per_range: usize,
    buffered: HashMap<ParamRange, Vec<ParamUpdate>>,
}

impl UpdateBuffer {
    /// `max_per_range = 0` means uncapped (tests only; production always caps).
    pub fn new(max_per_range: usize) -> Self {
        Self {
            max_per_range,
            buffered: HashMap::new(),
        }
    }

    /// Buffer a validated update. Rejects duplicates (same worker + clock on
    /// the same range) and pushes beyond the worker cap.
    pub fn add(&mut self, update: ParamUpdate) -> Result<(), PsError> {
        update.validate()?;
        let entry = self.buffered.entry(update.range).or_default();
        if entry
            .iter()
            .any(|u| u.worker == update.worker && u.clock == update.clock)
        {
            return Err(PsError::InvalidUpdate(format!(
                "duplicate update from {} at clock {} for {}",
                update.worker, update.clock, update.range
            )));
        }
        if self.max_per_range > 0 && entry.len() >= self.max_per_range {
            return Err(PsError::WorkerCapExceeded {
                cap: self.max_per_range,
            });
        }
        entry.push(update);
        Ok(())
    }

    /// Updates for a range, sorted by (worker, clock) for deterministic
    /// aggregation order (ADR-0003).
    pub fn updates_for(&self, range: &ParamRange) -> Vec<&ParamUpdate> {
        let mut v: Vec<&ParamUpdate> = self
            .buffered
            .get(range)
            .map(|u| u.iter().collect())
            .unwrap_or_default();
        v.sort_by_key(|u| (*u.worker.as_bytes(), u.clock));
        v
    }

    /// All ranges with at least one buffered update, sorted.
    pub fn ranges(&self) -> Vec<ParamRange> {
        let mut r: Vec<ParamRange> = self.buffered.keys().copied().collect();
        r.sort();
        r
    }

    pub fn len(&self) -> usize {
        self.buffered.values().map(|v| v.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Drain everything at the epoch barrier.
    pub fn clear(&mut self) {
        self.buffered.clear();
    }
}

#[cfg(test)]
mod aggregation_tests {
    use super::*;
    use qfc_types::Address;
    use rand::rngs::StdRng;
    use rand::{Rng, SeedableRng};

    fn upd(worker: u8, range: ParamRange, values: Vec<f32>) -> ParamUpdate {
        ParamUpdate {
            worker: Address::new([worker; 20]),
            epoch: 1,
            clock: 0,
            range,
            values,
            flops_estimated: 1,
        }
    }

    fn refs(updates: &[ParamUpdate]) -> Vec<&ParamUpdate> {
        updates.iter().collect()
    }

    /// Coordinate-wise mean of honest updates only, accumulated in f64 —
    /// the reference point the robustness bound is measured against.
    fn honest_mean(updates: &[ParamUpdate], dim: usize) -> Vec<f32> {
        (0..dim)
            .map(|j| {
                let sum: f64 = updates.iter().map(|u| f64::from(u.values[j])).sum();
                (sum / updates.len() as f64) as f32
            })
            .collect()
    }

    /// n honest updates drawn around `g` with per-coordinate uniform noise
    /// in [-sigma, sigma].
    fn honest_updates(
        rng: &mut StdRng,
        n: usize,
        g: &[f32],
        sigma: f32,
        range: ParamRange,
    ) -> Vec<ParamUpdate> {
        (0..n)
            .map(|w| {
                let values = g
                    .iter()
                    .map(|&gj| gj + sigma * rng.gen_range(-1.0f32..1.0))
                    .collect();
                upd(w as u8 + 1, range, values)
            })
            .collect()
    }

    /// f colluding malicious updates, worst case: all push the same
    /// direction with an extreme magnitude.
    fn malicious_updates(f: usize, dim: usize, range: ParamRange) -> Vec<ParamUpdate> {
        (0..f)
            .map(|k| upd(200 + k as u8, range, vec![1e30; dim]))
            .collect()
    }

    /// Test 1 (A2): hand-computed trimmed mean on a tiny fixture.
    /// 5 updates x 4 coords, beta = 0.2 -> ceil(0.2 * 5) = 1 dropped per end
    /// per coordinate.
    #[test]
    fn test_trimmed_mean_hand_computed() {
        let r = ParamRange::new(0, 4).unwrap();
        // Per-coordinate columns:
        //   c0: [1, 2, 3, 4, 100]   -> drop 1, 100   -> mean(2, 3, 4)    = 3
        //   c1: [3, -50, 0, 1, 2]   -> drop -50, 3   -> mean(0, 1, 2)    = 1
        //   c2: [5, 5, 5, 5, 5]     -> drop 5, 5     -> mean(5, 5, 5)    = 5
        //   c3: [40, 0, 30, 10, 20] -> drop 0, 40    -> mean(10, 20, 30) = 20
        let updates = vec![
            upd(1, r, vec![1.0, 3.0, 5.0, 40.0]),
            upd(2, r, vec![2.0, -50.0, 5.0, 0.0]),
            upd(3, r, vec![3.0, 0.0, 5.0, 30.0]),
            upd(4, r, vec![4.0, 1.0, 5.0, 10.0]),
            upd(5, r, vec![100.0, 2.0, 5.0, 20.0]),
        ];
        let rule = TrimmedMean::new(0.2).unwrap();
        let agg = rule.aggregate(&refs(&updates), &r).unwrap();
        assert_eq!(agg, vec![3.0, 1.0, 5.0, 20.0]);

        // Determinism / permutation invariance: reversed worker order gives
        // bit-identical output (per-coordinate sort erases input order).
        let mut rev = refs(&updates);
        rev.reverse();
        assert_eq!(rule.aggregate(&rev, &r).unwrap(), agg);
    }

    /// Test 2 (A2, ADR-0003): the robustness bound holds below the threshold.
    /// 20 honest + f = 3 colluding extremes (f < beta * n = 0.2 * 23 = 4.6);
    /// trim = ceil(0.2 * 23) = 5 per end, so every extreme is trimmed and the
    /// aggregate stays within a generous 4 * sigma of the honest mean. Fixed
    /// seeds (StdRng::seed_from_u64) keep CI deterministic.
    #[test]
    fn test_robustness_bound_holds_below_threshold() {
        let dim = 8usize;
        let r = ParamRange::new(0, dim as u64).unwrap();
        let sigma = 0.05f32;
        let rule = TrimmedMean::new(0.2).unwrap();
        let mut max_shift = 0.0f32;
        for seed in 0..300u64 {
            let mut rng = StdRng::seed_from_u64(seed);
            let g: Vec<f32> = (0..dim).map(|_| rng.gen_range(-1.0f32..1.0)).collect();
            let honest = honest_updates(&mut rng, 20, &g, sigma, r);
            let malicious = malicious_updates(3, dim, r);
            let reference = honest_mean(&honest, dim);

            let all: Vec<&ParamUpdate> = honest.iter().chain(malicious.iter()).collect();
            let agg = rule.aggregate(&all, &r).unwrap();
            for j in 0..dim {
                let shift = (agg[j] - reference[j]).abs();
                max_shift = max_shift.max(shift);
                assert!(
                    shift <= 4.0 * sigma,
                    "seed {}: coord {} shifted {} > 4*sigma = {}",
                    seed,
                    j,
                    shift,
                    4.0 * sigma
                );
            }
        }
        println!(
            "robustness bound: max adversarial shift {:.6} over 300 seeds (bound {:.6})",
            max_shift,
            4.0 * sigma
        );
    }

    /// Test 3 (A2, ADR-0003): "test documents the cliff". ADR-0003 tolerates
    /// f < beta * n; this pins down exactly where one-sided collusion breaks
    /// through, so the assignment-layer cap on malicious share (A3) is
    /// visibly load-bearing.
    ///
    /// With trim t = ceil(beta * n) per end, an extreme survives into the
    /// kept window iff f > t. NOTE: at f = ceil(beta * n) exactly (the
    /// nominal threshold) the ceil still trims every extreme — the
    /// implementation is one update *better* than ADR-0003's stated
    /// "f < beta * n" guarantee. The first breaking f is ceil(beta * n) + 1,
    /// which satisfies the ADR's break condition f >= beta * n. Both edges
    /// are asserted here: n = 20, beta = 0.2, t = 4 -> f = 4 still bounded,
    /// f = 5 catastrophic (a 1e30 extreme survives in every coordinate).
    #[test]
    fn test_robustness_cliff_at_threshold() {
        let dim = 8usize;
        let r = ParamRange::new(0, dim as u64).unwrap();
        let sigma = 0.05f32;
        let rule = TrimmedMean::new(0.2).unwrap();
        let mut broken_seeds = 0usize;
        let mut max_break_shift = 0.0f32;
        for seed in 0..100u64 {
            let mut rng = StdRng::seed_from_u64(seed);
            let g: Vec<f32> = (0..dim).map(|_| rng.gen_range(-1.0f32..1.0)).collect();

            // Edge of the guarantee: 16 honest + 4 extremes, n = 20,
            // f = t = ceil(0.2 * 20) = 4 -> all extremes still trimmed.
            let honest = honest_updates(&mut rng, 16, &g, sigma, r);
            let reference = honest_mean(&honest, dim);
            let malicious = malicious_updates(4, dim, r);
            let all: Vec<&ParamUpdate> = honest.iter().chain(malicious.iter()).collect();
            let agg = rule.aggregate(&all, &r).unwrap();
            for j in 0..dim {
                assert!(
                    (agg[j] - reference[j]).abs() <= 4.0 * sigma,
                    "seed {}: f = t should still be trimmed away",
                    seed
                );
            }

            // One past the edge: 15 honest + 5 extremes, n = 20, f = 5 > t = 4
            // (f >= beta * n = 4, ADR-0003's break condition). One 1e30
            // survives per coordinate; kept = 12, so the aggregate lands near
            // 1e30 / 12 ~ 8.3e28 — the adversary owns the result.
            let honest = honest_updates(&mut rng, 15, &g, sigma, r);
            let reference = honest_mean(&honest, dim);
            let malicious = malicious_updates(5, dim, r);
            let all: Vec<&ParamUpdate> = honest.iter().chain(malicious.iter()).collect();
            let agg = rule.aggregate(&all, &r).unwrap();
            let shift = (0..dim)
                .map(|j| (agg[j] - reference[j]).abs())
                .fold(0.0f32, f32::max);
            if shift > 4.0 * sigma {
                broken_seeds += 1;
                max_break_shift = max_break_shift.max(shift);
            }
            assert!(
                shift > 1e20,
                "seed {}: expected catastrophic break at f > ceil(beta*n), got shift {}",
                seed,
                shift
            );
        }
        assert_eq!(broken_seeds, 100, "the cliff must break for every seed");
        println!(
            "cliff: f = ceil(beta*n)+1 broke 100/100 seeds, max shift {:.3e}",
            max_break_shift
        );
    }

    /// Test 4 (A2, ADR-0003): plain Mean is poisonable — a single malicious
    /// worker among 19 honest moves the average arbitrarily far. This is why
    /// FedAvg is permanently rejected as a production rule.
    #[test]
    fn test_mean_baseline_poisonable() {
        let dim = 4usize;
        let r = ParamRange::new(0, dim as u64).unwrap();
        let mut rng = StdRng::seed_from_u64(42);
        let g: Vec<f32> = (0..dim).map(|_| rng.gen_range(-1.0f32..1.0)).collect();
        let honest = honest_updates(&mut rng, 19, &g, 0.05, r);
        let reference = honest_mean(&honest, dim);
        let malicious = malicious_updates(1, dim, r);
        let all: Vec<&ParamUpdate> = honest.iter().chain(malicious.iter()).collect();
        let agg = Mean.aggregate(&all, &r).unwrap();
        for j in 0..dim {
            // 1e30 / 20 = 5e28: one worker fully owns the aggregate.
            assert!(
                (agg[j] - reference[j]).abs() > 1e20,
                "Mean should be unboundedly poisonable"
            );
        }
    }

    /// Test 5 (A2): degenerate inputs and defensive checks.
    #[test]
    fn test_degenerate_inputs() {
        let r = ParamRange::new(0, 2).unwrap();
        let rule = TrimmedMean::new(0.2).unwrap();

        // Empty update set errors.
        assert!(matches!(
            rule.aggregate(&[], &r),
            Err(PsError::Aggregation(_))
        ));
        assert!(matches!(
            Mean.aggregate(&[], &r),
            Err(PsError::Aggregation(_))
        ));

        // Range mismatch errors.
        let other = ParamRange::new(2, 4).unwrap();
        let mismatched = upd(1, other, vec![1.0, 2.0]);
        assert!(matches!(
            rule.aggregate(&[&mismatched], &r),
            Err(PsError::Aggregation(_))
        ));

        // Value-length mismatch errors.
        let short = upd(1, r, vec![1.0]);
        assert!(matches!(
            rule.aggregate(&[&short], &r),
            Err(PsError::Aggregation(_))
        ));

        // Too few updates for beta: n = 2, beta = 0.4 trims ceil(0.8) = 1 per
        // end, leaving nothing. Error message names n and beta so operators
        // can see which knob to turn.
        let rule04 = TrimmedMean::new(0.4).unwrap();
        let updates = vec![upd(1, r, vec![1.0, 2.0]), upd(2, r, vec![3.0, 4.0])];
        match rule04.aggregate(&refs(&updates), &r) {
            Err(PsError::Aggregation(msg)) => {
                assert!(msg.contains("n = 2"), "message must name n: {}", msg);
                assert!(
                    msg.contains("beta = 0.4"),
                    "message must name beta: {}",
                    msg
                );
            }
            other => panic!("expected Aggregation error, got {:?}", other),
        }
        // A single update trims away at any beta > 0 too.
        assert!(rule.aggregate(&[&updates[0]], &r).is_err());

        // Single update with beta = 0 passes through as the identity.
        let rule0 = TrimmedMean::new(0.0).unwrap();
        let one = vec![upd(1, r, vec![1.25, -7.5])];
        assert_eq!(rule0.aggregate(&refs(&one), &r).unwrap(), vec![1.25, -7.5]);

        // beta = 0 over multiple updates == plain mean (exactly, with
        // dyadic-friendly values: both rules accumulate in f64).
        let many = vec![
            upd(1, r, vec![1.0, -2.0]),
            upd(2, r, vec![2.5, 0.5]),
            upd(3, r, vec![-0.5, 7.5]),
            upd(4, r, vec![5.0, 2.0]),
        ];
        let trimmed0 = rule0.aggregate(&refs(&many), &r).unwrap();
        let plain = Mean.aggregate(&refs(&many), &r).unwrap();
        assert_eq!(trimmed0, plain);
        assert_eq!(plain, vec![2.0, 2.0]);
    }

    /// `min_updates` is the exact boundary where trimming stops erasing
    /// everything: aggregation succeeds at `min_updates` and the barrier
    /// skips (rather than aborts on) anything below it.
    #[test]
    fn test_min_updates_boundary() {
        assert_eq!(Mean.min_updates(), 1);
        assert_eq!(TrimmedMean::new(0.0).unwrap().min_updates(), 1);
        // beta = 0.2: n = 3 trims 1 per side, keeps 1.
        assert_eq!(TrimmedMean::new(0.2).unwrap().min_updates(), 3);
        // beta = 0.4: n = 5 trims 2 per side, keeps 1 (n = 3, 4 keep 0).
        assert_eq!(TrimmedMean::new(0.4).unwrap().min_updates(), 5);

        let r = ParamRange::new(0, 1).unwrap();
        for beta in [0.0, 0.1, 0.2, 0.25, 0.4, 0.499] {
            let rule = TrimmedMean::new(beta).unwrap();
            let n = rule.min_updates();
            // Worker ids may repeat for large n (beta near 0.5 needs many
            // updates); aggregate() does not dedup, so that is fine here.
            let updates: Vec<ParamUpdate> = (0..n).map(|w| upd(w as u8, r, vec![1.0])).collect();
            assert!(
                rule.aggregate(&refs(&updates), &r).is_ok(),
                "beta {}: aggregate must succeed at min_updates = {}",
                beta,
                n
            );
            if n > 1 {
                assert!(
                    rule.aggregate(&refs(&updates[..n - 1]), &r).is_err(),
                    "beta {}: aggregate must fail below min_updates",
                    beta
                );
            }
        }
    }

    /// Test 6 (A2): constructor rejects beta outside [0, 0.5).
    #[test]
    fn test_new_rejects_invalid_beta() {
        assert!(matches!(
            TrimmedMean::new(-0.1),
            Err(PsError::Aggregation(_))
        ));
        assert!(matches!(
            TrimmedMean::new(0.5),
            Err(PsError::Aggregation(_))
        ));
        assert!(matches!(
            TrimmedMean::new(1.0),
            Err(PsError::Aggregation(_))
        ));
        assert!(matches!(
            TrimmedMean::new(f64::NAN),
            Err(PsError::Aggregation(_))
        ));
        assert_eq!(TrimmedMean::new(0.0).unwrap().beta, 0.0);
        assert_eq!(TrimmedMean::new(0.2).unwrap().beta, 0.2);
        assert_eq!(TrimmedMean::new(0.499).unwrap().beta, 0.499);
    }
}

#[cfg(test)]
mod buffer_tests {
    use super::*;
    use qfc_types::Address;

    fn update(worker: u8, clock: u64, range: ParamRange) -> ParamUpdate {
        ParamUpdate {
            worker: Address::new([worker; 20]),
            epoch: 1,
            clock,
            range,
            values: vec![1.0; range.len() as usize],
            flops_estimated: 1,
        }
    }

    #[test]
    fn test_buffer_dedup_and_cap() {
        let r = ParamRange::new(0, 4).unwrap();
        let mut buf = UpdateBuffer::new(2);
        buf.add(update(1, 0, r)).unwrap();
        // Same worker, new clock: allowed
        buf.add(update(1, 1, r)).unwrap();
        // Duplicate (worker, clock): rejected
        assert!(matches!(
            buf.add(update(1, 0, r)),
            Err(PsError::InvalidUpdate(_))
        ));
        // Cap reached
        assert!(matches!(
            buf.add(update(2, 0, r)),
            Err(PsError::WorkerCapExceeded { cap: 2 })
        ));
        assert_eq!(buf.len(), 2);
        buf.clear();
        assert!(buf.is_empty());
    }

    #[test]
    fn test_buffer_deterministic_order() {
        let r = ParamRange::new(0, 2).unwrap();
        let mut buf = UpdateBuffer::new(0);
        buf.add(update(3, 0, r)).unwrap();
        buf.add(update(1, 1, r)).unwrap();
        buf.add(update(1, 0, r)).unwrap();
        let order: Vec<(u8, u64)> = buf
            .updates_for(&r)
            .iter()
            .map(|u| (u.worker.as_bytes()[0], u.clock))
            .collect();
        assert_eq!(order, vec![(1, 0), (1, 1), (3, 0)]);
    }
}
