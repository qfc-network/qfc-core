//! Three-layer model scheduling: Hot / Warm / Cold VRAM management
//!
//! - Hot: Models loaded in VRAM, ready for instant inference
//! - Warm: One model kept in VRAM for fast swap
//! - Cold: Cached on disk, requires load time

use std::path::PathBuf;

use lru::LruCache;
use std::num::NonZeroUsize;

use crate::task::ModelId;
use crate::InferenceEngine;

/// Model layer classification
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelLayer {
    Hot,
    Warm,
    Cold,
}

impl std::fmt::Display for ModelLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ModelLayer::Hot => write!(f, "hot"),
            ModelLayer::Warm => write!(f, "warm"),
            ModelLayer::Cold => write!(f, "cold"),
        }
    }
}

/// A model currently loaded in VRAM
#[derive(Clone, Debug)]
pub struct LoadedModel {
    pub model_id: ModelId,
    pub layer: ModelLayer,
    pub vram_mb: u32,
}

/// VRAM budget allocation
#[derive(Clone, Debug)]
pub struct VramBudget {
    pub total_mb: u32,
    pub hot_budget_mb: u32,
    pub warm_budget_mb: u32,
    pub reserved_mb: u32,
}

impl VramBudget {
    pub fn new(total_mb: u32, reserved_mb: u32) -> Self {
        let available = total_mb.saturating_sub(reserved_mb);
        // 70% hot, 30% warm
        let hot_budget = available * 70 / 100;
        let warm_budget = available - hot_budget;
        Self {
            total_mb,
            hot_budget_mb: hot_budget,
            warm_budget_mb: warm_budget,
            reserved_mb,
        }
    }

    pub fn available_for_hot(&self) -> u32 {
        self.hot_budget_mb
    }

    pub fn available_for_warm(&self) -> u32 {
        self.warm_budget_mb
    }
}

/// Cached model info on disk
#[derive(Clone, Debug)]
pub struct CachedModelInfo {
    pub model_id: ModelId,
    pub disk_path: PathBuf,
    pub vram_required_mb: u32,
}

/// Three-layer model scheduler
pub struct ModelScheduler {
    hot_models: Vec<LoadedModel>,
    warm_model: Option<LoadedModel>,
    cold_cache: LruCache<String, CachedModelInfo>,
    #[allow(dead_code)]
    vram_budget: VramBudget,
}

impl ModelScheduler {
    pub fn new(vram_budget: VramBudget, cold_cache_size: usize) -> Self {
        Self {
            hot_models: Vec::new(),
            warm_model: None,
            cold_cache: LruCache::new(NonZeroUsize::new(cold_cache_size.max(1)).unwrap()),
            vram_budget,
        }
    }

    /// Get the layer of a model (None if not tracked)
    pub fn model_layer(&self, model_id: &ModelId) -> Option<ModelLayer> {
        if self.hot_models.iter().any(|m| m.model_id == *model_id) {
            return Some(ModelLayer::Hot);
        }
        if let Some(ref warm) = self.warm_model {
            if warm.model_id == *model_id {
                return Some(ModelLayer::Warm);
            }
        }
        let key = model_id.to_string();
        if self.cold_cache.peek(&key).is_some() {
            return Some(ModelLayer::Cold);
        }
        None
    }

    /// Report all currently loaded models
    pub fn report_loaded_models(&self) -> Vec<(ModelId, ModelLayer)> {
        let mut result: Vec<(ModelId, ModelLayer)> = self
            .hot_models
            .iter()
            .map(|m| (m.model_id.clone(), ModelLayer::Hot))
            .collect();

        if let Some(ref warm) = self.warm_model {
            result.push((warm.model_id.clone(), ModelLayer::Warm));
        }

        for (_, info) in self.cold_cache.iter() {
            result.push((info.model_id.clone(), ModelLayer::Cold));
        }

        result
    }

    /// Ensure a model is loaded, using the scheduler's layer management.
    /// Returns the layer the model ended up in.
    pub async fn ensure_model_loaded(
        &mut self,
        model_id: &ModelId,
        engine: &mut dyn InferenceEngine,
    ) -> Result<ModelLayer, crate::InferenceError> {
        // Already hot?
        if self.hot_models.iter().any(|m| m.model_id == *model_id) {
            return Ok(ModelLayer::Hot);
        }

        // Already warm? Promote to hot
        if let Some(ref warm) = self.warm_model {
            if warm.model_id == *model_id {
                let warm = self.warm_model.take().unwrap();
                self.hot_models.push(LoadedModel {
                    model_id: warm.model_id,
                    layer: ModelLayer::Hot,
                    vram_mb: warm.vram_mb,
                });
                return Ok(ModelLayer::Hot);
            }
        }

        // Need to load — evict warm if necessary to make room
        self.evict_warm();

        // Load the model
        engine.load_model(model_id).await?;

        // Add as warm (will be promoted to hot on next use)
        let vram_estimate = 1000; // default estimate
        self.warm_model = Some(LoadedModel {
            model_id: model_id.clone(),
            layer: ModelLayer::Warm,
            vram_mb: vram_estimate,
        });

        Ok(ModelLayer::Warm)
    }

