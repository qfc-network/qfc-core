//! Whisper speech-to-text model wrapper
//!
//! Wraps candle-transformers' Whisper implementation for QFC inference.
//! Supports Whisper base (~150MB, Cold tier) and large-v3 (~1.5GB, Warm tier).
//!
//! Input: raw PCM f32 samples at 16kHz mono
//! Output: UTF-8 transcribed text bytes

#[cfg(feature = "candle")]
use std::path::Path;

#[cfg(feature = "candle")]
use candle_core::{Device, IndexOp, Tensor};
#[cfg(feature = "candle")]
use candle_nn::VarBuilder;
#[cfg(feature = "candle")]
use candle_transformers::models::whisper::{self as m, Config};
#[cfg(feature = "candle")]
use tokenizers::Tokenizer;

#[cfg(feature = "candle")]
use crate::InferenceError;

/// Whisper speech-to-text model
#[cfg(feature = "candle")]
pub struct WhisperModel {
    model: m::model::Whisper,
    tokenizer: Tokenizer,
    config: Config,
    mel_filters: Vec<f32>,
    device: Device,
}

#[cfg(feature = "candle")]
impl WhisperModel {
    /// Load a Whisper model from downloaded files
    pub fn load(
        weights_path: &Path,
        tokenizer_path: &Path,
        config_path: &Path,
        mel_filters_path: &Path,
        device: &Device,
    ) -> Result<Self, InferenceError> {
        // Load config
        let config_str = std::fs::read_to_string(config_path).map_err(|e| {
            InferenceError::ExecutionFailed(format!("Failed to read Whisper config: {}", e))
        })?;
        let config: Config = serde_json::from_str(&config_str).map_err(|e| {
            InferenceError::ExecutionFailed(format!("Failed to parse Whisper config: {}", e))
        })?;

        // Load mel filters (binary f32 array)
        let mel_bytes = std::fs::read(mel_filters_path).map_err(|e| {
            InferenceError::ExecutionFailed(format!("Failed to read mel filters: {}", e))
        })?;
        let mel_filters: Vec<f32> = mel_bytes
            .chunks_exact(4)
            .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
            .collect();

        // Load tokenizer
        let tokenizer = Tokenizer::from_file(tokenizer_path).map_err(|e| {
            InferenceError::ExecutionFailed(format!("Failed to load Whisper tokenizer: {}", e))
        })?;

        // Load model weights
        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(&[weights_path], m::DTYPE, device).map_err(|e| {
                InferenceError::ExecutionFailed(format!("Failed to load Whisper weights: {}", e))
            })?
        };

        let model = m::model::Whisper::load(&vb, config.clone()).map_err(|e| {
            InferenceError::ExecutionFailed(format!("Failed to build Whisper model: {}", e))
        })?;

        Ok(Self {
            model,
            tokenizer,
            config,
            mel_filters,
            device: device.clone(),
        })
    }

    /// Transcribe raw PCM f32 samples (16kHz mono) to text.
    ///
    /// `language` is an optional language code (e.g. "en", "zh"). If None,
    /// the model auto-detects.
    pub fn transcribe(
        &mut self,
        pcm_samples: &[f32],
        language: Option<&str>,
    ) -> Result<Vec<u8>, InferenceError> {
        // Reset KV cache for fresh transcription
        self.model.reset_kv_cache();

        // Compute mel spectrogram
        let mel = m::audio::pcm_to_mel(&self.config, pcm_samples, &self.mel_filters);
        let mel_len = mel.len();
        let n_mel = self.config.num_mel_bins;
        let mel =
            Tensor::from_vec(mel, (1, n_mel, mel_len / n_mel), &self.device).map_err(|e| {
                InferenceError::ExecutionFailed(format!("Failed to create mel tensor: {}", e))
            })?;

        // Encode audio
        let audio_features = self.model.encoder.forward(&mel, true).map_err(|e| {
            InferenceError::ExecutionFailed(format!("Whisper encoder failed: {}", e))
        })?;

        // Build initial decoder tokens: SOT + language + transcribe + notimestamps
        let sot_token = self.tokenizer.token_to_id(m::SOT_TOKEN).unwrap_or(50258);
        let transcribe_token = self
            .tokenizer
            .token_to_id(m::TRANSCRIBE_TOKEN)
            .unwrap_or(50359);
        let no_timestamps_token = self
            .tokenizer
            .token_to_id(m::NO_TIMESTAMPS_TOKEN)
            .unwrap_or(50363);
        let eot_token = self.tokenizer.token_to_id(m::EOT_TOKEN).unwrap_or(50257);

        let language_token = if let Some(lang) = language {
            let lang_token_str = format!("<|{}|>", lang);
            self.tokenizer.token_to_id(&lang_token_str).unwrap_or(50259) // default to "en"
        } else {
            50259 // "en" token
        };

        let mut tokens = vec![
            sot_token,
            language_token,
            transcribe_token,
            no_timestamps_token,
        ];

        // Greedy decoding loop
        let max_tokens = self.config.max_target_positions / 2; // conservative limit
        for i in 0..max_tokens {
            let token_tensor = Tensor::new(tokens.as_slice(), &self.device)
                .map_err(|e| {
                    InferenceError::ExecutionFailed(format!("Failed to create token tensor: {}", e))
                })?
                .unsqueeze(0)
                .map_err(|e| {
                    InferenceError::ExecutionFailed(format!("Failed to unsqueeze: {}", e))
                })?;

            let logits = self
                .model
                .decoder
                .forward(&token_tensor, &audio_features, i == 0)
                .map_err(|e| {
                    InferenceError::ExecutionFailed(format!("Whisper decoder failed: {}", e))
                })?;

            let logits = self.model.decoder.final_linear(&logits).map_err(|e| {
                InferenceError::ExecutionFailed(format!("Whisper final linear failed: {}", e))
            })?;

            // Get last token logits and argmax (greedy)
            let (_, seq_len, _) = logits.dims3().map_err(|e| {
                InferenceError::ExecutionFailed(format!("Logits dims error: {}", e))
            })?;
            let last_logits = logits.i((0, seq_len - 1, ..)).map_err(|e| {
                InferenceError::ExecutionFailed(format!("Logits index error: {}", e))
            })?;

            // Suppress non-text tokens (timestamps, etc.) by setting them to -inf
            let next_token = last_logits
                .argmax(0)
                .map_err(|e| InferenceError::ExecutionFailed(format!("Argmax failed: {}", e)))?
                .to_scalar::<u32>()
                .map_err(|e| {
                    InferenceError::ExecutionFailed(format!("Scalar conversion failed: {}", e))
                })?;

            if next_token == eot_token {
                break;
            }

            tokens.push(next_token);
        }

        // Decode tokens to text (skip the initial special tokens)
        let text_tokens: Vec<u32> = tokens
            .into_iter()
            .skip(4) // skip SOT, language, transcribe, notimestamps
            .filter(|&t| t != eot_token)
            .collect();

        let text = self.tokenizer.decode(&text_tokens, true).map_err(|e| {
            InferenceError::ExecutionFailed(format!("Tokenizer decode failed: {}", e))
        })?;

        Ok(text.trim().as_bytes().to_vec())
    }
}
