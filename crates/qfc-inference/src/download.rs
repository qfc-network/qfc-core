//! HuggingFace model downloading and caching
//!
//! Downloads model weights and tokenizer files from HuggingFace Hub.

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

/// Download model files from HuggingFace Hub
///
/// Uses hf-hub's caching mechanism — files are only downloaded once.
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

    let weights_path = repo.get(repo_info.weights_file).map_err(|e| {
        InferenceError::ExecutionFailed(format!("Failed to download model weights: {}", e))
    })?;

    let tokenizer_path = repo.get(repo_info.tokenizer_file).map_err(|e| {
        InferenceError::ExecutionFailed(format!("Failed to download tokenizer: {}", e))
    })?;

    let config_path = repo.get(repo_info.config_file).map_err(|e| {
        InferenceError::ExecutionFailed(format!("Failed to download config: {}", e))
    })?;

    tracing::info!("Model {} downloaded successfully", model_name);

    Ok(DownloadedModel {
        weights_path,
        tokenizer_path,
        config_path,
    })
}
