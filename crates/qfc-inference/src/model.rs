//! Model loading, caching, and registry

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use borsh::{BorshDeserialize, BorshSerialize};

use crate::runtime::GpuTier;
use crate::task::ModelId;

/// Canonical format that ALL nodes must use for a given model.
///
/// This ensures deterministic output across different hardware:
/// - Same quantization → same integer arithmetic → same argmax → same tokens
/// - Eliminates FP16 vs FP32 divergence by mandating one format per model
///
/// Nodes that use a non-canonical format will produce different output hashes
/// and fail spot-check verification.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize,
)]
pub enum CanonicalFormat {
    /// FP32 safetensors on CPU (fully deterministic, for small models ≤1B)
    SafetensorsFp32,
    /// GGUF Q4_K_M quantized (deterministic integer math, for larger models)
    GgufQ4KM,
    /// GGUF Q5_K_M quantized (higher quality, for models where Q4 isn't sufficient)
    GgufQ5KM,
}

impl std::fmt::Display for CanonicalFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CanonicalFormat::SafetensorsFp32 => write!(f, "safetensors-fp32"),
            CanonicalFormat::GgufQ4KM => write!(f, "gguf-q4_k_m"),
            CanonicalFormat::GgufQ5KM => write!(f, "gguf-q5_k_m"),
        }
    }
}

/// Model metadata in the registry
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelInfo {
    /// Model identifier
    pub id: ModelId,
    /// Human-readable description
    pub description: String,
    /// Required minimum memory in MB
    pub min_memory_mb: u64,
    /// Minimum GPU tier required
    pub min_tier: GpuTier,
    /// Model file size in MB
    pub size_mb: u64,
    /// Whether this model is approved by governance
    pub approved: bool,
    /// Canonical format for deterministic verification.
    /// All nodes MUST use this format for spot-check to pass.
    pub canonical_format: CanonicalFormat,
    /// Blake3 hash of the model weight file (for integrity verification).
    /// If set, downloaded weights are verified against this hash.
    /// Prevents supply-chain attacks and ensures all miners use identical weights.
    pub weights_hash: Option<qfc_types::Hash>,
}

/// Local model cache with LRU eviction and auto-download
pub struct ModelCache {
    /// Cache directory
    cache_dir: PathBuf,
    /// Cached model metadata
    cached: HashMap<ModelId, CachedModel>,
    /// Maximum cache size in bytes (0 = unlimited)
    max_size_bytes: u64,
}

/// A cached model on disk
#[derive(Clone, Debug)]
pub struct CachedModel {
    pub id: ModelId,
    pub path: PathBuf,
    pub size_bytes: u64,
    /// Last access timestamp (milliseconds since epoch)
    pub last_accessed: u64,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

impl ModelCache {
    /// Create a new model cache at the given directory
    pub fn new(cache_dir: PathBuf) -> Self {
        Self {
            cache_dir,
            cached: HashMap::new(),
            max_size_bytes: 0,
        }
    }

    /// Create a model cache with a maximum size limit
    pub fn with_max_size(cache_dir: PathBuf, max_size_bytes: u64) -> Self {
        Self {
            cache_dir,
            cached: HashMap::new(),
            max_size_bytes,
        }
    }

    /// Check if a model is already cached
    pub fn is_cached(&self, model_id: &ModelId) -> bool {
        self.cached.contains_key(model_id)
    }

    /// Get the path to a cached model, updating last-accessed time
    pub fn get_path(&mut self, model_id: &ModelId) -> Option<&PathBuf> {
        if let Some(entry) = self.cached.get_mut(model_id) {
            entry.last_accessed = now_ms();
            Some(&entry.path)
        } else {
            None
        }
    }

    /// Register a model as cached (after download)
    pub fn register(&mut self, model_id: ModelId, path: PathBuf, size_bytes: u64) {
        self.cached.insert(
            model_id.clone(),
            CachedModel {
                id: model_id,
                path,
                size_bytes,
                last_accessed: now_ms(),
            },
        );
    }

    /// Get total size of all cached models in bytes
    pub fn total_size_bytes(&self) -> u64 {
        self.cached.values().map(|c| c.size_bytes).sum()
    }

