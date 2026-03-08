//! Qwen2 text generation model using candle
//!
//! Supports Qwen2.5-0.5B for deterministic text generation.
//! Uses greedy decoding (argmax) when temperature=0 to ensure
//! reproducible outputs for proof verification (spot-check).

use std::path::Path;

use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::qwen2::{Config as Qwen2Config, ModelForCausalLM};
use tokenizers::Tokenizer;

use crate::InferenceError;

/// Qwen2-based text generation model
pub struct Qwen2TextGen {
    model: ModelForCausalLM,
    tokenizer: Tokenizer,
    device: Device,
    /// EOS token ID for stopping generation
    eos_token_id: u32,
}

impl Qwen2TextGen {
    /// Load a Qwen2 model from downloaded files
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
        let config: Qwen2Config = serde_json::from_str(&config_str).map_err(|e| {
            InferenceError::ExecutionFailed(format!("Failed to parse Qwen2 config: {}", e))
        })?;

        // Load tokenizer
        let tokenizer = Tokenizer::from_file(tokenizer_path).map_err(|e| {
            InferenceError::ExecutionFailed(format!("Failed to load tokenizer: {}", e))
        })?;

        // Determine EOS token ID
        let eos_token_id = tokenizer
            .token_to_id("<|endoftext|>")
            .or_else(|| tokenizer.token_to_id("<|im_end|>"))
            .unwrap_or(151643); // Qwen2 default EOS

        // Load model weights
        let dtype = if device.is_cpu() {
            DType::F32
        } else {
            DType::F16
        };

        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(&[weights_path], dtype, device).map_err(|e| {
                InferenceError::ExecutionFailed(format!("Failed to load model weights: {}", e))
            })?
        };

        let model = ModelForCausalLM::new(&config, vb).map_err(|e| {
            InferenceError::ExecutionFailed(format!("Failed to build Qwen2 model: {}", e))
        })?;

        tracing::info!(
            "Qwen2 model loaded (hidden_size={}, layers={}, device={:?})",
            config.hidden_size,
            config.num_hidden_layers,
            device
        );

        Ok(Self {
            model,
            tokenizer,
            device: device.clone(),
            eos_token_id,
        })
    }

    /// Generate text from a prompt using greedy decoding (deterministic).
    ///
    /// Returns generated token bytes (UTF-8 encoded text).
    /// Temperature must be 0 for deterministic output suitable for spot-check.
    pub fn generate(
        &mut self,
        prompt: &str,
        max_tokens: u32,
    ) -> Result<Vec<u8>, InferenceError> {
        // Tokenize prompt
        let encoding = self.tokenizer.encode(prompt, true).map_err(|e| {
            InferenceError::ExecutionFailed(format!("Tokenization failed: {}", e))
        })?;

        let prompt_tokens = encoding.get_ids().to_vec();
        let prompt_len = prompt_tokens.len();

        // Create input tensor
        let mut tokens = prompt_tokens;
        let mut generated_tokens: Vec<u32> = Vec::new();

        // Clear KV cache for fresh generation
        self.model.clear_kv_cache();

        // Process prompt (prefill)
        let input = Tensor::new(tokens.as_slice(), &self.device)
            .map_err(|e| InferenceError::ExecutionFailed(format!("Tensor error: {}", e)))?
            .unsqueeze(0)
            .map_err(|e| InferenceError::ExecutionFailed(e.to_string()))?;

        let logits = self
            .model
            .forward(&input, 0)
            .map_err(|e| InferenceError::ExecutionFailed(format!("Forward pass failed: {}", e)))?;

        // Greedy: pick argmax of last token logits
        let next_token = logits
            .squeeze(0)
            .map_err(|e| InferenceError::ExecutionFailed(e.to_string()))?
            .squeeze(0)
            .map_err(|e| InferenceError::ExecutionFailed(e.to_string()))?
            .argmax(0)
            .map_err(|e| InferenceError::ExecutionFailed(e.to_string()))?
            .to_scalar::<u32>()
            .map_err(|e| InferenceError::ExecutionFailed(e.to_string()))?;

        if next_token == self.eos_token_id {
            return Ok(Vec::new());
        }
        generated_tokens.push(next_token);
        tokens.push(next_token);

        // Autoregressive decoding
        for _ in 1..max_tokens {
            let input = Tensor::new(&[*tokens.last().unwrap()], &self.device)
                .map_err(|e| InferenceError::ExecutionFailed(e.to_string()))?
                .unsqueeze(0)
                .map_err(|e| InferenceError::ExecutionFailed(e.to_string()))?;

            let seqlen_offset = tokens.len() - 1;
            let logits = self
                .model
                .forward(&input, seqlen_offset)
                .map_err(|e| {
                    InferenceError::ExecutionFailed(format!("Decode step failed: {}", e))
                })?;

            let next_token = logits
                .squeeze(0)
                .map_err(|e| InferenceError::ExecutionFailed(e.to_string()))?
                .squeeze(0)
                .map_err(|e| InferenceError::ExecutionFailed(e.to_string()))?
                .argmax(0)
                .map_err(|e| InferenceError::ExecutionFailed(e.to_string()))?
                .to_scalar::<u32>()
                .map_err(|e| InferenceError::ExecutionFailed(e.to_string()))?;

            if next_token == self.eos_token_id {
                break;
            }

            generated_tokens.push(next_token);
            tokens.push(next_token);
        }

        // Decode generated tokens to text
        let text = self
            .tokenizer
            .decode(&generated_tokens, true)
            .map_err(|e| InferenceError::ExecutionFailed(format!("Detokenize failed: {}", e)))?;

        Ok(text.into_bytes())
    }
}

/// Implement LoadedModel for Qwen2 (text generation mode)
///
/// `forward()` interprets the input as:
///   - First 4 bytes (u32 LE): max_tokens
///   - Remaining bytes: UTF-8 prompt text
impl super::LoadedModel for Qwen2TextGen {
    fn forward(&self, input: &[u8]) -> Result<Vec<u8>, InferenceError> {
        // We need &mut self for KV cache, so this trait method can't work
        // directly. Text generation uses generate() via the engine instead.
        Err(InferenceError::ExecutionFailed(
            "Use generate() for text generation tasks".to_string(),
        ))
    }

    fn embedding_dim(&self) -> usize {
        0 // Not an embedding model
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_qwen2_type_compiles() {
        // Verify the types compile correctly without needing model download
        assert!(std::mem::size_of::<super::Qwen2TextGen>() > 0);
    }
}
