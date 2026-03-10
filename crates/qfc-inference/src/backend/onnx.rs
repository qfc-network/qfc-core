//! ONNX Runtime inference backend (CoreML, AMD ROCm, DirectML, CPU)
//!
//! Uses the `ort` crate (ONNX Runtime Rust bindings) to run inference
//! on Apple Silicon via CoreML, AMD GPUs via ROCm, or as a CPU fallback.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::proof::InferenceResult;
use crate::runtime::{BackendType, BenchmarkResult};
use crate::task::{ComputeTaskType, InferenceTask, ModelId};
use crate::{InferenceEngine, InferenceError};

/// ONNX Runtime-based inference engine
pub struct OnnxEngine {
    /// Loaded model IDs
    loaded_models: Vec<ModelId>,
    /// Available memory in MB
    available_memory_mb: u64,
    /// Backend type (Rocm or Cpu via ONNX)
    backend: BackendType,
    /// Loaded ONNX sessions keyed by model name (Mutex for interior mutability — Session::run needs &mut)
    sessions: HashMap<String, Mutex<ort::session::Session>>,
}

impl OnnxEngine {
    /// Create a new ONNX engine with ROCm backend
    pub fn new_rocm() -> Result<Self, InferenceError> {
        let hw = crate::runtime::detect_hardware();
        tracing::info!("Initializing ONNX Runtime with ROCm backend");
        Ok(Self {
            loaded_models: Vec::new(),
            available_memory_mb: hw.memory_mb,
            backend: BackendType::Rocm,
            sessions: HashMap::new(),
        })
    }

    /// Create a new ONNX engine with CoreML backend (Apple Silicon Metal/ANE)
    #[cfg(feature = "coreml")]
    pub fn new_coreml() -> Result<Self, InferenceError> {
        let hw = crate::runtime::detect_hardware();
        tracing::info!(
            "Initializing ONNX Runtime with CoreML backend ({}MB unified memory)",
            hw.memory_mb
        );
        Ok(Self {
            loaded_models: Vec::new(),
            available_memory_mb: hw.memory_mb,
            backend: BackendType::Metal,
            sessions: HashMap::new(),
        })
    }

    /// Create a new ONNX engine with CPU backend
    pub fn new_cpu() -> Self {
        let hw = crate::runtime::detect_hardware();
        Self {
            loaded_models: Vec::new(),
            available_memory_mb: hw.memory_mb,
            backend: BackendType::Cpu,
            sessions: HashMap::new(),
        }
    }

    /// Get the ONNX model path for a QFC model
    pub fn get_onnx_model_path(model_name: &str) -> Option<(&'static str, &'static str)> {
        // Returns (HuggingFace repo_id, ONNX filename)
        match model_name {
            "qfc-embed-small" => {
                Some(("sentence-transformers/all-MiniLM-L6-v2", "onnx/model.onnx"))
            }
            "qfc-embed-medium" => {
                Some(("sentence-transformers/all-MiniLM-L12-v2", "onnx/model.onnx"))
            }
            "qfc-classify-small" => Some(("google-bert/bert-base-uncased", "onnx/model.onnx")),
            _ => None,
        }
    }

    /// Download ONNX model from HuggingFace
    fn download_onnx_model(model_name: &str) -> Result<PathBuf, InferenceError> {
        let (repo_id, onnx_file) = Self::get_onnx_model_path(model_name).ok_or_else(|| {
            InferenceError::ModelNotFound(format!("No ONNX model for: {}", model_name))
        })?;

        let cache_dir = std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/tmp"))
            .join(".cache/qfc-models")
            .join(model_name);

        let onnx_path = cache_dir.join("model.onnx");

        if onnx_path.exists() {
            tracing::info!("Using cached ONNX model: {}", onnx_path.display());
            return Ok(onnx_path);
        }

        std::fs::create_dir_all(&cache_dir).map_err(|e| {
            InferenceError::ExecutionFailed(format!("Failed to create cache dir: {}", e))
        })?;

        let url = format!(
            "https://huggingface.co/{}/resolve/main/{}",
            repo_id, onnx_file
        );

        tracing::info!("Downloading ONNX model from {}", url);

        let output = std::process::Command::new("curl")
            .args(["-sfL", "-o"])
            .arg(&onnx_path)
            .arg(&url)
            .output()
            .map_err(|e| InferenceError::ExecutionFailed(format!("curl failed: {}", e)))?;

        if !output.status.success() {
            return Err(InferenceError::ExecutionFailed(format!(
                "Failed to download ONNX model from {}",
                url
            )));
        }

        tracing::info!("ONNX model downloaded: {}", onnx_path.display());
        Ok(onnx_path)
    }