    /// Get the cache directory
    pub fn cache_dir(&self) -> &PathBuf {
        &self.cache_dir
    }

    /// List all cached models
    pub fn list_cached(&self) -> Vec<&CachedModel> {
        self.cached.values().collect()
    }

    /// Evict least-recently-used models until total size is under max_size_bytes.
    /// Returns the number of models evicted.
    pub fn evict_lru(&mut self) -> usize {
        if self.max_size_bytes == 0 {
            return 0;
        }

        let mut evicted = 0;
        while self.total_size_bytes() > self.max_size_bytes && !self.cached.is_empty() {
            // Find the LRU entry
            let lru_id = self
                .cached
                .values()
                .min_by_key(|c| c.last_accessed)
                .map(|c| c.id.clone());

            if let Some(id) = lru_id {
                if let Some(entry) = self.cached.remove(&id) {
                    // Best-effort delete from disk
                    let _ = std::fs::remove_file(&entry.path);
                    tracing::info!(
                        "Evicted model {} ({} bytes) from cache",
                        id,
                        entry.size_bytes
                    );
                    evicted += 1;
                }
            }
        }
        evicted
    }

    /// Ensure a model is cached, downloading it if necessary.
    /// Evicts LRU models if the cache is full.
    /// Returns the path to the cached model files.
    #[cfg(feature = "candle")]
    pub fn ensure_model(&mut self, model_id: &ModelId) -> Result<PathBuf, crate::InferenceError> {
        // Already cached — return path
        if let Some(entry) = self.cached.get_mut(model_id) {
            entry.last_accessed = now_ms();
            return Ok(entry.path.clone());
        }

        // Download the model
        let downloaded = crate::download::download_model(&model_id.name)?;

        // Verify weight file integrity if registry has an expected hash
        let registry = ModelRegistry::default_v2();
        let expected_hash = registry.get_model(model_id).and_then(|m| m.weights_hash);
        crate::download::verify_weights_hash(&downloaded.weights_path, expected_hash)?;

        let size_bytes = [
            &downloaded.weights_path,
            &downloaded.tokenizer_path,
            &downloaded.config_path,
        ]
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum();

        // Register
        let model_dir = downloaded
            .weights_path
            .parent()
            .unwrap_or(&self.cache_dir)
            .to_path_buf();
        self.register(model_id.clone(), model_dir.clone(), size_bytes);

        // Evict if over limit
        self.evict_lru();

        Ok(model_dir)
    }
}

/// Approved model registry (controlled by on-chain governance)
pub struct ModelRegistry {
    /// Approved models
    models: Vec<ModelInfo>,
}

impl ModelRegistry {
    /// Create a new empty registry
    pub fn new() -> Self {
        Self { models: Vec::new() }
    }

