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

    /// Get pending undelegations for an address
    #[method(name = "getPendingUndelegations")]
    async fn get_pending_undelegations(&self, address: String) -> RpcResult<Vec<RpcUndelegation>>;

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

    /// List all registered miners with their profiles
    #[method(name = "getRegisteredMiners")]
    async fn get_registered_miners(&self) -> RpcResult<Vec<RpcRegisteredMiner>>;

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

    // ---- v2.0: Treasury endpoints ----

    /// Get treasury balance and stats
    #[method(name = "getTreasuryInfo")]
    async fn get_treasury_info(&self) -> RpcResult<RpcTreasuryInfo>;

    /// Submit a treasury spend proposal
    #[method(name = "proposeSpend")]
    async fn propose_spend(&self, request: RpcProposeSpendRequest) -> RpcResult<String>;

    /// Vote on a treasury spend proposal
    #[method(name = "voteSpend")]
    async fn vote_spend(&self, request: RpcVoteSpendRequest) -> RpcResult<bool>;

    /// Get all treasury spend proposals
    #[method(name = "getSpendProposals")]
    async fn get_spend_proposals(&self) -> RpcResult<Vec<RpcSpendProposal>>;

    // ---- v2.0: Parameter Governance endpoints ----

    /// Submit a parameter change proposal
    #[method(name = "proposeParameter")]
    async fn propose_parameter(&self, request: RpcProposeParameterRequest) -> RpcResult<String>;

    /// Vote on a parameter proposal (stake-weighted)
    #[method(name = "voteParameter")]
    async fn vote_parameter(&self, request: RpcVoteParameterRequest) -> RpcResult<bool>;

    /// Get all parameter proposals
    #[method(name = "getParameterProposals")]
    async fn get_parameter_proposals(&self) -> RpcResult<Vec<RpcParameterProposal>>;

    /// Get current parameter overrides (values changed by governance)
    #[method(name = "getParameterOverrides")]
    async fn get_parameter_overrides(&self) -> RpcResult<Vec<RpcParameterOverride>>;

    // ---- v2.0: Public Inference API endpoints ----

    /// Submit a public inference task (paid).
    ///
    /// T5: may fail with JSON-RPC error `-32029` when the submitter's quota
    /// is exceeded (QPS / in-flight / FLOPs budget) or the pool is shedding
    /// low-priority tenants under pressure; the error `data` carries
    /// `{reason, retryAfterMs}`. See docs/AI-QUOTAS.md.
    #[method(name = "submitPublicTask")]
    async fn submit_public_task(&self, request: RpcSubmitPublicTask) -> RpcResult<String>;

    /// Get public task status
    #[method(name = "getPublicTaskStatus")]
    async fn get_public_task_status(&self, task_id: String) -> RpcResult<RpcPublicTaskStatus>;

    /// List public tasks with optional filtering and pagination
    #[method(name = "listPublicTasks")]
    async fn list_public_tasks(
        &self,
        filter: RpcListPublicTasksFilter,
    ) -> RpcResult<Vec<RpcPublicTaskStatus>>;

    /// Estimate the fee for an inference task
    #[method(name = "estimateInferenceFee")]
    async fn estimate_inference_fee(
        &self,
        request: RpcEstimateInferenceFee,
    ) -> RpcResult<RpcInferenceFeeEstimate>;

    /// Fetch a large inference result from IPFS by CID.
    /// Returns base64-encoded content. This proxies the request so clients
    /// do not need their own IPFS node.
    #[method(name = "getInferenceResult")]
    async fn get_inference_result(&self, cid: String) -> RpcResult<String>;

    /// Subscribe to task status updates (WebSocket only).
    /// Pushes RpcPublicTaskStatus whenever the task transitions state.
    #[subscription(name = "subscribeTaskStatus" => "taskStatus", unsubscribe = "unsubscribeTaskStatus", item = RpcPublicTaskStatus)]
    async fn subscribe_task_status(&self, task_id: String) -> SubscriptionResult;

    /// Get miner vesting schedule — locked vs available balance.
    /// Miner rewards vest linearly over 30 days with a 7-day cliff.
    #[method(name = "getMinerVesting")]
    async fn get_miner_vesting(&self, address: String) -> RpcResult<RpcMinerVesting>;

    /// Get miner earnings history.
    /// Returns per-block earning records for a miner address.
    /// `period` can be: "day" (last 24h), "week", "month", or "all".
    #[method(name = "getMinerEarnings")]
    async fn get_miner_earnings(
        &self,
        address: String,
        period: String,
    ) -> RpcResult<RpcMinerEarnings>;
    /// Subscribe to miner earning events (WebSocket only).
    /// Pushes RpcMinerEvent whenever the miner earns a reward or completes a task.
    #[subscription(name = "subscribeMinerEvents" => "minerEvent", unsubscribe = "unsubscribeMinerEvents", item = RpcMinerEvent)]
    async fn subscribe_miner_events(&self, address: String) -> SubscriptionResult;

    // ---- v2.0: Miner notification endpoints ----

    /// Register a webhook URL for miner event notifications.
    /// Supports Discord, Telegram bot, or any custom HTTPS endpoint.
    /// Requires miner signature for authentication.
    #[method(name = "registerWebhook")]
    async fn register_webhook(&self, request: RpcRegisterWebhookRequest) -> RpcResult<String>;

    /// Remove a previously registered webhook.
    #[method(name = "removeWebhook")]
    async fn remove_webhook(&self, request: RpcRemoveWebhookRequest) -> RpcResult<bool>;

    /// List active webhooks for a miner address.
    #[method(name = "getWebhooks")]
    async fn get_webhooks(&self, address: String) -> RpcResult<Vec<RpcWebhook>>;

    // ---- v2.0: Bridge endpoints ----

    /// Get bridge status (validators, TVL, pending operations)
    #[method(name = "getBridgeStatus")]
    async fn get_bridge_status(&self) -> RpcResult<RpcBridgeStatus>;

    /// Get a bridge deposit by ID
    #[method(name = "getBridgeDeposit")]
    async fn get_bridge_deposit(&self, deposit_id: String) -> RpcResult<Option<RpcBridgeDeposit>>;

    /// Get a bridge withdrawal by ID
    #[method(name = "getBridgeWithdrawal")]
    async fn get_bridge_withdrawal(
        &self,
        withdrawal_id: String,
    ) -> RpcResult<Option<RpcBridgeWithdrawal>>;

    /// Get rent info for an account (storage deposit, dormancy status, rent owed)
    #[method(name = "getAccountRentInfo")]
    async fn get_account_rent_info(&self, address: String) -> RpcResult<RpcAccountRentInfo>;

    /// Send a UserOperation (EIP-4337 account abstraction)
    #[method(name = "sendUserOperation")]
    async fn send_user_operation(&self, user_op: RpcUserOperation) -> RpcResult<String>;

    /// Get a UserOperation by hash
    #[method(name = "getUserOperationByHash")]
    async fn get_user_operation_by_hash(
        &self,
        hash: String,
    ) -> RpcResult<Option<RpcUserOperationStatus>>;

    /// Get the EntryPoint supported by this node
    #[method(name = "supportedEntryPoints")]
    async fn supported_entry_points(&self) -> RpcResult<Vec<String>>;

    // ---- v2.0: Agent Registry endpoints ----

    /// Get agent info from the AgentRegistry contract
    #[method(name = "getAgentInfo")]
    async fn get_agent_info(&self, agent_id: String) -> RpcResult<RpcAgentInfo>;

    /// List agents owned by a given address
    #[method(name = "listAgentsByOwner")]
    async fn list_agents_by_owner(&self, owner_address: String) -> RpcResult<Vec<RpcAgentInfo>>;

    /// Validate a session key against the AgentRegistry contract
    #[method(name = "validateSessionKey")]
    async fn validate_session_key(&self, key_address: String) -> RpcResult<RpcSessionKeyInfo>;

    /// Register a new agent (write — creates a ContractCall transaction)
    #[method(name = "registerAgent")]
    async fn register_agent(
        &self,
        request: RpcRegisterAgentRequest,
    ) -> RpcResult<RpcAgentWriteResult>;

    /// Fund an agent with deposit (write — creates a ContractCall transaction)
    #[method(name = "fundAgent")]
    async fn fund_agent(&self, request: RpcFundAgentRequest) -> RpcResult<RpcAgentWriteResult>;

    /// Revoke (deactivate) an agent (write — creates a ContractCall transaction)
    #[method(name = "revokeAgent")]
    async fn revoke_agent(&self, request: RpcRevokeAgentRequest) -> RpcResult<RpcAgentWriteResult>;

    // ---- v3.0: Agent Discovery + Resource endpoints ----

    /// List agents with filters, sorting, and pagination
    #[method(name = "listAgents")]
    async fn list_agents(&self, params: RpcListAgentsParams) -> RpcResult<RpcListAgentsResponse>;

    /// Query agents by capability with min reputation/stake filters
    #[method(name = "queryAgentsByCapability")]
    async fn query_agents_by_capability(
        &self,
        params: RpcQueryByCapabilityParams,
    ) -> RpcResult<Vec<RpcAgentDetailView>>;

    /// Query agents by protocol digest
    #[method(name = "queryAgentsByProtocolDigest")]
    async fn query_agents_by_protocol_digest(
        &self,
        protocol_digest: String,
    ) -> RpcResult<Vec<RpcAgentDetailView>>;

    /// Get detailed agent info (v3 resource-based)
    #[method(name = "getAgent")]
    async fn get_agent(&self, agent_id: String) -> RpcResult<RpcAgentDetailView>;

    /// Freeze an agent (owner or governance)
    #[method(name = "freezeAgent")]
    async fn freeze_agent(&self, params: RpcFreezeAgentParams) -> RpcResult<RpcAgentWriteResult>;

    /// Issue a session key for an agent
    #[method(name = "issueSessionKey")]
    async fn issue_session_key(
        &self,
        params: RpcIssueSessionKeyParams,
    ) -> RpcResult<RpcSessionKeyDetail>;

    /// Revoke a session key
    #[method(name = "revokeSessionKey")]
    async fn revoke_session_key_v3(&self, key_id: String) -> RpcResult<RpcAgentWriteResult>;

    /// Get session keys for an agent
    #[method(name = "getSessionKeys")]
    async fn get_session_keys(&self, agent_id: String) -> RpcResult<Vec<RpcSessionKeyDetail>>;

    /// Get agent balance (stake + deposit)
    #[method(name = "getAgentBalance")]
    async fn get_agent_balance(&self, agent_id: String) -> RpcResult<RpcAgentBalance>;

    // ---- SRE T8: hot-key analytics ----

    /// On-demand hot-key / hot-account analytics report.
    ///
    /// Returns the current sampling-window snapshot: per-CF traffic with the
    /// ranked hottest keys, plus the hottest accounts and contract bytecode —
    /// the full ranked identities that are deliberately kept out of the
    /// Prometheus labels. `topN` (default 16, clamped to 1..=256) bounds the
    /// ranked lists. When the node runs without `--hot-key-sampling`,
    /// `samplingEnabled` is false and the detail fields are null. The same
    /// data is logged per window to the `qfc::hot_keys` target when
    /// `--hot-key-window-secs` is set. See docs/profiling/HOT-KEYS.md.
    #[method(name = "hotKeyReport")]
    async fn hot_key_report(&self, top_n: Option<usize>) -> RpcResult<RpcHotKeyReport>;
}

