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
