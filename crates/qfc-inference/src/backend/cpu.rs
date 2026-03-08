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
    /// Loaded candle models for embedding/classification (when candle feature enabled)
    #[cfg(feature = "candle")]
    candle_models: HashMap<String, Box<dyn LoadedModel>>,
    /// Loaded Qwen2 models for text generation (needs &mut self for KV cache)
    #[cfg(feature = "candle")]
    qwen_models: HashMap<String, std::sync::Mutex<crate::models::qwen2::Qwen2TextGen>>,
    /// Loaded quantized Qwen2 models (GGUF format)
    #[cfg(feature = "candle")]
    quantized_models:
        HashMap<String, std::sync::Mutex<crate::models::quantized_qwen2::QuantizedQwen2TextGen>>,
    /// Loaded Whisper models for speech-to-text
    #[cfg(feature = "candle")]
    whisper_models: HashMap<String, std::sync::Mutex<crate::models::whisper::WhisperModel>>,
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
            #[cfg(feature = "candle")]
            qwen_models: HashMap::new(),
            #[cfg(feature = "candle")]
            quantized_models: HashMap::new(),
            #[cfg(feature = "candle")]
            whisper_models: HashMap::new(),
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
            use crate::download::{download_model, get_hf_repo, ModelFormat};
            use candle_core::Device;

            tracing::info!("Loading model {} on CPU backend (candle)", model_id);

            let downloaded = download_model(&model_id.name)?;
            let device = Device::Cpu;

            // Determine model format from registry
            let format = get_hf_repo(&model_id.name)
                .map(|r| r.format)
                .unwrap_or(ModelFormat::Safetensors);

            if is_whisper_model(&model_id.name) {
                // Load as Whisper speech-to-text model
                let mel_path = downloaded
                    .extra_paths
                    .iter()
                    .find(|(name, _)| name.contains("mel_filters"))
                    .map(|(_, p)| p.clone())
                    .ok_or_else(|| {
                        InferenceError::ExecutionFailed(
                            "mel_filters file not found in download".to_string(),
                        )
                    })?;
                let whisper = crate::models::whisper::WhisperModel::load(
                    &downloaded.weights_path,
                    &downloaded.tokenizer_path,
                    &downloaded.config_path,
                    &mel_path,
                    &device,
                )?;
                self.whisper_models
                    .insert(model_id.name.clone(), std::sync::Mutex::new(whisper));
            } else if is_text_gen_model(&model_id.name) {
                match format {
                    ModelFormat::Gguf => {
                        // Load as quantized GGUF model
                        let qmodel = crate::models::quantized_qwen2::QuantizedQwen2TextGen::load(
                            &downloaded.weights_path,
                            &downloaded.tokenizer_path,
                            &device,
                        )?;
                        self.quantized_models
                            .insert(model_id.name.clone(), std::sync::Mutex::new(qmodel));
                    }
                    ModelFormat::Safetensors => {
                        // Load as Qwen2 FP32 text generation model
                        let qwen = crate::models::qwen2::Qwen2TextGen::load(
                            &downloaded.weights_path,
                            &downloaded.tokenizer_path,
                            &downloaded.config_path,
                            &device,
                        )?;
                        self.qwen_models
                            .insert(model_id.name.clone(), std::sync::Mutex::new(qwen));
                    }
                }
            } else {
                // Load as BERT embedding/classification model
                let loaded: Box<dyn LoadedModel> =
                    Box::new(crate::models::bert::BertEmbedding::load(
                        &downloaded.weights_path,
                        &downloaded.tokenizer_path,
                        &downloaded.config_path,
                        &device,
                    )?);
                self.candle_models.insert(model_id.name.clone(), loaded);
            }

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
            match &task.task_type {
                ComputeTaskType::TextGeneration {
                    model_id,
                    max_tokens,
                    ..
                } => {
                    let prompt = std::str::from_utf8(&task.input_data).map_err(|e| {
                        InferenceError::ExecutionFailed(format!("Invalid UTF-8 input: {}", e))
                    })?;

                    // Try quantized (GGUF) models first, then FP32 models
                    if let Some(model_mutex) = self.quantized_models.get(model_id.name.as_str()) {
                        let mut model = model_mutex.lock().map_err(|e| {
                            InferenceError::ExecutionFailed(format!("Model lock failed: {}", e))
                        })?;
                        model.generate(prompt, *max_tokens)?
                    } else if let Some(model_mutex) = self.qwen_models.get(model_id.name.as_str()) {
                        let mut model = model_mutex.lock().map_err(|e| {
                            InferenceError::ExecutionFailed(format!("Model lock failed: {}", e))
                        })?;
                        model.generate(prompt, *max_tokens)?
                    } else {
                        return Err(InferenceError::ModelNotLoaded(model_id.name.clone()));
                    }
                }
                ComputeTaskType::SpeechToText {
                    model_id, language, ..
                } => {
                    // Use Whisper speech-to-text model
                    if let Some(model_mutex) = self.whisper_models.get(model_id.name.as_str()) {
                        let mut model = model_mutex.lock().map_err(|e| {
                            InferenceError::ExecutionFailed(format!("Model lock failed: {}", e))
                        })?;
                        // Input data is raw PCM f32 samples (4 bytes per sample, little-endian)
                        let pcm_samples: Vec<f32> = task
                            .input_data
                            .chunks_exact(4)
                            .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
                            .collect();
                        let lang = if language.is_empty() {
                            None
                        } else {
                            Some(language.as_str())
                        };
                        model.transcribe(&pcm_samples, lang)?
                    } else {
                        return Err(InferenceError::ModelNotLoaded(model_id.name.clone()));
                    }
                }
                _ => {
                    // Embedding / classification — use LoadedModel trait
                    if let Some(model_name) = task.task_type.model_id().map(|m| &m.name) {
                        if let Some(model) = self.candle_models.get(model_name.as_str()) {
                            model.forward(&task.input_data)?
                        } else {
                            return Err(InferenceError::ModelNotLoaded(model_name.clone()));
                        }
                    } else {
                        compute_deterministic_output(task)
                    }
                }
            }
        };

        #[cfg(not(feature = "candle"))]
        let output = {
            if let Some(model_id) = task.task_type.model_id() {
                if !self.loaded_models.iter().any(|m| m.name == model_id.name) {
                    return Err(InferenceError::ModelNotLoaded(model_id.name.clone()));
                }
            }
            compute_deterministic_output(task)
        };

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