/// T8: combined hot-key / hot-account report. Detail fields are `null` when
/// the node is not running with `--hot-key-sampling`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcHotKeyReport {
    /// Whether hot-key sampling is active on this node.
    pub sampling_enabled: bool,
    /// Effective 1-in-N sampling rate (0 when disabled).
    pub sampling_rate: u32,
    /// Size of the ranked lists requested.
    pub top_n: usize,
    /// Per-CF storage-level sampling, or `null` when disabled.
    pub storage: Option<RpcHotKeyStorage>,
    /// Account/bytecode-level sampling, or `null` when disabled.
    pub accounts: Option<RpcHotAccounts>,
}

/// Storage-level (per-column-family) hot-key sampling.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcHotKeyStorage {
    pub sampling_rate: u32,
    pub sketch_capacity: usize,
    pub window_start_unix_ms: u64,
    pub cfs: Vec<RpcCfHotKeys>,
}

/// Per-column-family hot-key stats.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcCfHotKeys {
    pub cf: String,
    pub reads_sampled: u64,
    pub writes_sampled: u64,
    pub estimated_reads: u64,
    pub estimated_writes: u64,
    pub top_keys: Vec<RpcHotKeyEntry>,
}

/// One ranked hot key (`keyHex` is the raw `0x…` key).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcHotKeyEntry {
    pub key_hex: String,
    pub sampled_count: u64,
    pub estimated_count: u64,
    pub max_overestimate: u64,
}

