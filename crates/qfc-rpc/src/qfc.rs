//! QFC-specific RPC methods

use jsonrpsee::core::RpcResult;
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
