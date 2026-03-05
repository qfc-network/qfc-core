//! Inference task definitions

use borsh::{BorshDeserialize, BorshSerialize};
use qfc_types::Hash;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Model identifier (registry name + version)
#[derive(
    Clone, Debug, PartialEq, Eq, Hash, BorshSerialize, BorshDeserialize, Serialize, Deserialize,
)]
pub struct ModelId {
    /// Model name (e.g. "llama-7b", "bert-base")
    pub name: String,
    /// Model version hash (content-addressed)
    pub version: String,
}

impl ModelId {
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
        }
    }
}

impl fmt::Display for ModelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.name, self.version)
    }
}

/// Compute task types supported by the network
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub enum ComputeTaskType {
    /// Text generation (LLM inference)
    TextGeneration {
        model_id: ModelId,
        /// Hash of input prompt
        prompt_hash: Hash,
        max_tokens: u32,
        /// Must be 0.0 for deterministic output (stored as fixed-point: 0 = 0.0)
        temperature_fp: u32,
        /// Deterministic seed
        seed: u64,
    },
    /// Image classification
    ImageClassification { model_id: ModelId, input_hash: Hash },
    /// Embedding generation
    Embedding { model_id: ModelId, input_hash: Hash },
    /// Generic ONNX model execution
    OnnxInference {
        /// Hash of ONNX model file
        model_hash: Hash,
        input_hash: Hash,
    },
}

impl ComputeTaskType {
    /// Get the model ID for this task (if applicable)
    pub fn model_id(&self) -> Option<&ModelId> {
        match self {
            Self::TextGeneration { model_id, .. } => Some(model_id),
            Self::ImageClassification { model_id, .. } => Some(model_id),
            Self::Embedding { model_id, .. } => Some(model_id),
            Self::OnnxInference { .. } => None,
        }
    }

    /// Get a short description of this task type
    pub fn task_type_name(&self) -> &'static str {
        match self {
            Self::TextGeneration { .. } => "text_generation",
            Self::ImageClassification { .. } => "image_classification",
            Self::Embedding { .. } => "embedding",
            Self::OnnxInference { .. } => "onnx_inference",
        }
    }
}

/// An inference task to be executed by a miner
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub struct InferenceTask {
    /// Unique task ID
    pub task_id: Hash,
    /// Epoch this task belongs to
    pub epoch: u64,
    /// The compute task specification
    pub task_type: ComputeTaskType,
    /// Input data (serialized)
    pub input_data: Vec<u8>,
    /// Timestamp when task was created
    pub created_at: u64,
    /// Deadline for task completion
    pub deadline: u64,
}

impl InferenceTask {
    pub fn new(
        task_id: Hash,
        epoch: u64,
        task_type: ComputeTaskType,
        input_data: Vec<u8>,
        created_at: u64,
        deadline: u64,
    ) -> Self {
        Self {
            task_id,
            epoch,
            task_type,
            input_data,
            created_at,
            deadline,
        }
    }

    /// Check if the task is still within its deadline
    pub fn is_active(&self, current_time: u64) -> bool {
        current_time >= self.created_at && current_time < self.deadline
    }

    /// Minimum GPU memory in MB required for this task
    pub fn min_memory_mb(&self) -> u64 {
        match &self.task_type {
            ComputeTaskType::TextGeneration { model_id, .. } => {
                estimate_model_memory(&model_id.name)
            }
            ComputeTaskType::ImageClassification { .. } => 512,
            ComputeTaskType::Embedding { model_id, .. } => {
                estimate_model_memory(&model_id.name).min(4096)
            }
            ComputeTaskType::OnnxInference { .. } => 1024,
        }
    }
}

/// Estimate memory required for a model by name (rough heuristic)
fn estimate_model_memory(model_name: &str) -> u64 {
    if model_name.contains("70b") || model_name.contains("70B") {
        40_000
    } else if model_name.contains("13b") || model_name.contains("13B") {
        10_000
    } else if model_name.contains("7b") || model_name.contains("7B") {
        6_000
    } else if model_name.contains("3b") || model_name.contains("3B") {
        3_000
    } else if model_name.contains("1b") || model_name.contains("1B") {
        2_000
    } else {
        4_000 // default estimate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_id_display() {
        let id = ModelId::new("llama-7b", "v1.0");
        assert_eq!(format!("{}", id), "llama-7b@v1.0");
    }

    #[test]
    fn test_task_type_name() {
        let task = ComputeTaskType::TextGeneration {
            model_id: ModelId::new("llama-7b", "v1.0"),
            prompt_hash: Hash::ZERO,
            max_tokens: 100,
            temperature_fp: 0,
            seed: 42,
        };
        assert_eq!(task.task_type_name(), "text_generation");
    }

    #[test]
    fn test_inference_task_active() {
        let task = InferenceTask::new(
            Hash::ZERO,
            1,
            ComputeTaskType::Embedding {
                model_id: ModelId::new("bert-base", "v1"),
                input_hash: Hash::ZERO,
            },
            vec![],
            1000,
            2000,
        );
        assert!(!task.is_active(999));
        assert!(task.is_active(1000));
        assert!(task.is_active(1500));
        assert!(!task.is_active(2000));
    }

    #[test]
    fn test_min_memory_estimation() {
        let task_7b = InferenceTask::new(
            Hash::ZERO,
            1,
            ComputeTaskType::TextGeneration {
                model_id: ModelId::new("llama-7b", "v1"),
                prompt_hash: Hash::ZERO,
                max_tokens: 100,
                temperature_fp: 0,
                seed: 42,
            },
            vec![],
            0,
            1000,
        );
        assert_eq!(task_7b.min_memory_mb(), 6000);

        let task_70b = InferenceTask::new(
            Hash::ZERO,
            1,
            ComputeTaskType::TextGeneration {
                model_id: ModelId::new("llama-70b", "v1"),
                prompt_hash: Hash::ZERO,
                max_tokens: 100,
                temperature_fp: 0,
                seed: 42,
            },
            vec![],
            0,
            1000,
        );
        assert_eq!(task_70b.min_memory_mb(), 40000);
    }
}