    /// Create a registry with the default approved models for v2.0
    ///
    /// These map to real HuggingFace models:
    /// - qfc-embed-small → sentence-transformers/all-MiniLM-L6-v2 (~80MB, 384-dim)
    /// - qfc-embed-medium → sentence-transformers/all-MiniLM-L12-v2 (~120MB, 384-dim)
    /// - qfc-classify-small → google-bert/bert-base-uncased (~440MB, 768-dim)
    pub fn default_v2() -> Self {
        let models = vec![
            ModelInfo {
                id: ModelId::new("qfc-embed-small", "v1.0"),
                description: "Small embedding model (all-MiniLM-L6-v2, 384-dim) for Cold tier"
                    .to_string(),
                min_memory_mb: 512,
                min_tier: GpuTier::Cold,
                size_mb: 80,
                approved: true,
                canonical_format: CanonicalFormat::SafetensorsFp32,
                weights_hash: None,
            },
            ModelInfo {
                id: ModelId::new("qfc-embed-medium", "v1.0"),
                description: "Medium embedding model (all-MiniLM-L12-v2, 384-dim) for Cold tier"
                    .to_string(),
                min_memory_mb: 512,
                min_tier: GpuTier::Cold,
                size_mb: 120,
                approved: true,
                canonical_format: CanonicalFormat::SafetensorsFp32,
                weights_hash: None,
            },
            ModelInfo {
                id: ModelId::new("qfc-classify-small", "v1.0"),
                description: "BERT classification model (bert-base-uncased) for Warm tier"
                    .to_string(),
                min_memory_mb: 2048,
                min_tier: GpuTier::Warm,
                size_mb: 440,
                approved: true,
                canonical_format: CanonicalFormat::SafetensorsFp32,
                weights_hash: None,
            },
            ModelInfo {
                id: ModelId::new("qfc-llm-0.5b", "v1.0"),
                description:
                    "Qwen2.5-0.5B-Instruct text generation model for Cold tier (CPU-friendly)"
                        .to_string(),
                min_memory_mb: 1024,
                min_tier: GpuTier::Cold,
                size_mb: 990,
                approved: true,
                canonical_format: CanonicalFormat::SafetensorsFp32,
                weights_hash: None,
            },
            ModelInfo {
                id: ModelId::new("qfc-llm-3b", "v1.0"),
                description:
                    "Qwen2.5-3B-Instruct-GGUF (Q4_K_M) quantized text generation for Warm tier"
                        .to_string(),
                min_memory_mb: 4096,
                min_tier: GpuTier::Warm,
                size_mb: 2200,
                approved: true,
                canonical_format: CanonicalFormat::GgufQ4KM,
                weights_hash: None,
            },
            ModelInfo {
                id: ModelId::new("qfc-llm-7b", "v1.0"),
                description:
                    "Qwen2.5-7B-Instruct-GGUF (Q4_K_M) quantized text generation for Hot tier"
                        .to_string(),
                min_memory_mb: 8192,
                min_tier: GpuTier::Hot,
                size_mb: 4700,
                approved: true,
                canonical_format: CanonicalFormat::GgufQ4KM,
                weights_hash: None,
            },
            ModelInfo {
                id: ModelId::new("qfc-whisper-base", "v1.0"),
                description: "Whisper base speech-to-text model (~150MB, Cold tier, CPU-capable)"
                    .to_string(),
                min_memory_mb: 512,
                min_tier: GpuTier::Cold,
                size_mb: 150,
                approved: true,
                canonical_format: CanonicalFormat::SafetensorsFp32,
                weights_hash: None,
            },
            ModelInfo {
                id: ModelId::new("qfc-whisper-large", "v1.0"),
                description: "Whisper large-v3 speech-to-text model (~1.5GB, Warm tier)"
                    .to_string(),
                min_memory_mb: 3072,
                min_tier: GpuTier::Warm,
                size_mb: 1500,
                approved: true,
                canonical_format: CanonicalFormat::SafetensorsFp32,
                weights_hash: None,
            },
            ModelInfo {
                id: ModelId::new("qfc-sd-1.5", "v1.0"),
                description:
                    "Stable Diffusion 1.5 image generation model (~4GB, Warm tier, GPU required)"
                        .to_string(),
                min_memory_mb: 6144,
                min_tier: GpuTier::Warm,
                size_mb: 4000,
                approved: true,
                canonical_format: CanonicalFormat::SafetensorsFp32,
                weights_hash: None,
            },
        ];

        Self { models }
    }

    /// Check if a model is approved
    pub fn is_approved(&self, model_id: &ModelId) -> bool {
        self.models.iter().any(|m| m.id == *model_id && m.approved)
    }

    /// Get model info
    pub fn get_model(&self, model_id: &ModelId) -> Option<&ModelInfo> {
        self.models.iter().find(|m| m.id == *model_id)
    }

    /// Get all approved models
    pub fn approved_models(&self) -> Vec<&ModelInfo> {
        self.models.iter().filter(|m| m.approved).collect()
    }

    /// Get the canonical format for a model (required for deterministic verification)
    pub fn canonical_format(&self, model_id: &ModelId) -> Option<CanonicalFormat> {
        self.get_model(model_id).map(|m| m.canonical_format)
    }

    /// Get models suitable for a given tier
    pub fn models_for_tier(&self, tier: GpuTier) -> Vec<&ModelInfo> {
        self.models
            .iter()
            .filter(|m| m.approved && tier_can_run(tier, m.min_tier))
            .collect()
    }

