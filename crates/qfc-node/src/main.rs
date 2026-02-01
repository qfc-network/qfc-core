//! QFC Blockchain Node
//!
//! Main entry point for running a QFC node.

use anyhow::Result;
use clap::Parser;
use parking_lot::RwLock;
use qfc_chain::{Chain, ChainConfig, GenesisConfig};
use qfc_consensus::{ConsensusConfig, ConsensusEngine};
use qfc_crypto::VrfKeypair;
use qfc_mempool::{Mempool, MempoolConfig};
use qfc_rpc::{RpcConfig, RpcServer};
use qfc_storage::{Database, StorageConfig};
use qfc_types::{Address, DEFAULT_CHAIN_ID};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;

/// QFC Blockchain Node
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Data directory
    #[arg(short, long, default_value = "./data")]
    datadir: PathBuf,

    /// Chain ID
    #[arg(long, default_value_t = DEFAULT_CHAIN_ID)]
    chain_id: u64,

    /// RPC HTTP listen address
    #[arg(long, default_value = "127.0.0.1:8545")]
    rpc_addr: SocketAddr,

    /// Enable RPC
    #[arg(long, default_value_t = true)]
    rpc: bool,

    /// Run in development mode
    #[arg(long)]
    dev: bool,

    /// Validator mode (provide secret key hex)
    #[arg(long)]
    validator: Option<String>,

    /// Log level
    #[arg(long, default_value = "info")]
    log_level: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Initialize logging
    let log_level = match args.log_level.as_str() {
        "trace" => Level::TRACE,
        "debug" => Level::DEBUG,
        "info" => Level::INFO,
        "warn" => Level::WARN,
        "error" => Level::ERROR,
        _ => Level::INFO,
    };

    let subscriber = FmtSubscriber::builder()
        .with_max_level(log_level)
        .with_target(true)
        .with_thread_ids(false)
        .with_file(false)
        .with_line_number(false)
        .finish();

    tracing::subscriber::set_global_default(subscriber)?;

    info!("Starting QFC Node v{}", env!("CARGO_PKG_VERSION"));
    info!("Data directory: {:?}", args.datadir);
    info!("Chain ID: {}", args.chain_id);

    // Create data directory
    std::fs::create_dir_all(&args.datadir)?;

    // Open database
    let storage_config = StorageConfig {
        path: args.datadir.join("db"),
        create_if_missing: true,
        ..Default::default()
    };
    let db = Database::open(storage_config)?;
    info!("Database opened");

    // Create genesis config
    let genesis = if args.dev {
        info!("Running in development mode");
        GenesisConfig::dev()
    } else {
        GenesisConfig::testnet()
    };

    // Create consensus engine
    let consensus_config = ConsensusConfig::default();
    let consensus = if let Some(validator_key_hex) = &args.validator {
        let key_bytes: [u8; 32] = hex::decode(validator_key_hex)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("Invalid validator key length"))?;

        let vrf_key = VrfKeypair::from_secret_bytes(&key_bytes)?;
        let address = qfc_crypto::address_from_public_key(&vrf_key.public_key());

        info!("Running as validator: {}", address);
        Arc::new(ConsensusEngine::new_validator(
            consensus_config,
            vrf_key,
            address,
        ))
    } else {
        Arc::new(ConsensusEngine::new(consensus_config))
    };

    // Create chain
    let chain_config = ChainConfig {
        chain_id: args.chain_id,
        genesis,
    };
    let chain = Arc::new(Chain::new(db.clone(), chain_config, consensus.clone())?);
    info!(
        "Chain initialized at block {}",
        chain.block_number()
    );

    // Create mempool
    let mempool = Arc::new(RwLock::new(Mempool::new(MempoolConfig::default())));

    // Start RPC server
    if args.rpc {
        let rpc_config = RpcConfig {
            http_addr: args.rpc_addr,
            http_enabled: true,
        };

        let rpc_server = RpcServer::new(chain.clone(), mempool.clone(), args.chain_id);
        let _handle = rpc_server
            .start(rpc_config)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to start RPC server: {}", e))?;
        info!("RPC server started on {}", args.rpc_addr);
    }

    // Print startup info
    info!("===========================================");
    info!("QFC Node is running!");
    info!("Chain ID: {}", args.chain_id);
    info!("Genesis hash: {:?}", chain.genesis_hash());
    info!("Current block: {}", chain.block_number());
    if args.rpc {
        info!("RPC endpoint: http://{}", args.rpc_addr);
    }
    info!("===========================================");

    // Wait for shutdown signal
    tokio::signal::ctrl_c().await?;
    info!("Shutting down...");

    Ok(())
}
