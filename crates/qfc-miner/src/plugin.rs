//! Inference plugin system — external runtime wrappers
//!
//! Wraps llama.cpp, whisper.cpp, and stable-diffusion.cpp CLI tools
//! as inference backends. These run as child processes, enabling native
//! C++ performance without Rust FFI bindings.
//!
//! # Plugin Detection
//!
//! Plugins are auto-detected from `$PATH` or configured model directories:
//! - `llama-cli` / `llama-server` — llama.cpp LLM inference
//! - `whisper-cli` / `main` (whisper.cpp) — speech-to-text
//! - `sd` (stable-diffusion.cpp) — image generation

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::process::Command;
use tracing::{debug, info, warn};

/// Plugin errors
#[derive(Debug, Error)]
pub enum PluginError {
    #[error("Plugin not found: {0}")]
    NotFound(String),

    #[error("Plugin execution failed: {0}")]
    ExecutionFailed(String),

    #[error("Plugin output invalid: {0}")]
    InvalidOutput(String),

    #[error("Model file not found: {0}")]
    ModelNotFound(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Plugin timed out after {0}s")]
    Timeout(u64),
}

/// Supported external runtime types
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PluginRuntime {
    /// llama.cpp — LLM inference (Llama, DeepSeek, Mistral, Qwen)
    LlamaCpp,
    /// whisper.cpp — speech-to-text
    WhisperCpp,
    /// stable-diffusion.cpp — image generation
    StableDiffusionCpp,
}

impl std::fmt::Display for PluginRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PluginRuntime::LlamaCpp => write!(f, "llama.cpp"),
            PluginRuntime::WhisperCpp => write!(f, "whisper.cpp"),
            PluginRuntime::StableDiffusionCpp => write!(f, "stable-diffusion.cpp"),
        }
    }
}

/// Configuration for inference plugins
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PluginConfig {
    /// Enable external plugin runtimes
    pub enabled: bool,
    /// Miner tier (1=basic, 2=mid, 3=high)
    pub tier: u8,
    /// List of model names this miner supports
    pub models: Vec<String>,
    /// Directory for model files (GGUF, bin, etc.)
    pub model_dir: PathBuf,
    /// Custom binary paths (overrides auto-detection)
    pub binary_paths: HashMap<String, PathBuf>,
    /// Execution timeout in seconds per task
    pub timeout_secs: u64,
    /// Number of GPU layers to offload (llama.cpp -ngl)
    pub gpu_layers: u32,
    /// Number of threads for CPU computation
    pub threads: u32,
}

impl Default for PluginConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            tier: 1,
            models: vec![],
            model_dir: dirs_model_default(),
            binary_paths: HashMap::new(),
            timeout_secs: 300,
            gpu_layers: 999, // offload all layers by default
            threads: num_cpus_default(),
        }
    }
}

fn dirs_model_default() -> PathBuf {
    std::env::var("HOME")
        .map(|h| PathBuf::from(h).join(".qfc-miner").join("models"))
        .unwrap_or_else(|_| PathBuf::from("./models"))
}

fn num_cpus_default() -> u32 {
    std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(4)
}

/// A detected plugin with its binary path and capabilities
#[derive(Clone, Debug)]
pub struct DetectedPlugin {
    /// Runtime type
    pub runtime: PluginRuntime,
    /// Path to the CLI binary
    pub binary_path: PathBuf,
    /// Version string (if detected)
    pub version: Option<String>,
    /// Whether GPU acceleration is available
    pub gpu_supported: bool,
}

/// Plugin registry — manages external inference runtimes
pub struct PluginRegistry {
    config: PluginConfig,
    plugins: HashMap<PluginRuntime, DetectedPlugin>,
    /// Model name → GGUF/bin file path mapping
    model_files: HashMap<String, PathBuf>,
}

impl PluginRegistry {
    /// Create a new registry and auto-detect available plugins
    pub fn new(config: PluginConfig) -> Self {
        let mut registry = Self {
            config,
            plugins: HashMap::new(),
            model_files: HashMap::new(),
        };
        registry.detect_plugins();
        registry.scan_model_files();
        registry
    }

