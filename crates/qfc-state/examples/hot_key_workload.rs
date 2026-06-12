//! Synthetic hot-key workload for SRE T8 findings (docs/profiling/HOT-KEYS.md).
//!
//! Simulates a block-import-shaped workload against a temporary database
//! with hot-key sampling enabled at the production default (1-in-64):
//! zipf-distributed transfers between accounts, zipf-distributed contract
//! code reads, and periodic state commits. Dumps the storage-level
//! `hot_key_report` and the state-level `hot_account_report` as JSON, plus a
//! rank/count table to eyeball the power-law shape.
//!
//! Run: `cargo run -p qfc-state --release --example hot_key_workload [transfers]`

use qfc_state::StateDB;
use qfc_storage::{Database, StorageConfig};
use qfc_types::{Address, U256};
use std::time::Instant;

/// Deterministic xorshift64* PRNG.
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

/// Zipf(s) sampler over ranks 0..n via inverse-CDF table.
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
    fn sample(&self, rng: &mut Rng) -> usize {
        let u = rng.next_f64();
        self.cdf.partition_point(|&c| c < u).min(self.cdf.len() - 1)
    }
}

fn addr_for(rank: usize) -> Address {
    let mut bytes = [0u8; 20];
    bytes[12..20].copy_from_slice(&(rank as u64).to_be_bytes());
    Address::new(bytes)
}

fn main() {
    let transfers: u64 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(100_000);

    const ACCOUNTS: usize = 5_000;
    const CONTRACTS: usize = 100;
    const COMMIT_EVERY: u64 = 500;

    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        path: dir.path().to_path_buf(),
        create_if_missing: true,
        hot_key_sampling: Some(64), // production default
        ..Default::default()
    };
    let db = Database::open(config).unwrap();
    let state = StateDB::new(db.clone());

    // Seed balances and contract code.
    for rank in 0..ACCOUNTS {
        state
            .set_balance(&addr_for(rank), U256::from_u64(1_000_000_000))
            .unwrap();
    }
    let contract_base = ACCOUNTS; // contracts live above the EOA range
    for c in 0..CONTRACTS {
        // Distinct bytecode per contract so code hashes differ.
        let code = format!("contract-{c:04}-{}", "f".repeat(64)).into_bytes();
        state.set_code(&addr_for(contract_base + c), code).unwrap();
    }
    state.commit().unwrap();
    // Measure only the steady-state workload, not the seeding phase.
    db.reset_hot_key_stats();
    state.reset_hot_account_stats();

    let acct_zipf = Zipf::new(ACCOUNTS, 1.1);
    let code_zipf = Zipf::new(CONTRACTS, 1.2);
    let mut rng = Rng(0x00C0_FFEE_D00D_F00D);

    let start = Instant::now();
    for i in 0..transfers {
        let from = addr_for(acct_zipf.sample(&mut rng));
        let to = addr_for(acct_zipf.sample(&mut rng));
        if from != to {
            let _ = state.transfer(&from, &to, U256::from_u64(1));
        }
        let _ = state.increment_nonce(&from);
        // One "contract call" (code fetch) per 4 transfers.
        if i % 4 == 0 {
            let c = code_zipf.sample(&mut rng);
            let _ = state.get_code(&addr_for(contract_base + c)).unwrap();
        }
        if (i + 1) % COMMIT_EVERY == 0 {
            state.commit().unwrap();
        }
    }
    state.commit().unwrap();
    let elapsed = start.elapsed();

    eprintln!(
        "workload: {transfers} transfers over {ACCOUNTS} accounts (zipf s=1.1), \
         {CONTRACTS} contracts (zipf s=1.2), commit every {COMMIT_EVERY}; \
         took {elapsed:.2?} ({:.0} transfers/s)",
        transfers as f64 / elapsed.as_secs_f64()
    );

    let storage_report = db.hot_key_report(8).unwrap();
    let state_report = state.hot_account_report(20).unwrap();

    println!("=== storage hot_key_report (top 8 keys/CF) ===");
    println!("{}", serde_json::to_string_pretty(&storage_report).unwrap());
    println!("=== state hot_account_report (top 20) ===");
    println!("{}", serde_json::to_string_pretty(&state_report).unwrap());

    // Rank/count table for the power-law eyeball check: estimated count vs
    // account rank (true zipf rank is recoverable from the address suffix).
    println!("=== top accounts: rank vs estimated count ===");
    println!("report_rank,address,true_zipf_rank,estimated_count,max_overestimate");
    for (i, e) in state_report.top_accounts.iter().enumerate() {
        let true_rank = u64::from_str_radix(&e.address[2..].to_string()[24..], 16).ok();
        println!(
            "{},{},{},{},{}",
            i + 1,
            e.address,
            true_rank.map_or("?".into(), |r| r.to_string()),
            e.estimated_count,
            e.max_overestimate
        );
    }
}
