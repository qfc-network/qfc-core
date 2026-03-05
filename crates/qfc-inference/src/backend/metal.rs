//! Metal GPU inference backend for Apple Silicon (requires `metal` feature)
//!
//! Uses Metal Performance Shaders via candle-core for GPU-accelerated
//! inference on M1/M2/M3/M4 chips. Apple Silicon's unified memory
//! architecture means the GPU can access the full system RAM.

use async_trait::async_trait;

use crate::proof::InferenceResult;
use crate::runtime::{BackendType, BenchmarkResult};
use crate::task::{InferenceTask, ModelId};
use crate::{InferenceEngine, InferenceError};

/// Metal-based inference engine for Apple Silicon
pub struct MetalEngine {
    loaded_models: Vec<ModelId>,
    /// Unified memory available (GPU = system RAM on Apple Silicon)
    unified_memory_mb: u64,
    device_name: String,
}

impl MetalEngine {
    pub fn new() -> Result<Self, InferenceError> {
        if !cfg!(target_os = "macos") {
            return Err(InferenceError::BackendUnavailable("Metal".to_string()));
        }

        let memory = crate::runtime::detect_hardware().memory_mb;
        let device_name = detect_apple_chip();

        Ok(Self {
            loaded_models: Vec::new(),
            unified_memory_mb: memory,
            device_name,
        })
    }
}

#[async_trait]
impl InferenceEngine for MetalEngine {
    fn backend_type(&self) -> BackendType {
        BackendType::Metal
    }

    fn supported_models(&self) -> Vec<ModelId> {
        self.loaded_models.clone()
    }

    fn available_memory_mb(&self) -> u64 {
        self.unified_memory_mb
    }

    async fn load_model(&mut self, model_id: &ModelId) -> Result<(), InferenceError> {
        tracing::info!(
            "Loading model {} on Metal backend ({})",
            model_id,
            self.device_name
        );
        // TODO: Load model weights via candle-core with Metal device
        self.loaded_models.push(model_id.clone());
        Ok(())
    }

    async fn run_inference(&self, task: &InferenceTask) -> Result<InferenceResult, InferenceError> {
        let start = std::time::Instant::now();

        // TODO: Run actual Metal inference via candle-core
        // Placeholder: deterministic output like CPU backend
        let output = crate::backend::cpu::deterministic_placeholder(task);
        let elapsed = start.elapsed().as_millis() as u64;

        Ok(InferenceResult::new(output, elapsed, 0))
    }

    fn benchmark(&self) -> Result<BenchmarkResult, InferenceError> {
        // TODO: Run Metal-specific benchmark (matrix ops via MPS)
        Ok(BenchmarkResult {
            flops: 0.0,
            tokens_per_second: 0.0,
            memory_bandwidth_gbps: 0.0,
            backend: BackendType::Metal,
            benchmark_time_ms: 0,
        })
    }
}

/// Detect Apple Silicon chip model
fn detect_apple_chip() -> String {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("sysctl")
            .args(["-n", "machdep.cpu.brand_string"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "Apple Silicon".to_string())
    }
    #[cfg(not(target_os = "macos"))]
    {
        "Apple Silicon (unavailable)".to_string()
    }
}
