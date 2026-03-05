//! Mining service for compute contribution
//!
//! This module provides mining support for the optional 20% compute contribution
//! in QFC's Proof of Contribution consensus.

use parking_lot::RwLock;
use qfc_chain::Chain;
use qfc_consensus::ConsensusEngine;
use qfc_crypto::blake3_hash;
use qfc_network::NetworkService;
use qfc_pow::{adjust_difficulty, initial_difficulty, Miner, MiningResult};
use qfc_types::{Address, DifficultyConfig, Hash, MiningTask, ValidatorMessage, U256};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::interval;
use tracing::{debug, error, info, warn};

/// Compute mode selection (v1 PoW or v2 inference)
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ComputeMode {
    /// v1: Blake3 Proof of Work (legacy)
    PowV1,
    /// v2: AI Inference tasks
    InferenceV2,
}

impl Default for ComputeMode {
    fn default() -> Self {
        // Default to PoW during transition period
        Self::PowV1
    }
}

/// Mining service configuration
#[derive(Clone, Debug)]
pub struct MiningConfig {
    /// Number of mining threads
    pub threads: usize,
    /// Epoch duration in milliseconds
    pub epoch_duration_ms: u64,
    /// Difficulty adjustment config
    pub difficulty_config: DifficultyConfig,
    /// Compute mode: pow (v1) or inference (v2)
    pub compute_mode: ComputeMode,
    /// Inference backend preference (for v2 mode)
    pub inference_backend: Option<String>,
    /// Model cache directory (for v2 mode)
    pub model_dir: Option<String>,
}

impl Default for MiningConfig {
    fn default() -> Self {
        Self {
            threads: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1),
            epoch_duration_ms: 10_000, // 10 seconds
            difficulty_config: DifficultyConfig::default(),
            compute_mode: ComputeMode::default(),
            inference_backend: None,
            model_dir: None,
        }
    }
}

impl MiningConfig {
    /// Create config with specified thread count
    pub fn with_threads(mut self, threads: usize) -> Self {
        self.threads = threads.max(1);
        self
    }
}

/// Mining service that runs alongside the node
pub struct MiningService {
    chain: Arc<Chain>,
    consensus: Arc<ConsensusEngine>,
    network: Option<Arc<NetworkService>>,
    config: MiningConfig,
    validator_address: Address,
    /// Current difficulty
    current_difficulty: RwLock<U256>,
    /// Total proofs submitted this epoch (for difficulty adjustment)
    epoch_proof_count: RwLock<u64>,
    /// Stop flag
    stop_flag: Arc<AtomicBool>,
}