    /// Detect available plugins from PATH and configured binary paths
    fn detect_plugins(&mut self) {
        // llama.cpp detection
        if let Some(path) = self.find_binary("llama-cli", &["llama-cli", "llama", "main"]) {
            let version = Self::probe_version(&path, &["--version"]);
            let gpu = Self::check_gpu_support_llama(&path);
            info!(
                "Detected llama.cpp: {} (version: {}, GPU: {})",
                path.display(),
                version.as_deref().unwrap_or("unknown"),
                gpu
            );
            self.plugins.insert(
                PluginRuntime::LlamaCpp,
                DetectedPlugin {
                    runtime: PluginRuntime::LlamaCpp,
                    binary_path: path,
                    version,
                    gpu_supported: gpu,
                },
            );
        }

        // whisper.cpp detection
        if let Some(path) = self.find_binary("whisper-cli", &["whisper-cli", "whisper", "main"]) {
            let version = Self::probe_version(&path, &["--version"]);
            info!(
                "Detected whisper.cpp: {} (version: {})",
                path.display(),
                version.as_deref().unwrap_or("unknown")
            );
            self.plugins.insert(
                PluginRuntime::WhisperCpp,
                DetectedPlugin {
                    runtime: PluginRuntime::WhisperCpp,
                    binary_path: path,
                    version,
                    gpu_supported: false,
                },
            );
        }

        // stable-diffusion.cpp detection
        if let Some(path) = self.find_binary("sd", &["sd", "stable-diffusion"]) {
            let version = Self::probe_version(&path, &["--version"]);
            info!(
                "Detected stable-diffusion.cpp: {} (version: {})",
                path.display(),
                version.as_deref().unwrap_or("unknown")
            );
            self.plugins.insert(
                PluginRuntime::StableDiffusionCpp,
                DetectedPlugin {
                    runtime: PluginRuntime::StableDiffusionCpp,
                    binary_path: path,
                    version,
                    gpu_supported: false,
                },
            );
        }

        if self.plugins.is_empty() {
            warn!("No inference plugins detected. Install llama.cpp, whisper.cpp, or stable-diffusion.cpp for external runtime support.");
        }
    }

    /// Find a binary by checking configured paths first, then PATH
    fn find_binary(&self, config_key: &str, candidates: &[&str]) -> Option<PathBuf> {
        // Check custom configured path first
        if let Some(custom) = self.config.binary_paths.get(config_key) {
            if custom.exists() {
                return Some(custom.clone());
            }
            warn!(
                "Configured binary {} not found at {}",
                config_key,
                custom.display()
            );
        }

        // Search PATH
        for name in candidates {
            if let Ok(output) = std::process::Command::new("which").arg(name).output() {
                if output.status.success() {
                    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    if !path.is_empty() {
                        return Some(PathBuf::from(path));
                    }
                }
            }
        }

        None
    }

    /// Try to get version from a binary
    fn probe_version(binary: &Path, args: &[&str]) -> Option<String> {
        std::process::Command::new(binary)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .ok()
            .and_then(|o| {
                let out = String::from_utf8_lossy(&o.stdout);
                let err = String::from_utf8_lossy(&o.stderr);
                let combined = if out.trim().is_empty() {
                    err.to_string()
                } else {
                    out.to_string()
                };
                combined.lines().next().map(|l| l.trim().to_string())
            })
            .filter(|v| !v.is_empty())
    }

    /// Check if llama.cpp build has GPU support (Metal/CUDA)
    fn check_gpu_support_llama(binary: &Path) -> bool {
        // llama-cli --help output mentions Metal/CUDA if compiled with support
        std::process::Command::new(binary)
            .arg("--help")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .ok()
            .map(|o| {
                let text = String::from_utf8_lossy(&o.stdout);
                let err = String::from_utf8_lossy(&o.stderr);
                text.contains("--gpu-layers")
                    || text.contains("-ngl")
                    || err.contains("Metal")
                    || err.contains("CUDA")
            })
            .unwrap_or(false)
    }

