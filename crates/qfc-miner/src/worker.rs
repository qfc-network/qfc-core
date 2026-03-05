//! Inference worker loop

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use qfc_inference::InferenceEngine;
use tokio::time::interval;
use tracing::{debug, info};

use crate::config::MinerConfig;

/// Inference worker that fetches tasks and submits proofs
pub struct InferenceWorker {
    #[allow(dead_code)]
    config: MinerConfig,
    engine: Box<dyn InferenceEngine>,
    stop_flag: Arc<AtomicBool>,
}

impl InferenceWorker {
    pub fn new(
        config: MinerConfig,
        engine: Box<dyn InferenceEngine>,
    ) -> Self {
        Self {
            config,
            engine,
            stop_flag: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Main worker loop
    pub async fn run(&self) {
        info!(
            "Starting inference worker (backend: {}, wallet: {})",
            self.config.backend, self.config.wallet_address
        );

        let mut epoch_timer = interval(Duration::from_secs(10));
        let mut epoch = 0u64;

        loop {
            epoch_timer.tick().await;

            if self.stop_flag.load(Ordering::Relaxed) {
                info!("Inference worker stopped");
                break;
            }

            epoch += 1;
            debug!("Worker epoch {}", epoch);

            // TODO: Fetch task from validator via RPC
            // TODO: Run inference
            // TODO: Submit proof via RPC

            // Placeholder: log heartbeat
            info!(
                "Epoch {}: waiting for tasks (backend: {}, memory: {} MB)",
                epoch,
                self.engine.backend_type(),
                self.engine.available_memory_mb()
            );
        }
    }

    /// Stop the worker
    #[allow(dead_code)]
    pub fn stop(&self) {
        self.stop_flag.store(true, Ordering::Relaxed);
    }
}