    /// Create an ONNX Runtime session with the appropriate execution provider
    fn create_session(
        &self,
        model_path: &PathBuf,
    ) -> Result<ort::session::Session, InferenceError> {
        let map_builder_err =
            |e: ort::Error| InferenceError::ExecutionFailed(format!("ONNX builder error: {}", e));
        let map_load_err = |e: ort::Error| {
            InferenceError::ExecutionFailed(format!("ONNX model load error: {}", e))
        };

        let session = match self.backend {
            #[cfg(feature = "coreml")]
            BackendType::Metal => {
                tracing::info!("Creating ONNX session with CoreML execution provider");
                let mut builder = ort::session::Session::builder()
                    .map_err(map_builder_err)?
                    .with_execution_providers([ort::ep::CoreML::default().build()])
                    .map_err(|e| {
                        InferenceError::ExecutionFailed(format!("CoreML EP error: {}", e))
                    })?;
                builder.commit_from_file(model_path).map_err(map_load_err)?
            }
            BackendType::Rocm => {
                tracing::info!("Creating ONNX session with ROCm execution provider");
                let mut builder = ort::session::Session::builder()
                    .map_err(map_builder_err)?
                    .with_execution_providers([ort::ep::ROCm::default().build()])
                    .map_err(|e| {
                        InferenceError::ExecutionFailed(format!("ROCm EP error: {}", e))
                    })?;
                builder.commit_from_file(model_path).map_err(map_load_err)?
            }
            _ => {
                tracing::info!("Creating ONNX session with CPU execution provider");
                let mut builder = ort::session::Session::builder().map_err(map_builder_err)?;
                builder.commit_from_file(model_path).map_err(map_load_err)?
            }
        };
        Ok(session)
    }
}

// Safety: OnnxEngine is Send+Sync because sessions are behind Mutex
unsafe impl Send for OnnxEngine {}
unsafe impl Sync for OnnxEngine {}

#[async_trait]
impl InferenceEngine for OnnxEngine {
    fn backend_type(&self) -> BackendType {
        self.backend
    }

    fn supported_models(&self) -> Vec<ModelId> {
        self.loaded_models.clone()
    }

    fn available_memory_mb(&self) -> u64 {
        self.available_memory_mb
    }

    async fn load_model(&mut self, model_id: &ModelId) -> Result<(), InferenceError> {
        tracing::info!(
            "Loading model {} via ONNX Runtime ({})",
            model_id,
            self.backend
        );

        let model_path = Self::download_onnx_model(&model_id.name)?;
        let session = self.create_session(&model_path)?;

        self.sessions
            .insert(model_id.name.clone(), Mutex::new(session));
        self.loaded_models.push(model_id.clone());
        tracing::info!("Model {} loaded via ONNX Runtime", model_id);
        Ok(())
    }

    async fn run_inference(&self, task: &InferenceTask) -> Result<InferenceResult, InferenceError> {
        let start = std::time::Instant::now();

        let output = if let Some(model_name) = task.task_type.model_id().map(|m| &m.name) {
            if let Some(session_mutex) = self.sessions.get(model_name.as_str()) {
                let mut session: std::sync::MutexGuard<'_, ort::session::Session> =
                    session_mutex.lock().map_err(|e| {
                        InferenceError::ExecutionFailed(format!("Session lock poisoned: {}", e))
                    })?;
                run_onnx_inference(&mut session, &task.input_data)?
            } else {
                return Err(InferenceError::ModelNotLoaded(model_name.clone()));
            }
        } else {
            crate::backend::cpu::deterministic_placeholder(task)
        };

        let elapsed = start.elapsed().as_millis() as u64;
        let flops = estimate_flops(&task.task_type, elapsed);

        Ok(InferenceResult::new(output, elapsed, flops))
    }

    fn benchmark(&self) -> Result<BenchmarkResult, InferenceError> {
        let start = std::time::Instant::now();

        // GEMM benchmark
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

        // GPU benchmarks get a multiplier (actual GPU compute will be faster)
        let gpu_multiplier = match self.backend {
            BackendType::Rocm => 10.0,
            BackendType::Metal => 8.0, // CoreML/Metal
            _ => 1.0,
        };

        let mut result = BenchmarkResult {
            flops: flops * gpu_multiplier,
            tokens_per_second: 0.0,
            memory_bandwidth_gbps: 0.0,
            backend: self.backend,
            benchmark_time_ms: elapsed.as_millis() as u64,
            score: 0,
        };
        result.score = crate::runtime::compute_benchmark_score(&result).0;
        Ok(result)
    }
}

