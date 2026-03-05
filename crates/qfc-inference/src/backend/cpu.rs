//! CPU-only inference backend

use async_trait::async_trait;

use crate::task::{ComputeTaskType, InferenceTask, ModelId};
use crate::proof::InferenceResult;
use crate::runtime::{BackendType, BenchmarkResult};
use crate::{InferenceEngine, InferenceError};

/// CPU-based inference engine
pub struct CpuEngine {
    /// Loaded model IDs
    loaded_models: Vec<ModelId>,
    /// Available system memory in MB
    available_memory_mb: u64,
}

impl CpuEngine {
    pub fn new() -> Self {
        let mem = crate::runtime::detect_hardware().memory_mb;
        Self {
            loaded_models: Vec::new(),
            available_memory_mb: mem,
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
        // Placeholder: In production, this would download and load model weights
        // using candle-core with CPU backend
        tracing::info!("Loading model {} on CPU backend", model_id);
        self.loaded_models.push(model_id.clone());
        Ok(())
    }

    async fn run_inference(&self, task: &InferenceTask) -> Result<InferenceResult, InferenceError> {
        let start = std::time::Instant::now();

        // Placeholder: In production, this runs actual inference via candle-core
        // For now, produce a deterministic output based on task input
        let output = compute_deterministic_output(task);

        let elapsed = start.elapsed().as_millis() as u64;
        let flops = estimate_flops(&task.task_type, elapsed);

        Ok(InferenceResult::new(output, elapsed, flops))
    }

    fn benchmark(&self) -> Result<BenchmarkResult, InferenceError> {
        let start = std::time::Instant::now();

        // Simple FLOPS benchmark: matrix multiplication proxy
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

        Ok(BenchmarkResult {
            flops,
            tokens_per_second: 0.0, // No LLM benchmark on CPU placeholder
            memory_bandwidth_gbps: 0.0,
            backend: BackendType::Cpu,
            benchmark_time_ms: elapsed.as_millis() as u64,
        })
    }
}

/// Deterministic placeholder output (used by all backends during development)
pub fn deterministic_placeholder(task: &InferenceTask) -> Vec<u8> {
    compute_deterministic_output(task)
}

/// Compute a deterministic output for a given task (placeholder)
///
/// In production, this calls candle-core to run actual model inference.
/// For now, we hash the input to produce a reproducible output.
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
fn estimate_flops(task_type: &ComputeTaskType, elapsed_ms: u64) -> u64 {
    if elapsed_ms == 0 {
        return 0;
    }

    // Rough FLOPS estimates per task type
    let ops = match task_type {
        ComputeTaskType::TextGeneration { max_tokens, .. } => {
            // ~2 * params * tokens for transformer inference
            // Assume 7B params for default
            2 * 7_000_000_000u64 * (*max_tokens as u64)
        }
        ComputeTaskType::ImageClassification { .. } => {
            // ~4 GFLOPS for a typical classification model
            4_000_000_000u64
        }
        ComputeTaskType::Embedding { .. } => {
            // ~1 GFLOPS for embedding generation
            1_000_000_000u64
        }
        ComputeTaskType::OnnxInference { .. } => {
            // Generic estimate
            2_000_000_000u64
        }
    };

    ops
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

        let model_id = ModelId::new("test-model", "v1");
        engine.load_model(&model_id).await.unwrap();
        assert_eq!(engine.supported_models().len(), 1);
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
