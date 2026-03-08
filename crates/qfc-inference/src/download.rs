//! HuggingFace model downloading and caching
//!
//! Downloads model weights and tokenizer files from HuggingFace Hub.
//! Uses hf-hub with a curl fallback for servers that don't support Range requests.

#[cfg(feature = "candle")]
use std::path::PathBuf;

#[cfg(feature = "candle")]
use crate::InferenceError;

/// Model format (safetensors vs GGUF quantized)
#[cfg(feature = "candle")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelFormat {
    /// Standard safetensors format (FP32/FP16)
    Safetensors,
    /// GGUF quantized format (Q4_K_M, Q5_K_M, etc.)
    Gguf,
}

/// HuggingFace repo IDs for approved QFC benchmark models
#[cfg(feature = "candle")]
pub struct HfModelRepo {
    /// HuggingFace repo ID (e.g. "sentence-transformers/all-MiniLM-L6-v2")
    pub repo_id: &'static str,
    /// Model weight file name
    pub weights_file: &'static str,
    /// Tokenizer file name
    pub tokenizer_file: &'static str,
    /// Config file name
    pub config_file: &'static str,
    /// Model format
    pub format: ModelFormat,
    /// Tokenizer repo ID (if different from weights repo, e.g. GGUF models
    /// use a separate repo for the GGUF file but tokenizer comes from the base repo)
    pub tokenizer_repo_id: Option<&'static str>,
}

/// Get HuggingFace repo info for a QFC model name
#[cfg(feature = "candle")]
pub fn get_hf_repo(model_name: &str) -> Option<HfModelRepo> {
    match model_name {
        "qfc-embed-small" => Some(HfModelRepo {
            repo_id: "sentence-transformers/all-MiniLM-L6-v2",
            weights_file: "model.safetensors",
            tokenizer_file: "tokenizer.json",
            config_file: "config.json",
            format: ModelFormat::Safetensors,
            tokenizer_repo_id: None,
        }),
        "qfc-embed-medium" => Some(HfModelRepo {
            repo_id: "sentence-transformers/all-MiniLM-L12-v2",
            weights_file: "model.safetensors",
            tokenizer_file: "tokenizer.json",
            config_file: "config.json",
            format: ModelFormat::Safetensors,
            tokenizer_repo_id: None,
        }),
        "qfc-classify-small" => Some(HfModelRepo {
            repo_id: "google-bert/bert-base-uncased",
            weights_file: "model.safetensors",
            tokenizer_file: "tokenizer.json",
            config_file: "config.json",
            format: ModelFormat::Safetensors,
            tokenizer_repo_id: None,
        }),
        "qfc-llm-0.5b" => Some(HfModelRepo {
            repo_id: "Qwen/Qwen2.5-0.5B-Instruct",
            weights_file: "model.safetensors",
            tokenizer_file: "tokenizer.json",
            config_file: "config.json",
            format: ModelFormat::Safetensors,
            tokenizer_repo_id: None,
        }),
        "qfc-llm-3b" => Some(HfModelRepo {
            repo_id: "Qwen/Qwen2.5-3B-Instruct-GGUF",
            weights_file: "qwen2.5-3b-instruct-q4_k_m.gguf",
            tokenizer_file: "tokenizer.json",
            config_file: "config.json",
            format: ModelFormat::Gguf,
            tokenizer_repo_id: Some("Qwen/Qwen2.5-3B-Instruct"),
        }),
        "qfc-llm-7b" => Some(HfModelRepo {
            repo_id: "Qwen/Qwen2.5-7B-Instruct-GGUF",
            weights_file: "qwen2.5-7b-instruct-q4_k_m.gguf",
            tokenizer_file: "tokenizer.json",
            config_file: "config.json",
            format: ModelFormat::Gguf,
            tokenizer_repo_id: Some("Qwen/Qwen2.5-7B-Instruct"),
        }),
        _ => None,
    }
}

/// Downloaded model files
#[cfg(feature = "candle")]
pub struct DownloadedModel {
    pub weights_path: PathBuf,
    pub tokenizer_path: PathBuf,
    pub config_path: PathBuf,
}

/// Download a single file using hf-hub, falling back to curl on failure
#[cfg(feature = "candle")]
fn download_file(
    repo: &hf_hub::api::sync::ApiRepo,
    repo_id: &str,
    filename: &str,
    cache_dir: &std::path::Path,
) -> Result<PathBuf, InferenceError> {
    // Try hf-hub first
    match repo.get(filename) {
        Ok(path) => return Ok(path),
        Err(e) => {
            tracing::warn!(
                "hf-hub failed to download {}/{}: {}, falling back to curl",
                repo_id,
                filename,
                e
            );
        }
    }

    // Fallback: download directly via curl
    let url = format!(
        "https://huggingface.co/{}/resolve/main/{}",
        repo_id, filename
    );
    let dest = cache_dir.join(filename);

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            InferenceError::ExecutionFailed(format!("Failed to create cache dir: {}", e))
        })?;
    }

    let output = std::process::Command::new("curl")
        .args(["-sfL", "-o"])
        .arg(&dest)
        .arg(&url)
        .output()
        .map_err(|e| InferenceError::ExecutionFailed(format!("Failed to run curl: {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(InferenceError::ExecutionFailed(format!(
            "curl failed to download {}: {}",
            url, stderr
        )));
    }

    tracing::info!("Downloaded {} via curl fallback", filename);
    Ok(dest)
}

/// Download model files from HuggingFace Hub
///
/// Uses hf-hub's caching mechanism with curl fallback for resilience.
#[cfg(feature = "candle")]
pub fn download_model(model_name: &str) -> Result<DownloadedModel, InferenceError> {
    let repo_info = get_hf_repo(model_name).ok_or_else(|| {
        InferenceError::ModelNotFound(format!("No HuggingFace repo for model: {}", model_name))
    })?;

    tracing::info!(
        "Downloading model {} from HuggingFace ({})",
        model_name,
        repo_info.repo_id
    );

    let api = hf_hub::api::sync::Api::new().map_err(|e| {
        InferenceError::ExecutionFailed(format!("Failed to create HuggingFace API client: {}", e))
    })?;

    let repo = api.model(repo_info.repo_id.to_string());

    // Cache dir for curl fallback
    let cache_dir = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
        .join(".cache/qfc-models")
        .join(model_name);

    // Download weights from the primary repo
    let weights_path = download_file(&repo, repo_info.repo_id, repo_info.weights_file, &cache_dir)?;

    // For GGUF models, tokenizer and config come from the base (non-GGUF) repo
    let (tokenizer_path, config_path) = if let Some(tok_repo_id) = repo_info.tokenizer_repo_id {
        let tok_repo = api.model(tok_repo_id.to_string());
        let tp = download_file(&tok_repo, tok_repo_id, repo_info.tokenizer_file, &cache_dir)?;
        let cp = download_file(&tok_repo, tok_repo_id, repo_info.config_file, &cache_dir)?;
        (tp, cp)
    } else {
        let tp = download_file(
            &repo,
            repo_info.repo_id,
            repo_info.tokenizer_file,
            &cache_dir,
        )?;
        let cp = download_file(&repo, repo_info.repo_id, repo_info.config_file, &cache_dir)?;
        (tp, cp)
    };

    tracing::info!("Model {} downloaded successfully", model_name);

    Ok(DownloadedModel {
        weights_path,
        tokenizer_path,
        config_path,
    })
}