    /// Scan model directory for GGUF/bin files and map to model names
    fn scan_model_files(&mut self) {
        let model_dir = &self.config.model_dir;
        if !model_dir.exists() {
            debug!(
                "Model directory {} does not exist, skipping scan",
                model_dir.display()
            );
            return;
        }

        let entries = match std::fs::read_dir(model_dir) {
            Ok(e) => e,
            Err(e) => {
                warn!("Failed to scan model dir {}: {}", model_dir.display(), e);
                return;
            }
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();

            // Map GGUF and bin files to model names
            if ext == "gguf" || ext == "bin" {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    let model_name = normalize_model_name(stem);
                    info!("Found model file: {} → {}", model_name, path.display());
                    self.model_files.insert(model_name, path);
                }
            }
        }
    }

    /// Check if a plugin runtime is available
    pub fn has_plugin(&self, runtime: &PluginRuntime) -> bool {
        self.plugins.contains_key(runtime)
    }

    /// Get all detected plugins
    pub fn detected_plugins(&self) -> Vec<&DetectedPlugin> {
        self.plugins.values().collect()
    }

    /// Check if a model file is available locally
    pub fn has_model(&self, model_name: &str) -> bool {
        let normalized = normalize_model_name(model_name);
        self.model_files.contains_key(&normalized)
    }

    /// Get the model file path for a given model name
    pub fn model_path(&self, model_name: &str) -> Option<&Path> {
        let normalized = normalize_model_name(model_name);
        self.model_files.get(&normalized).map(|p| p.as_path())
    }

    /// Determine which plugin runtime to use for a task type
    pub fn runtime_for_task(&self, task_type: &str) -> Option<&DetectedPlugin> {
        match task_type {
            "text_generation" => self.plugins.get(&PluginRuntime::LlamaCpp),
            "speech_to_text" => self.plugins.get(&PluginRuntime::WhisperCpp),
            "image_generation" => self.plugins.get(&PluginRuntime::StableDiffusionCpp),
            _ => None,
        }
    }

    /// Execute inference via external plugin
    pub async fn run_inference(
        &self,
        task_type: &str,
        model_name: &str,
        input_data: &[u8],
        params: &PluginTaskParams,
    ) -> Result<PluginResult, PluginError> {
        let plugin = self.runtime_for_task(task_type).ok_or_else(|| {
            PluginError::NotFound(format!("No plugin for task type: {}", task_type))
        })?;

        let model_path = self
            .model_path(model_name)
            .ok_or_else(|| PluginError::ModelNotFound(model_name.to_string()))?;

        match plugin.runtime {
            PluginRuntime::LlamaCpp => {
                self.run_llama_cpp(&plugin.binary_path, model_path, input_data, params)
                    .await
            }
            PluginRuntime::WhisperCpp => {
                self.run_whisper_cpp(&plugin.binary_path, model_path, input_data, params)
                    .await
            }
            PluginRuntime::StableDiffusionCpp => {
                self.run_sd_cpp(&plugin.binary_path, model_path, input_data, params)
                    .await
            }
        }
    }

    /// Run llama.cpp for text generation
    async fn run_llama_cpp(
        &self,
        binary: &Path,
        model_path: &Path,
        input_data: &[u8],
        params: &PluginTaskParams,
    ) -> Result<PluginResult, PluginError> {
        let prompt = std::str::from_utf8(input_data)
            .map_err(|e| PluginError::InvalidOutput(format!("Invalid UTF-8 prompt: {}", e)))?;

        let max_tokens = params.max_tokens.unwrap_or(128);
        let seed = params.seed.unwrap_or(42);

        let start = Instant::now();

        let mut cmd = Command::new(binary);
        cmd.arg("-m")
            .arg(model_path)
            .arg("-p")
            .arg(prompt)
            .arg("-n")
            .arg(max_tokens.to_string())
            .arg("--seed")
            .arg(seed.to_string())
            .arg("-t")
            .arg(self.config.threads.to_string());

        // GPU layer offloading
        if self.config.gpu_layers > 0 {
            cmd.arg("-ngl").arg(self.config.gpu_layers.to_string());
        }

        // Disable interactive/logging for clean output
        cmd.arg("--log-disable");

        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        debug!("Running llama.cpp: {:?}", cmd);

        let output = tokio::time::timeout(
            std::time::Duration::from_secs(self.config.timeout_secs),
            cmd.output(),
        )
        .await
        .map_err(|_| PluginError::Timeout(self.config.timeout_secs))?
        .map_err(PluginError::Io)?;

        let elapsed_ms = start.elapsed().as_millis() as u64;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(PluginError::ExecutionFailed(format!(
                "llama.cpp exit code {}: {}",
                output.status.code().unwrap_or(-1),
                stderr.chars().take(500).collect::<String>()
            )));
        }

        let stdout = output.stdout;
        let output_hash = blake3::hash(&stdout);

        Ok(PluginResult {
            output_data: stdout,
            output_hash: output_hash.as_bytes().to_vec(),
            execution_time_ms: elapsed_ms,
            flops_estimated: estimate_plugin_flops("text_generation", model_path, elapsed_ms),
        })
    }

    /// Run whisper.cpp for speech-to-text
    async fn run_whisper_cpp(
        &self,
        binary: &Path,
        model_path: &Path,
        input_data: &[u8],
        params: &PluginTaskParams,
    ) -> Result<PluginResult, PluginError> {
        // Write audio data to temp file (whisper.cpp reads from file)
        let tmp_dir = std::env::temp_dir().join("qfc-miner-whisper");
        std::fs::create_dir_all(&tmp_dir)?;
        let audio_path = tmp_dir.join("input.wav");
        std::fs::write(&audio_path, input_data)?;

        let output_path = tmp_dir.join("output.txt");

        let start = Instant::now();

        let mut cmd = Command::new(binary);
        cmd.arg("-m")
            .arg(model_path)
            .arg("-f")
            .arg(&audio_path)
            .arg("-of")
            .arg(output_path.with_extension("")) // whisper adds .txt
            .arg("-t")
            .arg(self.config.threads.to_string())
            .arg("--no-timestamps");

        if let Some(lang) = &params.language {
            cmd.arg("-l").arg(lang);
        }

        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        debug!("Running whisper.cpp: {:?}", cmd);

        let output = tokio::time::timeout(
            std::time::Duration::from_secs(self.config.timeout_secs),
            cmd.output(),
        )
        .await
        .map_err(|_| PluginError::Timeout(self.config.timeout_secs))?
        .map_err(PluginError::Io)?;

        let elapsed_ms = start.elapsed().as_millis() as u64;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(PluginError::ExecutionFailed(format!(
                "whisper.cpp exit code {}: {}",
                output.status.code().unwrap_or(-1),
                stderr.chars().take(500).collect::<String>()
            )));
        }

        // Read transcription output
        let txt_path = output_path.with_extension("txt");
        let transcript = if txt_path.exists() {
            std::fs::read(&txt_path)?
        } else {
            // Fallback to stdout
            output.stdout
        };

        // Clean up temp files
        let _ = std::fs::remove_file(&audio_path);
        let _ = std::fs::remove_file(&txt_path);

        let output_hash = blake3::hash(&transcript);

        Ok(PluginResult {
            output_data: transcript,
            output_hash: output_hash.as_bytes().to_vec(),
            execution_time_ms: elapsed_ms,
            flops_estimated: estimate_plugin_flops("speech_to_text", model_path, elapsed_ms),
        })
    }

    /// Run stable-diffusion.cpp for image generation
    async fn run_sd_cpp(
        &self,
        binary: &Path,
        model_path: &Path,
        input_data: &[u8],
        params: &PluginTaskParams,
    ) -> Result<PluginResult, PluginError> {
        let prompt = std::str::from_utf8(input_data)
            .map_err(|e| PluginError::InvalidOutput(format!("Invalid UTF-8 prompt: {}", e)))?;

        let width = params.width.unwrap_or(512);
        let height = params.height.unwrap_or(512);
        let steps = params.steps.unwrap_or(20);
        let seed = params.seed.unwrap_or(42);

        let tmp_dir = std::env::temp_dir().join("qfc-miner-sd");
        std::fs::create_dir_all(&tmp_dir)?;
        let output_path = tmp_dir.join("output.png");

        let start = Instant::now();

        let mut cmd = Command::new(binary);
        cmd.arg("-m")
            .arg(model_path)
            .arg("-p")
            .arg(prompt)
            .arg("-o")
            .arg(&output_path)
            .arg("-W")
            .arg(width.to_string())
            .arg("-H")
            .arg(height.to_string())
            .arg("--steps")
            .arg(steps.to_string())
            .arg("-s")
            .arg(seed.to_string())
            .arg("-t")
            .arg(self.config.threads.to_string());

        if let Some(neg) = &params.negative_prompt {
            cmd.arg("-n").arg(neg);
        }

        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        debug!("Running stable-diffusion.cpp: {:?}", cmd);

        let output = tokio::time::timeout(
            std::time::Duration::from_secs(self.config.timeout_secs),
            cmd.output(),
        )
        .await
        .map_err(|_| PluginError::Timeout(self.config.timeout_secs))?
        .map_err(PluginError::Io)?;

        let elapsed_ms = start.elapsed().as_millis() as u64;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(PluginError::ExecutionFailed(format!(
                "stable-diffusion.cpp exit code {}: {}",
                output.status.code().unwrap_or(-1),
                stderr.chars().take(500).collect::<String>()
            )));
        }

        let image_data = if output_path.exists() {
            std::fs::read(&output_path)?
        } else {
            return Err(PluginError::ExecutionFailed(
                "Output image not generated".to_string(),
            ));
        };

        // Clean up
        let _ = std::fs::remove_file(&output_path);

        let output_hash = blake3::hash(&image_data);

        Ok(PluginResult {
            output_data: image_data,
            output_hash: output_hash.as_bytes().to_vec(),
            execution_time_ms: elapsed_ms,
            flops_estimated: estimate_plugin_flops("image_generation", model_path, elapsed_ms),
        })
    }
}

