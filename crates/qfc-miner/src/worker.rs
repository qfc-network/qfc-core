//! Inference worker loop

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use qfc_inference::proof::InferenceProof;
use qfc_inference::runtime::{classify_tier, GpuTier};
use qfc_inference::scheduler::ModelScheduler;
use qfc_inference::task::{ComputeTaskType, InferenceTask, ModelId};
use qfc_inference::InferenceEngine;
use qfc_types::Hash;
use tokio::time::interval;
use tracing::{debug, error, info, warn};

use crate::config::MinerConfig;
use crate::dashboard::tui::SharedStats;
use crate::submit::{self, InferenceTaskResponse};

/// Inference worker that fetches tasks and submits proofs
pub struct InferenceWorker {
    config: MinerConfig,
    engine: Box<dyn InferenceEngine>,
    scheduler: ModelScheduler,
    stop_flag: Arc<AtomicBool>,
    stats: SharedStats,
    earnings_store: Arc<Mutex<crate::storage::EarningsStore>>,
    store_path: PathBuf,
}

impl InferenceWorker {
    pub fn new(
        config: MinerConfig,
        engine: Box<dyn InferenceEngine>,
        scheduler: ModelScheduler,
        stats: SharedStats,
        earnings_store: Arc<Mutex<crate::storage::EarningsStore>>,
        store_path: PathBuf,
    ) -> Self {
        Self {
            config,
            engine,
            scheduler,
            stop_flag: Arc::new(AtomicBool::new(false)),
            stats,
            earnings_store,
            store_path,
        }
    }

