//! Submit proofs and fetch tasks via validator RPC

use qfc_inference::proof::InferenceProof;
use qfc_inference::runtime::{BackendType, GpuTier};
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

/// Shared HTTP client (reuse connections)
static HTTP_CLIENT: std::sync::LazyLock<reqwest::Client> = std::sync::LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .expect("Failed to create HTTP client")
});

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
    /// Language code for speech_to_text tasks (e.g. "en", "zh")
    #[serde(default)]
    pub language: Option<String>,
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
    /// Estimated reward in wei (hex string), returned by validator on acceptance
    #[serde(default)]
    pub reward_estimate: Option<String>,
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

/// Send a JSON-RPC request and parse the response
async fn rpc_call<P: Serialize, R: for<'de> Deserialize<'de>>(
    rpc_url: &str,
    method: &'static str,
    params: Vec<P>,
) -> Result<Option<R>, SubmitError> {
    let rpc_request = JsonRpcRequest {
        jsonrpc: "2.0",
        method,
        params,
        id: 1,
    };

    let response = HTTP_CLIENT
        .post(rpc_url)
        .json(&rpc_request)
        .send()
        .await
        .map_err(|e| SubmitError::ConnectionFailed(format!("{}", e)))?;

    let status = response.status();
    let response_str = response
        .text()
        .await
        .map_err(|e| SubmitError::ConnectionFailed(format!("Failed to read response: {}", e)))?;

    if !status.is_success() {
        return Err(SubmitError::ConnectionFailed(format!(
            "HTTP {}: {}",
            status,
            &response_str[..response_str.len().min(200)]
        )));
    }

    if response_str.is_empty() {
        return Err(SubmitError::SerializationError(
            "Empty response from validator".to_string(),
        ));
    }

    debug!(
        "RPC {} response: {}",
        method,
        &response_str[..response_str.len().min(200)]
    );

    let rpc_response: JsonRpcResponse<R> = serde_json::from_str(&response_str).map_err(|e| {
        SubmitError::SerializationError(format!(
            "Parse response: {} (body: {})",
            e,
            &response_str[..response_str.len().min(200)]
        ))
    })?;

    if let Some(err) = rpc_response.error {
        return Err(SubmitError::ProofRejected(err.message));
    }

    Ok(rpc_response.result)
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

    debug!("Fetching task from {}", rpc_url);

    let result: Option<Option<InferenceTaskResponse>> =
        rpc_call(rpc_url, "qfc_getInferenceTask", vec![request]).await?;

    Ok(result.flatten())
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

    info!("Submitting proof for epoch {} to {}", proof.epoch, rpc_url);

    rpc_call::<_, ProofResult>(rpc_url, "qfc_submitInferenceProof", vec![submission])
        .await?
        .ok_or_else(|| SubmitError::SerializationError("No result in response".to_string()))
}

/// Register miner request
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RegisterMinerReq {
    miner_address: String,
    public_key: String,
    gpu_model: String,
    vram_mb: u64,
    benchmark_score: u32,
    backend: String,
    signature: String,
    os: String,
    arch: String,
    cpu_model: String,
    cpu_cores: u32,
    total_memory_mb: u64,
    version: String,
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
        public_key: hex::encode(keypair.public_key().as_bytes()),
        gpu_model: gpu_model.to_string(),
        vram_mb,
        benchmark_score,
        backend: format!("{}", backend),
        signature: hex::encode(signature.as_bytes()),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        cpu_model: get_cpu_model(),
        cpu_cores: std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(1),
        total_memory_mb: get_total_memory_mb(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    };

    info!("Registering miner at {}", rpc_url);

    rpc_call::<_, RegisterMinerResult>(rpc_url, "qfc_registerMiner", vec![req])
        .await?
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

    rpc_call::<_, bool>(rpc_url, "qfc_reportMinerStatus", vec![req])
        .await?
        .ok_or_else(|| SubmitError::SerializationError("No result in response".to_string()))
}

/// Epoch response from validator
#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EpochResponse {
    pub number: String, // hex
}

/// Fetch current epoch number from the validator
#[allow(dead_code)]
pub async fn fetch_epoch(rpc_url: &str) -> Result<u64, SubmitError> {
    let result: EpochResponse =
        rpc_call::<serde_json::Value, EpochResponse>(rpc_url, "qfc_getEpoch", vec![])
            .await?
            .ok_or_else(|| SubmitError::SerializationError("No result in response".to_string()))?;

    let hex_str = result.number.strip_prefix("0x").unwrap_or(&result.number);
    u64::from_str_radix(hex_str, 16)
        .map_err(|e| SubmitError::SerializationError(format!("Invalid epoch hex: {}", e)))
}

/// Get CPU model name from the system
fn get_cpu_model() -> String {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("sysctl")
            .args(["-n", "machdep.cpu.brand_string"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    }
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/cpuinfo")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("model name"))
                    .and_then(|l| l.split(':').nth(1))
                    .map(|s| s.trim().to_string())
            })
            .unwrap_or_default()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        String::new()
    }
}

/// Get total system memory in MB
fn get_total_memory_mb() -> u64 {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("sysctl")
            .args(["-n", "hw.memsize"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(|bytes| bytes / (1024 * 1024))
            .unwrap_or(0)
    }
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|s| {
                s.lines().find(|l| l.starts_with("MemTotal")).and_then(|l| {
                    l.split_whitespace()
                        .nth(1)
                        .and_then(|v| v.parse::<u64>().ok())
                })
            })
            .map(|kb| kb / 1024)
            .unwrap_or(0)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        0
    }
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
