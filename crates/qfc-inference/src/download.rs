//! HuggingFace model downloading and caching
//!
//! Downloads model weights and tokenizer files from HuggingFace Hub.
//! Uses hf-hub with a curl fallback for servers that don't support Range requests.

#[cfg(feature = "candle")]
use std::path::PathBuf;

#[cfg(feature = "candle")]
use crate::InferenceError;

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
        }),
        "qfc-embed-medium" => Some(HfModelRepo {
            repo_id: "sentence-transformers/all-mpnet-base-v2",
            weights_file: "model.safetensors",
            tokenizer_file: "tokenizer.json",
            config_file: "config.json",
        }),
        "qfc-classify-small" => Some(HfModelRepo {
            repo_id: "google-bert/bert-base-uncased",
            weights_file: "model.safetensors",
            tokenizer_file: "tokenizer.json",
            config_file: "config.json",
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
        .map_err(|e| {
            InferenceError::ExecutionFailed(format!("Failed to run curl: {}", e))
        })?;

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

    let weights_path =
        download_file(&repo, repo_info.repo_id, repo_info.weights_file, &cache_dir)?;
    let tokenizer_path =
        download_file(&repo, repo_info.repo_id, repo_info.tokenizer_file, &cache_dir)?;
    let config_path =
        download_file(&repo, repo_info.repo_id, repo_info.config_file, &cache_dir)?;

    tracing::info!("Model {} downloaded successfully", model_name);

    Ok(DownloadedModel {
        weights_path,
        tokenizer_path,
        config_path,
    })
}