/// Check if a model name refers to a text generation (LLM) model
#[cfg(feature = "candle")]
fn is_text_gen_model(model_name: &str) -> bool {
    model_name.starts_with("qfc-llm-")
}

/// Check if a model name refers to a Whisper speech-to-text model
#[cfg(feature = "candle")]
fn is_whisper_model(model_name: &str) -> bool {
    model_name.starts_with("qfc-whisper-")
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
        ComputeTaskType::TextGeneration {
            model_id,
            max_tokens,
            ..
        } => {
            // Approximate FLOPS: 2 * params * tokens
            let params = if model_id.name.contains("0.5b") || model_id.name.contains("0.5B") {
                500_000_000u64
            } else if model_id.name.contains("1b") || model_id.name.contains("1B") {
                1_000_000_000u64
            } else if model_id.name.contains("3b") || model_id.name.contains("3B") {
                3_000_000_000u64
            } else if model_id.name.contains("7b") || model_id.name.contains("7B") {
                7_000_000_000u64
            } else {
                7_000_000_000u64
            };
            2 * params * (*max_tokens as u64)
        }
        ComputeTaskType::ImageClassification { .. } => 4_000_000_000u64,
        ComputeTaskType::Embedding { .. } => 1_000_000_000u64,
        ComputeTaskType::SpeechToText { model_id, .. } => {
            // Whisper base: ~74M params, large: ~1.5B params
            if model_id.name.contains("large") {
                30_000_000_000u64
            } else {
                4_000_000_000u64
            }
        }
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
    async fn test_cpu_rejects_unloaded_model() {
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

        // Should fail with ModelNotLoaded when model isn't loaded
        let result = engine.run_inference(&task).await;
        assert!(result.is_err());
        assert!(
            format!("{}", result.unwrap_err()).contains("not loaded"),
            "Expected ModelNotLoaded error"
        );
    }

    #[test]
    fn test_cpu_benchmark() {
        let engine = CpuEngine::new();
        let result = engine.benchmark().unwrap();
        assert!(result.flops > 0.0);
        assert_eq!(result.backend, BackendType::Cpu);
    }
}