    /// Add a model to the registry
    pub fn add_model(&mut self, model: ModelInfo) {
        self.models.push(model);
    }
}

impl Default for ModelRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Check if a node's tier can run a model requiring min_tier
fn tier_can_run(node_tier: GpuTier, min_tier: GpuTier) -> bool {
    match (node_tier, min_tier) {
        (GpuTier::Hot, _) => true,
        (GpuTier::Warm, GpuTier::Hot) => false,
        (GpuTier::Warm, _) => true,
        (GpuTier::Cold, GpuTier::Cold) => true,
        (GpuTier::Cold, _) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_cache() {
        let mut cache = ModelCache::new(PathBuf::from("/tmp/models"));
        let model_id = ModelId::new("test-model", "v1");

        assert!(!cache.is_cached(&model_id));

        cache.register(
            model_id.clone(),
            PathBuf::from("/tmp/models/test-model-v1.bin"),
            1024 * 1024,
        );

        assert!(cache.is_cached(&model_id));
        assert_eq!(cache.total_size_bytes(), 1024 * 1024);
        assert_eq!(cache.list_cached().len(), 1);
        assert!(cache.get_path(&model_id).is_some());
    }

    #[test]
    fn test_model_cache_lru_eviction() {
        let mut cache = ModelCache::with_max_size(PathBuf::from("/tmp/models"), 2 * 1024 * 1024);

        // Add 3 models, each 1MB
        for i in 0..3 {
            let id = ModelId::new(&format!("model-{}", i), "v1");
            cache.register(
                id,
                PathBuf::from(format!("/tmp/models/m{}.bin", i)),
                1024 * 1024,
            );
            // Stagger last_accessed so eviction order is deterministic
            if let Some(entry) = cache
                .cached
                .get_mut(&ModelId::new(&format!("model-{}", i), "v1"))
            {
                entry.last_accessed = i as u64;
            }
        }

        assert_eq!(cache.total_size_bytes(), 3 * 1024 * 1024);

        // Evict should remove the LRU (model-0)
        let evicted = cache.evict_lru();
        assert_eq!(evicted, 1);
        assert_eq!(cache.total_size_bytes(), 2 * 1024 * 1024);
        assert!(!cache.is_cached(&ModelId::new("model-0", "v1")));
        assert!(cache.is_cached(&ModelId::new("model-1", "v1")));
        assert!(cache.is_cached(&ModelId::new("model-2", "v1")));
    }

    #[test]
    fn test_model_registry() {
        let registry = ModelRegistry::default_v2();

        let small = ModelId::new("qfc-embed-small", "v1.0");
        assert!(registry.is_approved(&small));

        let unknown = ModelId::new("unknown-model", "v1.0");
        assert!(!registry.is_approved(&unknown));

        // Cold tier: embed-small, embed-medium, llm-0.5b, whisper-base
        let cold_models = registry.models_for_tier(GpuTier::Cold);
        assert_eq!(cold_models.len(), 4);

        // Warm tier: Cold + classify-small, llm-3b, whisper-large, sd-1.5
        let warm_models = registry.models_for_tier(GpuTier::Warm);
        assert_eq!(warm_models.len(), 8);

        // Hot tier: all models (Warm + llm-7b)
        let hot_models = registry.models_for_tier(GpuTier::Hot);
        assert_eq!(hot_models.len(), 9);
    }

    #[test]
    fn test_tier_can_run() {
        assert!(tier_can_run(GpuTier::Hot, GpuTier::Cold));
        assert!(tier_can_run(GpuTier::Hot, GpuTier::Warm));
        assert!(tier_can_run(GpuTier::Hot, GpuTier::Hot));
        assert!(tier_can_run(GpuTier::Warm, GpuTier::Cold));
        assert!(tier_can_run(GpuTier::Warm, GpuTier::Warm));
        assert!(!tier_can_run(GpuTier::Warm, GpuTier::Hot));
        assert!(tier_can_run(GpuTier::Cold, GpuTier::Cold));
        assert!(!tier_can_run(GpuTier::Cold, GpuTier::Warm));
        assert!(!tier_can_run(GpuTier::Cold, GpuTier::Hot));
    }
}
