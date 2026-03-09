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
    /// Pinned HuggingFace git revision (commit hash) for reproducible downloads.
    /// If None, uses the latest version (main branch).
    pub revision: Option<&'static str>,
    /// Extra files to download (e.g. mel filters for Whisper)
    pub extra_files: &'static [&'static str],
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
            revision: None,
            extra_files: &[],
        }),
        "qfc-embed-medium" => Some(HfModelRepo {
            repo_id: "sentence-transformers/all-MiniLM-L12-v2",
            weights_file: "model.safetensors",
            tokenizer_file: "tokenizer.json",
            config_file: "config.json",
            format: ModelFormat::Safetensors,
            tokenizer_repo_id: None,
            revision: None,
            extra_files: &[],
        }),
        "qfc-classify-small" => Some(HfModelRepo {
            repo_id: "google-bert/bert-base-uncased",
            weights_file: "model.safetensors",
            tokenizer_file: "tokenizer.json",
            config_file: "config.json",
            format: ModelFormat::Safetensors,
            tokenizer_repo_id: None,
            revision: None,
            extra_files: &[],
        }),
        "qfc-llm-0.5b" => Some(HfModelRepo {
            repo_id: "Qwen/Qwen2.5-0.5B-Instruct",
            weights_file: "model.safetensors",
            tokenizer_file: "tokenizer.json",
            config_file: "config.json",
            format: ModelFormat::Safetensors,
            tokenizer_repo_id: None,
            revision: None,
            extra_files: &[],
        }),
        "qfc-llm-3b" => Some(HfModelRepo {
            repo_id: "Qwen/Qwen2.5-3B-Instruct-GGUF",
            weights_file: "qwen2.5-3b-instruct-q4_k_m.gguf",
            tokenizer_file: "tokenizer.json",
            config_file: "config.json",
            format: ModelFormat::Gguf,
            tokenizer_repo_id: Some("Qwen/Qwen2.5-3B-Instruct"),
            revision: None,
            extra_files: &[],
        }),
        "qfc-llm-7b" => Some(HfModelRepo {
            repo_id: "Qwen/Qwen2.5-7B-Instruct-GGUF",
            weights_file: "qwen2.5-7b-instruct-q4_k_m.gguf",
            tokenizer_file: "tokenizer.json",
            config_file: "config.json",
            format: ModelFormat::Gguf,
            tokenizer_repo_id: Some("Qwen/Qwen2.5-7B-Instruct"),
            revision: None,
            extra_files: &[],
        }),
        "qfc-whisper-base" => Some(HfModelRepo {
            repo_id: "openai/whisper-base",
            weights_file: "model.safetensors",
            tokenizer_file: "tokenizer.json",
            config_file: "config.json",
            format: ModelFormat::Safetensors,
            tokenizer_repo_id: None,
            revision: None,
            extra_files: &["mel_filters.npz"],
        }),
        "qfc-whisper-large" => Some(HfModelRepo {
            repo_id: "openai/whisper-large-v3",
            weights_file: "model.safetensors",
            tokenizer_file: "tokenizer.json",
            config_file: "config.json",
            format: ModelFormat::Safetensors,
            tokenizer_repo_id: None,
            revision: None,
            extra_files: &["mel_filters.npz"],
        }),
        "qfc-sd-1.5" => Some(HfModelRepo {
            repo_id: "stable-diffusion-v1-5/stable-diffusion-v1-5",
            weights_file: "unet/diffusion_pytorch_model.safetensors",
            tokenizer_file: "tokenizer/tokenizer.json",
            config_file: "unet/config.json",
            format: ModelFormat::Safetensors,
            tokenizer_repo_id: None,
            revision: None,
            extra_files: &[],
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
    /// Extra downloaded files (e.g. mel_filters.npz for Whisper)
    pub extra_paths: Vec<(String, PathBuf)>,
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

    // Pin to specific revision if available (ensures reproducible downloads)
    let repo = if let Some(rev) = repo_info.revision {
        tracing::info!("Pinning to HuggingFace revision: {}", rev);
        api.repo(hf_hub::Repo::with_revision(
            repo_info.repo_id.to_string(),
            hf_hub::RepoType::Model,
            rev.to_string(),
        ))
    } else {
        api.model(repo_info.repo_id.to_string())
    };

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

    // Download extra files (e.g. mel filters)
    let mut extra_paths = Vec::new();
    for extra_file in repo_info.extra_files {
        let path = download_file(&repo, repo_info.repo_id, extra_file, &cache_dir)?;
        extra_paths.push((extra_file.to_string(), path));
    }

    tracing::info!("Model {} downloaded successfully", model_name);

    Ok(DownloadedModel {
        weights_path,
        tokenizer_path,
        config_path,
        extra_paths,
    })
}

/// Verify that a downloaded weight file matches the expected Blake3 hash.
///
/// Returns Ok(computed_hash) if the hash matches or no expected hash is provided.
/// Returns Err if the hash doesn't match (possible supply-chain attack or version mismatch).
#[cfg(feature = "candle")]
pub fn verify_weights_hash(
    weights_path: &std::path::Path,
    expected_hash: Option<qfc_types::Hash>,
) -> Result<qfc_types::Hash, InferenceError> {
    let data = std::fs::read(weights_path).map_err(|e| {
        InferenceError::ExecutionFailed(format!("Failed to read weights file: {}", e))
    })?;

    let computed = qfc_crypto::blake3_hash(&data);

    if let Some(expected) = expected_hash {
        if computed != expected {
            return Err(InferenceError::ExecutionFailed(format!(
                "Weight hash mismatch for {:?}: expected {}, got {}. \
                 This may indicate a corrupted download or tampered model weights.",
                weights_path, expected, computed
            )));
        }
        tracing::info!("Weight hash verified: {}", computed);
    } else {
        tracing::warn!(
            "No expected weight hash configured — skipping verification (hash: {})",
            computed
        );
    }

    Ok(computed)
}
