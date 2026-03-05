//! Supported task categories and their requirements

use qfc_inference::{ComputeTaskType, GpuTier, ModelId};
use qfc_types::Hash;
use serde::{Deserialize, Serialize};

/// Requirements for a compute task
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskRequirements {
    /// Minimum GPU tier to execute this task
    pub min_tier: GpuTier,
    /// Minimum memory in MB
    pub min_memory_mb: u64,
    /// Required model ID (if any)
    pub model_id: Option<ModelId>,
    /// Estimated FLOPS for this task
    pub estimated_flops: u64,
    /// Maximum execution time in ms before task is considered failed
    pub timeout_ms: u64,
}

/// Get requirements for a given task type
pub fn task_requirements(task_type: &ComputeTaskType) -> TaskRequirements {
    match task_type {
        ComputeTaskType::TextGeneration { model_id, max_tokens, .. } => {
            let (tier, memory) = model_tier_and_memory(&model_id.name);
            TaskRequirements {
                min_tier: tier,
                min_memory_mb: memory,
                model_id: Some(model_id.clone()),
                estimated_flops: 2 * 7_000_000_000u64 * (*max_tokens as u64),
                timeout_ms: 30_000,
            }
        }
        ComputeTaskType::ImageClassification { model_id, .. } => {
            TaskRequirements {
                min_tier: GpuTier::Cold,
                min_memory_mb: 512,
                model_id: Some(model_id.clone()),
                estimated_flops: 4_000_000_000,
                timeout_ms: 10_000,
            }
        }
        ComputeTaskType::Embedding { model_id, .. } => {
            TaskRequirements {
                min_tier: GpuTier::Cold,
                min_memory_mb: 1024,
                model_id: Some(model_id.clone()),
                estimated_flops: 1_000_000_000,
                timeout_ms: 10_000,
            }
        }
        ComputeTaskType::OnnxInference { .. } => {
            TaskRequirements {
                min_tier: GpuTier::Cold,
                min_memory_mb: 1024,
                model_id: None,
                estimated_flops: 2_000_000_000,
                timeout_ms: 15_000,
            }
        }
    }
}

/// Determine tier and memory requirements from model name
fn model_tier_and_memory(model_name: &str) -> (GpuTier, u64) {
    if model_name.contains("70b") || model_name.contains("70B") {
        (GpuTier::Hot, 40_000)
    } else if model_name.contains("13b") || model_name.contains("13B") {
        (GpuTier::Warm, 10_000)
    } else if model_name.contains("7b") || model_name.contains("7B") {
        (GpuTier::Warm, 6_000)
    } else if model_name.contains("3b") || model_name.contains("3B") {
        (GpuTier::Cold, 3_000)
    } else {
        (GpuTier::Cold, 2_000)
    }
}

/// Generate a synthetic benchmark task for a given tier
pub fn synthetic_task_for_tier(tier: GpuTier, _epoch: u64, seed: u64) -> ComputeTaskType {
    match tier {
        GpuTier::Hot => ComputeTaskType::TextGeneration {
            model_id: ModelId::new("qfc-bench-large", "v1.0"),
            prompt_hash: Hash::new(synthetic_hash(seed, 0)),
            max_tokens: 256,
            temperature_fp: 0,
            seed,
        },
        GpuTier::Warm => ComputeTaskType::TextGeneration {
            model_id: ModelId::new("qfc-bench-medium", "v1.0"),
            prompt_hash: Hash::new(synthetic_hash(seed, 1)),
            max_tokens: 128,
            temperature_fp: 0,
            seed,
        },
        GpuTier::Cold => ComputeTaskType::Embedding {
            model_id: ModelId::new("qfc-bench-small", "v1.0"),
            input_hash: Hash::new(synthetic_hash(seed, 2)),
        },
    }
}

/// Generate a deterministic hash from seed and index
fn synthetic_hash(seed: u64, index: u8) -> [u8; 32] {
    let mut data = Vec::with_capacity(9);
    data.extend_from_slice(&seed.to_le_bytes());
    data.push(index);
    let hash = qfc_crypto::blake3_hash(&data);
    *hash.as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_task_requirements_text_gen() {
        let task = ComputeTaskType::TextGeneration {
            model_id: ModelId::new("llama-7b", "v1"),
            prompt_hash: Hash::ZERO,
            max_tokens: 100,
            temperature_fp: 0,
            seed: 42,
        };
        let reqs = task_requirements(&task);
        assert_eq!(reqs.min_tier, GpuTier::Warm);
        assert_eq!(reqs.min_memory_mb, 6000);
    }

    #[test]
    fn test_synthetic_tasks() {
        let hot_task = synthetic_task_for_tier(GpuTier::Hot, 1, 42);
        assert!(matches!(hot_task, ComputeTaskType::TextGeneration { .. }));

        let cold_task = synthetic_task_for_tier(GpuTier::Cold, 1, 42);
        assert!(matches!(cold_task, ComputeTaskType::Embedding { .. }));
    }
}
