//! QFC-specific RPC methods

use jsonrpsee::core::RpcResult;
use jsonrpsee::core::SubscriptionResult;
use jsonrpsee::proc_macros::rpc;
use serde::{Deserialize, Serialize};

/// Validator information
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcValidator {
    pub address: String,
    pub stake: String,
    pub contribution_score: String,
    pub uptime: String,
    pub is_active: bool,
    /// Whether this validator provides compute contribution
    pub provides_compute: bool,
    /// Current hashrate in H/s (0 if not mining)
    pub hashrate: String,
    /// v2.0: Inference score (replaces hashrate for inference validators)
    pub inference_score: String,
    /// v2.0: Compute mode — "pow", "inference", or "none"
    pub compute_mode: String,
    /// v2.0: Total inference tasks completed
    pub tasks_completed: String,
}

/// Detailed validator score breakdown
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcValidatorScoreBreakdown {
    pub address: String,
    /// Total contribution score (0-10000 representing 0-100%)
    pub total_score: String,
    /// Stake amount
    pub stake: String,
    /// Stake score component (30% weight)
    pub stake_score: String,
    /// Compute score component (20% weight)
    pub compute_score: String,
    /// Uptime score component (15% weight)
    pub uptime_score: String,
    /// Accuracy score component (15% weight)
    pub accuracy_score: String,
    /// Network score component (10% weight)
    pub network_score: String,
    /// Storage score component (5% weight)
    pub storage_score: String,
    /// Reputation score component (5% weight)
    pub reputation_score: String,
    /// Raw metrics
    pub metrics: RpcValidatorMetrics,
}

/// Raw validator metrics
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcValidatorMetrics {
    /// Uptime percentage (0-100)
    pub uptime_percent: String,
    /// Accuracy percentage (0-100)
    pub accuracy_percent: String,
    /// Reputation percentage (0-100)
    pub reputation_percent: String,
    /// Average latency in ms
    pub avg_latency_ms: u32,
    /// Bandwidth in Mbps
    pub bandwidth_mbps: u32,
    /// Storage provided in GB
    pub storage_gb: u32,
    /// Whether provides compute
    pub provides_compute: bool,
    /// Hashrate if provides compute
    pub hashrate: String,
    /// Blocks produced
    pub blocks_produced: String,
    /// Valid votes
    pub valid_votes: String,
    /// Invalid votes
    pub invalid_votes: String,
}

/// QFC RPC API trait
#[rpc(server, namespace = "qfc")]
pub trait QfcApi {
    /// Get list of active validators
    #[method(name = "getValidators")]
    async fn get_validators(&self) -> RpcResult<Vec<RpcValidator>>;

    /// Get contribution score for an address
    #[method(name = "getContributionScore")]
    async fn get_contribution_score(&self, address: String) -> RpcResult<String>;

    /// Get detailed score breakdown for a validator
    #[method(name = "getValidatorScoreBreakdown")]
    async fn get_validator_score_breakdown(
        &self,
        address: String,
    ) -> RpcResult<RpcValidatorScoreBreakdown>;

    /// Get stake amount for an address
    #[method(name = "getStake")]
    async fn get_stake(&self, address: String) -> RpcResult<String>;

    /// Get current epoch info
    #[method(name = "getEpoch")]
    async fn get_epoch(&self) -> RpcResult<RpcEpoch>;

    /// Get finalized block number
    #[method(name = "getFinalizedBlock")]
    async fn get_finalized_block(&self) -> RpcResult<String>;

    /// Get node info
    #[method(name = "nodeInfo")]
    async fn node_info(&self) -> RpcResult<RpcNodeInfo>;

    /// Get current network state
    #[method(name = "getNetworkState")]
    async fn get_network_state(&self) -> RpcResult<String>;

    /// Request tokens from faucet (dev mode only)
    /// Returns transaction hash
    #[method(name = "requestFaucet")]
    async fn request_faucet(&self, address: String, amount: String)
        -> RpcResult<RpcFaucetResponse>;

    // ---- v2.0: Miner registration ----

    /// Register a miner with GPU profile and benchmark score
    #[method(name = "registerMiner")]
    async fn register_miner(
        &self,
        req: RpcRegisterMinerRequest,
    ) -> RpcResult<RpcRegisterMinerResult>;

    /// Report miner status (loaded models, pending tasks)
    #[method(name = "reportMinerStatus")]
    async fn report_miner_status(&self, req: RpcMinerStatusReport) -> RpcResult<bool>;

    // ---- v2.0: AI Compute endpoints ----

    /// Get compute info for this node (backend, models, GPU memory, inference score)
    #[method(name = "getComputeInfo")]
    async fn get_compute_info(&self) -> RpcResult<RpcComputeInfo>;

    /// Get list of supported/approved models for AI inference
    #[method(name = "getSupportedModels")]
    async fn get_supported_models(&self) -> RpcResult<Vec<RpcModel>>;