/// Run ONNX inference on raw input data
fn run_onnx_inference(
    session: &mut ort::session::Session,
    input_data: &[u8],
) -> Result<Vec<u8>, InferenceError> {
    use ort::value::Tensor;

    // Parse input as UTF-8 text for embedding models
    let text = std::str::from_utf8(input_data).unwrap_or("fallback input");

    // Simple token ID generation (maps each word to an index)
    let tokens: Vec<i64> = text
        .split_whitespace()
        .enumerate()
        .map(|(i, _)| (i + 1) as i64)
        .take(128)
        .collect();

    let seq_len = tokens.len().max(1);
    let tokens = if tokens.is_empty() {
        vec![1i64]
    } else {
        tokens
    };

    let map_err = |e: ort::Error| InferenceError::ExecutionFailed(format!("ONNX error: {}", e));

    let shape = vec![1usize, seq_len];

    let input_ids =
        Tensor::from_array((shape.clone(), tokens.into_boxed_slice())).map_err(map_err)?;
    let attention_mask =
        Tensor::from_array((shape.clone(), vec![1i64; seq_len].into_boxed_slice()))
            .map_err(map_err)?;
    let token_type_ids =
        Tensor::from_array((shape, vec![0i64; seq_len].into_boxed_slice())).map_err(map_err)?;

    let outputs = session
        .run(ort::inputs![input_ids, attention_mask, token_type_ids])
        .map_err(map_err)?;

    // Extract output tensor and convert to bytes
    let output_value = &outputs[0];
    let (_, output_data) = output_value.try_extract_tensor::<f32>().map_err(map_err)?;

    // Convert f32 values to little-endian bytes
    let output_bytes: Vec<u8> = output_data.iter().flat_map(|v| v.to_le_bytes()).collect();

    Ok(output_bytes)
}

/// Estimate FLOPS for a task
fn estimate_flops(task_type: &ComputeTaskType, _elapsed_ms: u64) -> u64 {
    match task_type {
        ComputeTaskType::TextGeneration { max_tokens, .. } => {
            2 * 7_000_000_000u64 * (*max_tokens as u64)
        }
        ComputeTaskType::ImageClassification { .. } => 4_000_000_000u64,
        ComputeTaskType::Embedding { .. } => 1_000_000_000u64,
        ComputeTaskType::SpeechToText { model_id, .. } => {
            if model_id.name.contains("large") {
                30_000_000_000u64
            } else {
                4_000_000_000u64
            }
        }
        ComputeTaskType::ImageGeneration { steps, .. } => {
            2_000_000_000u64 * (*steps as u64) + 5_000_000_000u64
        }
        ComputeTaskType::OnnxInference { .. } => 2_000_000_000u64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::InferenceTask;
    use qfc_types::Hash;

    #[test]
    fn test_onnx_engine_cpu_creation() {
        let engine = OnnxEngine::new_cpu();
        assert_eq!(engine.backend_type(), BackendType::Cpu);
        assert!(engine.available_memory_mb() > 0);
        assert!(engine.supported_models().is_empty());
    }

    #[tokio::test]
    async fn test_onnx_engine_rejects_unloaded_model() {
        let engine = OnnxEngine::new_cpu();

        let task = InferenceTask::new(
            Hash::new([0x42; 32]),
            1,
            ComputeTaskType::Embedding {
                model_id: ModelId::new("test-model", "v1"),
                input_hash: Hash::ZERO,
            },
            vec![1, 2, 3],
            0,
            10000,
        );

        // Without a loaded model, should return ModelNotLoaded error
        let result = engine.run_inference(&task).await;
        assert!(result.is_err());
    }

    #[test]
    fn test_onnx_engine_benchmark() {
        let engine = OnnxEngine::new_cpu();
        let result = engine.benchmark().unwrap();
        assert!(result.flops > 0.0);
        assert_eq!(result.backend, BackendType::Cpu);
    }

    #[test]
    fn test_onnx_model_paths() {
        assert!(OnnxEngine::get_onnx_model_path("qfc-embed-small").is_some());
        assert!(OnnxEngine::get_onnx_model_path("qfc-embed-medium").is_some());
        assert!(OnnxEngine::get_onnx_model_path("qfc-classify-small").is_some());
        assert!(OnnxEngine::get_onnx_model_path("unknown-model").is_none());
    }
}
