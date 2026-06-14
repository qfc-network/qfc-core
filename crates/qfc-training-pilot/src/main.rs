//! Demo binary for the A6 training pilot (ADR-0010).
//!
//! Runs `run_pilot` with a sensible default config and prints the per-epoch
//! loss trajectory, cheater-detection result, settlement summary, and the
//! per-epoch commit hashes — the first runnable end-to-end Feature-A demo.

use qfc_training_pilot::{run_pilot, PilotConfig};

#[tokio::main]
async fn main() {
    let cfg = PilotConfig {
        num_miners: 4,
        num_cheaters: 1,
        dim: 8,
        num_shards: 4,
        epochs: 10,
        steps_per_epoch: 4,
        reward_pool_per_epoch: 1_000_000_000_000_000_000, // 1e18 wei
        lr: 0.3,
    };

    println!("=== QFC training pilot (A6, ADR-0010) ===");
    println!(
        "miners={} cheaters={} dim={} shards={} epochs={} steps/epoch={} lr={}",
        cfg.num_miners,
        cfg.num_cheaters,
        cfg.dim,
        cfg.num_shards,
        cfg.epochs,
        cfg.steps_per_epoch,
        cfg.lr
    );
    println!();

    // run_pilot drives a current-thread tokio runtime internally; calling it
    // from an async main is fine (it builds its own runtime for the blocking
    // verify_step calls).
    let report = tokio::task::spawn_blocking(move || run_pilot(cfg))
        .await
        .expect("pilot run");

    println!("--- per-epoch loss trajectory ---");
    println!("  initial loss: {:.6e}", report.initial_loss);
    for (e, loss) in report.losses.iter().enumerate() {
        println!("  epoch {e}: loss = {loss:.6e}");
    }
    let final_loss = report.losses.last().copied().unwrap_or(report.initial_loss);
    let ratio = if final_loss > 0.0 {
        report.initial_loss / final_loss
    } else {
        f64::INFINITY
    };
    println!(
        "  => loss {} ({:.6e} -> {:.6e}, {:.1}x reduction)",
        if final_loss < report.initial_loss {
            "DECREASED"
        } else {
            "DID NOT decrease"
        },
        report.initial_loss,
        final_loss,
        ratio
    );
    println!();

    println!("--- verification ---");
    println!(
        "  cheater caught: {} / {} planted",
        report.cheaters_caught,
        report.cheater_addresses.len()
    );
    for c in &report.cheater_addresses {
        println!("    cheater {c}");
    }
    println!();

    println!("--- settlement summary ---");
    let total_paid: u128 = report.final_balances.values().sum();
    println!("  total rewards paid: {total_paid} wei");
    let mut bals: Vec<_> = report.final_balances.iter().collect();
    bals.sort_by_key(|(a, _)| *a.as_bytes());
    for (addr, bal) in bals {
        let role = if report.cheater_addresses.contains(addr) {
            "cheater"
        } else {
            "honest "
        };
        println!("    [{role}] {addr}: balance = {bal} wei");
    }
    println!();
    println!("  stakes after slashing:");
    let mut stakes: Vec<_> = report.final_stakes.iter().collect();
    stakes.sort_by_key(|(a, _)| *a.as_bytes());
    for (addr, stake) in stakes {
        let role = if report.cheater_addresses.contains(addr) {
            "cheater"
        } else {
            "honest "
        };
        println!("    [{role}] {addr}: stake = {stake} wei");
    }
    println!();

    println!("--- per-epoch commit hashes ---");
    for commit in &report.commits {
        println!(
            "  epoch {}: commit_hash = {} (accepted={}, flops={})",
            commit.epoch,
            commit.commit_hash(),
            commit.accepted_count,
            commit.total_flops_accepted
        );
    }
}
