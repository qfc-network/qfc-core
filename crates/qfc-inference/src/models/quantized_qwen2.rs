//! Quantized Qwen2 text generation model using GGUF format
//!
//! Supports Q4_K_M and other GGUF quantization formats for running
//! larger models (3B, 7B) on consumer hardware with reduced VRAM.
//! Uses greedy decoding (argmax) for deterministic, verifiable output.

use std::path::Path;

use candle_core::{Device, Tensor};
use candle_transformers::models::quantized_qwen2::ModelWeights;
use tokenizers::Tokenizer;

use crate::InferenceError;

/// Quantized Qwen2 text generation model (GGUF format)
pub struct QuantizedQwen2TextGen {
    model: ModelWeights,
    tokenizer: Tokenizer,
    device: Device,
    eos_token_id: u32,
}

impl QuantizedQwen2TextGen {
    /// Load a quantized Qwen2 model from a GGUF file + tokenizer
    pub fn load(
        gguf_path: &Path,
        tokenizer_path: &Path,
        device: &Device,
    ) -> Result<Self, InferenceError> {
        // Load GGUF model
        let mut file = std::fs::File::open(gguf_path).map_err(|e| {
            InferenceError::ExecutionFailed(format!("Failed to open GGUF file: {}", e))
        })?;

        let content = candle_core::quantized::gguf_file::Content::read(&mut file).map_err(|e| {
            InferenceError::ExecutionFailed(format!("Failed to read GGUF content: {}", e))
        })?;

        let model = ModelWeights::from_gguf(content, &mut file, device).map_err(|e| {
            InferenceError::ExecutionFailed(format!("Failed to load quantized Qwen2: {}", e))
        })?;

        // Load tokenizer
        let tokenizer = Tokenizer::from_file(tokenizer_path).map_err(|e| {
            InferenceError::ExecutionFailed(format!("Failed to load tokenizer: {}", e))
        })?;

        let eos_token_id = tokenizer
            .token_to_id("<|endoftext|>")
            .or_else(|| tokenizer.token_to_id("<|im_end|>"))
            .unwrap_or(151643);

        tracing::info!("Quantized Qwen2 model loaded from {:?}", gguf_path);

        Ok(Self {
            model,
            tokenizer,
            device: device.clone(),
            eos_token_id,
        })
    }

    /// Generate text using greedy decoding (deterministic)
    pub fn generate(&mut self, prompt: &str, max_tokens: u32) -> Result<Vec<u8>, InferenceError> {
        let encoding = self
            .tokenizer
            .encode(prompt, true)
            .map_err(|e| InferenceError::ExecutionFailed(format!("Tokenization failed: {}", e)))?;

        let mut tokens = encoding.get_ids().to_vec();
        let mut generated_tokens: Vec<u32> = Vec::new();

        // Prefill: process all prompt tokens at once
        let input = Tensor::new(tokens.as_slice(), &self.device)
            .map_err(|e| InferenceError::ExecutionFailed(format!("Tensor error: {}", e)))?
            .unsqueeze(0)
            .map_err(|e| InferenceError::ExecutionFailed(e.to_string()))?;

        let logits = self
            .model
            .forward(&input, 0)
            .map_err(|e| InferenceError::ExecutionFailed(format!("Forward pass failed: {}", e)))?;

        let next_token = logits
            .squeeze(0)
            .map_err(|e| InferenceError::ExecutionFailed(e.to_string()))?
            .argmax(candle_core::D::Minus1)
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
            let logits = self.model.forward(&input, seqlen_offset).map_err(|e| {
                InferenceError::ExecutionFailed(format!("Decode step failed: {}", e))
            })?;

            let next_token = logits
                .squeeze(0)
                .map_err(|e| InferenceError::ExecutionFailed(e.to_string()))?
                .argmax(candle_core::D::Minus1)
                .map_err(|e| InferenceError::ExecutionFailed(e.to_string()))?
                .to_scalar::<u32>()
                .map_err(|e| InferenceError::ExecutionFailed(e.to_string()))?;

            if next_token == self.eos_token_id {
                break;
            }

            generated_tokens.push(next_token);
            tokens.push(next_token);
        }

        let text = self
            .tokenizer
            .decode(&generated_tokens, true)
            .map_err(|e| InferenceError::ExecutionFailed(format!("Detokenize failed: {}", e)))?;

        Ok(text.into_bytes())
    }
}

impl super::LoadedModel for QuantizedQwen2TextGen {
    fn forward(&self, _input: &[u8]) -> Result<Vec<u8>, InferenceError> {
        Err(InferenceError::ExecutionFailed(
            "Use generate() for text generation tasks".to_string(),
        ))
    }

    fn embedding_dim(&self) -> usize {
        0
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_quantized_qwen2_type_compiles() {
        assert!(std::mem::size_of::<super::QuantizedQwen2TextGen>() > 0);
    }
}
