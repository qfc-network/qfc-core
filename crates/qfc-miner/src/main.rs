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
    tracing_subscriber::fmt().with_env_filter(filter).init();

    info!("QFC AI Inference Miner v{}", env!("CARGO_PKG_VERSION"));

    // Detect hardware
    let hw = gpu::detect_and_log();

    // Parse wallet address
    let wallet_hex = cli.wallet.strip_prefix("0x").unwrap_or(&cli.wallet);
    let wallet_bytes =
        hex::decode(wallet_hex).map_err(|e| anyhow::anyhow!("Invalid wallet address: {}", e))?;
    let wallet_address = Address::from_slice(&wallet_bytes)
        .ok_or_else(|| anyhow::anyhow!("Wallet address must be 20 bytes"))?;

    // Parse and validate private key
    let pk_hex = cli
        .private_key
        .strip_prefix("0x")
        .unwrap_or(&cli.private_key);
    let pk_bytes =
        hex::decode(pk_hex).map_err(|e| anyhow::anyhow!("Invalid private key hex: {}", e))?;
    let mut secret_key = [0u8; 32];
    if pk_bytes.len() != 32 {
        anyhow::bail!("Private key must be 32 bytes");
    }
    secret_key.copy_from_slice(&pk_bytes);
    let keypair = qfc_crypto::Keypair::from_secret_bytes(&secret_key)?;
    let derived = qfc_crypto::address_from_keypair(&keypair);
    if derived != wallet_address {
        anyhow::bail!("Private key does not match wallet address");
    }
    info!("Private key validated for wallet {}", wallet_hex);

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
        secret_key,
    };

    // Create inference engine
    let engine = qfc_inference::create_engine_for_backend(backend)?;

    // Run benchmark
    info!("Running hardware benchmark...");
    let bench_score = match engine.benchmark() {
        Ok(bench) => {
            let (score, tier) = qfc_inference::compute_benchmark_score(&bench);
            info!(
                "Benchmark: {:.2} MFLOPS ({} ms), score: {}, tier: T{}",
                bench.flops / 1e6,
                bench.benchmark_time_ms,
                score,
                tier
            );
            Some((score, tier))
        }
        Err(e) => {
            tracing::warn!("Benchmark failed: {}", e);
            None
        }
    };

    // Register miner with validator
    if let Some((score, _tier)) = bench_score {
        let keypair = qfc_crypto::Keypair::from_secret_bytes(&secret_key).expect("validated above");
        let miner_addr = hex::encode(wallet_address.as_bytes());
        match submit::register_miner(
            &validator_rpc,
            &miner_addr,
            &hw.device_name,
            hw.memory_mb,
            score,
            backend,
            &keypair,
        )
        .await
        {
            Ok(result) => {
                if result.registered {
                    info!(
                        "Miner registered: T{} — {}",
                        result.assigned_tier, result.message
                    );
                } else {
                    tracing::warn!("Registration failed: {}", result.message);
                }
            }
            Err(e) => {
                tracing::warn!("Failed to register miner: {}", e);
            }
        }
    }

    // Create model scheduler
    let vram_budget =
        qfc_inference::scheduler::VramBudget::new(max_memory as u32, cli.vram_reserved_mb);
    let mut scheduler = qfc_inference::scheduler::ModelScheduler::new(vram_budget, 20);

    // Register hot models from CLI
    if !cli.hot_models.is_empty() {
        for model_spec in cli.hot_models.split(',') {
            let model_spec = model_spec.trim();
            if !model_spec.is_empty() {
                let model_id = qfc_inference::task::ModelId::new(model_spec, "v1");
                scheduler.add_hot_model(model_id.clone(), 0);
                info!("Hot model registered: {}", model_spec);
            }
        }
    }

    // Start worker
    let mut worker = worker::InferenceWorker::new(config, engine, scheduler);

    info!("Connecting to validator at {}...", validator_rpc);
    worker.run().await;

    Ok(())
}
