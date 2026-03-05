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

/// Register miner request
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RegisterMinerReq {
    miner_address: String,
    gpu_model: String,
    vram_mb: u64,
    benchmark_score: u32,
    backend: String,
    signature: String,
}

/// Register miner result
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisterMinerResult {
    pub registered: bool,
    pub assigned_tier: u8,
    pub message: String,
}

/// Register the miner with the validator node
pub async fn register_miner(
    rpc_url: &str,
    miner_address: &str,
    gpu_model: &str,
    vram_mb: u64,
    benchmark_score: u32,
    backend: BackendType,
    keypair: &qfc_crypto::Keypair,
) -> Result<RegisterMinerResult, SubmitError> {
    let sig_payload = format!("{}{}{}", miner_address, gpu_model, benchmark_score);
    let sig_hash = qfc_crypto::blake3_hash(sig_payload.as_bytes());
    let signature = keypair.sign_hash(&sig_hash);

    let req = RegisterMinerReq {
        miner_address: miner_address.to_string(),
        gpu_model: gpu_model.to_string(),
        vram_mb,
        benchmark_score,
        backend: format!("{}", backend),
        signature: hex::encode(signature.as_bytes()),
    };

    let rpc_request = JsonRpcRequest {
        jsonrpc: "2.0",
        method: "qfc_registerMiner",
        params: vec![req],
        id: 1,
    };

    let body = serde_json::to_string(&rpc_request)
        .map_err(|e| SubmitError::SerializationError(e.to_string()))?;

    info!("Registering miner at {}", rpc_url);

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

    let response: JsonRpcResponse<RegisterMinerResult> = serde_json::from_str(&response_str)
        .map_err(|e| SubmitError::SerializationError(format!("Parse response: {}", e)))?;

    if let Some(err) = response.error {
        return Err(SubmitError::ProofRejected(err.message));
    }

    response
        .result
        .ok_or_else(|| SubmitError::SerializationError("No result in response".to_string()))
}

/// Miner status report request
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MinerStatusReportReq {
    miner_address: String,
    loaded_models: Vec<MinerModelStatusReq>,
    pending_tasks: u32,
    signature: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MinerModelStatusReq {
    model_name: String,
    model_version: String,
    layer: String,
}

/// Report miner status to the validator
pub async fn report_miner_status(
    rpc_url: &str,
    miner_address: &str,
    loaded_models: Vec<(String, String, String)>, // (name, version, layer)
    pending_tasks: u32,
    keypair: &qfc_crypto::Keypair,
) -> Result<bool, SubmitError> {
    let sig_payload = format!("{}{}", miner_address, pending_tasks);
    let sig_hash = qfc_crypto::blake3_hash(sig_payload.as_bytes());
    let signature = keypair.sign_hash(&sig_hash);

    let req = MinerStatusReportReq {
        miner_address: miner_address.to_string(),
        loaded_models: loaded_models
            .into_iter()
            .map(|(n, v, l)| MinerModelStatusReq {
                model_name: n,
                model_version: v,
                layer: l,
            })
            .collect(),
        pending_tasks,
        signature: hex::encode(signature.as_bytes()),
    };

    let rpc_request = JsonRpcRequest {
        jsonrpc: "2.0",
        method: "qfc_reportMinerStatus",
        params: vec![req],
        id: 1,
    };

    let body = serde_json::to_string(&rpc_request)
        .map_err(|e| SubmitError::SerializationError(e.to_string()))?;

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

    let response: JsonRpcResponse<bool> = serde_json::from_str(&response_str)
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
