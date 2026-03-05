//! CUDA GPU inference backend (requires `cuda` feature)
//!
//! Uses candle-core's CUDA backend for NVIDIA GPU inference.

use std::collections::HashMap;

use async_trait::async_trait;

use crate::models::LoadedModel;
use crate::proof::InferenceResult;
use crate::runtime::{BackendType, BenchmarkResult};
use crate::task::{ComputeTaskType, InferenceTask, ModelId};
use crate::{InferenceEngine, InferenceError};

/// CUDA-based inference engine
pub struct CudaEngine {
    loaded_models: Vec<ModelId>,
    device_memory_mb: u64,
    device_name: String,
    device: candle_core::Device,
    candle_models: HashMap<String, Box<dyn LoadedModel>>,
}

impl CudaEngine {
    pub fn new() -> Result<Self, InferenceError> {
        let device = candle_core::Device::new_cuda(0)
            .map_err(|e| InferenceError::BackendUnavailable(format!("CUDA device 0: {}", e)))?;

        let (memory, name) = detect_cuda_device().unwrap_or((0, "CUDA GPU".to_string()));

        tracing::info!("CUDA engine initialized: {} ({}MB VRAM)", name, memory);

        Ok(Self {
            loaded_models: Vec::new(),
            device_memory_mb: memory,
            device_name: name,
            device,
            candle_models: HashMap::new(),
        })
    }
}

#[async_trait]
impl InferenceEngine for CudaEngine {
    fn backend_type(&self) -> BackendType {
        BackendType::Cuda
    }

    fn supported_models(&self) -> Vec<ModelId> {
        self.loaded_models.clone()
    }

    fn available_memory_mb(&self) -> u64 {
        self.device_memory_mb
    }

    async fn load_model(&mut self, model_id: &ModelId) -> Result<(), InferenceError> {
        use crate::download::download_model;
        use crate::models::bert::BertEmbedding;

        tracing::info!(
            "Loading model {} on CUDA backend ({})",
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
        tracing::info!("Model {} loaded on CUDA", model_id);
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
        let flops = estimate_flops_cuda(&task.task_type);

        Ok(InferenceResult::new(output, elapsed, flops))
    }

    fn benchmark(&self) -> Result<BenchmarkResult, InferenceError> {
        let start = std::time::Instant::now();

        // CUDA GEMM benchmark: 1024x1024 matrix multiply
        let n = 1024usize;
        let a = candle_core::Tensor::randn(0f32, 1.0, (n, n), &self.device)
            .map_err(|e| InferenceError::ExecutionFailed(e.to_string()))?;
        let b = candle_core::Tensor::randn(0f32, 1.0, (n, n), &self.device)
            .map_err(|e| InferenceError::ExecutionFailed(e.to_string()))?;

        let _c = a
            .matmul(&b)
            .map_err(|e| InferenceError::ExecutionFailed(e.to_string()))?;

        let elapsed = start.elapsed();
        // GEMM FLOPS: 2 * N^3
        let ops = 2.0 * (n as f64).powi(3);
        let flops = ops / elapsed.as_secs_f64();

        let mut result = BenchmarkResult {
            flops,
            tokens_per_second: 0.0,
            memory_bandwidth_gbps: 0.0,
            backend: BackendType::Cuda,
            benchmark_time_ms: elapsed.as_millis() as u64,
            score: 0,
        };
        result.score = crate::runtime::compute_benchmark_score(&result).0;
        Ok(result)
    }
}

fn estimate_flops_cuda(task_type: &ComputeTaskType) -> u64 {
    match task_type {
        ComputeTaskType::TextGeneration { max_tokens, .. } => {
            2 * 7_000_000_000u64 * (*max_tokens as u64)
        }
        ComputeTaskType::ImageClassification { .. } => 4_000_000_000u64,
        ComputeTaskType::Embedding { .. } => 1_000_000_000u64,
        ComputeTaskType::OnnxInference { .. } => 2_000_000_000u64,
    }
}

fn detect_cuda_device() -> Result<(u64, String), InferenceError> {
    let output = std::process::Command::new("nvidia-smi")
        .args([
            "--query-gpu=memory.total,name",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .map_err(|_| InferenceError::BackendUnavailable("CUDA".to_string()))?;

    if !output.status.success() {
        return Err(InferenceError::BackendUnavailable("CUDA".to_string()));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().next().unwrap_or("");
    let parts: Vec<&str> = line.split(", ").collect();

    let memory_mb = parts
        .first()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0);
    let name = parts
        .get(1)
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "Unknown CUDA GPU".to_string());

    Ok((memory_mb, name))
}
