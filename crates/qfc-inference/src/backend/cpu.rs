//! CPU-only inference backend

use std::collections::HashMap;

use async_trait::async_trait;

use crate::proof::InferenceResult;
use crate::runtime::{BackendType, BenchmarkResult};
use crate::task::{ComputeTaskType, InferenceTask, ModelId};
use crate::{InferenceEngine, InferenceError};

#[cfg(feature = "candle")]
use crate::models::LoadedModel;

/// CPU-based inference engine
pub struct CpuEngine {
    /// Loaded model IDs
    loaded_models: Vec<ModelId>,
    /// Available system memory in MB
    available_memory_mb: u64,
    /// Loaded candle models (when candle feature enabled)
    #[cfg(feature = "candle")]
    candle_models: HashMap<String, Box<dyn LoadedModel>>,
    #[cfg(not(feature = "candle"))]
    _models: HashMap<String, ()>,
}

impl CpuEngine {
    pub fn new() -> Self {
        let mem = crate::runtime::detect_hardware().memory_mb;
        Self {
            loaded_models: Vec::new(),
            available_memory_mb: mem,
            #[cfg(feature = "candle")]
            candle_models: HashMap::new(),
            #[cfg(not(feature = "candle"))]
            _models: HashMap::new(),
        }
    }
}

impl Default for CpuEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl InferenceEngine for CpuEngine {
    fn backend_type(&self) -> BackendType {
        BackendType::Cpu
    }

    fn supported_models(&self) -> Vec<ModelId> {
        self.loaded_models.clone()
    }

    fn available_memory_mb(&self) -> u64 {
        self.available_memory_mb
    }

    async fn load_model(&mut self, model_id: &ModelId) -> Result<(), InferenceError> {
        #[cfg(feature = "candle")]
        {
            use crate::download::download_model;
            use crate::models::bert::BertEmbedding;
            use candle_core::Device;

            tracing::info!("Loading model {} on CPU backend (candle)", model_id);

            let downloaded = download_model(&model_id.name)?;
            let device = Device::Cpu;

            let loaded: Box<dyn LoadedModel> = Box::new(BertEmbedding::load(
                &downloaded.weights_path,
                &downloaded.tokenizer_path,
                &downloaded.config_path,
                &device,
            )?);

            self.candle_models.insert(model_id.name.clone(), loaded);
            self.loaded_models.push(model_id.clone());
            tracing::info!("Model {} loaded successfully on CPU", model_id);
        }

        #[cfg(not(feature = "candle"))]
        {
            tracing::info!(
                "Loading model {} on CPU backend (placeholder — enable 'candle' feature for real inference)",
                model_id
            );
            self.loaded_models.push(model_id.clone());
        }

        Ok(())
    }

    async fn run_inference(&self, task: &InferenceTask) -> Result<InferenceResult, InferenceError> {
        let start = std::time::Instant::now();

        #[cfg(feature = "candle")]
        let output = {
            // Try to use loaded candle model
            if let Some(model_name) = task.task_type.model_id().map(|m| &m.name) {
                if let Some(model) = self.candle_models.get(model_name.as_str()) {
                    model.forward(&task.input_data)?
                } else {
                    // Model not loaded, fall back to deterministic placeholder
                    compute_deterministic_output(task)
                }
            } else {
                compute_deterministic_output(task)
            }
        };

        #[cfg(not(feature = "candle"))]
        let output = compute_deterministic_output(task);

        let elapsed = start.elapsed().as_millis() as u64;
        let flops = estimate_flops(&task.task_type, elapsed);

        Ok(InferenceResult::new(output, elapsed, flops))
    }