/// Account/bytecode-level hot-key sampling.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcHotAccounts {
    pub sampling_rate: u32,
    pub sketch_capacity: usize,
    pub window_start_unix_ms: u64,
    pub estimated_account_reads: u64,
    pub estimated_account_writes: u64,
    pub estimated_code_reads: u64,
    pub top_accounts: Vec<RpcHotAccountEntry>,
    pub top_code: Vec<RpcHotCodeEntry>,
}

/// One ranked hot account.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcHotAccountEntry {
    pub address: String,
    pub sampled_count: u64,
    pub estimated_count: u64,
    pub max_overestimate: u64,
}

/// One ranked hot contract bytecode (by code hash).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcHotCodeEntry {
    pub code_hash: String,
    pub sampled_count: u64,
    pub estimated_count: u64,
    pub max_overestimate: u64,
}

/// Faucet response
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcFaucetResponse {
    pub tx_hash: String,
    pub amount: String,
    pub to: String,
}

/// Pending undelegation record
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcUndelegation {
    pub delegator: String,
    pub validator: String,
    pub amount: String,
    pub unlock_at: u64,
    pub is_unlocked: bool,
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
    /// Estimated reward in wei (hex string). This is the miner's estimated
    /// share based on base fee calculation. Actual reward is determined
    /// at block production time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reward_estimate: Option<String>,
}

