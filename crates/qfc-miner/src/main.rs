//! QFC AI Inference Miner — standalone binary
//!
//! Connects to a validator node and contributes AI inference compute
//! to earn block rewards.
//!
//! Usage:
//!   qfc-miner --wallet 0x... --backend auto --model-dir ./models

mod config;
mod gpu;
mod submit;
mod worker;

use clap::Parser;
use config::{MinerCli, MinerConfig};
use qfc_types::Address;
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = MinerCli::parse();

    // Setup logging
    let filter = if cli.verbose { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .init();

    info!("QFC AI Inference Miner v{}", env!("CARGO_PKG_VERSION"));

    // Detect hardware
    let hw = gpu::detect_and_log();

    // Parse wallet address
    let wallet_hex = cli.wallet.strip_prefix("0x").unwrap_or(&cli.wallet);
    let wallet_bytes = hex::decode(wallet_hex)
        .map_err(|e| anyhow::anyhow!("Invalid wallet address: {}", e))?;
    let wallet_address = Address::from_slice(&wallet_bytes)
        .ok_or_else(|| anyhow::anyhow!("Wallet address must be 20 bytes"))?;

    // Determine backend
    let backend = cli.backend_type();
    let max_memory = if cli.max_memory > 0 {
        cli.max_memory
    } else {
        hw.memory_mb
    };

    info!("Using backend: {}, max memory: {} MB", backend, max_memory);

    // Create config
    let validator_rpc = cli.validator_rpc.clone();
    let config = MinerConfig {
        validator_rpc: cli.validator_rpc,
        wallet_address,
        backend,
        model_dir: cli.model_dir,
        max_memory_mb: max_memory,
    };

    // Create inference engine
    let engine = qfc_inference::create_engine_for_backend(backend)?;

    // Run benchmark
    info!("Running hardware benchmark...");
    match engine.benchmark() {
        Ok(bench) => {
            info!("Benchmark: {:.2} MFLOPS ({} ms)", bench.flops / 1e6, bench.benchmark_time_ms);
        }
        Err(e) => {
            tracing::warn!("Benchmark failed: {}", e);
        }
    }

    // Start worker
    let mut worker = worker::InferenceWorker::new(config, engine);

    info!("Connecting to validator at {}...", validator_rpc);
    worker.run().await;

    Ok(())
}
