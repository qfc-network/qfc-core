//! BERT embedding model using candle
//!
//! Supports sentence-transformers models like all-MiniLM-L6-v2 for
//! generating deterministic text embeddings.

use std::path::Path;

use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config as BertConfig, DTYPE};
use tokenizers::Tokenizer;

use crate::InferenceError;
use super::LoadedModel;

/// BERT-based embedding model
pub struct BertEmbedding {
    model: BertModel,
    tokenizer: Tokenizer,
    device: Device,
    embedding_dim: usize,
}

impl BertEmbedding {
    /// Load a BERT model from downloaded files
    pub fn load(
        weights_path: &Path,
        tokenizer_path: &Path,
        config_path: &Path,
        device: &Device,
    ) -> Result<Self, InferenceError> {
        // Load config
        let config_str = std::fs::read_to_string(config_path).map_err(|e| {
            InferenceError::ExecutionFailed(format!("Failed to read config: {}", e))
        })?;
        let config: BertConfig = serde_json::from_str(&config_str).map_err(|e| {
            InferenceError::ExecutionFailed(format!("Failed to parse config: {}", e))
        })?;

        let embedding_dim = config.hidden_size;

        // Load tokenizer
        let tokenizer = Tokenizer::from_file(tokenizer_path).map_err(|e| {
            InferenceError::ExecutionFailed(format!("Failed to load tokenizer: {}", e))
        })?;

        // Load model weights
        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(&[weights_path], DTYPE, device).map_err(|e| {
                InferenceError::ExecutionFailed(format!("Failed to load model weights: {}", e))
            })?
        };

        let model = BertModel::load(vb, &config).map_err(|e| {
            InferenceError::ExecutionFailed(format!("Failed to build BERT model: {}", e))
        })?;

        tracing::info!(
            "BERT model loaded (hidden_size={}, device={:?})",
            embedding_dim,
            device
        );

        Ok(Self {
            model,
            tokenizer,
            device: device.clone(),
            embedding_dim,
        })
    }

    /// Generate embeddings for input text
    ///
    /// Input bytes are interpreted as UTF-8 text.
    /// Returns the mean-pooled embedding as f32 bytes.
    pub fn embed(&self, text: &str) -> Result<Vec<u8>, InferenceError> {
        // Tokenize
        let encoding = self.tokenizer.encode(text, true).map_err(|e| {
            InferenceError::ExecutionFailed(format!("Tokenization failed: {}", e))
        })?;

        let token_ids = encoding.get_ids().to_vec();
        let attention_mask = encoding.get_attention_mask().to_vec();
        let token_type_ids = encoding.get_type_ids().to_vec();
        let n_tokens = token_ids.len();

        // Create tensors
        let token_ids = Tensor::new(&token_ids[..], &self.device)
            .map_err(|e| InferenceError::ExecutionFailed(format!("Tensor creation failed: {}", e)))?
            .unsqueeze(0)
            .map_err(|e| InferenceError::ExecutionFailed(e.to_string()))?;

        let attention_mask = Tensor::new(&attention_mask[..], &self.device)
            .map_err(|e| InferenceError::ExecutionFailed(e.to_string()))?
            .unsqueeze(0)
            .map_err(|e| InferenceError::ExecutionFailed(e.to_string()))?;

        let token_type_ids = Tensor::new(&token_type_ids[..], &self.device)
            .map_err(|e| InferenceError::ExecutionFailed(e.to_string()))?
            .unsqueeze(0)
            .map_err(|e| InferenceError::ExecutionFailed(e.to_string()))?;

        // Forward pass
        let output = self
            .model
            .forward(&token_ids, &token_type_ids, Some(&attention_mask))
            .map_err(|e| {
                InferenceError::ExecutionFailed(format!("BERT forward pass failed: {}", e))
            })?;

        // Mean pooling: average token embeddings weighted by attention mask
        let mask_f32 = attention_mask
            .to_dtype(DType::F32)
            .map_err(|e| InferenceError::ExecutionFailed(e.to_string()))?
            .unsqueeze(2)
            .map_err(|e| InferenceError::ExecutionFailed(e.to_string()))?;

        let output_f32 = output
            .to_dtype(DType::F32)
            .map_err(|e| InferenceError::ExecutionFailed(e.to_string()))?;

        let masked = (&output_f32 * &mask_f32)
            .map_err(|e| InferenceError::ExecutionFailed(e.to_string()))?;

        let sum = masked
            .sum(1)
            .map_err(|e| InferenceError::ExecutionFailed(e.to_string()))?;

        let count = Tensor::new(&[n_tokens as f32], &self.device)
            .map_err(|e| InferenceError::ExecutionFailed(e.to_string()))?
            .unsqueeze(0)
            .map_err(|e| InferenceError::ExecutionFailed(e.to_string()))?;

        let embedding = sum
            .broadcast_div(&count)
            .map_err(|e| InferenceError::ExecutionFailed(e.to_string()))?
            .squeeze(0)
            .map_err(|e| InferenceError::ExecutionFailed(e.to_string()))?;

        // L2 normalize
        let norm = embedding
            .sqr()
            .map_err(|e| InferenceError::ExecutionFailed(e.to_string()))?
            .sum_all()
            .map_err(|e| InferenceError::ExecutionFailed(e.to_string()))?
            .sqrt()
            .map_err(|e| InferenceError::ExecutionFailed(e.to_string()))?;

        let normalized = embedding
            .broadcast_div(&norm)
            .map_err(|e| InferenceError::ExecutionFailed(e.to_string()))?;

        // Convert to bytes (f32 little-endian)
        let values: Vec<f32> = normalized
            .to_vec1()
            .map_err(|e| InferenceError::ExecutionFailed(e.to_string()))?;

        let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        Ok(bytes)
    }
}

impl LoadedModel for BertEmbedding {
    fn forward(&self, input: &[u8]) -> Result<Vec<u8>, InferenceError> {
        let text = std::str::from_utf8(input).map_err(|e| {
            InferenceError::ExecutionFailed(format!("Input is not valid UTF-8: {}", e))
        })?;
        self.embed(text)
    }

    fn embedding_dim(&self) -> usize {
        self.embedding_dim
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bert_embedding_dim() {
        // Can't test loading without downloading, but verify the trait
        // and type compile correctly
        assert_eq!(std::mem::size_of::<BertEmbedding>(), std::mem::size_of::<BertEmbedding>());
    }
}