// ============ v2.0: Miner Earnings RPC Types ============

/// Miner earnings summary and history
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcMinerEarnings {
    /// Miner address
    pub address: String,
    /// Total earnings in wei (hex string)
    pub total_earnings: String,
    /// Total FLOPS contributed
    pub total_flops: String,
    /// Total tasks completed
    pub total_tasks: String,
    /// Current balance in wei (hex string)
    pub balance: String,
    /// Earning records (newest first)
    pub records: Vec<RpcEarningRecord>,
}

/// A single earning record for one block
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcEarningRecord {
    /// Block height
    pub block_height: String,
    /// Reward earned in wei (hex string)
    pub reward: String,
    /// FLOPS contributed
    pub flops: String,
    /// Number of tasks in this block
    pub task_count: u32,
    /// Block timestamp (ms since epoch)
    pub timestamp: String,
}

// ============ v2.0: Miner Event Subscription Types ============

/// Real-time miner event pushed via WebSocket subscription
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcMinerEvent {
    /// Event type: "proof_accepted", "proof_rejected", "task_assigned",
    /// "reward_settled", "slashing_applied", "score_changed", "daily_summary"
    pub event_type: String,
    /// Miner address
    pub miner: String,
    /// Block height (if applicable)
    pub block_height: Option<String>,
    /// Task type (e.g. "embedding", "text_generation")
    pub task_type: Option<String>,
    /// FLOPS contributed
    pub flops: Option<String>,
    /// Reward amount in wei (hex)
    pub reward: Option<String>,
    /// Whether spot-checked
    pub spot_checked: Option<bool>,
    /// Timestamp (ms since epoch)
    pub timestamp: String,
    /// Human-readable message
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
    /// Blake3 hash of the model weight file (0x-hex, optional)
    #[serde(default)]
    pub weights_hash: Option<String>,
    /// Sharded distribution manifest (ADR-0001, optional). Reuses the
    /// canonical `ShardManifest` serde shape: hashes are 0x-hex strings.
    #[serde(default)]
    pub shard_manifest: Option<qfc_inference::ShardManifest>,
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
    /// Language code for speech_to_text tasks (e.g. "en", "zh")
    #[serde(default)]
    pub language: Option<String>,
}

