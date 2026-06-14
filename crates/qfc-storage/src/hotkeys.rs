//! Hot-key access analytics (SRE roadmap T8).
//!
//! Low-overhead 1-in-N sampling of `get`/`put`/`delete`/batch traffic, with a
//! per-column-family heavy-hitter sketch over the sampled keys. The output
//! (top-N hot keys per CF, plus per-CF sampled read/write totals) feeds cache
//! sizing decisions (T3) and is the chain-side mirror of embedding hot-key
//! skew in the AI stack.
//!
//! # Design
//!
//! - **Sampling**: a single shared `AtomicU64` tick, incremented with
//!   `Ordering::Relaxed` on every recorded operation. An op is sampled when
//!   `tick & (rate - 1) == 0`; the configured rate is rounded **up** to a
//!   power of two so the check is a mask, not a division. When sampling is
//!   disabled the `Database` holds no sampler at all, so the per-op cost is a
//!   single `Option` branch (no atomics, no allocation).
//! - **Sketch**: the *space-saving* algorithm (Metwally, Agrawal, El Abbadi,
//!   "Efficient computation of frequent and top-k elements in data streams",
//!   2005) with a fixed capacity `C` per CF. See [`SpaceSaving`] for the
//!   error characteristics.
//! - **Stride-sampling caveat**: deterministic 1-in-N sampling can alias with
//!   access loops whose period divides N. For hash-keyed blockchain traffic
//!   (trie nodes, tx hashes) this is a non-issue; for adversarial or strictly
//!   periodic workloads, interpret results accordingly.
//!
//! # Error bounds (what the report numbers mean)
//!
//! Let `S` = number of *sampled* increments a CF sketch has absorbed in the
//! current window, `C` = sketch capacity, and `N` = sampling rate.
//!
//! 1. **Space-saving guarantee** (within sampled counts): for every tracked
//!    key, `sampled_count - error <= true_sampled <= sampled_count`, and the
//!    per-entry `error` is at most `S / C`. Any key whose true sampled count
//!    exceeds `S / C` is guaranteed to be present in the sketch.
//! 2. **Sampling noise**: `estimated_count = sampled_count * N` estimates the
//!    true access count `T` with standard deviation ~ `sqrt(T * N)`, i.e.
//!    relative error ~ `sqrt(N / T)`. For genuinely hot keys (large `T`) this
//!    shrinks fast — which is exactly the regime the sketch targets.
//!
//! The report exposes both raw `sampled_count` and the scaled
//! `estimated_count`, plus `max_overestimate` (= space-saving `error * N`).

use parking_lot::Mutex;
use serde::Serialize;
use std::collections::HashMap;
use std::hash::Hash as StdHash;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Default 1-in-N sampling rate when hot-key analytics is enabled.
pub const DEFAULT_HOT_KEY_SAMPLING: u32 = 64;

/// Default heavy-hitter sketch capacity (entries per column family).
pub const DEFAULT_HOT_KEY_CAPACITY: usize = 256;

/// Current Unix time in milliseconds (0 if the clock is before the epoch).
fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A counter tracked by [`SpaceSaving`].
#[derive(Clone, Copy, Debug)]
struct SsCounter {
    /// Estimated count (never underestimates the true count).
    count: u64,
    /// Maximum possible overestimation, inherited from the evicted entry.
    error: u64,
}

/// Fixed-capacity *space-saving* heavy-hitter sketch.
///
/// Tracks at most `capacity` distinct keys. When a new key arrives and the
/// sketch is full, the entry with the minimum count is evicted and the new
/// key inherits `min + 1` as its count with `min` recorded as its error.
///
/// Guarantees, for a stream of `S` offers:
/// - `count - error <= true_count <= count` for every tracked key;
/// - the minimum tracked count (hence every `error`) is at most `S / capacity`;
/// - every key with `true_count > S / capacity` is tracked (no false
///   negatives above that threshold).
///
/// Storage layout: entries live in a contiguous `Vec` with a `HashMap`
/// index over keys. Eviction (only on the sampled, new-key, sketch-full
/// path) does an O(capacity) min-scan over the contiguous counters —
/// L1/L2-resident at the default capacity of 256, a few hundred
/// nanoseconds even in the adversarial all-distinct-keys stream.
pub struct SpaceSaving<K> {
    capacity: usize,
    /// key -> position in `entries`.
    index: HashMap<K, usize>,
    entries: Vec<(K, SsCounter)>,
}

