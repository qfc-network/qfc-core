//! QFC Inference Engine
//!
//! Multi-platform AI inference runtime that abstracts CUDA / Metal / CPU
//! behind a unified trait. This crate provides the foundation for
//! QFC v2.0's useful compute contribution (replacing Blake3 PoW).
//!
//! # Backends
//!
//! - **CPU**: Always available, uses candle-core CPU backend
//! - **CUDA**: NVIDIA GPUs via candle-core CUDA backend (requires `cuda` feature)
//! - **Metal**: Apple Silicon via candle-core Metal backend (requires `metal` feature)
//!
//! # Feature Flags
//!
//! - `cpu` (default): CPU-only inference
//! - `cuda`: Enable NVIDIA CUDA GPU support
//! - `metal`: Enable Apple Metal GPU support (macOS only)

pub mod backend;
pub mod model;
pub mod proof;
pub mod runtime;
pub mod task;

pub use proof::{ComputeProof, InferenceProof, InferenceResult};
pub use runtime::{BackendType, BenchmarkResult, GpuTier, HardwareInfo};
pub use task::{ComputeTaskType, InferenceTask, ModelId};

use async_trait::async_trait;
use thiserror::Error;

/// Errors from inference operations
#[derive(Debug, Error)]
pub enum InferenceError {
    #[error("Backend not available: {0}")]
    BackendUnavailable(String),

    #[error("Model not found: {0}")]
    ModelNotFound(String),

    #[error("Model not loaded: {0}")]
    ModelNotLoaded(String),

    #[error("Insufficient memory: need {required_mb}MB, have {available_mb}MB")]
    InsufficientMemory { required_mb: u64, available_mb: u64 },

    #[error("Inference execution failed: {0}")]
    ExecutionFailed(String),

    #[error("Task expired: deadline {deadline}, current time {current_time}")]
    TaskExpired { deadline: u64, current_time: u64 },

    #[error("Unsupported task type: {0}")]
    UnsupportedTaskType(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Core inference engine trait — implemented by each backend
#[async_trait]
pub trait InferenceEngine: Send + Sync {
    /// Get the backend type
    fn backend_type(&self) -> BackendType;

    /// Get list of currently loaded/supported models
    fn supported_models(&self) -> Vec<ModelId>;

    /// Get available GPU/system memory in MB
    fn available_memory_mb(&self) -> u64;

    /// Load a model into memory
    async fn load_model(&mut self, model_id: &ModelId) -> Result<(), InferenceError>;

    /// Run inference on a task
    async fn run_inference(&self, task: &InferenceTask) -> Result<InferenceResult, InferenceError>;

    /// Run a hardware benchmark and return FLOPS measurement
    fn benchmark(&self) -> Result<BenchmarkResult, InferenceError>;
}

/// Create the best available inference engine for this system
pub fn create_engine() -> Result<Box<dyn InferenceEngine>, InferenceError> {
    let backend = runtime::detect_backend();
    create_engine_for_backend(backend)
}

/// Create an inference engine for a specific backend
pub fn create_engine_for_backend(
    backend: BackendType,
) -> Result<Box<dyn InferenceEngine>, InferenceError> {
    match backend {
        #[cfg(feature = "cuda")]
        BackendType::Cuda => {
            let engine = backend::cuda::CudaEngine::new()?;
            Ok(Box::new(engine))
        }
        #[cfg(not(feature = "cuda"))]
        BackendType::Cuda => Err(InferenceError::BackendUnavailable(
            "CUDA (not compiled with cuda feature)".to_string(),
        )),

        #[cfg(feature = "metal")]
        BackendType::Metal => {
            let engine = backend::metal::MetalEngine::new()?;
            Ok(Box::new(engine))
        }
        #[cfg(not(feature = "metal"))]
        BackendType::Metal => Err(InferenceError::BackendUnavailable(
            "Metal (not compiled with metal feature)".to_string(),
        )),

        BackendType::Cpu => {
            let engine = backend::cpu::CpuEngine::new();
            Ok(Box::new(engine))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_engine_default() {
        // Should always succeed with at least CPU backend
        let engine = create_engine().unwrap();
        assert!(matches!(
            engine.backend_type(),
            BackendType::Cpu | BackendType::Metal | BackendType::Cuda
        ));
    }

    #[test]
    fn test_create_cpu_engine() {
        let engine = create_engine_for_backend(BackendType::Cpu).unwrap();
        assert_eq!(engine.backend_type(), BackendType::Cpu);
    }

    #[tokio::test]
    async fn test_engine_full_workflow() {
        let mut engine = backend::cpu::CpuEngine::new();

        // Load a model
        let model_id = ModelId::new("test-model", "v1");
        engine.load_model(&model_id).await.unwrap();

        // Run inference
        let task = InferenceTask::new(
            qfc_types::Hash::new([0x42; 32]),
            1,
            ComputeTaskType::Embedding {
                model_id: model_id.clone(),
                input_hash: qfc_types::Hash::ZERO,
            },
            vec![1, 2, 3],
            0,
            10000,
        );

        let result = engine.run_inference(&task).await.unwrap();
        assert!(!result.output_data.is_empty());
        assert_ne!(result.output_hash, qfc_types::Hash::ZERO);

        // Benchmark
        let bench = engine.benchmark().unwrap();
        assert!(bench.flops > 0.0);
    }
}