impl MiningService {
    /// Create a new mining service
    pub fn new(
        chain: Arc<Chain>,
        consensus: Arc<ConsensusEngine>,
        network: Option<Arc<NetworkService>>,
        config: MiningConfig,
        validator_address: Address,
    ) -> Self {
        let initial_diff = initial_difficulty(&config.difficulty_config);

        Self {
            chain,
            consensus,
            network,
            config,
            validator_address,
            current_difficulty: RwLock::new(initial_diff),
            epoch_proof_count: RwLock::new(0),
            stop_flag: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Start the mining service
    pub async fn start(self: Arc<Self>) {
        match self.config.compute_mode {
            ComputeMode::PowV1 => self.start_pow_v1().await,
            ComputeMode::InferenceV2 => self.start_inference_v2().await,
        }
    }

    /// Start v1 PoW mining loop (legacy)
    async fn start_pow_v1(self: Arc<Self>) {
        info!(
            "Starting PoW mining service (v1) with {} threads for validator {}",
            self.config.threads, self.validator_address
        );

        // Mark validator as providing compute
        self.consensus
            .set_provides_compute(&self.validator_address, true);

        let mut epoch_timer = interval(Duration::from_millis(self.config.epoch_duration_ms));
        let mut current_epoch = 0u64;

        loop {
            epoch_timer.tick().await;

            if self.stop_flag.load(Ordering::Relaxed) {
                info!("Mining service stopped");
                break;
            }

            current_epoch += 1;

            // Create mining task for this epoch
            let task = self.create_mining_task(current_epoch);

            info!(
                "Starting mining epoch {}, difficulty: {:?}",
                current_epoch,
                *self.current_difficulty.read()
            );

            // Mine for the epoch duration
            let mining_service = Arc::clone(&self);
            let task_clone = task.clone();

            // Run mining in a blocking thread pool
            let result = tokio::task::spawn_blocking(move || {
                let miner = Miner::new(
                    mining_service.validator_address,
                    mining_service.config.threads,
                );
                miner.mine_for_duration(
                    &task_clone,
                    Duration::from_millis(mining_service.config.epoch_duration_ms - 500),
                )
            })
            .await;

            match result {
                Ok(mining_result) => {
                    self.handle_mining_result(current_epoch, &task, mining_result)
                        .await;
                }
                Err(e) => {
                    error!("Mining task failed: {}", e);
                }
            }
        }
    }

    /// Start v2 AI inference mining loop
    async fn start_inference_v2(self: Arc<Self>) {
        let backend = match self.config.inference_backend.as_deref() {
            Some("cuda") => qfc_inference::BackendType::Cuda,
            Some("metal") => qfc_inference::BackendType::Metal,
            Some("cpu") => qfc_inference::BackendType::Cpu,
            _ => qfc_inference::runtime::detect_backend(),
        };

        info!(
            "Starting AI inference mining service (v2) for validator {} (backend: {})",
            self.validator_address, backend
        );

        let mut engine = match qfc_inference::create_engine_for_backend(backend) {
            Ok(e) => e,
            Err(e) => {
                error!("Failed to create inference engine: {}", e);
                return;
            }
        };

        // Mark validator as providing compute
        self.consensus
            .set_provides_compute(&self.validator_address, true);

        let tier = qfc_inference::runtime::classify_tier(backend, engine.available_memory_mb());
        let model_registry = qfc_inference::model::ModelRegistry::default_v2();
        let mut task_pool = qfc_ai_coordinator::TaskPool::new();

        let mut epoch_timer = interval(Duration::from_millis(self.config.epoch_duration_ms));
        let mut current_epoch = 0u64;
        let mut tasks_completed = 0u64;

        loop {
            epoch_timer.tick().await;

            if self.stop_flag.load(Ordering::Relaxed) {
                info!("Inference mining service stopped");
                break;
            }

            current_epoch += 1;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;

            // Generate synthetic tasks if pool is empty
            if task_pool.pending_count() == 0 {
                let head_hash = self.chain.head().map(|h| h.hash).unwrap_or_default();
                let seed = head_hash.as_bytes()[0] as u64 ^ current_epoch;
                task_pool.generate_synthetic_tasks(current_epoch, seed, now + 30_000);
            }

            // Fetch a task matching our capabilities
            let task = match task_pool.fetch_task(tier, engine.available_memory_mb()) {
                Some(t) => t,
                None => {
                    debug!(
                        "Epoch {}: no matching tasks for tier {}",
                        current_epoch, tier
                    );
                    continue;
                }
            };

            // Load model if needed
            if let Some(model_id) = task.task_type.model_id() {
                if let Err(e) = engine.load_model(model_id).await {
                    warn!("Failed to load model {}: {}", model_id, e);
                    continue;
                }
            }

            // Run inference
            let result = match engine.run_inference(&task).await {
                Ok(r) => r,
                Err(e) => {
                    error!("Inference failed for epoch {}: {}", current_epoch, e);
                    continue;
                }
            };

            info!(
                "Epoch {}: inference complete ({} ms, {} FLOPS)",
                current_epoch, result.execution_time_ms, result.flops_estimated
            );

            // Build inference proof (using qfc_inference types for local verification)
            let mut inf_proof = qfc_inference::InferenceProof::new(
                self.validator_address,
                current_epoch,
                task.task_type.clone(),
                task.task_id,
                result.output_hash,
                result.execution_time_ms,
                result.flops_estimated,
                backend,
                now / 1000,
            );

            // Sign the proof
            let proof_hash = blake3_hash(&inf_proof.to_bytes_without_signature());
            match self.consensus.sign_hash(&proof_hash) {
                Ok(sig) => inf_proof.set_signature(sig),
                Err(e) => {
                    error!("Failed to sign inference proof: {}", e);
                    continue;
                }
            }

            // Verify locally before broadcasting
            match qfc_ai_coordinator::verify_basic(&inf_proof, current_epoch, &model_registry) {
                Ok(_) => {
                    tasks_completed += 1;

                    // Update inference score in consensus
                    self.consensus.update_inference_score(
                        &self.validator_address,
                        result.flops_estimated,
                        tasks_completed,
                    );

                    info!(
                        "Epoch {}: proof verified (tasks_completed: {})",
                        current_epoch, tasks_completed
                    );

                    // Convert to qfc_types::InferenceProof for network broadcast
                    let types_proof = convert_inference_proof(&inf_proof);

                    // Broadcast proof to network
                    if let Some(network) = &self.network {
                        let msg = ValidatorMessage::InferenceProof(types_proof);
                        if let Err(e) = network.broadcast_validator_msg(msg.to_bytes()).await {
                            warn!("Failed to broadcast inference proof: {}", e);
                        }
                    }
                }
                Err(e) => {
                    warn!("Local proof verification failed: {}", e);
                }
            }
        }
    }

    /// Create a mining task for an epoch
    fn create_mining_task(&self, epoch: u64) -> MiningTask {
        // Generate seed from epoch number and latest block hash
        let head_hash = self.chain.head().map(|h| h.hash).unwrap_or_default();
        let seed = self.generate_seed(epoch, &head_hash);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        MiningTask::new(
            epoch,
            seed,
            *self.current_difficulty.read(),
            now,
            now + self.config.epoch_duration_ms,
        )
    }

    /// Generate mining seed from epoch and block hash
    fn generate_seed(&self, epoch: u64, block_hash: &Hash) -> [u8; 32] {
        let mut data = Vec::with_capacity(40);
        data.extend_from_slice(&epoch.to_le_bytes());
        data.extend_from_slice(block_hash.as_bytes());
        let hash = blake3_hash(&data);
        let mut seed = [0u8; 32];
        seed.copy_from_slice(hash.as_bytes());
        seed
    }

    /// Handle mining result after an epoch
    async fn handle_mining_result(&self, epoch: u64, task: &MiningTask, result: MiningResult) {
        if result.work_count == 0 {
            debug!("Epoch {}: No valid hashes found", epoch);
            return;
        }

        info!(
            "Epoch {}: Found {} valid hashes, hashrate: {:.2} H/s",
            epoch,
            result.work_count,
            result.hashrate()
        );

        // Create and sign work proof
        let miner = Miner::new(self.validator_address, 1);
        let mut proof = miner.create_proof(task, &result);

        // Sign the proof
        let proof_hash = blake3_hash(&proof.to_bytes_without_signature());
        match self.consensus.sign_hash(&proof_hash) {
            Ok(sig) => proof.set_signature(sig),
            Err(e) => {
                error!("Failed to sign work proof: {}", e);
                return;
            }
        }

        // Update local hashrate
        let hashrate = qfc_pow::calculate_hashrate(&proof, task);
        self.consensus
            .update_hashrate(&self.validator_address, hashrate);

        info!(
            "Updated hashrate for {}: {} H/s",
            self.validator_address, hashrate
        );

        // Broadcast proof to network
        if let Some(network) = &self.network {
            let msg = ValidatorMessage::WorkProof(proof.clone());
            if let Err(e) = network.broadcast_validator_msg(msg.to_bytes()).await {
                warn!("Failed to broadcast work proof: {}", e);
            } else {
                debug!("Broadcasted work proof for epoch {}", epoch);
            }
        }

        // Update epoch proof count for difficulty adjustment
        *self.epoch_proof_count.write() += result.work_count;

        // Adjust difficulty at end of epoch
        self.adjust_difficulty();
    }

    /// Adjust difficulty based on proof count
    fn adjust_difficulty(&self) {
        let proof_count = *self.epoch_proof_count.read();
        let current = *self.current_difficulty.read();

        let new_difficulty =
            adjust_difficulty(&current, proof_count, &self.config.difficulty_config);

        if new_difficulty != current {
            debug!(
                "Difficulty adjusted: proofs={}, old={:?}, new={:?}",
                proof_count, current, new_difficulty
            );
            *self.current_difficulty.write() = new_difficulty;
        }

        // Reset proof count for next epoch
        *self.epoch_proof_count.write() = 0;
    }

    /// Stop the mining service
    #[allow(dead_code)]
    pub fn stop(&self) {
        self.stop_flag.store(true, Ordering::Relaxed);
    }
}

/// Convert qfc_inference::InferenceProof → qfc_types::InferenceProof
/// Both types have identical Borsh layout, so we serialize/deserialize.
fn convert_inference_proof(proof: &qfc_inference::InferenceProof) -> qfc_types::InferenceProof {
    let bytes = borsh::to_vec(proof).expect("InferenceProof serialization should not fail");
    borsh::from_slice(&bytes).expect("InferenceProof deserialization should not fail")
}

/// Get the number of CPUs available
#[allow(dead_code)]
fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}