impl<K: Eq + StdHash + Clone> SpaceSaving<K> {
    /// Create a sketch tracking at most `capacity` keys (min 1).
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            capacity,
            index: HashMap::with_capacity(capacity),
            entries: Vec::with_capacity(capacity),
        }
    }

    /// Offer one occurrence of `key` to the sketch.
    pub fn offer(&mut self, key: K) {
        if let Some(&i) = self.index.get(&key) {
            self.entries[i].1.count += 1;
            return;
        }
        self.insert_new(key);
    }

    fn insert_new(&mut self, key: K) {
        if self.entries.len() < self.capacity {
            self.index.insert(key.clone(), self.entries.len());
            self.entries.push((key, SsCounter { count: 1, error: 0 }));
            return;
        }
        // Full: evict the minimum-count entry in place; the newcomer
        // inherits its count as the upper bound on how often it might have
        // been seen before being tracked.
        let min_idx = self
            .entries
            .iter()
            .enumerate()
            .min_by_key(|(_, (_, c))| c.count)
            .map(|(i, _)| i)
            .expect("sketch capacity >= 1");
        let min_count = self.entries[min_idx].1.count;
        let new_entry = (
            key.clone(),
            SsCounter {
                count: min_count + 1,
                error: min_count,
            },
        );
        let (old_key, _) = std::mem::replace(&mut self.entries[min_idx], new_entry);
        self.index.remove(&old_key);
        self.index.insert(key, min_idx);
    }

    /// Top `n` keys by estimated count, descending. Returns
    /// `(key, count, error)` triples in *sampled* (unscaled) units.
    pub fn top(&self, n: usize) -> Vec<(K, u64, u64)> {
        let mut all: Vec<_> = self
            .entries
            .iter()
            .map(|(k, c)| (k.clone(), c.count, c.error))
            .collect();
        all.sort_by_key(|e| std::cmp::Reverse(e.1));
        all.truncate(n);
        all
    }

    /// Number of keys currently tracked.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the sketch is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Maximum number of tracked keys.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Drop all tracked keys (capacity is retained).
    pub fn clear(&mut self) {
        self.index.clear();
        self.entries.clear();
    }
}

impl SpaceSaving<Box<[u8]>> {
    /// Byte-slice fast path: avoids allocating the key on the (common)
    /// already-tracked path; allocates only when inserting a new entry.
    pub fn offer_bytes(&mut self, key: &[u8]) {
        if let Some(&i) = self.index.get(key) {
            self.entries[i].1.count += 1;
            return;
        }
        self.insert_new(Box::from(key));
    }
}

/// Per-CF sampling state.
struct CfSampler {
    reads_sampled: AtomicU64,
    writes_sampled: AtomicU64,
    sketch: Mutex<SpaceSaving<Box<[u8]>>>,
}

impl CfSampler {
    fn new(capacity: usize) -> Self {
        Self {
            reads_sampled: AtomicU64::new(0),
            writes_sampled: AtomicU64::new(0),
            sketch: Mutex::new(SpaceSaving::new(capacity)),
        }
    }
}

/// Samples database key accesses and maintains per-CF heavy-hitter sketches.
///
/// Created by [`crate::Database::open`] when
/// [`crate::StorageConfig::hot_key_sampling`] is `Some(rate)` with `rate > 0`.
pub struct HotKeySampler {
    /// Effective 1-in-N rate (configured rate rounded up to a power of two).
    rate: u32,
    /// `rate - 1`, for the mask fast path.
    mask: u64,
    tick: AtomicU64,
    window_start_unix_ms: AtomicU64,
    cfs: HashMap<&'static str, CfSampler>,
}

