//! Model registry (approved models list, controlled by governance)

use qfc_inference::model::{ModelInfo, ModelRegistry};
use qfc_inference::{GpuTier, ModelId};

/// Create the default model registry for QFC v2.0
pub fn default_registry() -> ModelRegistry {
    ModelRegistry::default_v2()
}

/// Check if a model is approved for network use
pub fn is_model_approved(registry: &ModelRegistry, model_id: &ModelId) -> bool {
    registry.is_approved(model_id)
}

/// Get models that a miner with given tier can execute
pub fn available_models_for_tier(registry: &ModelRegistry, tier: GpuTier) -> Vec<&ModelInfo> {
    registry.models_for_tier(tier)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_registry_has_models() {
        let registry = default_registry();
        assert!(!registry.approved_models().is_empty());
    }

    #[test]
    fn test_cold_tier_limited_models() {
        let registry = default_registry();
        let cold_models = available_models_for_tier(&registry, GpuTier::Cold);
        // Cold tier should only see small models
        assert!(cold_models.len() < registry.approved_models().len());
    }
}
