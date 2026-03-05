//! Submit proofs and fetch tasks via validator RPC

use qfc_inference::proof::InferenceProof;
use qfc_inference::runtime::{BackendType, GpuTier};
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

/// Task request sent to validator
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TaskRequest {
    miner_address: String,
    gpu_tier: String,
    available_memory_mb: u64,
    backend: String,
}

/// Inference task received from validator
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InferenceTaskResponse {
    pub task_id: String,
    pub epoch: u64,
    pub task_type: String,
    pub model_name: String,
    pub model_version: String,
    pub input_data: String,
    pub deadline: u64,
}

/// Proof submission sent to validator
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProofSubmission {
    miner_address: String,
    task_id: String,
    epoch: u64,
    output_hash: String,
    execution_time_ms: u64,
    flops_estimated: u64,
    backend: String,
    proof_bytes: String,
}

/// Result from proof submission
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProofResult {
    pub accepted: bool,
    pub spot_checked: bool,
    pub message: String,
}

/// JSON-RPC request/response types
#[derive(Serialize)]
struct JsonRpcRequest<T: Serialize> {
    jsonrpc: &'static str,
    method: &'static str,
    params: Vec<T>,
    id: u64,
}

#[derive(Deserialize)]
struct JsonRpcResponse<T> {
    #[allow(dead_code)]
    jsonrpc: String,
    result: Option<T>,
    error: Option<JsonRpcError>,
    #[allow(dead_code)]
    id: u64,
}

#[derive(Deserialize)]
struct JsonRpcError {
    #[allow(dead_code)]
    code: i64,
    message: String,
}

/// Fetch an inference task from the validator node
pub async fn fetch_task(
    rpc_url: &str,
    miner_address: &str,
    tier: GpuTier,
    memory_mb: u64,
    backend: BackendType,
) -> Result<Option<InferenceTaskResponse>, SubmitError> {
    let request = TaskRequest {
        miner_address: miner_address.to_string(),
        gpu_tier: match tier {
            GpuTier::Hot => "Hot".to_string(),
            GpuTier::Warm => "Warm".to_string(),
            GpuTier::Cold => "Cold".to_string(),
        },
        available_memory_mb: memory_mb,
        backend: format!("{}", backend),
    };

    let rpc_request = JsonRpcRequest {
        jsonrpc: "2.0",
        method: "qfc_getInferenceTask",
        params: vec![request],
        id: 1,
    };

    let body = serde_json::to_string(&rpc_request)
        .map_err(|e| SubmitError::SerializationError(e.to_string()))?;

    debug!("Fetching task from {}", rpc_url);

    let output = tokio::process::Command::new("curl")
        .args([
            "-s",
            "-X",
            "POST",
            "-H",
            "Content-Type: application/json",
            "-d",
            &body,
            rpc_url,
        ])
        .output()
        .await
        .map_err(|e| SubmitError::ConnectionFailed(e.to_string()))?;

    if !output.status.success() {
        return Err(SubmitError::ConnectionFailed(
            "curl request failed".to_string(),
        ));
    }

    let response_str = String::from_utf8(output.stdout)
        .map_err(|e| SubmitError::SerializationError(e.to_string()))?;

    let response: JsonRpcResponse<Option<InferenceTaskResponse>> =
        serde_json::from_str(&response_str)
            .map_err(|e| SubmitError::SerializationError(format!("Parse response: {}", e)))?;

    if let Some(err) = response.error {
        return Err(SubmitError::ProofRejected(err.message));
    }

    Ok(response.result.flatten())
}

/// Submit an inference proof to the validator node
pub async fn submit_proof(
    rpc_url: &str,
    miner_address: &str,
    proof: &InferenceProof,
) -> Result<ProofResult, SubmitError> {
    let proof_bytes = hex::encode(proof.to_bytes());
    let output_hash = hex::encode(proof.output_hash.as_bytes());

    let submission = ProofSubmission {
        miner_address: miner_address.to_string(),
        task_id: hex::encode(proof.input_hash.as_bytes()),
        epoch: proof.epoch,
        output_hash,
        execution_time_ms: proof.execution_time_ms,
        flops_estimated: proof.flops_estimated,
        backend: format!("{}", proof.backend),
        proof_bytes,
    };

    let rpc_request = JsonRpcRequest {
        jsonrpc: "2.0",
        method: "qfc_submitInferenceProof",
        params: vec![submission],
        id: 1,
    };

    let body = serde_json::to_string(&rpc_request)
        .map_err(|e| SubmitError::SerializationError(e.to_string()))?;

    info!("Submitting proof for epoch {} to {}", proof.epoch, rpc_url);

    let output = tokio::process::Command::new("curl")
        .args([
            "-s",
            "-X",
            "POST",
            "-H",
            "Content-Type: application/json",
            "-d",
            &body,
            rpc_url,
        ])
        .output()
        .await
        .map_err(|e| SubmitError::ConnectionFailed(e.to_string()))?;

    if !output.status.success() {
        return Err(SubmitError::ConnectionFailed(
            "curl request failed".to_string(),
        ));
    }

    let response_str = String::from_utf8(output.stdout)
        .map_err(|e| SubmitError::SerializationError(e.to_string()))?;

    let response: JsonRpcResponse<ProofResult> = serde_json::from_str(&response_str)
        .map_err(|e| SubmitError::SerializationError(format!("Parse response: {}", e)))?;

    if let Some(err) = response.error {
        return Err(SubmitError::ProofRejected(err.message));
    }

    response
        .result
        .ok_or_else(|| SubmitError::SerializationError("No result in response".to_string()))
}

#[derive(Debug, thiserror::Error)]
pub enum SubmitError {
    #[error("RPC connection failed: {0}")]
    ConnectionFailed(String),

    #[error("Proof rejected by validator: {0}")]
    ProofRejected(String),

    #[error("Serialization error: {0}")]
    SerializationError(String),
}