    fn benchmark(&self) -> Result<BenchmarkResult, InferenceError> {
        let start = std::time::Instant::now();

        // FLOPS benchmark: matrix multiplication proxy
        let size = 512;
        let mut _sum = 0.0f64;
        for i in 0..size {
            for j in 0..size {
                _sum += (i as f64 * j as f64).sin();
            }
        }

        let elapsed = start.elapsed();
        let ops = (size * size) as f64;
        let flops = ops / elapsed.as_secs_f64();

        let mut result = BenchmarkResult {
            flops,
            tokens_per_second: 0.0,
            memory_bandwidth_gbps: 0.0,
            backend: BackendType::Cpu,
            benchmark_time_ms: elapsed.as_millis() as u64,
            score: 0,
        };
        result.score = crate::runtime::compute_benchmark_score(&result).0;
        Ok(result)
    }
}

/// Deterministic placeholder output (used by all backends during development)
pub fn deterministic_placeholder(task: &InferenceTask) -> Vec<u8> {
    compute_deterministic_output(task)
}

/// Compute a deterministic output for a given task (placeholder)
///
/// When candle feature is enabled and model is loaded, this is only
/// used as fallback. Otherwise this is the primary output method.
fn compute_deterministic_output(task: &InferenceTask) -> Vec<u8> {
    use blake3::Hasher;

    let mut hasher = Hasher::new();
    hasher.update(&task.task_id.0);
    hasher.update(&task.epoch.to_le_bytes());
    hasher.update(&task.input_data);

    // Generate output of reasonable size
    let hash = hasher.finalize();
    let mut output = Vec::with_capacity(256);
    output.extend_from_slice(hash.as_bytes());
    // Extend with derived data for larger output
    for i in 0u8..7 {
        let mut h = Hasher::new();
        h.update(hash.as_bytes());
        h.update(&[i]);
        output.extend_from_slice(h.finalize().as_bytes());
    }
    output
}

/// Estimate FLOPS for a task based on type and execution time
fn estimate_flops(task_type: &ComputeTaskType, _elapsed_ms: u64) -> u64 {
    match task_type {
        ComputeTaskType::TextGeneration { max_tokens, .. } => {
            2 * 7_000_000_000u64 * (*max_tokens as u64)
        }
        ComputeTaskType::ImageClassification { .. } => 4_000_000_000u64,
        ComputeTaskType::Embedding { .. } => 1_000_000_000u64,
        ComputeTaskType::OnnxInference { .. } => 2_000_000_000u64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::InferenceTask;
    use qfc_types::Hash;

    #[tokio::test]
    async fn test_cpu_engine_basic() {
        let mut engine = CpuEngine::new();
        assert_eq!(engine.backend_type(), BackendType::Cpu);
        assert!(engine.available_memory_mb() > 0 || cfg!(not(target_os = "macos")));

        // With candle feature, load_model tries real download — use placeholder model
        // Without candle, any model name works (no-op)
        let model_id = ModelId::new("test-model", "v1");
        #[cfg(not(feature = "candle"))]
        {
            engine.load_model(&model_id).await.unwrap();
            assert_eq!(engine.supported_models().len(), 1);
        }
        #[cfg(feature = "candle")]
        {
            // Unknown model should fail gracefully
            let result = engine.load_model(&model_id).await;
            assert!(result.is_err());
            assert_eq!(engine.supported_models().len(), 0);
        }
    }

    #[tokio::test]
    async fn test_cpu_deterministic_output() {
        let engine = CpuEngine::new();

        let task = InferenceTask::new(
            Hash::new([0x42; 32]),
            1,
            ComputeTaskType::Embedding {
                model_id: ModelId::new("bert-base", "v1"),
                input_hash: Hash::ZERO,
            },
            vec![1, 2, 3],
            0,
            10000,
        );

        let result1 = engine.run_inference(&task).await.unwrap();
        let result2 = engine.run_inference(&task).await.unwrap();

        // Same task should produce same output hash (deterministic)
        assert_eq!(result1.output_hash, result2.output_hash);
        assert_eq!(result1.output_data, result2.output_data);
    }

    #[test]
    fn test_cpu_benchmark() {
        let engine = CpuEngine::new();
        let result = engine.benchmark().unwrap();
        assert!(result.flops > 0.0);
        assert_eq!(result.backend, BackendType::Cpu);
    }
}
