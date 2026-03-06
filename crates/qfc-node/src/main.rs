//! QFC Blockchain Node
//!
//! Main entry point for running a QFC node.

mod miner;
mod producer;
mod sync;

use anyhow::Result;
use clap::Parser;
use miner::{MiningConfig, MiningService};
use parking_lot::RwLock;
use producer::{BlockProducer, ProducerConfig};
use qfc_ai_coordinator::{ProofPool, TaskPool};
use qfc_chain::{Chain, ChainConfig, GenesisConfig};
use qfc_consensus::{ConsensusConfig, ConsensusEngine};
use qfc_crypto::VrfKeypair;
use qfc_mempool::{Mempool, MempoolConfig};
use qfc_network::{NetworkConfig, NetworkService};
use qfc_rpc::{RpcConfig, RpcServer};
use qfc_storage::{Database, StorageConfig};
use qfc_types::DEFAULT_CHAIN_ID;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use sync::SyncManager;
use tracing::{info, warn, Level};
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

    /// P2P listen port
    #[arg(long, default_value_t = 30303)]
    p2p_port: u16,

    /// Disable P2P networking
    #[arg(long)]
    no_network: bool,

    /// Bootnode addresses (multiaddr format)
    #[arg(long)]
    bootnodes: Vec<String>,

    /// Enable mining for compute contribution (20% weight in PoC)
    #[arg(long)]
    mine: bool,

    /// Number of mining threads (default: number of CPUs)
    #[arg(long)]
    threads: Option<usize>,

    /// Compute mode: pow (v1 Blake3 PoW) or inference (v2 AI inference)
    #[arg(long, default_value = "pow")]
    compute_mode: String,

    /// Inference backend: auto, cuda, metal, cpu (for inference mode)
    #[arg(long, default_value = "auto")]
    inference_backend: String,

    /// Model cache directory (for inference mode)
    #[arg(long)]
    model_dir: Option<PathBuf>,
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
        // Explicit validator key provided
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
    } else if args.dev {
        // Dev mode: generate a deterministic validator key
        let dev_secret = [0x42u8; 32]; // Deterministic dev key
        let vrf_key = VrfKeypair::from_secret_bytes(&dev_secret)?;
        let address = qfc_crypto::address_from_public_key(&vrf_key.public_key());

        info!("Dev mode validator: {}", address);
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
    info!("Chain initialized at block {}", chain.block_number());

    // Create mempool
    let mempool = Arc::new(RwLock::new(Mempool::new(MempoolConfig::default())));

    // v2.0: Shared proof pool and task pool for RPC, sync, and block producer
    let proof_pool = Arc::new(RwLock::new(ProofPool::new()));
    let task_pool = Arc::new(RwLock::new(TaskPool::new()));

    // v2.0 P2: Shared challenge generator, redundant verifier, task router
    let challenge_generator = Arc::new(RwLock::new(
        qfc_ai_coordinator::challenge::ChallengeGenerator::new(),
    ));
    let redundant_verifier = Arc::new(RwLock::new(
        qfc_ai_coordinator::redundant::RedundantVerifier::default(),
    ));
    let task_router = Arc::new(RwLock::new(qfc_ai_coordinator::router::TaskRouter::new()));

    // Initialize challenge pool with CpuEngine
    {
        let cpu_engine = qfc_inference::backend::cpu::CpuEngine::new();
        let epoch = consensus.get_epoch();
        let epoch_seed = u64::from_le_bytes(epoch.seed[..8].try_into().unwrap_or([0u8; 8]));
        // Take ownership to avoid holding RwLock guard across .await
        let mut gen = challenge_generator.write().clone();
        gen.generate_challenges(&cpu_engine, epoch.number, epoch_seed).await;
        *challenge_generator.write() = gen;
    }

    // Start P2P network first (so we can pass it to RPC server)
    let network_result: Option<(Arc<NetworkService>, Arc<SyncManager>)> = if !args.no_network {
        let mut network_config = if args.dev {
            NetworkConfig::dev()
        } else {
            NetworkConfig::default()
        };

        // Set listen address with specified port
        network_config.listen_addresses = vec![format!("/ip4/0.0.0.0/tcp/{}", args.p2p_port)
            .parse()
            .unwrap()];

        // Add bootnodes
        for bootnode in &args.bootnodes {
            match bootnode.parse() {
                Ok(addr) => network_config.bootnodes.push(addr),
                Err(e) => warn!("Invalid bootnode address '{}': {}", bootnode, e),
            }
        }

        match NetworkService::start(network_config).await {
            Ok((service, mut message_rx, mut sync_event_rx)) => {
                info!("P2P network started, peer ID: {}", service.local_peer_id());
                let service = Arc::new(service);

                // Start sync manager (with inference engine for spot-check verification)
                let sync_manager = {
                    let sm = SyncManager::new(chain.clone(), mempool.clone(), service.clone());
                    // Attach a CPU inference engine for spot-check re-execution
                    let engine = qfc_inference::backend::cpu::CpuEngine::new();
                    sm.with_inference_engine(Box::new(engine))
                        .with_proof_pool(proof_pool.clone())
                        .with_challenge_generator(challenge_generator.clone())
                        .with_redundant_verifier(redundant_verifier.clone())
                };
                let sync_manager = Arc::new(sync_manager);
                let sync_manager_for_messages = sync_manager.clone();
                let sync_manager_for_events = sync_manager.clone();

                // Handle incoming gossip messages
                tokio::spawn(async move {
                    while let Some(msg) = message_rx.recv().await {
                        sync_manager_for_messages.handle_message(msg).await;
                    }
                });

                // Handle sync requests
                tokio::spawn(async move {
                    while let Some(event) = sync_event_rx.recv().await {
                        sync_manager_for_events.handle_sync_event(event).await;
                    }
                });

                Some((service, sync_manager))
            }
            Err(e) => {
                warn!("Failed to start P2P network: {}", e);
                None
            }
        }
    } else {
        info!("P2P networking disabled");
        None
    };

    // Extract network service and sync manager
    let (network_service, sync_manager) = match network_result {
        Some((net, sync)) => (Some(net), Some(sync)),
        None => (None, None),
    };

    // Start RPC server (with network for transaction broadcasting)
    let _rpc_handle = if args.rpc {
        let rpc_config = RpcConfig {
            http_addr: args.rpc_addr,
            http_enabled: true,
        };

        let mut rpc_server = RpcServer::new(chain.clone(), mempool.clone(), args.chain_id);
        if let Some(ref network) = network_service {
            rpc_server = rpc_server.with_network(network.clone());
        }
        if let Some(ref sync) = sync_manager {
            rpc_server = rpc_server.with_sync_status(sync.clone());
        }
        // Attach CPU inference engine for spot-check verification
        let rpc_engine = qfc_inference::create_engine_for_backend(qfc_inference::BackendType::Cpu)?;
        rpc_server = rpc_server
            .with_inference_engine(rpc_engine)
            .with_proof_pool(proof_pool.clone())
            .with_task_pool(task_pool.clone())
            .with_challenge_generator(challenge_generator.clone())
            .with_redundant_verifier(redundant_verifier.clone())
            .with_task_router(task_router.clone());

        let handle = rpc_server
            .start(rpc_config)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to start RPC server: {}", e))?;
        info!("RPC server started on {}", args.rpc_addr);
        Some(handle)
    } else {
        None
    };

    // Keep network service alive
    let _network_service = network_service;
    let _sync_manager = sync_manager;

    // Start block producer if we're a validator or in dev mode
    let is_validator = consensus.is_validator();
    if is_validator {
        let producer_config = ProducerConfig {
            block_interval_ms: if args.dev { 3000 } else { 5000 },
            produce_empty_blocks: true, // Always produce empty blocks for testing
            ..Default::default()
        };

        let network_for_producer = _network_service.clone();

        let producer = BlockProducer::new(
            chain.clone(),
            consensus.clone(),
            mempool.clone(),
            network_for_producer,
            producer_config,
            args.chain_id,
            proof_pool.clone(),
            task_pool.clone(),
        );

        tokio::spawn(async move {
            producer.start().await;
        });
    }

    // Start mining service if enabled
    let mining_active = if args.mine && is_validator {
        let validator_address = consensus.our_address().unwrap();
        let threads = args.threads.unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
        });

        let compute_mode = match args.compute_mode.as_str() {
            "inference" => miner::ComputeMode::InferenceV2,
            _ => miner::ComputeMode::PowV1,
        };

        let mut mining_config = MiningConfig::default().with_threads(threads);
        mining_config.compute_mode = compute_mode.clone();
        mining_config.inference_backend = Some(args.inference_backend.clone());
        mining_config.model_dir = args
            .model_dir
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned());

        info!("Compute mode: {:?}", compute_mode);

        let mining_service = Arc::new(MiningService::new(
            chain.clone(),
            consensus.clone(),
            _network_service.clone(),
            mining_config,
            validator_address,
        ));

        let mining_service_clone = Arc::clone(&mining_service);
        tokio::spawn(async move {
            mining_service_clone.start().await;
        });

        info!("Mining enabled with {} threads", threads);
        true
    } else if args.mine && !is_validator {
        warn!("Mining requires validator mode (--validator or --dev)");
        false
    } else {
        false
    };

    // Print startup info
    info!("===========================================");
    info!("QFC Node is running!");
    info!("Chain ID: {}", args.chain_id);
    info!("Genesis hash: {:?}", chain.genesis_hash());
    info!("Current block: {}", chain.block_number());
    if args.rpc {
        info!("RPC endpoint: http://{}", args.rpc_addr);
    }
    if is_validator {
        info!("Block producer: ACTIVE");
    }
    if mining_active {
        info!(
            "Mining: ACTIVE (20% compute contribution, mode: {})",
            args.compute_mode
        );
    }
    info!("===========================================");

    // Wait for shutdown signal
    tokio::signal::ctrl_c().await?;
    info!("Shutting down...");

    Ok(())
}