impl HotKeySampler {
    /// Build a sampler for the given CF names. `rate` is rounded up to the
    /// next power of two (`rate >= 1`).
    pub(crate) fn new(rate: u32, capacity: usize, cf_names: &[&'static str]) -> Self {
        let rate = rate.max(1).next_power_of_two();
        Self {
            rate,
            mask: u64::from(rate) - 1,
            tick: AtomicU64::new(0),
            window_start_unix_ms: AtomicU64::new(unix_ms()),
            cfs: cf_names
                .iter()
                .map(|name| (*name, CfSampler::new(capacity)))
                .collect(),
        }
    }

    /// Effective sampling rate (power of two).
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// One relaxed atomic increment + mask check.
    #[inline]
    fn should_sample(&self) -> bool {
        self.tick.fetch_add(1, Ordering::Relaxed) & self.mask == 0
    }

    /// Record a read access. Hot path: one relaxed `fetch_add`; the sketch is
    /// touched only for the 1-in-N sampled ops.
    #[inline]
    pub(crate) fn record_read(&self, cf: &str, key: &[u8]) {
        if !self.should_sample() {
            return;
        }
        if let Some(s) = self.cfs.get(cf) {
            s.reads_sampled.fetch_add(1, Ordering::Relaxed);
            s.sketch.lock().offer_bytes(key);
        }
    }

    /// Record a write access (put or delete). Same cost profile as
    /// [`Self::record_read`].
    #[inline]
    pub(crate) fn record_write(&self, cf: &str, key: &[u8]) {
        if !self.should_sample() {
            return;
        }
        if let Some(s) = self.cfs.get(cf) {
            s.writes_sampled.fetch_add(1, Ordering::Relaxed);
            s.sketch.lock().offer_bytes(key);
        }
    }

    /// Snapshot the current window into a serializable report. CFs with no
    /// sampled traffic are omitted; CFs are sorted by total sampled traffic
    /// descending; each CF carries at most `top_n` hot keys.
    pub fn report(&self, top_n: usize) -> HotKeyReport {
        let mut cfs: Vec<CfHotKeyStats> = Vec::new();
        let mut capacity = DEFAULT_HOT_KEY_CAPACITY;
        for (name, s) in &self.cfs {
            let reads = s.reads_sampled.load(Ordering::Relaxed);
            let writes = s.writes_sampled.load(Ordering::Relaxed);
            if reads == 0 && writes == 0 {
                continue;
            }
            let sketch = s.sketch.lock();
            capacity = sketch.capacity();
            let rate = u64::from(self.rate);
            let top_keys = sketch
                .top(top_n)
                .into_iter()
                .map(|(key, count, error)| HotKeyEntry {
                    key_hex: hex(&key),
                    sampled_count: count,
                    estimated_count: count * rate,
                    max_overestimate: error * rate,
                })
                .collect();
            cfs.push(CfHotKeyStats {
                cf: (*name).to_string(),
                reads_sampled: reads,
                writes_sampled: writes,
                estimated_reads: reads * rate,
                estimated_writes: writes * rate,
                top_keys,
            });
        }
        cfs.sort_by(|a, b| {
            (b.reads_sampled + b.writes_sampled).cmp(&(a.reads_sampled + a.writes_sampled))
        });
        HotKeyReport {
            sampling_rate: self.rate,
            sketch_capacity: capacity,
            window_start_unix_ms: self.window_start_unix_ms.load(Ordering::Relaxed),
            cfs,
        }
    }

    /// Reset all counters and sketches and start a new window.
    pub fn reset(&self) {
        for s in self.cfs.values() {
            s.reads_sampled.store(0, Ordering::Relaxed);
            s.writes_sampled.store(0, Ordering::Relaxed);
            s.sketch.lock().clear();
        }
        self.window_start_unix_ms
            .store(unix_ms(), Ordering::Relaxed);
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(2 + bytes.len() * 2);
    out.push_str("0x");
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Serializable hot-key report for one sampling window. See the module docs
/// for the exact meaning of the estimate and error fields.
#[derive(Clone, Debug, Serialize)]
pub struct HotKeyReport {
    /// Effective 1-in-N sampling rate (power of two).
    pub sampling_rate: u32,
    /// Heavy-hitter sketch capacity per CF.
    pub sketch_capacity: usize,
    /// Window start (Unix milliseconds); set at open and on `reset()`.
    pub window_start_unix_ms: u64,
    /// Per-CF stats, sorted by total sampled traffic descending. CFs with no
    /// sampled traffic are omitted.
    pub cfs: Vec<CfHotKeyStats>,
}

/// Per-column-family hot-key statistics.
#[derive(Clone, Debug, Serialize)]
pub struct CfHotKeyStats {
    /// Column family name.
    pub cf: String,
    /// Sampled read ops in this window.
    pub reads_sampled: u64,
    /// Sampled write ops (puts + deletes) in this window.
    pub writes_sampled: u64,
    /// `reads_sampled * sampling_rate` — estimated true reads.
    pub estimated_reads: u64,
    /// `writes_sampled * sampling_rate` — estimated true writes.
    pub estimated_writes: u64,
    /// Hottest keys, descending by estimated count.
    pub top_keys: Vec<HotKeyEntry>,
}

/// One hot key with its estimated access count and error bound.
#[derive(Clone, Debug, Serialize)]
pub struct HotKeyEntry {
    /// Hex-encoded raw key (`0x…`).
    pub key_hex: String,
    /// Raw sampled count (space-saving estimate, unscaled).
    pub sampled_count: u64,
    /// `sampled_count * sampling_rate` — estimated true access count.
    pub estimated_count: u64,
    /// Space-saving overestimation bound, scaled by the sampling rate:
    /// the true (estimated) count is in
    /// `[estimated_count - max_overestimate, estimated_count]`
    /// up to sampling noise.
    pub max_overestimate: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic xorshift64* PRNG (no external deps).
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545F4914F6CDD1D)
        }
        fn next_f64(&mut self) -> f64 {
            (self.next() >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    /// Zipf(s) sampler over ranks 1..=n via inverse-CDF table.
    struct Zipf {
        cdf: Vec<f64>,
    }
    impl Zipf {
        fn new(n: usize, s: f64) -> Self {
            let mut weights: Vec<f64> = (1..=n).map(|k| 1.0 / (k as f64).powf(s)).collect();
            let total: f64 = weights.iter().sum();
            let mut acc = 0.0;
            for w in &mut weights {
                acc += *w / total;
                *w = acc;
            }
            Self { cdf: weights }
        }
        /// Returns a rank in 0..n (0 = hottest).
        fn sample(&self, rng: &mut Rng) -> usize {
            let u = rng.next_f64();
            self.cdf.partition_point(|&c| c < u).min(self.cdf.len() - 1)
        }
    }

    #[test]
    fn space_saving_exact_when_under_capacity() {
        let mut ss = SpaceSaving::<u32>::new(16);
        for _ in 0..5 {
            ss.offer(1);
        }
        for _ in 0..3 {
            ss.offer(2);
        }
        ss.offer(3);
        let top = ss.top(10);
        assert_eq!(top[0], (1, 5, 0));
        assert_eq!(top[1], (2, 3, 0));
        assert_eq!(top[2], (3, 1, 0));
    }

    #[test]
    fn space_saving_zipf_accuracy() {
        // 10k distinct keys, zipf(1.05), 200k offers, capacity 256.
        let n = 10_000;
        let offers = 200_000u64;
        let capacity = 256;
        let zipf = Zipf::new(n, 1.05);
        let mut rng = Rng(0x9E3779B97F4A7C15);

        let mut ss = SpaceSaving::<u64>::new(capacity);
        let mut truth = HashMap::<u64, u64>::new();
        for _ in 0..offers {
            let rank = zipf.sample(&mut rng) as u64;
            *truth.entry(rank).or_default() += 1;
            ss.offer(rank);
        }

        let top = ss.top(capacity);
        // Top-1 must always be found: the true hottest key (rank 0).
        assert_eq!(top[0].0, 0, "true hottest key must rank first");

        let bound = offers / capacity as u64;
        for (key, count, error) in &top {
            let true_count = truth.get(key).copied().unwrap_or(0);
            // Documented guarantees: count - error <= true <= count,
            // error <= S/C.
            assert!(*count >= true_count, "estimate must not underestimate");
            assert!(
                count - error <= true_count,
                "count({count}) - error({error}) must lower-bound true({true_count})"
            );
            assert!(*error <= bound, "error({error}) must be <= S/C({bound})");
        }

        // Every key with true count > S/C must be tracked.
        let tracked: std::collections::HashSet<u64> = top.iter().map(|(k, _, _)| *k).collect();
        for (key, true_count) in &truth {
            if *true_count > bound {
                assert!(tracked.contains(key), "key {key} above S/C must be tracked");
            }
        }
    }

    #[test]
    fn space_saving_eviction_inherits_min() {
        let mut ss = SpaceSaving::<u32>::new(2);
        for _ in 0..10 {
            ss.offer(1);
        }
        ss.offer(2); // fills capacity
        ss.offer(3); // evicts key 2 (count 1) -> count 2, error 1
        let top = ss.top(2);
        assert_eq!(top[0], (1, 10, 0));
        assert_eq!(top[1], (3, 2, 1));
    }

    #[test]
    fn sampler_rate_rounds_up_to_power_of_two() {
        let s = HotKeySampler::new(3, 8, &["a"]);
        assert_eq!(s.rate(), 4);
        let s = HotKeySampler::new(64, 8, &["a"]);
        assert_eq!(s.rate(), 64);
        let s = HotKeySampler::new(0, 8, &["a"]); // clamped to 1
        assert_eq!(s.rate(), 1);
    }

    #[test]
    fn sampler_rate_one_counts_everything() {
        let s = HotKeySampler::new(1, 8, &["a", "b"]);
        for _ in 0..10 {
            s.record_read("a", b"k1");
        }
        s.record_write("a", b"k1");
        s.record_write("b", b"k2");

        let report = s.report(10);
        assert_eq!(report.sampling_rate, 1);
        assert_eq!(report.cfs.len(), 2);
        let a = report.cfs.iter().find(|c| c.cf == "a").unwrap();
        assert_eq!(a.reads_sampled, 10);
        assert_eq!(a.writes_sampled, 1);
        assert_eq!(a.estimated_reads, 10);
        assert_eq!(a.top_keys[0].key_hex, "0x6b31"); // "k1"
        assert_eq!(a.top_keys[0].estimated_count, 11);

        // Per-CF isolation: k1 must not appear under "b".
        let b = report.cfs.iter().find(|c| c.cf == "b").unwrap();
        assert_eq!(b.top_keys.len(), 1);
        assert_eq!(b.top_keys[0].key_hex, "0x6b32"); // "k2"
    }

    #[test]
    fn sampler_rate_n_samples_one_in_n() {
        let s = HotKeySampler::new(8, 8, &["a"]);
        for _ in 0..800 {
            s.record_read("a", b"k");
        }
        let report = s.report(10);
        let a = &report.cfs[0];
        assert_eq!(a.reads_sampled, 100);
        assert_eq!(a.estimated_reads, 800);
    }

    #[test]
    fn sampler_unknown_cf_is_ignored() {
        let s = HotKeySampler::new(1, 8, &["a"]);
        s.record_read("nope", b"k");
        assert!(s.report(10).cfs.is_empty());
    }

    #[test]
    fn sampler_reset_clears_and_restarts_window() {
        let s = HotKeySampler::new(1, 8, &["a"]);
        s.record_read("a", b"k");
        let before = s.report(10);
        assert_eq!(before.cfs.len(), 1);

        s.reset();
        let after = s.report(10);
        assert!(after.cfs.is_empty(), "counters and sketches cleared");
        assert!(after.window_start_unix_ms >= before.window_start_unix_ms);
    }

    #[test]
    fn report_serializes_to_json() {
        let s = HotKeySampler::new(1, 8, &["a"]);
        s.record_read("a", b"\x01\x02");
        let json = serde_json::to_string(&s.report(10)).unwrap();
        assert!(json.contains("\"key_hex\":\"0x0102\""));
        assert!(json.contains("\"sampling_rate\":1"));
    }
}