/// Parameters for plugin task execution
#[derive(Clone, Debug, Default)]
pub struct PluginTaskParams {
    /// Max tokens for text generation
    pub max_tokens: Option<u32>,
    /// Deterministic seed
    pub seed: Option<u64>,
    /// Language code for speech-to-text
    pub language: Option<String>,
    /// Image width for image generation
    pub width: Option<u32>,
    /// Image height for image generation
    pub height: Option<u32>,
    /// Diffusion steps for image generation
    pub steps: Option<u32>,
    /// Negative prompt for image generation
    pub negative_prompt: Option<String>,
}

/// Result from plugin execution
#[derive(Debug)]
pub struct PluginResult {
    /// Raw output data (text bytes, image bytes, etc.)
    pub output_data: Vec<u8>,
    /// Blake3 hash of output data
    pub output_hash: Vec<u8>,
    /// Execution time in milliseconds
    pub execution_time_ms: u64,
    /// Estimated FLOPS
    pub flops_estimated: u64,
}

/// Normalize model name for file lookup
/// e.g. "llama3-8b" matches "llama3-8b-q4_k_m.gguf"
fn normalize_model_name(filename: &str) -> String {
    // Strip common quantization suffixes for matching
    let name = filename
        .to_lowercase()
        .replace(".gguf", "")
        .replace(".bin", "");

    // Remove trailing quantization markers (e.g. -q4_k_m, -q5_0)
    if let Some(idx) = name.rfind("-q") {
        if idx > 0 {
            return name[..idx].to_string();
        }
    }
    if let Some(idx) = name.rfind("-f16") {
        if idx > 0 {
            return name[..idx].to_string();
        }
    }
    if let Some(idx) = name.rfind("-f32") {
        if idx > 0 {
            return name[..idx].to_string();
        }
    }

    name
}

