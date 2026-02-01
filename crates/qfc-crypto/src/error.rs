//! Cryptographic error types

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("Invalid signature")]
    InvalidSignature,

    #[error("Invalid public key")]
    InvalidPublicKey,

    #[error("Invalid secret key")]
    InvalidSecretKey,

    #[error("Invalid VRF proof")]
    InvalidVrfProof,

    #[error("Signature verification failed")]
    VerificationFailed,

    #[error("Key generation failed: {0}")]
    KeyGenerationFailed(String),

    #[error("Serialization error: {0}")]
    SerializationError(String),
}

pub type Result<T> = std::result::Result<T, CryptoError>;