/// Filter parameters for listing public tasks
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcListPublicTasksFilter {
    /// Filter by submitter address (hex, optional)
    #[serde(default)]
    pub submitter: Option<String>,
    /// Filter by status (e.g. "Pending", "Completed", optional)
    #[serde(default)]
    pub status: Option<String>,
    /// Max results (default 50, max 200)
    #[serde(default = "default_list_limit")]
    pub limit: usize,
    /// Offset for pagination (default 0)
    #[serde(default)]
    pub offset: usize,
}

fn default_list_limit() -> usize {
    50
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

/// Request to estimate inference fee
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcEstimateInferenceFee {
    /// Model ID (e.g. "qfc-embed-small")
    pub model_id: String,
    /// Task type: "TextEmbedding", "TextGeneration", "ImageClassification", "OnnxInference"
    #[serde(default = "default_task_type")]
    pub task_type: String,
    /// Input size in bytes (used for future dynamic pricing, currently unused)
    #[serde(default)]
    pub input_size: u64,
    /// Max tokens (for TextGeneration tasks, default 100)
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u64,
}

fn default_task_type() -> String {
    "TextEmbedding".into()
}

fn default_max_tokens() -> u64 {
    100
}

/// Response for inference fee estimation
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcInferenceFeeEstimate {
    /// Estimated base fee in wei (hex)
    pub base_fee: String,
    /// Model ID
    pub model_id: String,
    /// GPU tier required: "Hot", "Warm", "Cold"
    pub gpu_tier: String,
    /// Estimated execution time in ms
    pub estimated_time_ms: u64,
    /// Minimum memory required in MB
    pub min_memory_mb: u64,
    /// Estimated FLOPS
    pub estimated_flops: u64,
}

// ============ v2.0 P2: Miner List ============

/// Summary of a registered miner for the miners list
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcRegisteredMiner {
    pub address: String,
    pub gpu_model: String,
    pub benchmark_score: u32,
    pub tier: u8,
    pub vram_mb: u64,
    pub backend: String,
    pub registered_at: String,
    /// Operating system: "linux", "macos", "windows"
    pub os: String,
    /// CPU architecture: "x86_64", "aarch64"
    pub arch: String,
    /// CPU model name (e.g. "Apple M2 Pro", "AMD Ryzen 9 7950X")
    pub cpu_model: String,
    /// Number of CPU cores
    pub cpu_cores: u32,
    /// Total system memory in MB
    pub total_memory_mb: u64,
    /// Miner software version (e.g. "2.2.1")
    pub version: String,
}

// ============ v2.0 P2: Miner Registration & Status Report Types ============

/// Request to register a miner with GPU profile
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcRegisterMinerRequest {
    /// Miner wallet address (hex)
    pub miner_address: String,
    /// Ed25519 public key (hex) — used to verify signature and derive address
    pub public_key: String,
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
    /// Operating system: "linux", "macos", "windows"
    #[serde(default)]
    pub os: String,
    /// CPU architecture: "x86_64", "aarch64"
    #[serde(default)]
    pub arch: String,
    /// CPU model name (e.g. "Apple M2 Pro")
    #[serde(default)]
    pub cpu_model: String,
    /// Number of CPU cores
    #[serde(default)]
    pub cpu_cores: u32,
    /// Total system memory in MB
    #[serde(default)]
    pub total_memory_mb: u64,
    /// Miner software version
    #[serde(default)]
    pub version: String,
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

// ============ v2.0: Miner Vesting Types ============

/// Miner vesting schedule — shows locked vs available mining rewards.
/// Rewards vest linearly over 30 days with a 7-day cliff.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcMinerVesting {
    /// Miner address
    pub miner: String,
    /// Total lifetime earnings (hex wei)
    pub total_earned: String,
    /// Currently vesting / locked amount (hex wei)
    pub locked: String,
    /// Available to withdraw (hex wei)
    pub available: String,
    /// Number of vesting tranches still active
    pub active_tranches: u64,
    /// Detailed vesting tranches (most recent first)
    pub tranches: Vec<RpcVestingTranche>,
    /// Explanatory note (set while miner rewards are not paid by the block
    /// path, so all vesting figures are zero)
    pub note: String,
}

/// A single vesting tranche from a block reward
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcVestingTranche {
    /// Block height where reward was earned
    pub block_height: String,
    /// Total reward amount in this tranche (hex wei)
    pub amount: String,
    /// Amount already vested / unlocked (hex wei)
    pub vested: String,
    /// Timestamp when vesting started (seconds)
    pub start_time: String,
    /// Timestamp when cliff ends (seconds)
    pub cliff_end: String,
    /// Timestamp when fully vested (seconds)
    pub end_time: String,
    /// Vesting progress percentage (0-100)
    pub percent_vested: u8,
}

// ============ v2.0: Miner Notification/Webhook Types ============

/// Request to register a webhook for miner event notifications
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcRegisterWebhookRequest {
    /// Miner address (hex)
    pub miner_address: String,
    /// Webhook URL (must be HTTPS, or Discord/Telegram webhook URL)
    pub url: String,
    /// Event types to receive: "all", "proof_accepted", "proof_rejected",
    /// "reward_settled", "slashing_applied", "score_changed", "daily_summary"
    #[serde(default = "default_events")]
    pub events: Vec<String>,
    /// Ed25519 signature of the URL for authentication
    pub signature: String,
}

fn default_events() -> Vec<String> {
    vec!["all".to_string()]
}

/// Request to remove a webhook
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcRemoveWebhookRequest {
    /// Miner address (hex)
    pub miner_address: String,
    /// Webhook ID to remove
    pub webhook_id: String,
    /// Ed25519 signature for authentication
    pub signature: String,
}

/// Registered webhook info
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcWebhook {
    /// Unique webhook ID
    pub id: String,
    /// Webhook URL (partially masked for security)
    pub url: String,
    /// Subscribed event types
    pub events: Vec<String>,
    /// Registration timestamp (seconds)
    pub created_at: String,
    /// Whether the webhook is active
    pub active: bool,
}

// ============ v2.0: Parameter Governance RPC Types ============

/// Request to propose a parameter change
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcProposeParameterRequest {
    /// Proposer address (hex)
    pub proposer: String,
    /// Parameter key (e.g. "block_reward", "min_gas_price")
    pub parameter: String,
    /// Proposed new value (decimal string)
    pub proposed_value: String,
    /// Description of the change
    pub description: String,
}

/// Request to vote on a parameter proposal
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcVoteParameterRequest {
    /// Proposal ID (hex)
    pub proposal_id: String,
    /// Voter address (hex)
    pub voter: String,
    /// Whether to approve
    pub approve: bool,
}

/// Parameter proposal information
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcParameterProposal {
    pub proposal_id: String,
    pub proposer: String,
    pub parameter: String,
    pub current_value: String,
    pub proposed_value: String,
    pub description: String,
    pub stake_for: String,
    pub stake_against: String,
    pub status: String,
    pub created_at: u64,
    pub voting_deadline: u64,
}

/// A parameter override applied by governance
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcParameterOverride {
    pub parameter: String,
    pub value: String,
}

// ============ v2.0: Treasury RPC Types ============

/// Treasury info (balance, stats)
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcTreasuryInfo {
    /// Treasury address (hex)
    pub address: String,
    /// Current balance in wei (decimal string)
    pub balance: String,
    /// Total amount disbursed via governance
    pub total_disbursed: String,
    /// Number of active spend proposals
    pub active_proposals: u64,
}

/// Request to propose a treasury spend
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcProposeSpendRequest {
    /// Proposer address (hex)
    pub proposer: String,
    /// Recipient address (hex)
    pub recipient: String,
    /// Amount in wei (decimal string)
    pub amount: String,
    /// Description of the spend
    pub description: String,
}

/// Request to vote on a treasury spend proposal
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcVoteSpendRequest {
    /// Proposal ID (hex)
    pub proposal_id: String,
    /// Voter address (hex)
    pub voter: String,
    /// Whether to approve
    pub approve: bool,
}

/// Treasury spend proposal information
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcSpendProposal {
    pub proposal_id: String,
    pub proposer: String,
    pub recipient: String,
    pub amount: String,
    pub description: String,
    pub stake_for: String,
    pub stake_against: String,
    pub status: String,
    pub created_at: u64,
    pub voting_deadline: u64,
}

// ============ v2.0: Bridge RPC Types ============

/// Bridge status
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcBridgeStatus {
    pub active: bool,
    pub validator_count: usize,
    pub threshold: usize,
    pub total_deposits: u64,
    pub total_withdrawals: u64,
    pub pending_deposits: u64,
    pub pending_withdrawals: u64,
    pub total_value_locked: String,
}

/// Bridge deposit info
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcBridgeDeposit {
    pub deposit_id: String,
    pub eth_tx_hash: String,
    pub eth_block_number: u64,
    pub eth_sender: String,
    pub qfc_recipient: String,
    pub token_address: String,
    pub amount: String,
    pub status: String,
    pub signature_count: usize,
    pub observed_at: u64,
}

/// Bridge withdrawal info
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcBridgeWithdrawal {
    pub withdrawal_id: String,
    pub qfc_tx_hash: String,
    pub qfc_block_number: u64,
    pub qfc_sender: String,
    pub eth_recipient: String,
    pub token_address: String,
    pub amount: String,
    pub status: String,
    pub signature_count: usize,
    pub observed_at: u64,
    pub eth_unlock_tx: Option<String>,
}

/// Account rent info
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcAccountRentInfo {
    pub address: String,
    pub storage_deposit: String,
    pub storage_slot_count: u64,
    pub last_active_epoch: u64,
    pub is_dormant: bool,
    pub rent_owed: String,
    pub current_epoch: u64,
    pub reactivation_fee: String,
}

/// UserOperation request (EIP-4337)
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcUserOperation {
    pub sender: String,
    pub nonce: String,
    pub init_code: String,
    pub call_data: String,
    pub call_gas_limit: String,
    pub verification_gas_limit: String,
    pub pre_verification_gas: String,
    pub max_fee_per_gas: String,
    pub max_priority_fee_per_gas: String,
    pub paymaster_and_data: String,
    pub signature: String,
}

/// UserOperation status response
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcUserOperationStatus {
    pub user_op_hash: String,
    pub sender: String,
    pub nonce: u64,
    pub status: String,
    pub paymaster: Option<String>,
}

// ============ v2.0: Agent Registry RPC Types ============

/// Agent information from the AgentRegistry contract
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcAgentInfo {
    pub agent_id: String,
    pub owner: String,
    pub agent_address: String,
    pub permissions: Vec<u8>,
    pub daily_limit: String,
    pub max_per_tx: String,
    pub deposit: String,
    pub spent_today: String,
    pub last_reset: String,
    pub nonce: String,
    pub active: bool,
}

/// Session key validation result
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcSessionKeyInfo {
    pub valid: bool,
    pub agent_id: String,
    pub expires_at: String,
}

// ============ v2.0: Agent Registry Write RPC Types ============

/// Request to register a new agent
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcRegisterAgentRequest {
    /// Unique agent identifier (human-readable string)
    pub agent_id: String,
    /// Owner address (hex, the account that controls the agent)
    pub owner: String,
    /// Ed25519 public key of the owner (hex)
    pub public_key: String,
    /// Permission bitmask (array of allowed permission codes)
    #[serde(default)]
    pub permissions: Vec<u8>,
    /// Daily spending limit in wei (hex or decimal string)
    pub daily_limit: String,
    /// Max amount per transaction in wei (hex or decimal string)
    pub max_per_tx: String,
    /// Ed25519 signature over (agent_id || owner || daily_limit || max_per_tx) for authorization
    pub signature: String,
}

/// Request to fund an agent with deposit
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcFundAgentRequest {
    /// Agent ID to fund
    pub agent_id: String,
    /// Funder address (hex) — must be the agent owner
    pub funder: String,
    /// Ed25519 public key of the funder (hex)
    pub public_key: String,
    /// Amount to deposit in wei (hex or decimal string)
    pub amount: String,
    /// Ed25519 signature over (agent_id || funder || amount) for authorization
    pub signature: String,
}

/// Request to revoke (deactivate) an agent
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcRevokeAgentRequest {
    /// Agent ID to revoke
    pub agent_id: String,
    /// Owner address (hex) — must be the agent owner
    pub owner: String,
    /// Ed25519 public key of the owner (hex)
    pub public_key: String,
    /// Ed25519 signature over (agent_id || owner || "revoke") for authorization
    pub signature: String,
}

/// Result of an agent write operation (register/fund/revoke)
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcAgentWriteResult {
    /// Transaction hash (hex)
    pub tx_hash: String,
    /// Agent ID affected
    pub agent_id: String,
    /// Human-readable status message
    pub message: String,
}

// ============ v3.0: Agent Discovery + Resource RPC Types ============

/// Params for listing agents
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcListAgentsParams {
    /// "active" | "frozen" | "all" (default "active")
    #[serde(default = "default_status_active")]
    pub status: String,
    /// "reputation_score" | "stake" | "created_at" (default "reputation_score")
    #[serde(default = "default_sort_reputation")]
    pub sort_by: String,
    /// "desc" | "asc" (default "desc")
    #[serde(default = "default_sort_desc")]
    pub sort_order: String,
    #[serde(default = "default_limit_20")]
    pub limit: usize,
    #[serde(default)]
    pub offset: usize,
}

fn default_status_active() -> String {
    "active".to_string()
}
fn default_sort_reputation() -> String {
    "reputation_score".to_string()
}
fn default_sort_desc() -> String {
    "desc".to_string()
}
fn default_limit_20() -> usize {
    20
}

/// Response for listing agents
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcListAgentsResponse {
    pub agents: Vec<RpcAgentDetailView>,
    pub total: usize,
    pub has_more: bool,
}

/// Detailed agent view (v3 resource-based)
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcAgentDetailView {
    pub agent_id: String,
    pub owner: String,
    pub capabilities: Vec<String>,
    pub protocol_digests: Vec<String>,
    pub endpoint: String,
    pub stake: String,
    pub frozen: bool,
    pub reputation_score: u64,
    pub total_tasks_completed: u64,
    pub total_tasks_failed: u64,
    pub last_heartbeat: u64,
    pub created_at: u64,
}

/// Params for querying agents by capability
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcQueryByCapabilityParams {
    pub capability: String,
    #[serde(default)]
    pub min_reputation: u64,
    #[serde(default)]
    pub min_stake: String,
    #[serde(default = "default_limit_10")]
    pub limit: usize,
}

fn default_limit_10() -> usize {
    10
}

/// Params for freezing an agent
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcFreezeAgentParams {
    pub agent_id: String,
    #[serde(default)]
    pub reason: String,
}

/// Params for issuing a session key
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcIssueSessionKeyParams {
    pub agent_id: String,
    pub public_key: String,
    /// Permission bitmask (e.g. 0x01 for inference, 0x03 for inference+transfer)
    pub permissions: u64,
    /// Max spend per period (decimal string)
    #[serde(default)]
    pub spending_limit: String,
    /// Period duration in seconds (default 86400 = 1 day)
    #[serde(default = "default_period_day")]
    pub period_duration: u64,
    /// TTL in seconds (max 604800 = 7 days)
    pub ttl_secs: u64,
}

fn default_period_day() -> u64 {
    86400
}

/// Session key detail (v3)
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcSessionKeyDetail {
    pub key_id: String,
    pub agent_id: String,
    pub public_key: String,
    pub permissions: u64,
    pub permissions_names: Vec<String>,
    pub spending_limit: String,
    pub spent_this_period: String,
    pub period_duration: u64,
    pub expires_at: u64,
    pub nonce: u64,
    pub created_at: u64,
}

/// Agent balance response
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcAgentBalance {
    pub agent_id: String,
    pub stake: String,
    pub stake_tier: String,
}
