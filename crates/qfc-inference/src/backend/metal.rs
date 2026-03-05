//! Metal GPU inference backend for Apple Silicon (requires `metal` feature)
//!
//! Uses Metal Performance Shaders via candle-core for GPU-accelerated
//! inference on M1/M2/M3/M4 chips. Apple Silicon's unified memory
//! architecture means the GPU can access the full system RAM.

use std::collections::HashMap;

use async_trait::async_trait;

use crate::models::LoadedModel;
use crate::proof::InferenceResult;
use crate::runtime::{BackendType, BenchmarkResult};
use crate::task::{ComputeTaskType, InferenceTask, ModelId};
use crate::{InferenceEngine, InferenceError};

/// Metal-based inference engine for Apple Silicon
pub struct MetalEngine {
    loaded_models: Vec<ModelId>,
    /// Unified memory available (GPU = system RAM on Apple Silicon)
    unified_memory_mb: u64,
    device_name: String,
    device: candle_core::Device,
    candle_models: HashMap<String, Box<dyn LoadedModel>>,
}

impl MetalEngine {
    pub fn new() -> Result<Self, InferenceError> {
        if !cfg!(target_os = "macos") {
            return Err(InferenceError::BackendUnavailable(
                "Metal requires macOS".to_string(),
            ));
        }

        let device = candle_core::Device::new_metal(0)
            .map_err(|e| InferenceError::BackendUnavailable(format!("Metal device: {}", e)))?;

        let memory = crate::runtime::detect_hardware().memory_mb;
        let device_name = detect_apple_chip();

        tracing::info!(
            "Metal engine initialized: {} ({}MB unified memory)",
            device_name,
            memory
        );

        Ok(Self {
            loaded_models: Vec::new(),
            unified_memory_mb: memory,
            device_name,
            device,
            candle_models: HashMap::new(),
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
        use crate::download::download_model;
        use crate::models::bert::BertEmbedding;

        tracing::info!(
            "Loading model {} on Metal backend ({})",
            model_id,
            self.device_name
        );

        let downloaded = download_model(&model_id.name)?;
        let loaded: Box<dyn LoadedModel> = Box::new(BertEmbedding::load(
            &downloaded.weights_path,
            &downloaded.tokenizer_path,
            &downloaded.config_path,
            &self.device,
        )?);

        self.candle_models.insert(model_id.name.clone(), loaded);
        self.loaded_models.push(model_id.clone());
        tracing::info!("Model {} loaded on Metal", model_id);
        Ok(())
    }

    async fn run_inference(&self, task: &InferenceTask) -> Result<InferenceResult, InferenceError> {
        let start = std::time::Instant::now();

        let output = if let Some(model_name) = task.task_type.model_id().map(|m| &m.name) {
            if let Some(model) = self.candle_models.get(model_name.as_str()) {
                model.forward(&task.input_data)?
            } else {
                crate::backend::cpu::deterministic_placeholder(task)
            }
        } else {
            crate::backend::cpu::deterministic_placeholder(task)
        };

        let elapsed = start.elapsed().as_millis() as u64;
        let flops = estimate_flops_metal(&task.task_type);

        Ok(InferenceResult::new(output, elapsed, flops))
    }

    fn benchmark(&self) -> Result<BenchmarkResult, InferenceError> {
        let start = std::time::Instant::now();

        // Metal GEMM benchmark: 1024x1024 matrix multiply
        let n = 1024usize;
        let a = candle_core::Tensor::randn(0f32, 1.0, (n, n), &self.device)
            .map_err(|e| InferenceError::ExecutionFailed(e.to_string()))?;
        let b = candle_core::Tensor::randn(0f32, 1.0, (n, n), &self.device)
            .map_err(|e| InferenceError::ExecutionFailed(e.to_string()))?;

        let _c = a
            .matmul(&b)
            .map_err(|e| InferenceError::ExecutionFailed(e.to_string()))?;

        let elapsed = start.elapsed();
        let ops = 2.0 * (n as f64).powi(3);
        let flops = ops / elapsed.as_secs_f64();

        let mut result = BenchmarkResult {
            flops,
            tokens_per_second: 0.0,
            memory_bandwidth_gbps: 0.0,
            backend: BackendType::Metal,
            benchmark_time_ms: elapsed.as_millis() as u64,
            score: 0,
        };
        result.score = crate::runtime::compute_benchmark_score(&result).0;
        Ok(result)
    }
}

fn estimate_flops_metal(task_type: &ComputeTaskType) -> u64 {
    match task_type {
        ComputeTaskType::TextGeneration { max_tokens, .. } => {
            2 * 7_000_000_000u64 * (*max_tokens as u64)
        }
        ComputeTaskType::ImageClassification { .. } => 4_000_000_000u64,
        ComputeTaskType::Embedding { .. } => 1_000_000_000u64,
        ComputeTaskType::OnnxInference { .. } => 2_000_000_000u64,
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
