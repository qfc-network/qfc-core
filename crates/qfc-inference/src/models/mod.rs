//! Model implementations for inference
//!
//! Each model type implements the `LoadedModel` trait for running inference.

#[cfg(feature = "candle")]
pub mod bert;

#[cfg(feature = "candle")]
use crate::InferenceError;

/// A loaded model ready for inference
#[cfg(feature = "candle")]
pub trait LoadedModel: Send + Sync {
    /// Run inference on raw input bytes, returning output bytes
    fn forward(&self, input: &[u8]) -> Result<Vec<u8>, InferenceError>;

    /// Get the embedding dimension (for embedding models)
    fn embedding_dim(&self) -> usize;
}