    /// Get inference statistics (tasks completed, avg time, FLOPS, pass rate)
    #[method(name = "getInferenceStats")]
    async fn get_inference_stats(&self) -> RpcResult<RpcInferenceStats>;

    /// Fetch an inference task for a miner (miner provides its capabilities)
    #[method(name = "getInferenceTask")]
    async fn get_inference_task(
        &self,
        request: RpcTaskRequest,
    ) -> RpcResult<Option<RpcInferenceTask>>;

    /// Submit an inference proof from a miner
    #[method(name = "submitInferenceProof")]
    async fn submit_inference_proof(
        &self,
        proof: RpcInferenceProofSubmission,
    ) -> RpcResult<RpcProofResult>;

    // ---- v2.0: Model Governance endpoints ----

    /// Submit a model proposal
    #[method(name = "proposeModel")]
    async fn propose_model(&self, request: RpcProposeModelRequest) -> RpcResult<String>;

    /// Vote on a model proposal
    #[method(name = "voteModel")]
    async fn vote_model(&self, request: RpcVoteModelRequest) -> RpcResult<bool>;

    /// Get all model proposals
    #[method(name = "getModelProposals")]
    async fn get_model_proposals(&self) -> RpcResult<Vec<RpcModelProposal>>;

    // ---- v2.0: Public Inference API endpoints ----

    /// Submit a public inference task (paid)
    #[method(name = "submitPublicTask")]
    async fn submit_public_task(&self, request: RpcSubmitPublicTask) -> RpcResult<String>;

    /// Get public task status
    #[method(name = "getPublicTaskStatus")]
    async fn get_public_task_status(&self, task_id: String) -> RpcResult<RpcPublicTaskStatus>;

    /// Fetch a large inference result from IPFS by CID.
    /// Returns base64-encoded content. This proxies the request so clients
    /// do not need their own IPFS node.
    #[method(name = "getInferenceResult")]
    async fn get_inference_result(&self, cid: String) -> RpcResult<String>;

    /// Subscribe to task status updates (WebSocket only).
    /// Pushes RpcPublicTaskStatus whenever the task transitions state.
    #[subscription(name = "subscribeTaskStatus" => "taskStatus", unsubscribe = "unsubscribeTaskStatus", item = RpcPublicTaskStatus)]
    async fn subscribe_task_status(&self, task_id: String) -> SubscriptionResult;
}

/// Faucet response
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcFaucetResponse {
    pub tx_hash: String,
    pub amount: String,
    pub to: String,
}

/// Epoch information
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcEpoch {
    pub number: String,
    pub start_time: String,
    pub duration_ms: String,
}

/// Node information
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcNodeInfo {
    pub version: String,
    pub chain_id: String,
    pub peer_count: u64,
    pub is_validator: bool,
    pub syncing: bool,
}

// ============ v2.0: AI Compute RPC Types ============

/// Compute information for a node
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcComputeInfo {
    /// Compute backend: "CUDA", "Metal", "CPU", or "none"
    pub backend: String,
    /// Supported model IDs
    pub supported_models: Vec<String>,
    /// GPU/system memory in MB
    pub gpu_memory_mb: u64,
    /// Current inference score
    pub inference_score: String,
    /// GPU tier: "Hot", "Warm", "Cold", or "none"
    pub gpu_tier: String,
    /// Whether this node provides AI compute
    pub provides_compute: bool,
}

/// Model information
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcModel {
    /// Model name
    pub name: String,
    /// Model version
    pub version: String,
    /// Required minimum memory in MB
    pub min_memory_mb: u64,
    /// Minimum GPU tier
    pub min_tier: String,
    /// Whether the model is approved by governance
    pub approved: bool,
}

/// Inference statistics
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcInferenceStats {
    /// Total tasks completed
    pub tasks_completed: String,
    /// Average execution time in ms
    pub avg_time_ms: String,
    /// Total FLOPS accumulated
    pub flops_total: String,
    /// Verification pass rate (0-100%)
    pub pass_rate: String,
}

// ============ v2.0: Task Assignment & Proof Submission Types ============

/// Miner's request for an inference task
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcTaskRequest {
    /// Miner wallet address
    pub miner_address: String,
    /// GPU tier: "Hot", "Warm", "Cold"
    pub gpu_tier: String,
    /// Available memory in MB
    pub available_memory_mb: u64,
    /// Backend type: "CUDA", "Metal", "CPU"
    pub backend: String,
}

/// An inference task assigned to a miner
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcInferenceTask {
    /// Task ID (hex)
    pub task_id: String,
    /// Epoch number
    pub epoch: u64,
    /// Task type: "embedding", "text_generation", "image_classification"
    pub task_type: String,
    /// Model name
    pub model_name: String,
    /// Model version
    pub model_version: String,
    /// Input data (hex-encoded)
    pub input_data: String,
    /// Deadline timestamp (ms)
    pub deadline: u64,
}

