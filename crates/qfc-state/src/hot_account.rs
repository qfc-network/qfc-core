//! Hot-account attribution (SRE roadmap T8).
//!
//! `qfc-storage`'s hot-key sampler sees raw keys — for the `state` CF those
//! are trie *node hashes*, which cannot be mapped back to accounts. This
//! module does the attribution at the layer that knows the key encoding:
//! [`crate::StateDB`] samples its own account-level API
//! (`get_account` / `set_account` / `get_code`) into heavy-hitter sketches
//! keyed by [`Address`] (accounts) and code [`Hash`] (contract bytecode).
//!
//! Sampling is recorded at StateDB API entry, *before* the LRU caches — the
//! report therefore reflects the logical access distribution (what cache
//! sizing needs), not just cache misses. Internal helpers funnel through
//! `get_account`/`set_account`, so e.g. one `set_balance` counts one account
//! read + one account write.
//!
//! Enabled automatically (at the same 1-in-N rate and sketch capacity) when
//! the underlying [`qfc_storage::Database`] was opened with
//! [`qfc_storage::StorageConfig::hot_key_sampling`] set. The sampling
//! machinery and error bounds are shared with `qfc-storage` — see
//! [`qfc_storage::hotkeys`] for the space-saving sketch guarantees.

use parking_lot::Mutex;
use qfc_storage::SpaceSaving;
use qfc_types::{Address, Hash};
use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Samples StateDB account/code accesses into heavy-hitter sketches.
pub struct HotAccountTracker {
    /// Effective 1-in-N rate (power of two).
    rate: u32,
    mask: u64,
    tick: AtomicU64,
    window_start_unix_ms: AtomicU64,
    account_reads_sampled: AtomicU64,
    account_writes_sampled: AtomicU64,
    code_reads_sampled: AtomicU64,
    /// Hot accounts (reads + writes combined in one sketch).
    accounts: Mutex<SpaceSaving<Address>>,
    /// Hot contract code, keyed by code hash (content-addressed).
    code: Mutex<SpaceSaving<Hash>>,
}

impl HotAccountTracker {
    /// `rate` is rounded up to the next power of two (min 1); `capacity` is
    /// the per-sketch tracked-key budget.
    pub fn new(rate: u32, capacity: usize) -> Self {
        let rate = rate.max(1).next_power_of_two();
        Self {
            rate,
            mask: u64::from(rate) - 1,
            tick: AtomicU64::new(0),
            window_start_unix_ms: AtomicU64::new(unix_ms()),
            account_reads_sampled: AtomicU64::new(0),
            account_writes_sampled: AtomicU64::new(0),
            code_reads_sampled: AtomicU64::new(0),
            accounts: Mutex::new(SpaceSaving::new(capacity)),
            code: Mutex::new(SpaceSaving::new(capacity)),
        }
    }

    /// Effective sampling rate (power of two).
    pub fn rate(&self) -> u32 {
        self.rate
    }

    #[inline]
    fn should_sample(&self) -> bool {
        self.tick.fetch_add(1, Ordering::Relaxed) & self.mask == 0
    }

    /// Record an account read (StateDB `get_account`).
    #[inline]
    pub(crate) fn record_account_read(&self, address: &Address) {
        if !self.should_sample() {
            return;
        }
        self.account_reads_sampled.fetch_add(1, Ordering::Relaxed);
        self.accounts.lock().offer(*address);
    }

    /// Record an account write (StateDB `set_account`).
    #[inline]
    pub(crate) fn record_account_write(&self, address: &Address) {
        if !self.should_sample() {
            return;
        }
        self.account_writes_sampled.fetch_add(1, Ordering::Relaxed);
        self.accounts.lock().offer(*address);
    }

    /// Record a contract-code read (StateDB `get_code` with a code hash).
    #[inline]
    pub(crate) fn record_code_read(&self, code_hash: &Hash) {
        if !self.should_sample() {
            return;
        }
        self.code_reads_sampled.fetch_add(1, Ordering::Relaxed);
        self.code.lock().offer(*code_hash);
    }

    /// Snapshot the current window with at most `top_n` accounts / code
    /// hashes.
    pub fn report(&self, top_n: usize) -> HotAccountReport {
        let rate = u64::from(self.rate);
        let accounts = self.accounts.lock();
        let top_accounts = accounts
            .top(top_n)
            .into_iter()
            .map(|(addr, count, error)| HotAccountEntry {
                address: addr.to_string(),
                sampled_count: count,
                estimated_count: count * rate,
                max_overestimate: error * rate,
            })
            .collect();
        let sketch_capacity = accounts.capacity();
        drop(accounts);

        let top_code = self
            .code
            .lock()
            .top(top_n)
            .into_iter()
            .map(|(hash, count, error)| HotCodeEntry {
                code_hash: hash.to_string(),
                sampled_count: count,
                estimated_count: count * rate,
                max_overestimate: error * rate,
            })
            .collect();

        let reads = self.account_reads_sampled.load(Ordering::Relaxed);
        let writes = self.account_writes_sampled.load(Ordering::Relaxed);
        let code_reads = self.code_reads_sampled.load(Ordering::Relaxed);
        HotAccountReport {
            sampling_rate: self.rate,
            sketch_capacity,
            window_start_unix_ms: self.window_start_unix_ms.load(Ordering::Relaxed),
            account_reads_sampled: reads,
            account_writes_sampled: writes,
            code_reads_sampled: code_reads,
            estimated_account_reads: reads * rate,
            estimated_account_writes: writes * rate,
            estimated_code_reads: code_reads * rate,
            top_accounts,
            top_code,
        }
    }

