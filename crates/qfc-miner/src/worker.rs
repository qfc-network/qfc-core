//! Inference worker loop

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
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
use crate::submit::{self, InferenceTaskResponse};

/// Inference worker that fetches tasks and submits proofs
pub struct InferenceWorker {
    config: MinerConfig,
    engine: Box<dyn InferenceEngine>,
    scheduler: ModelScheduler,
    stop_flag: Arc<AtomicBool>,
}

impl InferenceWorker {
    pub fn new(
        config: MinerConfig,
        engine: Box<dyn InferenceEngine>,
        scheduler: ModelScheduler,
    ) -> Self {
        Self {
            config,
            engine,
            scheduler,
            stop_flag: Arc::new(AtomicBool::new(false)),
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

            info!(
                "Received task {} (epoch {}, model: {})",
                &task_response.task_id[..16.min(task_response.task_id.len())],
                task_response.epoch,
                task_response.model_name
            );

            // 2. Convert RPC response to InferenceTask
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

            // 4. Run inference
            let result = match self.engine.run_inference(&task).await {
                Ok(r) => r,
                Err(e) => {
                    error!("Inference failed: {}", e);
                    tasks_failed += 1;
                    continue;
                }
            };

            info!(
                "Inference complete: {} ms, {} FLOPS, output hash: {}",
                result.execution_time_ms,
                result.flops_estimated,
                hex::encode(&result.output_hash.as_bytes()[..8])
            );

            // 5. Build proof
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();

            let mut proof = InferenceProof::new(
                self.config.wallet_address,
                task_response.epoch,
                task.task_type.clone(),
                task.task_id, // input hash = task_id
                result.output_hash,
                result.execution_time_ms,
                result.flops_estimated,
                self.engine.backend_type(),
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

            match submit::submit_proof(rpc_url, &miner_addr, &proof).await {
                Ok(result) => {
                    tasks_completed += 1;
                    if result.accepted {
                        info!(
                            "Proof accepted! (spot_checked: {}, total: {}, failed: {})",
                            result.spot_checked, tasks_completed, tasks_failed
                        );
                    } else {
                        warn!("Proof rejected: {}", result.message);
                    }
                }
                Err(e) => {
                    error!("Failed to submit proof: {}", e);
                    tasks_failed += 1;
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