/// Estimate FLOPS for external plugin execution
fn estimate_plugin_flops(task_type: &str, model_path: &Path, elapsed_ms: u64) -> u64 {
    let file_size_mb = std::fs::metadata(model_path)
        .map(|m| m.len() / (1024 * 1024))
        .unwrap_or(0);

    // Rough estimation based on model size and task type
    match task_type {
        "text_generation" => {
            // ~2 FLOPS per parameter, estimate params from file size
            // GGUF Q4: ~0.5 bytes/param, FP16: ~2 bytes/param
            let est_params = file_size_mb * 2_000_000; // conservative estimate
            est_params * 2
        }
        "speech_to_text" => {
            if file_size_mb > 1000 {
                30_000_000_000 // whisper-large
            } else {
                4_000_000_000 // whisper-base/medium
            }
        }
        "image_generation" => {
            // SD: ~2 GFLOPS per step
            2_000_000_000 * 20 + 5_000_000_000
        }
        _ => {
            // Fallback: estimate from elapsed time assuming 1 TFLOPS throughput
            elapsed_ms.saturating_mul(1_000_000_000)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_model_name() {
        assert_eq!(normalize_model_name("llama-3-8b-q4_k_m"), "llama-3-8b");
        assert_eq!(normalize_model_name("llama-3-8b-q4_k_m.gguf"), "llama-3-8b");
        assert_eq!(normalize_model_name("whisper-large-v3"), "whisper-large-v3");
        assert_eq!(normalize_model_name("sd-v1.5-f16"), "sd-v1.5");
        assert_eq!(normalize_model_name("model-f32"), "model");
    }

    #[test]
    fn test_plugin_config_default() {
        let config = PluginConfig::default();
        assert!(config.enabled);
        assert_eq!(config.tier, 1);
        assert!(config.models.is_empty());
        assert_eq!(config.timeout_secs, 300);
        assert_eq!(config.gpu_layers, 999);
        assert!(config.threads > 0);
    }

    #[test]
    fn test_plugin_runtime_display() {
        assert_eq!(format!("{}", PluginRuntime::LlamaCpp), "llama.cpp");
        assert_eq!(format!("{}", PluginRuntime::WhisperCpp), "whisper.cpp");
        assert_eq!(
            format!("{}", PluginRuntime::StableDiffusionCpp),
            "stable-diffusion.cpp"
        );
    }

    #[test]
    fn test_plugin_registry_creation() {
        let config = PluginConfig {
            enabled: true,
            model_dir: PathBuf::from("/nonexistent"),
            ..Default::default()
        };
        let registry = PluginRegistry::new(config);
        // On most dev machines, at least one plugin may or may not be available
        // The registry should be created without errors regardless
        let _ = registry.detected_plugins();
    }

    #[test]
    fn test_runtime_for_task() {
        let config = PluginConfig::default();
        let registry = PluginRegistry::new(config);

        // These return None on machines without the binaries installed
        // but should not panic
        let _ = registry.runtime_for_task("text_generation");
        let _ = registry.runtime_for_task("speech_to_text");
        let _ = registry.runtime_for_task("image_generation");
        assert!(registry.runtime_for_task("unknown_type").is_none());
    }

    #[test]
    fn test_estimate_plugin_flops() {
        let path = PathBuf::from("/nonexistent/model.gguf");
        let flops = estimate_plugin_flops("text_generation", &path, 1000);
        assert_eq!(flops, 0); // file doesn't exist, size = 0

        let flops = estimate_plugin_flops("speech_to_text", &path, 1000);
        assert_eq!(flops, 4_000_000_000);

        let flops = estimate_plugin_flops("image_generation", &path, 1000);
        assert_eq!(flops, 45_000_000_000);
    }

    #[test]
    fn test_plugin_error_display() {
        let e = PluginError::NotFound("llama-cli".to_string());
        assert!(format!("{}", e).contains("llama-cli"));

        let e = PluginError::Timeout(300);
        assert!(format!("{}", e).contains("300"));
    }
}
