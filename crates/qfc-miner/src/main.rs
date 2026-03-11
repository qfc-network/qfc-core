//! QFC AI Inference Miner — standalone binary
//!
//! Connects to a validator node and contributes AI inference compute
//! to earn block rewards.
//!
//! Usage:
//!   qfc-miner --wallet 0x... --backend auto --model-dir ./models

mod config;
pub mod dashboard;
mod gpu;
pub mod plugin;
mod storage;
mod submit;
mod worker;

use clap::Parser;
use config::{MinerCli, MinerConfig};
use qfc_inference::{BackendType, InferenceEngine};
use qfc_types::Address;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = MinerCli::parse();

    // Initialize shared stats (needed early for TUI log layer)
    let stats = dashboard::tui::new_shared_stats();

    // Setup logging — when TUI is active, capture logs to both file and TUI panel
    let filter = if cli.verbose { "debug" } else { "info" };
    #[cfg(feature = "tui")]
    if cli.dashboard {
        dashboard::tui::init_tui_tracing(stats.clone(), cli.verbose);
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }
    #[cfg(not(feature = "tui"))]
    tracing_subscriber::fmt().with_env_filter(filter).init();

    info!("QFC AI Inference Miner v{}", env!("CARGO_PKG_VERSION"));

    // Generate wallet mode
    if cli.generate_wallet {
        let keypair = qfc_crypto::Keypair::generate();
        let address = qfc_crypto::address_from_keypair(&keypair);
        let secret = keypair.secret_bytes();
        println!("=== New QFC Miner Wallet (Ed25519) ===");
        println!("Address:     0x{}", hex::encode(address.as_bytes()));
        println!("Private Key: 0x{}", hex::encode(secret));
        println!();
        println!("Save these! Then fund the address with test QFC:");
        println!("  curl https://faucet.testnet.qfc.network/api/faucet -X POST \\");
        println!("    -H 'Content-Type: application/json' \\",);
        println!(
            "    -d '{{\"address\":\"0x{}\"}}'",
            hex::encode(address.as_bytes())
        );
        println!();
        println!("Start mining with:");
        println!(
            "  qfc-miner --wallet 0x{} --private-key 0x{} --backend metal",
            hex::encode(address.as_bytes()),
            hex::encode(secret)
        );
        return Ok(());
    }

    // Detect hardware
    let hw = gpu::detect_and_log();

    // Validate required args
    if cli.wallet.is_empty() || cli.private_key.is_empty() {
        anyhow::bail!("--wallet and --private-key are required. Use --generate-wallet to create a new wallet.");
    }

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

    // Create inference engine (with auto-fallback to CPU on failure)
    let (engine, actual_backend) = create_engine_with_fallback(backend)?;
    let backend = actual_backend;
    info!("Using backend: {}, max memory: {} MB", backend, max_memory);

    // Initialize inference plugins if enabled
    if cli.plugins {
        let plugin_config = cli.plugin_config();
        let plugin_registry = plugin::PluginRegistry::new(plugin_config);
        let detected = plugin_registry.detected_plugins();
        if detected.is_empty() {
            tracing::warn!("No inference plugins detected. External runtimes unavailable.");
        } else {
            info!(
                "Inference plugins enabled: {}",
                detected
                    .iter()
                    .map(|p| format!("{}", p.runtime))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }

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

    // Update shared stats with initial info
    {
        let tier_str = bench_score
            .map(|(_, t)| format!("{}", t))
            .unwrap_or_else(|| "?".to_string());
        let mut s = stats.lock().unwrap();
        s.address = format!("0x{}", wallet_hex);
        s.backend = format!("{}", backend);
        s.tier = tier_str;
        s.device_name = hw.device_name.clone();
        s.device_memory_mb = hw.memory_mb;
        s.compute_cores = hw.compute_cores;
        s.rpc_url = validator_rpc.clone();
        s.session_start = Some(std::time::Instant::now());
        s.status = "Starting...".to_string();
    }

    // Start worker
    // Setup persistent earnings storage
    let store_dir = std::env::var("HOME")
        .map(|h| PathBuf::from(h).join(".qfc-miner"))
        .unwrap_or_else(|_| PathBuf::from(".qfc-miner"));
    if let Err(e) = std::fs::create_dir_all(&store_dir) {
        tracing::warn!("Failed to create earnings dir {:?}: {}", store_dir, e);
    }
    let store_path = store_dir.join("earnings.json");
    let earnings_store = storage::EarningsStore::load(&store_path);

    // Log lifetime stats
    if earnings_store.lifetime_tasks_completed > 0 {
        let lifetime_qfc = earnings_store.lifetime_earnings_wei as f64 / 1e18;
        info!(
            "Lifetime: {} tasks completed, {:.6} QFC earned across {} sessions",
            earnings_store.lifetime_tasks_completed,
            lifetime_qfc,
            earnings_store.sessions.len()
        );
    }

    // Update dashboard with lifetime stats
    {
        let mut s = stats.lock().unwrap();
        s.lifetime_earnings_wei = earnings_store.lifetime_earnings_wei;
        s.lifetime_tasks = earnings_store.lifetime_tasks_completed;
        s.earnings_24h_wei = earnings_store.earnings_last_hours(24);
    }

    let earnings_store = Arc::new(Mutex::new(earnings_store));

    let mut worker = worker::InferenceWorker::new(
        config,
        engine,
        scheduler,
        stats.clone(),
        earnings_store,
        store_path,
    );

    info!("Connecting to validator at {}...", validator_rpc);

    // Launch TUI dashboard if enabled
    #[cfg(feature = "tui")]
    if cli.dashboard {
        let dashboard_stats = stats.clone();
        let dashboard_handle =
            std::thread::spawn(move || dashboard::tui::run_dashboard(dashboard_stats));

        worker.run().await;

        // Wait for dashboard thread
        let _ = dashboard_handle.join();
    } else {
        worker.run().await;
    }

    #[cfg(not(feature = "tui"))]
    {
        if cli.dashboard {
            tracing::warn!("TUI dashboard requires --features tui. Running without dashboard.");
        }
        worker.run().await;
    }

    Ok(())
}

/// Create an inference engine, falling back to CPU if the requested backend fails.
fn create_engine_with_fallback(
    backend: BackendType,
) -> anyhow::Result<(Box<dyn InferenceEngine>, BackendType)> {
    match qfc_inference::create_engine_for_backend(backend) {
        Ok(engine) => Ok((engine, backend)),
        Err(e) if backend != BackendType::Cpu => {
            tracing::info!(
                "{} backend not available, using CPU instead (compile with --features {} for GPU acceleration)",
                backend,
                format!("{}", backend).to_lowercase()
            );
            let engine = qfc_inference::create_engine_for_backend(BackendType::Cpu)?;
            Ok((engine, BackendType::Cpu))
        }
        Err(e) => Err(e.into()),
    }
}
