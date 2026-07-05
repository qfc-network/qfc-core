//! QFC Blockchain Node
//!
//! Main entry point for running a QFC node.

mod metrics;
mod miner;
mod producer;
mod snapshot_backup;
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
    #[arg(short, long, default_value = "./data", env = "QFC_DATA_DIR")]
    datadir: PathBuf,

    /// Chain ID
    #[arg(long, default_value_t = DEFAULT_CHAIN_ID, env = "QFC_CHAIN_ID")]
    chain_id: u64,

    /// RPC HTTP listen address
    #[arg(long, default_value = "127.0.0.1:8545", env = "QFC_RPC_ADDR")]
    rpc_addr: SocketAddr,

    /// Enable RPC
    #[arg(long, default_value_t = true)]
    rpc: bool,

    /// Run in development mode
    #[arg(long, env = "QFC_DEV_MODE")]
    dev: bool,

    /// Validator mode (provide secret key hex)
    #[arg(long, env = "QFC_VALIDATOR_KEY")]
    validator: Option<String>,

    /// Log level
    #[arg(long, default_value = "info", env = "QFC_LOG_LEVEL")]
    log_level: String,

    /// P2P listen port
    #[arg(long, default_value_t = 30303, env = "QFC_P2P_PORT")]
    p2p_port: u16,

    /// Disable P2P networking
    #[arg(long, env = "QFC_NO_NETWORK")]
    no_network: bool,

    /// Bootnode addresses (multiaddr format, comma-separated via env)
    #[arg(long, env = "QFC_BOOTNODES", value_delimiter = ',')]
    bootnodes: Vec<String>,

    /// Allow block production with ZERO connected peers (sync-before-produce
    /// gate, ADR-0012 §Phase B). Set on exactly one node when bootstrapping a
    /// network (compose: node-1 only). Unset, it defaults to true only for
    /// --dev with no bootnodes configured (single-node dev / integration
    /// tests); any multi-node deployment must set it explicitly.
    #[arg(
        long,
        env = "QFC_PRODUCE_WHEN_ALONE",
        value_parser = parse_lenient_bool,
        num_args = 0..=1,
        default_missing_value = "true"
    )]
    produce_when_alone: Option<bool>,

    /// Enable mining for compute contribution (20% weight in PoC)
    #[arg(long, env = "QFC_MINING_ENABLED")]
    mine: bool,

    /// Number of mining threads (default: number of CPUs)
    #[arg(long, env = "QFC_MINING_THREADS")]
    threads: Option<usize>,

    /// Compute mode: pow (v1 Blake3 PoW) or inference (v2 AI inference)
    #[arg(long, default_value = "pow", env = "QFC_COMPUTE_MODE")]
    compute_mode: String,

    /// Inference backend: auto, cuda, metal, opencl, cpu (for inference mode)
    #[arg(long, default_value = "auto", env = "QFC_INFERENCE_BACKEND")]
    inference_backend: String,

    /// Model cache directory (for inference mode)
    #[arg(long, env = "QFC_MODEL_DIR")]
    model_dir: Option<PathBuf>,

    /// Prometheus metrics listen address
    #[arg(long, default_value = "0.0.0.0:6060", env = "QFC_METRICS_ADDR")]
    metrics_addr: SocketAddr,

    /// Enable RocksDB statistics collection (tickers + histograms; costs a few
    /// percent of storage throughput). Exposes qfc_rocksdb_* counter metrics
    /// on /metrics; property-based gauges are exported regardless.
    #[arg(long, env = "QFC_DB_STATISTICS")]
    db_statistics: bool,

    /// T8: enable hot-key/hot-account access sampling at a 1-in-N rate
    /// (rounded up to a power of two; e.g. 64). Unset/0 = disabled (zero
    /// per-op cost). Results surface on /metrics (aggregate skew gauges) and
    /// via the in-memory report accessors. See docs/profiling/HOT-KEYS.md.
    #[arg(long, env = "QFC_HOT_KEY_SAMPLING")]
    hot_key_sampling: Option<u32>,

    /// T8: hot-key sampling window in seconds. Every window the node logs a
    /// ranked report (target `qfc::hot_keys`) and resets the sketches, so the
    /// space-saving estimates stay accurate and the /metrics gauges read
    /// per-window instead of cumulative-since-start. 0 = never reset
    /// (cumulative). No effect without --hot-key-sampling.
    #[arg(long, default_value_t = 0, env = "QFC_HOT_KEY_WINDOW_SECS")]
    hot_key_window_secs: u64,

    /// IPFS Kubo API URL for large inference result storage (optional)
    #[arg(long, env = "QFC_IPFS_API_URL")]
    ipfs_api_url: Option<String>,

    /// IPFS Gateway URL for fetching results (optional)
    #[arg(long, env = "QFC_IPFS_GATEWAY_URL")]
    ipfs_gateway_url: Option<String>,

    /// IPFS size threshold in bytes (default: 1MB, results larger than this go to IPFS)
    #[arg(long, env = "QFC_IPFS_SIZE_THRESHOLD", default_value_t = 1_048_576)]
    ipfs_size_threshold: usize,

    /// Run in light client mode (header-only sync, no state storage)
    #[arg(long, env = "QFC_LIGHT")]
    light: bool,

    /// T4.2: take a live RocksDB snapshot (file-level checkpoint, near-instant)
    /// every N seconds, pack it as <prefix>-<height>-<unix_ts>.tar.gz and
    /// optionally ship it via --snapshot-upload-cmd. Unset/0 = disabled.
    #[arg(long, env = "QFC_SNAPSHOT_INTERVAL_SECS")]
    snapshot_interval_secs: Option<u64>,

    /// Directory for snapshot archives (default: <datadir>/snapshots). Must
    /// be on the same filesystem as the database for hard-link snapshots.
    #[arg(long, env = "QFC_SNAPSHOT_DIR")]
    snapshot_dir: Option<PathBuf>,

    /// Shell command to ship each snapshot archive to object storage; {file}
    /// is replaced with the archive path (appended if no placeholder).
    /// Examples: "rclone copy {file} remote:qfc-backups/",
    /// "aws s3 cp {file} s3://qfc-backups/", "mc cp {file} minio/qfc-backups/".
    /// Failures are logged + counted (qfc_snapshot_failures_total), never fatal.
    #[arg(long, env = "QFC_SNAPSHOT_UPLOAD_CMD")]
    snapshot_upload_cmd: Option<String>,

    /// How many snapshot archives to keep locally (older ones are pruned;
    /// minimum 1).
    #[arg(long, default_value_t = 2, env = "QFC_SNAPSHOT_RETAIN")]
    snapshot_retain: usize,

    /// Snapshot archive name prefix.
    #[arg(long, default_value = "qfc-snapshot", env = "QFC_SNAPSHOT_PREFIX")]
    snapshot_prefix: String,

    /// T5: per-tenant AI task-pool quota config (JSON; see docs/AI-QUOTAS.md).
    /// Unset = quotas off (admission always succeeds). The file is
    /// hot-reloaded: changes are picked up within ~30s (mtime check); a
    /// parse error keeps the previous config and logs the failure.
    #[arg(long, env = "QFC_AI_QUOTAS")]
    ai_quotas: Option<PathBuf>,

    /// T5: interval in seconds between AI cost reports (per-tenant/per-miner
    /// FLOPs + fees, emitted as a structured log and handed to the treasury
    /// hook). 0 = disabled.
    #[arg(long, default_value_t = 600, env = "QFC_AI_COST_REPORT_INTERVAL_SECS")]
    ai_cost_report_interval_secs: u64,
}

