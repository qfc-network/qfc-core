//! Deterministic CPU linear-regression trainer (ADR-0010 Decision 2).
//!
//! The pilot model is **linear regression with MSE loss**, trained by
//! gradient descent. Every step is **bit-reproducible on CPU**: gradients are
//! summed over the dataset rows in a FIXED index order, accumulated in `f64`,
//! then narrowed to `f32`. That is exactly the determinism class ADR-0005
//! stage-1 exact-match verification requires — an honest worker's pushed
//! `ParamUpdate` equals the verifier's `replay_step` output bit-for-bit, and
//! any deviation is caught.
//!
//! Candle / a real BERT-class model is deliberately NOT used: GPU kernel
//! nondeterminism is the precise problem stage-1 sidesteps (and A4b's
//! tolerance band defers). A tiny deterministic model is the honest
//! demonstration of the determinism property.

use qfc_ps::ParamRange;

/// One training example: `features` (length == model `dim`) and a scalar
/// `label`.
#[derive(Clone, Debug)]
pub struct Dataset {
    /// `(features, label)` rows. `features.len() == dim` for every row.
    pub rows: Vec<(Vec<f32>, f32)>,
}

impl Dataset {
    /// Empty dataset.
    pub fn new() -> Self {
        Self { rows: Vec::new() }
    }

    /// Concatenate another dataset's rows onto this one (used to assemble a
    /// worker's assigned shards into one training set, in shard-index order).
    pub fn extend(&mut self, other: &Dataset) {
        self.rows.extend(other.rows.iter().cloned());
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

impl Default for Dataset {
    fn default() -> Self {
        Self::new()
    }
}

/// A tiny linear model: `y_hat = params · x` (no separate bias term; callers
/// that want a bias append a constant `1.0` feature). `dim` is the parameter
/// (and feature) count; `learning_rate` scales each gradient step.
#[derive(Clone, Copy, Debug)]
pub struct PilotModel {
    pub dim: usize,
    pub learning_rate: f64,
}

impl PilotModel {
    pub fn new(dim: usize, learning_rate: f64) -> Self {
        Self { dim, learning_rate }
    }
}

/// A small deterministic LCG (Numerical Recipes constants) used ONLY to pick
/// which dataset rows enter a step's batch. Kept local so batch selection is a
/// pure, documented function of `(seed, clock)` with no dependency on the
/// global RNG state — the seed in the ADR-0005 replay tuple is therefore
/// load-bearing: a verifier replaying with the same seed selects the same
/// batch and gets a bit-identical delta.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        // Numerical Recipes LCG; deterministic across platforms.
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0
    }
}

/// Deterministically select `batch_size` distinct row indices from `[0, n)`,
/// returned in SORTED order, driven by `seed`. Sorting fixes the gradient
/// summation order independent of selection order — load-bearing for
/// bit-reproducibility.
///
/// If `batch_size >= n` (or `batch_size == 0`), the full row set `[0, n)` is
/// used (full-batch gradient descent). Documented as DETERMINISTIC: identical
/// `(seed, n, batch_size)` always yields the identical index vector.
fn select_batch(seed: u64, n: usize, batch_size: usize) -> Vec<usize> {
    if n == 0 {
        return Vec::new();
    }
    if batch_size == 0 || batch_size >= n {
        return (0..n).collect();
    }
    let mut rng = Lcg::new(seed);
    let mut chosen = vec![false; n];
    let mut out = Vec::with_capacity(batch_size);
    let mut picked = 0;
    while picked < batch_size {
        let idx = (rng.next() % n as u64) as usize;
        if !chosen[idx] {
            chosen[idx] = true;
            out.push(idx);
            picked += 1;
        }
    }
    out.sort_unstable();
    out
}