/// Inference proof submitted by a miner
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcInferenceProofSubmission {
    /// Miner wallet address
    pub miner_address: String,
    /// Task ID (hex)
    pub task_id: String,
    /// Epoch
    pub epoch: u64,
    /// Output hash (hex)
    pub output_hash: String,
    /// Execution time in ms
    pub execution_time_ms: u64,
    /// Estimated FLOPS
    pub flops_estimated: u64,
    /// Backend used: "CUDA", "Metal", "CPU"
    pub backend: String,
    /// Serialized proof bytes (hex)
    pub proof_bytes: String,
    /// Optional result data (hex) for public task completion
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_data: Option<String>,
}

/// Result of submitting an inference proof
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcProofResult {
    /// Whether the proof was accepted
    pub accepted: bool,
    /// Whether the proof was spot-checked
    pub spot_checked: bool,
    /// Detail message
    pub message: String,
}

// ============ v2.0: Model Governance RPC Types ============

/// Model proposal information
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcModelProposal {
    pub proposal_id: String,
    pub proposer: String,
    pub model_name: String,
    pub model_version: String,
    pub description: String,
    pub min_memory_mb: u64,
    pub min_tier: String,
    pub size_mb: u64,
    pub votes_for: u64,
    pub votes_against: u64,
    pub status: String,
    pub created_at: u64,
    pub voting_deadline: u64,
}

/// Request to propose a new model
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcProposeModelRequest {
    pub proposer: String,
    pub model_name: String,
    pub model_version: String,
    pub description: String,
    pub min_memory_mb: u64,
    pub min_tier: String,
    pub size_mb: u64,
}

/// Request to vote on a model proposal
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcVoteModelRequest {
    pub proposal_id: String,
    pub voter: String,
    pub approve: bool,
}

// ============ v2.0: Public Inference API Types ============

/// Request to submit a public inference task
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcSubmitPublicTask {
    pub task_type: String,
    pub model_id: String,
    pub input_data: String,
    pub max_fee: String,
    /// Submitter address (hex)
    pub submitter: String,
    /// Ed25519 signature over (task_type || model_id || input_data || max_fee) hex
    pub signature: String,
}

/// Status of a public inference task (B1: structured result envelope)
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcPublicTaskStatus {
    pub task_id: String,
    pub status: String,
    /// Submitter address
    pub submitter: String,
    /// Task type (e.g. "embedding", "text_generation")
    pub task_type: String,
    /// Model used (e.g. "qfc-embed-small:v1.0")
    pub model_id: String,
    /// Task creation timestamp (ms)
    pub created_at: u64,
    /// Task deadline timestamp (ms)
    pub deadline: u64,
    /// Max fee in wei (hex)
    pub max_fee: String,
    /// Result payload (base64-encoded bytes), present when status=Completed and result_type="inline"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    /// Result size in bytes
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_size: Option<usize>,
    /// "inline" or "ipfs" — how result is stored
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_type: Option<String>,
    /// IPFS CID (only when result_type="ipfs")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_cid: Option<String>,
    /// Preview of result (base64, first 1KB, only for IPFS results)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_preview: Option<String>,
    /// Miner that completed the task
    #[serde(skip_serializing_if = "Option::is_none")]
    pub miner_address: Option<String>,
    /// Execution time in milliseconds
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_time_ms: Option<u64>,
}

// ============ v2.0 P2: Miner Registration & Status Report Types ============

/// Request to register a miner with GPU profile
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcRegisterMinerRequest {
    /// Miner wallet address (hex)
    pub miner_address: String,
    /// GPU model name (e.g. "NVIDIA RTX 4090")
    pub gpu_model: String,
    /// VRAM in MB
    pub vram_mb: u64,
    /// Benchmark score (0-10000)
    pub benchmark_score: u32,
    /// Backend: "CUDA", "Metal", "CPU"
    pub backend: String,
    /// Ed25519 signature over (miner_address || gpu_model || benchmark_score)
    pub signature: String,
}

/// Result of miner registration
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcRegisterMinerResult {
    pub registered: bool,
    pub assigned_tier: u8,
    pub message: String,
}

/// Miner status report (loaded models, pending tasks)
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcMinerStatusReport {
    /// Miner wallet address (hex)
    pub miner_address: String,
    /// Currently loaded models with their layer status
    pub loaded_models: Vec<RpcModelStatus>,
    /// Number of pending tasks
    pub pending_tasks: u32,
    /// Ed25519 signature
    pub signature: String,
}

/// Model status on a miner
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcModelStatus {
    /// Model name
    pub model_name: String,
    /// Model version
    pub model_version: String,
    /// Layer: "hot", "warm", "cold"
    pub layer: String,
}