    /// Main worker loop
    pub async fn run(&mut self) {
        let tier = classify_tier(self.config.backend, self.config.max_memory_mb);
        info!(
            "Starting inference worker (backend: {}, tier: {}, wallet: {})",
            self.config.backend,
            tier,
            hex::encode(self.config.wallet_address.as_bytes())
        );

        let mut epoch_timer = interval(Duration::from_secs(10));
        let mut status_timer = interval(Duration::from_secs(30));
        let mut tasks_completed: u64 = 0;
        let mut tasks_failed: u64 = 0;
        let mut total_earnings_wei: u128 = 0;
        let mut total_flops: u64 = 0;
        let _session_earnings_wei: u128 = 0;

        // Start a new persistent session
        {
            let session_now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let mut store = self.earnings_store.lock().unwrap();
            store.start_session(session_now);
            if let Err(e) = store.save(&self.store_path) {
                warn!("Failed to save earnings on session start: {}", e);
            }
        }

        // Update dashboard status
        if let Ok(mut s) = self.stats.lock() {
            s.status = "Waiting for tasks...".to_string();
        }

        loop {
            tokio::select! {
                _ = epoch_timer.tick() => {},
                _ = status_timer.tick() => {
                    // Periodic status report
                    self.report_status().await;
                    continue;
                },
            }

            if self.stop_flag.load(Ordering::Relaxed) {
                info!("Inference worker stopped");
                // End persistent session
                {
                    let end_now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    let mut store = self.earnings_store.lock().unwrap();
                    store.end_session(end_now);
                    if let Err(e) = store.save(&self.store_path) {
                        warn!("Failed to save earnings on shutdown: {}", e);
                    }
                }
                break;
            }

            // 1. Fetch task from validator
            let task_response = match self.fetch_task(tier).await {
                Ok(Some(task)) => task,
                Ok(None) => {
                    debug!("No tasks available, waiting...");
                    continue;
                }
                Err(e) => {
                    warn!("Failed to fetch task: {}", e);
                    continue;
                }
            };

            // Update dashboard status
            if let Ok(mut s) = self.stats.lock() {
                s.status = format!(
                    "Processing task {} ({})",
                    &task_response.task_id[..8.min(task_response.task_id.len())],
                    task_response.model_name
                );
            }

            info!(
                "Received task {} (epoch {}, model: {})",
                &task_response.task_id[..16.min(task_response.task_id.len())],
                task_response.epoch,
                task_response.model_name
            );

            // 2. Deadline check — skip if task already expired
            {
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                if now_ms > task_response.deadline {
                    warn!(
                        "Task already past deadline (deadline: {}, now: {}), skipping",
                        task_response.deadline, now_ms
                    );
                    continue;
                }
            }

            // 3. Convert RPC response to InferenceTask
            let task = match self.convert_task(&task_response) {
                Ok(t) => t,
                Err(e) => {
                    error!("Failed to parse task: {}", e);
                    tasks_failed += 1;
                    continue;
                }
            };

            // 3. Ensure model is loaded via scheduler
            let model_id = ModelId::new(&task_response.model_name, &task_response.model_version);
            match self
                .scheduler
                .ensure_model_loaded(&model_id, &mut *self.engine)
                .await
            {
                Ok(layer) => {
                    debug!("Model {} loaded as {:?}", model_id, layer);
                }
                Err(e) => {
                    warn!("Failed to load model {}: {}", model_id, e);
                    tasks_failed += 1;
                    continue;
                }
            }

            // 4. Run inference (with auto-fallback to CPU on Metal/CUDA runtime errors)
            let result = match self.engine.run_inference(&task).await {
                Ok(r) => r,
                Err(e) => {
                    let err_msg = format!("{}", e);

                    // Auto-fallback to CPU if Metal/CUDA has unsupported ops
                    if (err_msg.contains("Metal error")
                        || err_msg.contains("no metal implementation")
                        || err_msg.contains("CUDA error"))
                        && self.engine.backend_type() != qfc_inference::BackendType::Cpu
                    {
                        warn!(
                            "{} backend failed: {}. Auto-switching to CPU backend.",
                            self.engine.backend_type(),
                            err_msg
                        );
                        match qfc_inference::create_engine_for_backend(
                            qfc_inference::BackendType::Cpu,
                        ) {
                            Ok(cpu_engine) => {
                                self.engine = cpu_engine;
                                self.scheduler.clear_loaded(); // old engine's models are gone
                                info!("Switched to CPU backend. Reloading model and retrying...");
                                // Reload model on the new CPU engine
                                if let Err(e2) = self
                                    .scheduler
                                    .ensure_model_loaded(&model_id, &mut *self.engine)
                                    .await
                                {
                                    error!("Failed to load model on CPU: {}", e2);
                                    tasks_failed += 1;
                                    continue;
                                }
                                // Retry this task with CPU
                                match self.engine.run_inference(&task).await {
                                    Ok(r) => r,
                                    Err(e2) => {
                                        error!("Inference failed on CPU fallback: {}", e2);
                                        tasks_failed += 1;
                                        continue;
                                    }
                                }
                            }
                            Err(e2) => {
                                error!("Failed to create CPU fallback engine: {}", e2);
                                tasks_failed += 1;
                                continue;
                            }
                        }
                    } else {
                        error!("Inference failed: {}", err_msg);
                        tasks_failed += 1;
                        continue;
                    }
                }
            };

            let task_flops = result.flops_estimated;
            let task_duration_ms = result.execution_time_ms;
            total_flops += task_flops;

            info!(
                "Inference complete: {} ms, {} FLOPS, output hash: {}",
                task_duration_ms,
                task_flops,
                hex::encode(&result.output_hash.as_bytes()[..8])
            );

            // 5. Build proof
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();

            // Determine canonical format from model registry
            let canonical_format = task
                .task_type
                .model_id()
                .and_then(|mid| {
                    qfc_inference::model::ModelRegistry::default_v2().canonical_format(mid)
                })
                .unwrap_or(qfc_inference::CanonicalFormat::SafetensorsFp32);

            let mut proof = InferenceProof::new(
                self.config.wallet_address,
                task_response.epoch,
                task.task_type.clone(),
                task.task_id, // input hash = task_id
                result.output_hash,
                result.execution_time_ms,
                result.flops_estimated,
                self.engine.backend_type(),
                canonical_format,
                now,
            );

            // 5b. Sign the proof
            let keypair = qfc_crypto::Keypair::from_secret_bytes(&self.config.secret_key)
                .expect("validated at startup");
            let proof_hash = qfc_crypto::blake3_hash(&proof.to_bytes_without_signature());
            let signature = keypair.sign_hash(&proof_hash);
            proof.set_signature(signature);

            // 6. Submit proof to validator
            let rpc_url = &self.config.validator_rpc;
            let miner_addr = hex::encode(self.config.wallet_address.as_bytes());
            let task_type_name = task_response.task_type.clone();
            let model_name = task_response.model_name.clone();

            match submit::submit_proof(rpc_url, &miner_addr, &proof).await {
                Ok(result) => {
                    tasks_completed += 1;
                    if result.accepted {
                        // Parse reward estimate and accumulate
                        let reward_wei = result
                            .reward_estimate
                            .as_deref()
                            .and_then(parse_hex_u128)
                            .unwrap_or(0);
                        total_earnings_wei += reward_wei;

                        let reward_display = format_wei_as_qfc(reward_wei);
                        let total_display = format_wei_as_qfc(total_earnings_wei);

                        info!(
                            "PROOF ACCEPTED | +{} QFC | session: {} QFC | {}/{} tasks{}",
                            reward_display,
                            total_display,
                            tasks_completed,
                            tasks_completed + tasks_failed,
                            if result.spot_checked { " | spot-check: PASSED" } else { "" }
                        );

                        // Update dashboard stats
                        if let Ok(mut s) = self.stats.lock() {
                            s.tasks_completed = tasks_completed;
                            s.tasks_failed = tasks_failed;
                            s.total_flops = total_flops;
                            s.session_earnings_wei = total_earnings_wei;
                            s.status = "Waiting for tasks...".to_string();

                            // Add task log entry
                            let now = chrono_now_hms();
                            let reward_str = format!("{} QFC", reward_display);
                            s.task_log.insert(
                                0,
                                crate::dashboard::tui::TaskLogEntry {
                                    timestamp: now,
                                    task_type: task_type_name,
                                    model: model_name,
                                    duration_ms: task_duration_ms,
                                    reward_qfc: reward_str,
                                    accepted: true,
                                },
                            );
                            if s.task_log.len() > 20 {
                                s.task_log.truncate(20);
                            }

                            // Add to earning sparkline (micro-QFC units)
                            let micro_qfc = (reward_wei / 1_000_000_000_000).max(1) as u64;
                            s.earning_history.push(micro_qfc);
                            if s.earning_history.len() > 60 {
                                s.earning_history.remove(0);
                            }
                        }

                        // Persist task record to disk
                        {
                            let mut store = self.earnings_store.lock().unwrap();
                            store.record_task(crate::storage::TaskRecord {
                                timestamp: now,
                                task_id: task_response.task_id.clone(),
                                task_type: task_response.task_type.clone(),
                                model: task_response.model_name.clone(),
                                duration_ms: task_duration_ms,
                                reward_wei: reward_wei,
                                accepted: true,
                                epoch: task_response.epoch,
                            });
                            store.update_session(
                                tasks_completed,
                                tasks_failed,
                                total_earnings_wei,
                                total_flops,
                            );
                            if let Err(e) = store.save(&self.store_path) {
                                warn!("Failed to save earnings: {}", e);
                            }

                            // Update dashboard lifetime stats
                            if let Ok(mut s) = self.stats.lock() {
                                s.lifetime_earnings_wei = store.lifetime_earnings_wei;
                                s.lifetime_tasks = store.lifetime_tasks_completed;
                                s.earnings_24h_wei = store.earnings_last_hours(24);
                            }
                        }
                    } else {
                        warn!("Proof rejected: {}", result.message);
                        // Persist rejected task record
                        {
                            let mut store = self.earnings_store.lock().unwrap();
                            store.record_task(crate::storage::TaskRecord {
                                timestamp: now,
                                task_id: task_response.task_id.clone(),
                                task_type: task_response.task_type.clone(),
                                model: task_response.model_name.clone(),
                                duration_ms: task_duration_ms,
                                reward_wei: 0,
                                accepted: false,
                                epoch: task_response.epoch,
                            });
                            store.update_session(
                                tasks_completed,
                                tasks_failed,
                                total_earnings_wei,
                                total_flops,
                            );
                            if let Err(e) = store.save(&self.store_path) {
                                warn!("Failed to save earnings: {}", e);
                            }
                        }
                        if let Ok(mut s) = self.stats.lock() {
                            s.status = format!("Proof rejected: {}", result.message);
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to submit proof: {}", e);
                    tasks_failed += 1;
                    if let Ok(mut s) = self.stats.lock() {
                        s.tasks_failed = tasks_failed;
                        s.status = format!("Submit failed: {}", e);
                    }
                }
            }
        }
    }

    /// Fetch a task from the validator RPC
    async fn fetch_task(
        &self,
        tier: GpuTier,
    ) -> Result<Option<InferenceTaskResponse>, submit::SubmitError> {
        let rpc_url = &self.config.validator_rpc;
        let miner_addr = hex::encode(self.config.wallet_address.as_bytes());

        submit::fetch_task(
            rpc_url,
            &miner_addr,
            tier,
            self.config.max_memory_mb,
            self.engine.backend_type(),
        )
        .await
    }

    /// Convert an RPC task response into an InferenceTask
    fn convert_task(&self, resp: &InferenceTaskResponse) -> Result<InferenceTask, String> {
        let task_id_bytes =
            hex::decode(&resp.task_id).map_err(|e| format!("Invalid task_id hex: {}", e))?;
        let task_id = Hash::from_slice(&task_id_bytes)
            .ok_or_else(|| "task_id must be 32 bytes".to_string())?;

        let input_data =
            hex::decode(&resp.input_data).map_err(|e| format!("Invalid input_data hex: {}", e))?;

        let input_hash = qfc_crypto::blake3_hash(&input_data);

        let model_id = ModelId::new(&resp.model_name, &resp.model_version);
        let task_type = match resp.task_type.as_str() {
            "embedding" => ComputeTaskType::Embedding {
                model_id,
                input_hash,
            },
            "image_classification" => ComputeTaskType::ImageClassification {
                model_id,
                input_hash,
            },
            "text_generation" => ComputeTaskType::TextGeneration {
                model_id,
                prompt_hash: input_hash,
                max_tokens: 128,
                temperature_fp: 0,
                seed: resp.epoch,
            },
            "speech_to_text" => ComputeTaskType::SpeechToText {
                model_id,
                audio_hash: input_hash,
                language: resp.language.clone().unwrap_or_default(),
            },
            "image_generation" => ComputeTaskType::ImageGeneration {
                model_id,
                prompt_hash: input_hash,
                negative_prompt_hash: qfc_types::Hash::ZERO,
                width: 512,
                height: 512,
                steps: 20,
                seed: resp.epoch,
            },
            other => return Err(format!("Unknown task type: {}", other)),
        };

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        Ok(InferenceTask::new(
            task_id,
            resp.epoch,
            task_type,
            input_data,
            now,
            resp.deadline,
        ))
    }

    /// Report miner status to the validator
    async fn report_status(&self) {
        let miner_addr = hex::encode(self.config.wallet_address.as_bytes());
        let loaded_models: Vec<(String, String, String)> = self
            .scheduler
            .report_loaded_models()
            .into_iter()
            .map(|(id, layer)| (id.name.clone(), id.version.clone(), layer.to_string()))
            .collect();

        let keypair = qfc_crypto::Keypair::from_secret_bytes(&self.config.secret_key)
            .expect("validated at startup");

        if let Err(e) = submit::report_miner_status(
            &self.config.validator_rpc,
            &miner_addr,
            loaded_models,
            0, // pending tasks
            &keypair,
        )
        .await
        {
            debug!("Failed to report status: {}", e);
        }
    }

    /// Stop the worker
    #[allow(dead_code)]
    pub fn stop(&self) {
        self.stop_flag.store(true, Ordering::Relaxed);
    }
}

/// Parse a hex string like "0x1a2b" into u128
fn parse_hex_u128(s: &str) -> Option<u128> {
    let hex_str = s.strip_prefix("0x").unwrap_or(s);
    u128::from_str_radix(hex_str, 16).ok()
}

/// Format wei amount as human-readable QFC (1 QFC = 1e18 wei)
fn format_wei_as_qfc(wei: u128) -> String {
    if wei == 0 {
        return "0.000000".to_string();
    }
    let whole = wei / 1_000_000_000_000_000_000;
    let frac = wei % 1_000_000_000_000_000_000;
    format!("{}.{:06}", whole, frac / 1_000_000_000_000)
}

/// Get current time as HH:MM:SS string
fn chrono_now_hms() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let h = (secs % 86400) / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    format!("{:02}:{:02}:{:02}", h, m, s)
}