/// Lenient boolean parser for env-style flags (QFC_PRODUCE_WHEN_ALONE=1).
fn parse_lenient_bool(s: &str) -> std::result::Result<bool, String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" | "" => Ok(false),
        other => Err(format!("invalid boolean value: {other:?}")),
    }
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

    let mode = if args.light { "light" } else { "full" };
    info!(
        "Starting QFC Node v{} ({})",
        env!("CARGO_PKG_VERSION"),
        mode
    );
    info!("Data directory: {:?}", args.datadir);
    info!("Chain ID: {}", args.chain_id);
    if args.light {
        info!("Light client mode: header-only sync, no block execution");
    }

    // Create data directory
    std::fs::create_dir_all(&args.datadir)?;

    // Open database
    let storage_config = StorageConfig {
        path: args.datadir.join("db"),
        create_if_missing: true,
        enable_statistics: args.db_statistics,
        hot_key_sampling: args.hot_key_sampling.filter(|&n| n > 0),
        ..Default::default()
    };
    let db = Database::open(storage_config)?;
    info!(
        "Database opened (statistics: {}, hot-key sampling: {})",
        if args.db_statistics { "on" } else { "off" },
        match db.hot_key_sampling() {
            Some(rate) => format!("1-in-{rate}"),
            None => "off".to_string(),
        }
    );

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
        // Dev mode: deterministic key for local development only
        // Address: 0x8d1dd4291ea7fe924cd4b2e577f6c81f3e4025c8
        // NOT used in production — testnet validators use --validator flag
        let dev_secret = [0x01u8; 32];
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

    // Reorg re-injection: transactions displaced by a reorg (carried by an
    // abandoned block but absent from the winning branch) would otherwise be
    // lost forever — the producer purges them on inclusion and importers
    // never held them. The chain forwards each displaced tx here; a task
    // re-adds it to the mempool via the same nonce-validated path the network
    // handler uses, so it can be re-included on the canonical chain (ADR-0013).
    let (reorg_tx, mut reorg_rx) = tokio::sync::mpsc::unbounded_channel::<qfc_types::Transaction>();
    chain.set_reorg_tx_sink(reorg_tx);
    {
        let chain = chain.clone();
        let mempool = mempool.clone();
        tokio::spawn(async move {
            while let Some(tx) = reorg_rx.recv().await {
                let tx_hash = qfc_crypto::blake3_hash(&tx.to_bytes_without_signature());
                // Sender derivation mirrors the network handler (sync.rs).
                let sender_hash = qfc_crypto::blake3_hash(tx.signature.as_bytes());
                let sender = match qfc_types::Address::from_slice(&sender_hash.as_bytes()[12..32]) {
                    Some(addr) => addr,
                    None => continue,
                };
                let state = chain.state();
                match mempool
                    .write()
                    .add_with_nonce_check(tx, sender, Some(state.as_ref()))
                {
                    Ok(_) => info!(
                        "Re-injected reorg-displaced tx {} into mempool",
                        hex::encode(&tx_hash.as_bytes()[..8])
                    ),
                    Err(e) => tracing::debug!(
                        "Skipped reorg-displaced tx {} (stale/already-included): {}",
                        hex::encode(&tx_hash.as_bytes()[..8]),
                        e
                    ),
                }
            }
        });
    }

    // v2.0: Shared proof pool and task pool for RPC, sync, and block producer
    let proof_pool = Arc::new(RwLock::new(ProofPool::new()));
    let task_pool = Arc::new(RwLock::new(TaskPool::new()));

    // T5: per-tenant quotas (--ai-quotas) + periodic cost reports.
    if let Some(ref quota_path) = args.ai_quotas {
        let cfg = qfc_ai_coordinator::QuotaConfig::from_file(quota_path)
            .map_err(|e| anyhow::anyhow!("Invalid --ai-quotas file {:?}: {}", quota_path, e))?;
        info!(
            "AI quotas enabled from {:?} ({} tenant overrides, default tier: {} qps / {} in-flight, window {}s, max pending {})",
            quota_path,
            cfg.tenants.len(),
            cfg.default_tier.max_qps,
            cfg.default_tier.max_inflight,
            cfg.window_secs,
            cfg.max_pending
        );
        task_pool.write().set_quota_config(Some(cfg));
    }
    spawn_ai_quota_task(
        task_pool.clone(),
        args.ai_quotas.clone(),
        args.ai_cost_report_interval_secs,
    );

    // T8: periodic hot-key/hot-account window reset (no-op unless sampling is
    // on and a window is configured).
    spawn_hot_key_window_task(db.clone(), chain.clone(), args.hot_key_window_secs);

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
        // Clone to avoid holding RwLock guard across .await
        let mut gen = challenge_generator.write().clone();
        gen.generate_challenges(&cpu_engine, epoch.number, epoch_seed)
            .await;
        *challenge_generator.write() = gen;
    }

    // Start P2P network first (so we can pass it to RPC server)
    let network_result: Option<(Arc<NetworkService>, Arc<SyncManager>)> = if !args.no_network {
        let mut network_config = if args.dev {
            NetworkConfig::dev()
        } else {
            NetworkConfig::default()
        };

        // Use validator key as P2P identity for deterministic peer ID
        if let Some(validator_key_hex) = &args.validator {
            if let Ok(key_bytes) = hex::decode(validator_key_hex) {
                if key_bytes.len() == 32 {
                    let mut arr = [0u8; 32];
                    arr.copy_from_slice(&key_bytes);
                    network_config.secret_key = Some(arr);
                }
            }
        } else if args.dev {
            network_config.secret_key = Some([0x01u8; 32]);
        }

        // Set listen address with specified port
        network_config.listen_addresses = vec![format!("/ip4/0.0.0.0/tcp/{}", args.p2p_port)
            .parse()
            .unwrap()];

        // Add bootnodes. A node whose every configured bootnode fails to
        // parse can never join the network — with the produce gate that
        // means it would sit gated (or, worse, produce alone) forever, so
        // this is a hard startup error, not a warning (ADR-0012 §Phase B).
        let configured_bootnodes: Vec<&String> = args
            .bootnodes
            .iter()
            .filter(|s| !s.trim().is_empty())
            .collect();
        for bootnode in &configured_bootnodes {
            match bootnode.parse() {
                Ok(addr) => network_config.bootnodes.push(addr),
                Err(e) => warn!("Invalid bootnode address '{}': {}", bootnode, e),
            }
        }
        if !configured_bootnodes.is_empty() && network_config.bootnodes.is_empty() {
            anyhow::bail!(
                "all {} configured bootnode address(es) failed to parse — refusing to start \
                 an isolated node (fix QFC_BOOTNODES / --bootnodes)",
                configured_bootnodes.len()
            );
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

                // Forward catch-up loop: range-sync missing blocks in order
                // when we fall behind peers (initial sync + recovery).
                let sync_manager_for_catchup = sync_manager.clone();
                tokio::spawn(async move {
                    sync_manager_for_catchup.run_catch_up_loop().await;
                });

                // Active peer-status poll loop (ADR-0012 §Phase B): polls
                // every connected peer with GetStatus (~2s until fresh),
                // feeding the sync-before-produce gate and catch-up peer
                // selection with verified heads.
                let sync_manager_for_status = sync_manager.clone();
                tokio::spawn(async move {
                    sync_manager_for_status.run_status_poll_loop().await;
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

    // T2.2: RPC metrics registry, shared between the RPC middleware (writer)
    // and the Prometheus exporter (reader)
    let rpc_metrics = Arc::new(qfc_rpc::metrics::RpcMetrics::new());

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
            .with_metrics(rpc_metrics.clone())
            .with_inference_engine(rpc_engine)
            .with_proof_pool(proof_pool.clone())
            .with_task_pool(task_pool.clone())
            .with_challenge_generator(challenge_generator.clone())
            .with_redundant_verifier(redundant_verifier.clone())
            .with_task_router(task_router.clone());

        // B2: IPFS client for large inference result storage
        if let Some(ref api_url) = args.ipfs_api_url {
            let ipfs_config = qfc_ai_coordinator::ipfs::IpfsConfig {
                api_url: api_url.clone(),
                gateway_url: args
                    .ipfs_gateway_url
                    .clone()
                    .unwrap_or_else(|| "http://127.0.0.1:8080".into()),
                size_threshold: args.ipfs_size_threshold,
                ..Default::default()
            };
            rpc_server =
                rpc_server.with_ipfs_client(qfc_ai_coordinator::ipfs::IpfsClient::new(ipfs_config));
            info!(
                "IPFS client configured: api={}, threshold={}",
                api_url, args.ipfs_size_threshold
            );
        }

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

    // Start block producer if we're a validator or in dev mode.
    // NOTE: the slot length is the chain constant BLOCK_INTERVAL_MS — there
    // is deliberately no per-mode override (--dev used to shorten the slot to
    // 3000ms, which silently forked dev nodes off the shared schedule; §6 of
    // ADR-0012).
    let is_validator = consensus.is_validator();
    if is_validator {
        // Zero-peer production rule (ADR-0012 §Phase B): explicit
        // --produce-when-alone / QFC_PRODUCE_WHEN_ALONE always wins; unset,
        // it defaults to true only for --dev with no bootnodes configured
        // (single-node dev + the release-binary integration tests, which run
        // --dev --no-network). Testnet/compose nodes have bootnodes, so they
        // default to false and must opt in exactly one bootstrap node.
        let has_bootnodes = args.bootnodes.iter().any(|s| !s.trim().is_empty());
        let produce_when_alone = args
            .produce_when_alone
            .unwrap_or(args.dev && !has_bootnodes);
        info!(
            "Sync-before-produce gate active (produce_when_alone: {})",
            produce_when_alone
        );

        let producer_config = ProducerConfig {
            produce_empty_blocks: true, // Always produce empty blocks for testing
            produce_when_alone,
            ..Default::default()
        };

        let network_for_producer = _network_service.clone();

        let mut producer = BlockProducer::new(
            chain.clone(),
            consensus.clone(),
            mempool.clone(),
            network_for_producer,
            producer_config,
            proof_pool.clone(),
            task_pool.clone(),
        );
        if let Some(ref sync) = _sync_manager {
            producer = producer.with_sync_manager(sync.clone());
        }

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

    // T4.2: periodic snapshot backups (RocksDB checkpoint → tar.gz → object
    // storage). Off unless --snapshot-interval-secs is set.
    let snapshot_metrics = match args.snapshot_interval_secs {
        Some(secs) if secs > 0 => {
            let backup_config = snapshot_backup::SnapshotBackupConfig {
                interval: std::time::Duration::from_secs(secs),
                dir: args
                    .snapshot_dir
                    .clone()
                    .unwrap_or_else(|| args.datadir.join("snapshots")),
                prefix: args.snapshot_prefix.clone(),
                upload_cmd: args.snapshot_upload_cmd.clone(),
                retain: args.snapshot_retain.max(1),
            };
            let backup_metrics = Arc::new(snapshot_backup::SnapshotBackupMetrics::default());
            snapshot_backup::spawn(db.clone(), backup_config, backup_metrics.clone());
            Some(backup_metrics)
        }
        _ => None,
    };

    // Start Prometheus metrics server
    let mut metrics_server = metrics::MetricsServer::new(
        args.metrics_addr,
        chain.clone(),
        consensus.clone(),
        mempool.clone(),
        _network_service.clone(),
        proof_pool.clone(),
        args.chain_id,
    )
    .with_rpc_metrics(rpc_metrics.clone())
    .with_storage(db.clone())
    .with_task_pool(task_pool.clone());
    if let Some(ref sync) = _sync_manager {
        metrics_server =
            metrics_server.with_sync_status(sync.clone() as Arc<dyn qfc_rpc::SyncStatusProvider>);
    }
    if let Some(snap) = snapshot_metrics {
        metrics_server = metrics_server.with_snapshot_backup(snap);
    }
    metrics_server.start();

    // Print startup info
    info!("===========================================");
    info!("QFC Node is running!");
    info!("Chain ID: {}", args.chain_id);
    info!("Genesis hash: {:?}", chain.genesis_hash());
    info!("Current block: {}", chain.block_number());
    if args.rpc {
        info!("RPC endpoint: http://{}", args.rpc_addr);
    }
    info!("Metrics endpoint: http://{}/metrics", args.metrics_addr);
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

/// T5: background task driving (a) periodic AI cost reports
/// (`--ai-cost-report-interval-secs`, 0 = disabled) and (b) hot reload of
/// the `--ai-quotas` config file (mtime check every tick; a file that fails
/// to parse keeps the previous config). No-op when both are disabled.
fn spawn_ai_quota_task(
    task_pool: Arc<RwLock<TaskPool>>,
    quota_path: Option<PathBuf>,
    report_interval_secs: u64,
) {
    if quota_path.is_none() && report_interval_secs == 0 {
        return;
    }

    const TICK_SECS: u64 = 30;
    tokio::spawn(async move {
        let mut last_mtime: Option<std::time::SystemTime> = quota_path
            .as_ref()
            .and_then(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
        let mut secs_since_report = 0u64;

        loop {
            tokio::time::sleep(std::time::Duration::from_secs(TICK_SECS)).await;

            // Hot reload on mtime change.
            if let Some(ref path) = quota_path {
                let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
                if mtime.is_some() && mtime != last_mtime {
                    last_mtime = mtime;
                    match qfc_ai_coordinator::QuotaConfig::from_file(path) {
                        Ok(cfg) => {
                            info!(
                                "AI quota config reloaded from {:?} ({} tenant overrides)",
                                path,
                                cfg.tenants.len()
                            );
                            task_pool.write().set_quota_config(Some(cfg));
                        }
                        Err(e) => {
                            warn!(
                                "AI quota config reload failed, keeping previous config: {}",
                                e
                            );
                        }
                    }
                }
            }

            // Periodic cost report.
            if report_interval_secs > 0 {
                secs_since_report += TICK_SECS;
                if secs_since_report >= report_interval_secs {
                    secs_since_report = 0;
                    let now_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0);
                    // generate_cost_report logs via the treasury hook
                    // (target qfc::ai_cost) and retains the report for
                    // the exporter/RPC.
                    let _ = task_pool.write().generate_cost_report(now_ms);
                }
            }
        }
    });
}

/// T8: drive the hot-key/hot-account sampling window. Every
/// `--hot-key-window-secs`, [`hot_key_window_tick`] logs a ranked report
/// (target `qfc::hot_keys` — the permanent record of the hot *identities*
/// that are deliberately kept out of the Prometheus labels) and resets both
/// sketches, keeping the space-saving estimates accurate and making the
/// `/metrics` gauges per-window instead of cumulative. No-op unless sampling
/// is enabled and a window is configured.
fn spawn_hot_key_window_task(db: Database, chain: Arc<Chain>, window_secs: u64) {
    if window_secs == 0 || db.hot_key_sampling().is_none() {
        return;
    }
    info!(
        "Hot-key sampling window: {}s (reports logged to target qfc::hot_keys, then sketches reset)",
        window_secs
    );
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(window_secs));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick fires immediately; skip it so the first reset lands
        // after a full window rather than at startup.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            hot_key_window_tick(&db, &chain);
        }
    });
}

/// Log the current hot-key/hot-account reports, then reset both windows.
/// Split out from the spawn loop so it is directly unit-testable.
fn hot_key_window_tick(db: &Database, chain: &Chain) {
    const TOP_N: usize = 16;

    if let Some(report) = db.hot_key_report(TOP_N) {
        let busiest = report
            .cfs
            .iter()
            .max_by_key(|c| c.estimated_reads + c.estimated_writes);
        let hottest = report
            .cfs
            .iter()
            .filter_map(|c| c.top_keys.first().map(|k| (c.cf.as_str(), k)))
            .max_by_key(|(_, k)| k.estimated_count);
        info!(
            target: "qfc::hot_keys",
            rate = report.sampling_rate,
            cfs_active = report.cfs.len(),
            busiest_cf = busiest.map(|c| c.cf.as_str()).unwrap_or("-"),
            busiest_cf_reads = busiest.map(|c| c.estimated_reads).unwrap_or(0),
            busiest_cf_writes = busiest.map(|c| c.estimated_writes).unwrap_or(0),
            hottest_key_cf = hottest.map(|(cf, _)| cf).unwrap_or("-"),
            hottest_key = hottest.map(|(_, k)| k.key_hex.as_str()).unwrap_or("-"),
            hottest_key_count = hottest.map(|(_, k)| k.estimated_count).unwrap_or(0),
            "hot-key window closed (storage)"
        );
        db.reset_hot_key_stats();
    }

    let state = chain.state();
    if let Some(acct) = state.hot_account_report(TOP_N) {
        let top_acct = acct.top_accounts.first();
        let top_code = acct.top_code.first();
        info!(
            target: "qfc::hot_keys",
            est_account_reads = acct.estimated_account_reads,
            est_account_writes = acct.estimated_account_writes,
            est_code_reads = acct.estimated_code_reads,
            hottest_account = top_acct.map(|a| a.address.as_str()).unwrap_or("-"),
            hottest_account_count = top_acct.map(|a| a.estimated_count).unwrap_or(0),
            hottest_code = top_code.map(|c| c.code_hash.as_str()).unwrap_or("-"),
            hottest_code_count = top_code.map(|c| c.estimated_count).unwrap_or(0),
            "hot-key window closed (accounts)"
        );
        state.reset_hot_account_stats();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qfc_chain::{ChainConfig, GenesisConfig};
    use qfc_consensus::{ConsensusConfig, ConsensusEngine};

    fn total_traffic(db: &Database) -> u64 {
        db.hot_key_report(16)
            .map(|r| {
                r.cfs
                    .iter()
                    .map(|c| c.estimated_reads + c.estimated_writes)
                    .sum()
            })
            .unwrap_or(0)
    }

    /// A window tick logs and then resets both sketches: traffic accumulated
    /// before the tick is gone after it, and the window keeps sampling.
    #[test]
    fn hot_key_window_tick_resets_sketches() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open(StorageConfig {
            path: tmp.path().join("db"),
            create_if_missing: true,
            hot_key_sampling: Some(1),
            ..Default::default()
        })
        .unwrap();
        let consensus = Arc::new(ConsensusEngine::new(ConsensusConfig::default()));
        let chain = Chain::new(
            db.clone(),
            ChainConfig {
                chain_id: 9000,
                genesis: GenesisConfig::dev(),
            },
            consensus,
        )
        .unwrap();

        // Genesis init wrote to several CFs, so the window has traffic.
        assert!(total_traffic(&db) > 0, "expected genesis sampling traffic");
        let window_before = db.hot_key_report(16).unwrap().window_start_unix_ms;

        hot_key_window_tick(&db, &chain);

        // Reset: counts cleared, sampling still on, new window opened.
        assert_eq!(total_traffic(&db), 0, "tick must reset the sketch");
        assert!(db.hot_key_sampling().is_some(), "sampling stays enabled");
        let window_after = db.hot_key_report(16).unwrap().window_start_unix_ms;
        assert!(window_after >= window_before, "reset opens a fresh window");
    }

    /// With sampling off, a tick is a harmless no-op (reports are `None`).
    #[test]
    fn hot_key_window_tick_noop_when_sampling_off() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open(StorageConfig {
            path: tmp.path().join("db"),
            create_if_missing: true,
            ..Default::default()
        })
        .unwrap();
        let consensus = Arc::new(ConsensusEngine::new(ConsensusConfig::default()));
        let chain = Chain::new(
            db.clone(),
            ChainConfig {
                chain_id: 9000,
                genesis: GenesisConfig::dev(),
            },
            consensus,
        )
        .unwrap();

        assert!(db.hot_key_sampling().is_none());
        hot_key_window_tick(&db, &chain); // must not panic
        assert!(db.hot_key_report(16).is_none());
    }
}