/// Mix the per-(job, epoch, worker) `seed` with the step `clock` so successive
/// steps in an epoch select different batches (otherwise every step would
/// produce the identical delta). Pure and deterministic — the verifier mixes
/// the same way from `ctx.seed` + `ctx.clock`.
fn step_seed(seed: u64, clock: u64) -> u64 {
    // Splitmix-style finalizer of (seed XOR clock-stream) — deterministic.
    let mut z = seed ^ clock.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Default batch size for one step: a fixed fraction of the worker's local
/// rows, at least 1. Deterministic given the row count.
fn batch_size_for(n: usize) -> usize {
    // Half the rows per step (seed-selected) — exercises the seed without
    // collapsing to full-batch. At least 1.
    (n / 2).max(1)
}

/// DETERMINISTIC per-step delta (ADR-0010 Decision 2).
///
/// Computes the MSE gradient `g` of a linear model over a `seed`/`clock`-
/// selected batch of `data`'s rows, in FIXED (sorted) index order, accumulated
/// in `f64`, narrowed to `f32`, and returns the delta SEGMENT for `range`:
/// `(-lr · g)[range.start..range.end]`.
///
/// MSE loss for a row is `(params·x − y)²`; its gradient w.r.t. `params[j]` is
/// `2·(params·x − y)·x[j]`. Averaged over the batch.
///
/// **Determinism contract**: identical `(params, data, seed, clock, range, lr)`
/// produce a BIT-IDENTICAL `Vec<f32>`. Batch selection is deterministic
/// (`select_batch` over `step_seed(seed, clock)`); accumulation order is fixed
/// (sorted batch indices, then ascending coordinate). This is what makes the
/// honest worker's `ParamUpdate` equal the verifier's `replay_step` output
/// bit-for-bit under ADR-0005 exact-match.
pub fn compute_delta(
    params: &[f32],
    data: &Dataset,
    seed: u64,
    clock: u64,
    range: ParamRange,
    lr: f64,
) -> Vec<f32> {
    let dim = params.len();
    let seg_lo = range.start as usize;
    let seg_hi = range.end as usize;
    let seg_len = seg_hi - seg_lo;

    // No data (worker assigned zero shards) → zero delta for the segment. A
    // zero delta is still a well-formed, bit-reproducible update.
    if data.is_empty() || dim == 0 {
        return vec![0.0f32; seg_len];
    }

    let batch = select_batch(
        step_seed(seed, clock),
        data.len(),
        batch_size_for(data.len()),
    );
    let batch_n = batch.len().max(1) as f64;

    // Full-dim gradient accumulator in f64 (ADR-0010: fixed order, f64 accum).
    let mut grad = vec![0.0f64; dim];
    for &i in &batch {
        let (x, y) = &data.rows[i];
        // prediction = params · x, accumulated in f64.
        let mut pred = 0.0f64;
        for j in 0..dim {
            pred += f64::from(params[j]) * f64::from(x[j]);
        }
        let err = pred - f64::from(*y); // residual
                                        // grad[j] += 2 * err * x[j]
        for j in 0..dim {
            grad[j] += 2.0 * err * f64::from(x[j]);
        }
    }
    // Mean over the batch.
    for g in grad.iter_mut() {
        *g /= batch_n;
    }

    // Delta = -lr * grad, narrowed to f32, only the requested segment.
    grad[seg_lo..seg_hi]
        .iter()
        .map(|&g| (-lr * g) as f32)
        .collect()
}

/// Mean-squared error of `params` over the FULL `data` (fixed row order, f64
/// accumulation). Used to report convergence per epoch.
pub fn mse_loss(params: &[f32], data: &Dataset) -> f64 {
    if data.is_empty() {
        return 0.0;
    }
    let dim = params.len();
    let mut sum = 0.0f64;
    for (x, y) in &data.rows {
        let mut pred = 0.0f64;
        for j in 0..dim {
            pred += f64::from(params[j]) * f64::from(x[j]);
        }
        let e = pred - f64::from(*y);
        sum += e * e;
    }
    sum / data.rows.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(start: u64, end: u64) -> ParamRange {
        ParamRange::new(start, end).unwrap()
    }

    /// y = 3*x0 - 2*x1 + 1*bias, exact (no noise).
    fn linear_dataset(n: usize) -> Dataset {
        let mut rows = Vec::new();
        for i in 0..n {
            let x0 = (i as f32) * 0.1;
            let x1 = (i as f32) * 0.05 - 1.0;
            let bias = 1.0f32;
            let y = 3.0 * x0 - 2.0 * x1 + 1.0 * bias;
            rows.push((vec![x0, x1, bias], y));
        }
        Dataset { rows }
    }

    #[test]
    fn compute_delta_is_bit_deterministic() {
        let data = linear_dataset(20);
        let params = vec![0.0f32; 3];
        let a = compute_delta(&params, &data, 42, 0, range(0, 3), 0.01);
        let b = compute_delta(&params, &data, 42, 0, range(0, 3), 0.01);
        // Byte-identical, not just approximately equal.
        assert_eq!(a, b);
        assert_eq!(
            a.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
            b.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn compute_delta_segment_matches_full() {
        let data = linear_dataset(20);
        let params = vec![0.1f32, -0.2, 0.3];
        let full = compute_delta(&params, &data, 7, 1, range(0, 3), 0.05);
        let seg = compute_delta(&params, &data, 7, 1, range(1, 3), 0.05);
        // The segment for [1,3) is exactly the [1..3] slice of the full delta.
        assert_eq!(&full[1..3], &seg[..]);
    }

    #[test]
    fn different_clocks_select_different_batches() {
        let data = linear_dataset(40);
        let params = vec![0.0f32; 3];
        let s0 = compute_delta(&params, &data, 99, 0, range(0, 3), 0.01);
        let s1 = compute_delta(&params, &data, 99, 1, range(0, 3), 0.01);
        // Different step clocks pick different sub-batches → different deltas.
        assert_ne!(s0, s1);
    }

    #[test]
    fn gradient_descent_converges() {
        let data = linear_dataset(50);
        let dim = 3;
        let mut params = vec![0.0f32; dim];
        let lr = 0.02;
        let initial = mse_loss(&params, &data);
        // Full-batch GD: batch_size_for >= picks half, but a fixed seed/clock
        // is enough to demonstrate monotone decrease over many steps.
        for step in 0..400u64 {
            let delta = compute_delta(&params, &data, 1, step, range(0, dim as u64), lr);
            for (p, d) in params.iter_mut().zip(&delta) {
                *p += *d;
            }
        }
        let final_loss = mse_loss(&params, &data);
        assert!(
            final_loss < initial,
            "loss should decrease: {initial} -> {final_loss}"
        );
        // It should converge close to zero on a noiseless linear target.
        assert!(final_loss < initial * 0.1, "loss should drop by >10x");
    }

    #[test]
    fn select_batch_is_sorted_and_distinct() {
        let b = select_batch(12345, 100, 10);
        assert_eq!(b.len(), 10);
        let mut sorted = b.clone();
        sorted.sort_unstable();
        assert_eq!(b, sorted, "must be returned sorted");
        sorted.dedup();
        assert_eq!(sorted.len(), 10, "must be distinct");
    }

    #[test]
    fn empty_data_yields_zero_delta() {
        let data = Dataset::new();
        let params = vec![1.0f32; 3];
        let d = compute_delta(&params, &data, 5, 0, range(0, 3), 0.1);
        assert_eq!(d, vec![0.0f32; 3]);
    }
}
