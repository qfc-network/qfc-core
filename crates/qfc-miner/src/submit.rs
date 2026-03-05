//! Submit proofs to validator via RPC

use qfc_inference::proof::InferenceProof;
use tracing::debug;

/// Submit an inference proof to the validator node
#[allow(dead_code)]
pub async fn submit_proof(
    rpc_url: &str,
    proof: &InferenceProof,
) -> Result<(), SubmitError> {
    // TODO: Implement RPC call to validator
    // This will use jsonrpsee client to call qfc_submitInferenceProof
    debug!(
        "Would submit proof for epoch {} to {}",
        proof.epoch, rpc_url
    );
    Ok(())
}

#[derive(Debug, thiserror::Error)]
#[allow(dead_code)]
pub enum SubmitError {
    #[error("RPC connection failed: {0}")]
    ConnectionFailed(String),

    #[error("Proof rejected by validator: {0}")]
    ProofRejected(String),

    #[error("Serialization error: {0}")]
    SerializationError(String),
}