    /// Add a model as hot (pre-loaded at startup)
    pub fn add_hot_model(&mut self, model_id: ModelId, vram_mb: u32) {
        if !self.hot_models.iter().any(|m| m.model_id == model_id) {
            self.hot_models.push(LoadedModel {
                model_id,
                layer: ModelLayer::Hot,
                vram_mb,
            });
        }
    }

    /// Evict the warm model to cold cache
    pub fn evict_warm(&mut self) {
        if let Some(warm) = self.warm_model.take() {
            let key = warm.model_id.to_string();
            self.cold_cache.put(
                key,
                CachedModelInfo {
                    model_id: warm.model_id,
                    disk_path: PathBuf::new(), // models are already on disk
                    vram_required_mb: warm.vram_mb,
                },
            );
        }
    }

    /// Register a model in the cold cache
    pub fn register_cold_model(&mut self, info: CachedModelInfo) {
        let key = info.model_id.to_string();
        self.cold_cache.put(key, info);
    }

    /// Total VRAM used by hot + warm models
    pub fn vram_used(&self) -> u32 {
        let hot: u32 = self.hot_models.iter().map(|m| m.vram_mb).sum();
        let warm: u32 = self.warm_model.as_ref().map(|m| m.vram_mb).unwrap_or(0);
        hot + warm
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_layer_hot() {
        let budget = VramBudget::new(16000, 800);
        let mut sched = ModelScheduler::new(budget, 10);

        let model_a = ModelId::new("model-a", "v1");
        sched.add_hot_model(model_a.clone(), 2000);

        assert_eq!(sched.model_layer(&model_a), Some(ModelLayer::Hot));
    }

    #[test]
    fn test_model_layer_cold() {
        let budget = VramBudget::new(16000, 800);
        let mut sched = ModelScheduler::new(budget, 10);

        let model_b = ModelId::new("model-b", "v1");
        sched.register_cold_model(CachedModelInfo {
            model_id: model_b.clone(),
            disk_path: PathBuf::from("/tmp/model-b"),
            vram_required_mb: 4000,
        });

        assert_eq!(sched.model_layer(&model_b), Some(ModelLayer::Cold));
    }

    #[test]
    fn test_report_loaded_models() {
        let budget = VramBudget::new(16000, 800);
        let mut sched = ModelScheduler::new(budget, 10);

        let model_a = ModelId::new("model-a", "v1");
        let model_b = ModelId::new("model-b", "v1");
        sched.add_hot_model(model_a.clone(), 2000);
        sched.register_cold_model(CachedModelInfo {
            model_id: model_b.clone(),
            disk_path: PathBuf::from("/tmp/b"),
            vram_required_mb: 3000,
        });

        let models = sched.report_loaded_models();
        assert_eq!(models.len(), 2);
        assert!(models
            .iter()
            .any(|(id, l)| *id == model_a && *l == ModelLayer::Hot));
        assert!(models
            .iter()
            .any(|(id, l)| *id == model_b && *l == ModelLayer::Cold));
    }

    #[test]
    fn test_evict_warm_to_cold() {
        let budget = VramBudget::new(16000, 800);
        let mut sched = ModelScheduler::new(budget, 10);

        let model_c = ModelId::new("model-c", "v1");
        sched.warm_model = Some(LoadedModel {
            model_id: model_c.clone(),
            layer: ModelLayer::Warm,
            vram_mb: 3000,
        });

        assert_eq!(sched.model_layer(&model_c), Some(ModelLayer::Warm));
        sched.evict_warm();
        assert_eq!(sched.model_layer(&model_c), Some(ModelLayer::Cold));
        assert!(sched.warm_model.is_none());
    }

    #[test]
    fn test_vram_budget() {
        let budget = VramBudget::new(16000, 800);
        assert_eq!(budget.total_mb, 16000);
        assert_eq!(budget.reserved_mb, 800);
        // 15200 available, 70% = 10640 hot, 30% = 4560 warm
        assert_eq!(budget.hot_budget_mb, 10640);
        assert_eq!(budget.warm_budget_mb, 4560);
    }

    #[test]
    fn test_vram_used() {
        let budget = VramBudget::new(16000, 800);
        let mut sched = ModelScheduler::new(budget, 10);

        sched.add_hot_model(ModelId::new("a", "v1"), 2000);
        sched.add_hot_model(ModelId::new("b", "v1"), 3000);
        sched.warm_model = Some(LoadedModel {
            model_id: ModelId::new("c", "v1"),
            layer: ModelLayer::Warm,
            vram_mb: 1500,
        });

        assert_eq!(sched.vram_used(), 6500);
    }
}
