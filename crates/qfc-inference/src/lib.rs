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
//! - **ROCm**: AMD GPUs via ONNX Runtime ROCm backend (requires `rocm` feature)
//! - **OpenCL**: AMD/Intel GPUs on Linux via ONNX Runtime (requires `opencl` feature)
//!
//! # Feature Flags
//!
//! - `cpu` (default): CPU-only inference
//! - `cuda`: Enable NVIDIA CUDA GPU support
//! - `metal`: Enable Apple Metal GPU support (macOS only)
//! - `onnx`: Enable ONNX Runtime inference
//! - `rocm`: Enable AMD ROCm GPU support (implies `onnx`)
//! - `opencl`: Enable OpenCL GPU support for Linux AMD/Intel GPUs (implies `onnx`)

pub mod backend;
pub mod data_store;
pub mod download;
pub mod gpu_monitor;
pub mod model;
pub mod models;
pub mod proof;
pub mod runtime;
pub mod scheduler;
pub mod task;

pub use data_store::{DataRef, LocalDataStore, TaskData, MAX_INLINE_SIZE};
pub use gpu_monitor::{collect_gpu_metrics, GpuMetrics};
pub use model::CanonicalFormat;
pub use proof::{ComputeProof, InferenceProof, InferenceResult};
pub use runtime::{
    compute_benchmark_score, validate_gpu_claim, BackendType, BenchmarkResult, GpuTier,
    HardwareInfo,
};
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

        // Metal: prefer ONNX+CoreML (full op coverage), fallback to candle Metal
        #[cfg(feature = "coreml")]
        BackendType::Metal => {
            tracing::info!("Using ONNX Runtime + CoreML for Metal acceleration");
            let engine = backend::onnx::OnnxEngine::new_coreml()?;
            Ok(Box::new(engine))
        }
        #[cfg(all(feature = "metal", not(feature = "coreml")))]
        BackendType::Metal => {
            let engine = backend::metal::MetalEngine::new()?;
            Ok(Box::new(engine))
        }
        #[cfg(not(any(feature = "metal", feature = "coreml")))]
        BackendType::Metal => Err(InferenceError::BackendUnavailable(
            "Metal (not compiled with metal or coreml feature)".to_string(),
        )),

        #[cfg(feature = "onnx")]
        BackendType::Rocm => {
            let engine = backend::onnx::OnnxEngine::new_rocm()?;
            Ok(Box::new(engine))
        }
        #[cfg(not(feature = "onnx"))]
        BackendType::Rocm => Err(InferenceError::BackendUnavailable(
            "ROCm (not compiled with rocm feature)".to_string(),
        )),

        #[cfg(feature = "opencl")]
        BackendType::OpenCl => {
            let engine = backend::opencl::OpenClEngine::new()?;
            Ok(Box::new(engine))
        }
        #[cfg(not(feature = "opencl"))]
        BackendType::OpenCl => Err(InferenceError::BackendUnavailable(
            "OpenCL (not compiled with opencl feature)".to_string(),
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
            BackendType::Cpu
                | BackendType::Metal
                | BackendType::Cuda
                | BackendType::Rocm
                | BackendType::OpenCl
        ));
    }

    #[test]
    fn test_create_cpu_engine() {
        let engine = create_engine_for_backend(BackendType::Cpu).unwrap();
        assert_eq!(engine.backend_type(), BackendType::Cpu);
    }

    #[tokio::test]
    async fn test_engine_full_workflow() {
        let engine = backend::cpu::CpuEngine::new();

        // Run inference with a model that isn't loaded — should fail
        let model_id = ModelId::new("test-model", "v1");
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

        let result = engine.run_inference(&task).await;
        assert!(
            result.is_err(),
            "Should reject inference when model not loaded"
        );

        // Benchmark should always work
        let bench = engine.benchmark().unwrap();
        assert!(bench.flops > 0.0);
    }
}