    /// Reset counters and sketches and start a new window.
    pub fn reset(&self) {
        self.account_reads_sampled.store(0, Ordering::Relaxed);
        self.account_writes_sampled.store(0, Ordering::Relaxed);
        self.code_reads_sampled.store(0, Ordering::Relaxed);
        self.accounts.lock().clear();
        self.code.lock().clear();
        self.window_start_unix_ms
            .store(unix_ms(), Ordering::Relaxed);
    }
}

/// Serializable hot-account report for one sampling window.
///
/// Estimates carry the same error characteristics as the storage-level
/// report: space-saving overestimation bounded by `max_overestimate`, plus
/// 1-in-N sampling noise (relative error ~ `sqrt(N / true_count)`).
#[derive(Clone, Debug, Serialize)]
pub struct HotAccountReport {
    /// Effective 1-in-N sampling rate (power of two).
    pub sampling_rate: u32,
    /// Heavy-hitter sketch capacity (accounts / code hashes tracked).
    pub sketch_capacity: usize,
    /// Window start (Unix milliseconds); set at creation and on `reset()`.
    pub window_start_unix_ms: u64,
    /// Sampled `get_account` calls in this window.
    pub account_reads_sampled: u64,
    /// Sampled `set_account` calls in this window.
    pub account_writes_sampled: u64,
    /// Sampled contract-code reads in this window.
    pub code_reads_sampled: u64,
    /// `account_reads_sampled * sampling_rate`.
    pub estimated_account_reads: u64,
    /// `account_writes_sampled * sampling_rate`.
    pub estimated_account_writes: u64,
    /// `code_reads_sampled * sampling_rate`.
    pub estimated_code_reads: u64,
    /// Hottest accounts (reads + writes), descending by estimated count.
    pub top_accounts: Vec<HotAccountEntry>,
    /// Hottest contract code by code hash, descending by estimated count.
    pub top_code: Vec<HotCodeEntry>,
}

/// One hot account.
#[derive(Clone, Debug, Serialize)]
pub struct HotAccountEntry {
    /// Account address (`0x…`, 20 bytes hex).
    pub address: String,
    /// Raw sampled count (unscaled).
    pub sampled_count: u64,
    /// `sampled_count * sampling_rate`.
    pub estimated_count: u64,
    /// Space-saving overestimation bound, scaled by the sampling rate.
    pub max_overestimate: u64,
}

/// One hot contract bytecode entry.
#[derive(Clone, Debug, Serialize)]
pub struct HotCodeEntry {
    /// Contract code hash (content-addressed; shared by all instances of the
    /// same bytecode).
    pub code_hash: String,
    /// Raw sampled count (unscaled).
    pub sampled_count: u64,
    /// `sampled_count * sampling_rate`.
    pub estimated_count: u64,
    /// Space-saving overestimation bound, scaled by the sampling rate.
    pub max_overestimate: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_rounds_up_and_clamps() {
        assert_eq!(HotAccountTracker::new(0, 8).rate(), 1);
        assert_eq!(HotAccountTracker::new(3, 8).rate(), 4);
        assert_eq!(HotAccountTracker::new(64, 8).rate(), 64);
    }

    #[test]
    fn tracks_accounts_and_code_separately() {
        let t = HotAccountTracker::new(1, 8);
        let a = Address::new([0x11; 20]);
        let b = Address::new([0x22; 20]);
        let h = Hash::new([0x33; 32]);

        for _ in 0..5 {
            t.record_account_read(&a);
        }
        t.record_account_write(&a);
        t.record_account_read(&b);
        for _ in 0..3 {
            t.record_code_read(&h);
        }

        let r = t.report(10);
        assert_eq!(r.account_reads_sampled, 6);
        assert_eq!(r.account_writes_sampled, 1);
        assert_eq!(r.code_reads_sampled, 3);
        assert_eq!(r.top_accounts[0].address, a.to_string());
        assert_eq!(r.top_accounts[0].estimated_count, 6);
        assert_eq!(r.top_code[0].code_hash, h.to_string());
        assert_eq!(r.top_code[0].estimated_count, 3);
    }

    #[test]
    fn reset_clears_window() {
        let t = HotAccountTracker::new(1, 8);
        t.record_account_read(&Address::new([0x11; 20]));
        t.reset();
        let r = t.report(10);
        assert_eq!(r.account_reads_sampled, 0);
        assert!(r.top_accounts.is_empty());
    }
}
