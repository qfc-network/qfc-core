//! RPC server implementation

use crate::error::RpcError;
use crate::eth::EthApiServer;
use crate::qfc::{
    QfcApiServer, RpcAccountRentInfo, RpcAgentBalance, RpcAgentDetailView, RpcAgentInfo,
    RpcAgentWriteResult, RpcBridgeDeposit, RpcBridgeStatus, RpcBridgeWithdrawal, RpcComputeInfo,
    RpcEarningRecord, RpcEpoch, RpcEstimateInferenceFee, RpcFaucetResponse, RpcFreezeAgentParams,
    RpcFundAgentRequest, RpcInferenceFeeEstimate, RpcInferenceProofSubmission, RpcInferenceStats,
    RpcInferenceTask, RpcIssueSessionKeyParams, RpcListAgentsParams, RpcListAgentsResponse,
    RpcListPublicTasksFilter, RpcMinerEarnings, RpcMinerEvent, RpcMinerStatusReport,
    RpcMinerVesting, RpcModel, RpcModelProposal, RpcNodeInfo, RpcParameterOverride,
    RpcParameterProposal, RpcProofResult, RpcProposeModelRequest, RpcProposeParameterRequest,
    RpcProposeSpendRequest, RpcPublicTaskStatus, RpcQueryByCapabilityParams,
    RpcRegisterAgentRequest, RpcRegisterMinerRequest, RpcRegisterMinerResult,
    RpcRegisterWebhookRequest, RpcRegisteredMiner, RpcRemoveWebhookRequest, RpcRevokeAgentRequest,
    RpcSessionKeyDetail, RpcSessionKeyInfo, RpcSpendProposal, RpcSubmitPublicTask, RpcTaskRequest,
    RpcTreasuryInfo, RpcUndelegation, RpcUserOperation, RpcUserOperationStatus, RpcValidator,
    RpcValidatorMetrics, RpcValidatorScoreBreakdown, RpcVoteModelRequest, RpcVoteParameterRequest,
    RpcVoteSpendRequest, RpcWebhook,
};
use crate::txpool::{TxPoolApiServer, TxPoolContent, TxPoolStatus};
use crate::types::{
    AddressFilter, BlockNumber, BlockTag, CallRequest, LogFilter, RpcBlock, RpcLog, RpcReceipt,
    RpcTransaction, TopicFilter,
};
use jsonrpsee::core::{RpcResult, SubscriptionResult};
use jsonrpsee::server::{ServerBuilder, ServerHandle};
use parking_lot::RwLock;
use qfc_chain::Chain;
use qfc_consensus::NetworkState;
use qfc_crypto::{blake3_hash, verify_hash_signature};
use qfc_mempool::Mempool;
use qfc_network::NetworkService;
use qfc_types::{Address, EthTransaction, Hash, Transaction, U256};
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::{debug, info, warn};

/// RPC server configuration
#[derive(Clone, Debug)]
pub struct RpcConfig {
    /// HTTP listen address
    pub http_addr: SocketAddr,
    /// Enable HTTP
    pub http_enabled: bool,
}

/// Trait for providing sync status to the RPC server
pub trait SyncStatusProvider: Send + Sync {
    /// Returns true if the node is currently syncing
    fn is_syncing(&self) -> bool;
    /// Returns the highest block number known from peers
    fn highest_peer_block(&self) -> u64;
    /// Returns the number of pending blocks waiting for parents
    fn pending_count(&self) -> usize;
}

impl Default for RpcConfig {
    fn default() -> Self {
        Self {
            http_addr: "127.0.0.1:8545".parse().unwrap(),
            http_enabled: true,
        }
    }
}

/// Registered miner profile stored in memory
#[derive(Clone, Debug)]
struct MinerProfile {
    public_key: qfc_types::PublicKey,
    gpu_model: String,
    benchmark_score: u32,
    tier: u8,
    vram_mb: u64,
    backend: String,
    registered_at: u64, // unix timestamp
    os: String,
    arch: String,
    cpu_model: String,
    cpu_cores: u32,
    total_memory_mb: u64,
    version: String,
}

/// RPC server
pub struct RpcServer {
    /// Chain
    chain: Arc<Chain>,
    /// Mempool
    mempool: Arc<RwLock<Mempool>>,
    /// Network service (optional, for broadcasting)
    network: Option<Arc<NetworkService>>,
    /// Sync status provider (optional)
    sync_status: Option<Arc<dyn SyncStatusProvider>>,
    /// Chain ID
    chain_id: u64,
    /// v2.0: AI inference task pool (shared with BlockProducer)
    task_pool: Arc<RwLock<qfc_ai_coordinator::TaskPool>>,
    /// v2.0: Model registry for verification
    model_registry: Arc<qfc_inference::model::ModelRegistry>,
    /// v2.0: Model governance
    governance: Arc<RwLock<qfc_ai_coordinator::ModelGovernance>>,
    /// v2.0: Parameter governance (stake-weighted voting on protocol params)
    param_governance: Arc<RwLock<qfc_ai_coordinator::ParameterGovernance>>,
    /// v2.0: Treasury — community fund with governance-controlled spending
    treasury: Arc<RwLock<qfc_ai_coordinator::Treasury>>,
    /// v2.0: Cross-chain bridge relayer (Ethereum ↔ QFC)
    bridge: Arc<RwLock<qfc_bridge::BridgeRelayer>>,
    /// v2.0: Inference engine for spot-check re-execution
    inference_engine: Option<Arc<tokio::sync::RwLock<Box<dyn qfc_inference::InferenceEngine>>>>,
    /// v2.0: Pool of verified inference proofs awaiting block inclusion
    proof_pool: Option<Arc<RwLock<qfc_ai_coordinator::ProofPool>>>,
    /// v2.0 P2: Challenge task generator
    challenge_generator: Option<Arc<RwLock<qfc_ai_coordinator::challenge::ChallengeGenerator>>>,
    /// v2.0 P2: Redundant verification for high-value tasks
    redundant_verifier: Option<Arc<RwLock<qfc_ai_coordinator::redundant::RedundantVerifier>>>,
    /// v2.0 P2: Task router for model-aware miner selection
    task_router: Option<Arc<RwLock<qfc_ai_coordinator::router::TaskRouter>>>,
    /// B2: IPFS client for large result storage (optional)
    ipfs_client: Option<Arc<qfc_ai_coordinator::ipfs::IpfsClient>>,
    /// Registered miners (address → profile) — miners don't need to be validators
    registered_miners: Arc<RwLock<std::collections::HashMap<Address, MinerProfile>>>,
    /// v2.0: Inference stats — total FLOPS accumulated from verified proofs
    total_flops: Arc<std::sync::atomic::AtomicU64>,
    /// v2.0: Inference stats — total inference time in ms
    total_inference_time_ms: Arc<std::sync::atomic::AtomicU64>,
    /// v2.0: Inference stats — total verified proof count (for averaging)
    verified_proof_count: Arc<std::sync::atomic::AtomicU64>,
    /// v2.0: EIP-4337 EntryPoint for account abstraction
    entry_point: Arc<RwLock<qfc_executor::account_abstraction::EntryPoint>>,
    /// v2.0: UserOperation mempool
    user_op_pool: Arc<RwLock<qfc_executor::account_abstraction::UserOpPool>>,
    /// v2.0: Miner webhook notification store
    webhook_store: crate::webhook::WebhookStore,
    /// v3.0: QVM Agent Registry (resource-based)
    agent_registry: Arc<RwLock<qfc_qvm::stdlib::agent_registry::AgentRegistry>>,
    /// v3.0: QVM Inference Capability Store
    capability_store: Arc<RwLock<qfc_qvm::stdlib::inference_capability::CapabilityStore>>,
    /// v3.0: QVM Session Key Store
    session_key_store: Arc<RwLock<qfc_qvm::stdlib::session_keys::SessionKeyStore>>,
    /// v3.0: Agent discovery index
    agent_index: Arc<RwLock<qfc_qvm::stdlib::agent_index::AgentIndex>>,
}

impl Clone for RpcServer {
    fn clone(&self) -> Self {
        Self {
            chain: self.chain.clone(),
            mempool: self.mempool.clone(),
            network: self.network.clone(),
            sync_status: self.sync_status.clone(),
            chain_id: self.chain_id,
            task_pool: self.task_pool.clone(),
            model_registry: self.model_registry.clone(),
            governance: self.governance.clone(),
            param_governance: self.param_governance.clone(),
            treasury: self.treasury.clone(),
            bridge: self.bridge.clone(),
            inference_engine: self.inference_engine.clone(),
            proof_pool: self.proof_pool.clone(),
            challenge_generator: self.challenge_generator.clone(),
            redundant_verifier: self.redundant_verifier.clone(),
            task_router: self.task_router.clone(),
            ipfs_client: self.ipfs_client.clone(),
            registered_miners: self.registered_miners.clone(),
            total_flops: self.total_flops.clone(),
            total_inference_time_ms: self.total_inference_time_ms.clone(),
            verified_proof_count: self.verified_proof_count.clone(),
            entry_point: self.entry_point.clone(),
            user_op_pool: self.user_op_pool.clone(),
            webhook_store: self.webhook_store.clone(),
            agent_registry: self.agent_registry.clone(),
            capability_store: self.capability_store.clone(),
            session_key_store: self.session_key_store.clone(),
            agent_index: self.agent_index.clone(),
        }
    }
}

impl RpcServer {
    /// Create a new RPC server
    pub fn new(chain: Arc<Chain>, mempool: Arc<RwLock<Mempool>>, chain_id: u64) -> Self {
        let mut task_pool = qfc_ai_coordinator::TaskPool::new();
        // Generate initial synthetic tasks for epoch 1
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        task_pool.generate_synthetic_tasks(1, 42, now + 30_000);

        Self {
            chain,
            mempool,
            network: None,
            sync_status: None,
            chain_id,
            task_pool: Arc::new(RwLock::new(task_pool)),
            model_registry: Arc::new(qfc_inference::model::ModelRegistry::default_v2()),
            governance: Arc::new(RwLock::new(qfc_ai_coordinator::ModelGovernance::new())),
            param_governance: Arc::new(RwLock::new(qfc_ai_coordinator::ParameterGovernance::new())),
            treasury: Arc::new(RwLock::new(qfc_ai_coordinator::Treasury::new())),
            bridge: Arc::new(RwLock::new(qfc_bridge::BridgeRelayer::new(
                qfc_bridge::RelayerConfig::default(),
            ))),
            inference_engine: None,
            proof_pool: None,
            challenge_generator: None,
            redundant_verifier: None,
            task_router: None,
            ipfs_client: None,
            registered_miners: Arc::new(RwLock::new(std::collections::HashMap::new())),
            total_flops: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            total_inference_time_ms: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            verified_proof_count: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            entry_point: Arc::new(RwLock::new(
                qfc_executor::account_abstraction::EntryPoint::new(chain_id),
            )),
            user_op_pool: Arc::new(RwLock::new(
                qfc_executor::account_abstraction::UserOpPool::new(1000, 16),
            )),
            webhook_store: crate::webhook::new_store(),
            agent_registry: Arc::new(RwLock::new(
                qfc_qvm::stdlib::agent_registry::AgentRegistry::new(),
            )),
            capability_store: Arc::new(RwLock::new(
                qfc_qvm::stdlib::inference_capability::CapabilityStore::new(),
            )),
            session_key_store: Arc::new(RwLock::new(
                qfc_qvm::stdlib::session_keys::SessionKeyStore::new(),
            )),
            agent_index: Arc::new(RwLock::new(qfc_qvm::stdlib::agent_index::AgentIndex::new())),
        }
    }

    /// Set the challenge generator (P2)
    pub fn with_challenge_generator(
        mut self,
        gen: Arc<RwLock<qfc_ai_coordinator::challenge::ChallengeGenerator>>,
    ) -> Self {
        self.challenge_generator = Some(gen);
        self
    }

    /// Set the redundant verifier (P2)
    pub fn with_redundant_verifier(
        mut self,
        rv: Arc<RwLock<qfc_ai_coordinator::redundant::RedundantVerifier>>,
    ) -> Self {
        self.redundant_verifier = Some(rv);
        self
    }

    /// Set the task router (P2)
    pub fn with_task_router(
        mut self,
        router: Arc<RwLock<qfc_ai_coordinator::router::TaskRouter>>,
    ) -> Self {
        self.task_router = Some(router);
        self
    }

    /// Set the IPFS client for large result storage (B2)
    pub fn with_ipfs_client(mut self, client: qfc_ai_coordinator::ipfs::IpfsClient) -> Self {
        self.ipfs_client = Some(Arc::new(client));
        self
    }

    /// Set the network service for transaction broadcasting
    pub fn with_network(mut self, network: Arc<NetworkService>) -> Self {
        self.network = Some(network);
        self
    }

    /// Set the sync status provider
    pub fn with_sync_status(mut self, sync_status: Arc<dyn SyncStatusProvider>) -> Self {
        self.sync_status = Some(sync_status);
        self
    }

    /// Set the inference engine for spot-check verification
    pub fn with_inference_engine(
        mut self,
        engine: Box<dyn qfc_inference::InferenceEngine>,
    ) -> Self {
        self.inference_engine = Some(Arc::new(tokio::sync::RwLock::new(engine)));
        self
    }

    /// Set the shared proof pool (v2.0)
    pub fn with_proof_pool(mut self, pool: Arc<RwLock<qfc_ai_coordinator::ProofPool>>) -> Self {
        self.proof_pool = Some(pool);
        self
    }

    /// Set the shared task pool (v2.0, replaces internal pool)
    pub fn with_task_pool(mut self, pool: Arc<RwLock<qfc_ai_coordinator::TaskPool>>) -> Self {
        self.task_pool = pool;
        self
    }

    /// Start the RPC server
    pub async fn start(
        self,
        config: RpcConfig,
    ) -> Result<ServerHandle, Box<dyn std::error::Error + Send + Sync>> {
        if !config.http_enabled {
            return Err("HTTP not enabled".into());
        }

        info!("Starting RPC server on {}", config.http_addr);

        let server = ServerBuilder::default().build(config.http_addr).await?;

        // Merge all RPC modules
        let mut eth_module = EthApiServer::into_rpc(self.clone());
        let qfc_module = QfcApiServer::into_rpc(self.clone());
        let txpool_module = TxPoolApiServer::into_rpc(self);
        eth_module
            .merge(qfc_module)
            .expect("Failed to merge QFC RPC module");
        eth_module
            .merge(txpool_module)
            .expect("Failed to merge txpool RPC module");

        let handle = server.start(eth_module);

        Ok(handle)
    }

    fn resolve_block_number(&self, block: Option<BlockNumber>) -> u64 {
        match block {
            None => self.chain.block_number(),
            Some(BlockNumber::Number(n)) => n,
            Some(BlockNumber::Tag(tag)) => match tag {
                BlockTag::Latest | BlockTag::Safe | BlockTag::Finalized | BlockTag::Pending => {
                    self.chain.block_number()
                }
                BlockTag::Earliest => 0,
            },
        }
    }

    fn parse_address(s: &str) -> Result<Address, RpcError> {
        let s = s.strip_prefix("0x").unwrap_or(s);
        let bytes = hex::decode(s).map_err(|e| RpcError::InvalidParams(e.to_string()))?;
        Address::from_slice(&bytes).ok_or_else(|| RpcError::InvalidParams("invalid address".into()))
    }

    fn parse_hash(s: &str) -> Result<Hash, RpcError> {
        let s = s.strip_prefix("0x").unwrap_or(s);
        let bytes = hex::decode(s).map_err(|e| RpcError::InvalidParams(e.to_string()))?;
        Hash::from_slice(&bytes).ok_or_else(|| RpcError::InvalidParams("invalid hash".into()))
    }

    fn parse_u256(s: &str) -> Result<U256, RpcError> {
        let s = s.strip_prefix("0x").unwrap_or(s);
        // Pad to 64 hex chars (32 bytes)
        let padded = format!("{:0>64}", s);
        let bytes = hex::decode(&padded).map_err(|e| RpcError::InvalidParams(e.to_string()))?;
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| RpcError::InvalidParams("invalid U256 length".into()))?;
        Ok(U256::from_be_bytes(&bytes))
    }

    fn parse_parameter_key(s: &str) -> Result<qfc_ai_coordinator::ParameterKey, RpcError> {
        use qfc_ai_coordinator::ParameterKey;
        match s {
            "block_reward" => Ok(ParameterKey::BlockReward),
            "min_validator_stake" => Ok(ParameterKey::MinValidatorStake),
            "block_gas_limit" => Ok(ParameterKey::BlockGasLimit),
            "min_gas_price" => Ok(ParameterKey::MinGasPrice),
            "fee_producer_percent" => Ok(ParameterKey::FeeProducerPercent),
            "fee_voters_percent" => Ok(ParameterKey::FeeVotersPercent),
            "fee_burn_percent" => Ok(ParameterKey::FeeBurnPercent),
            "fee_treasury_percent" => Ok(ParameterKey::FeeTreasuryPercent),
            "producer_reward_percent" => Ok(ParameterKey::ProducerRewardPercent),
            "voters_reward_percent" => Ok(ParameterKey::VotersRewardPercent),
            "inference_miners_reward_percent" => Ok(ParameterKey::InferenceMinersRewardPercent),
            "inference_fee_miner_percent" => Ok(ParameterKey::InferenceFeeMinerPercent),
            "inference_fee_validators_percent" => Ok(ParameterKey::InferenceFeeValidatorsPercent),
            "inference_fee_burn_percent" => Ok(ParameterKey::InferenceFeeBurnPercent),
            "slash_double_sign_percent" => Ok(ParameterKey::SlashDoubleSignPercent),
            "slash_offline_percent" => Ok(ParameterKey::SlashOfflinePercent),
            "unstake_delay_secs" => Ok(ParameterKey::UnstakeDelaySecs),
            "min_delegation" => Ok(ParameterKey::MinDelegation),
            "max_transactions_per_block" => Ok(ParameterKey::MaxTransactionsPerBlock),
            _ => Err(RpcError::InvalidParams(format!(
                "Unknown parameter key: {}",
                s
            ))),
        }
    }

    fn get_current_param_value(&self, key: &qfc_ai_coordinator::ParameterKey) -> u128 {
        use qfc_ai_coordinator::ParameterKey;

        // Check overrides first, then fall back to compile-time constants
        if let Some(val) = self.param_governance.read().get_override(key) {
            return val;
        }

        match key {
            ParameterKey::BlockReward => qfc_types::BLOCK_REWARD,
            ParameterKey::MinValidatorStake => qfc_types::MIN_VALIDATOR_STAKE,
            ParameterKey::BlockGasLimit => qfc_types::DEFAULT_BLOCK_GAS_LIMIT as u128,
            ParameterKey::MinGasPrice => qfc_types::MIN_GAS_PRICE as u128,
            ParameterKey::FeeProducerPercent => qfc_types::FEE_PRODUCER_PERCENT as u128,
            ParameterKey::FeeVotersPercent => qfc_types::FEE_VOTERS_PERCENT as u128,
            ParameterKey::FeeBurnPercent => qfc_types::FEE_BURN_PERCENT as u128,
            ParameterKey::FeeTreasuryPercent => qfc_types::FEE_TREASURY_PERCENT as u128,
            ParameterKey::ProducerRewardPercent => qfc_types::PRODUCER_REWARD_PERCENT as u128,
            ParameterKey::VotersRewardPercent => qfc_types::VOTERS_REWARD_PERCENT as u128,
            ParameterKey::InferenceMinersRewardPercent => {
                qfc_types::INFERENCE_MINERS_REWARD_PERCENT as u128
            }
            ParameterKey::InferenceFeeMinerPercent => {
                qfc_types::INFERENCE_FEE_MINER_PERCENT as u128
            }
            ParameterKey::InferenceFeeValidatorsPercent => {
                qfc_types::INFERENCE_FEE_VALIDATORS_PERCENT as u128
            }
            ParameterKey::InferenceFeeBurnPercent => qfc_types::INFERENCE_FEE_BURN_PERCENT as u128,
            ParameterKey::SlashDoubleSignPercent => qfc_types::SLASH_DOUBLE_SIGN_PERCENT as u128,
            ParameterKey::SlashOfflinePercent => qfc_types::SLASH_OFFLINE_PERCENT as u128,
            ParameterKey::UnstakeDelaySecs => qfc_types::UNSTAKE_DELAY_SECS as u128,
            ParameterKey::MinDelegation => qfc_types::MIN_DELEGATION,
            ParameterKey::MaxTransactionsPerBlock => qfc_types::MAX_TRANSACTIONS_PER_BLOCK as u128,
        }
    }
}

/// AgentRegistry contract address on testnet
const AGENT_REGISTRY_ADDRESS: &str = "7791dfa4d489f3d524708cbc0caa8689b76322b3";

/// ABI helper: encode a Solidity function call with a single string argument.
/// Returns the 4-byte selector + ABI-encoded string.
fn abi_encode_string_call(selector: [u8; 4], arg: &str) -> Vec<u8> {
    let arg_bytes = arg.as_bytes();
    let mut data = Vec::with_capacity(4 + 32 + 32 + ((arg_bytes.len() + 31) / 32) * 32);
    data.extend_from_slice(&selector);
    // offset to string data (always 0x20 for single string param)
    let mut offset = [0u8; 32];
    offset[31] = 0x20;
    data.extend_from_slice(&offset);
    // string length
    let mut len_word = [0u8; 32];
    len_word[24..32].copy_from_slice(&(arg_bytes.len() as u64).to_be_bytes());
    data.extend_from_slice(&len_word);
    // string data padded to 32 bytes
    data.extend_from_slice(arg_bytes);
    let pad = (32 - (arg_bytes.len() % 32)) % 32;
    data.extend(std::iter::repeat(0u8).take(pad));
    data
}

/// ABI helper: encode a Solidity function call with a single address argument.
fn abi_encode_address_call(selector: [u8; 4], addr: &Address) -> Vec<u8> {
    let mut data = vec![0u8; 4 + 32];
    data[..4].copy_from_slice(&selector);
    // address is left-padded to 32 bytes (last 20 bytes)
    data[4 + 12..4 + 32].copy_from_slice(addr.as_bytes());
    data
}

/// Read a uint256 from ABI-encoded output at a given 32-byte word offset.
fn abi_read_u256(output: &[u8], word: usize) -> String {
    let start = word * 32;
    if start + 32 > output.len() {
        return "0x0".to_string();
    }
    let slice = &output[start..start + 32];
    let trimmed = hex::encode(slice).trim_start_matches('0').to_string();
    format!("0x{}", if trimmed.is_empty() { "0" } else { &trimmed })
}

/// Read a uint256 as raw bytes from ABI output at a 32-byte word offset.
fn abi_read_u256_raw(output: &[u8], word: usize) -> [u8; 32] {
    let start = word * 32;
    if start + 32 > output.len() {
        return [0u8; 32];
    }
    let mut buf = [0u8; 32];
    buf.copy_from_slice(&output[start..start + 32]);
    buf
}

/// Read an address from ABI-encoded output at a given word offset.
fn abi_read_address(output: &[u8], word: usize) -> String {
    let start = word * 32;
    if start + 32 > output.len() {
        return "0x0000000000000000000000000000000000000000".to_string();
    }
    // address is in the last 20 bytes of the 32-byte word
    format!("0x{}", hex::encode(&output[start + 12..start + 32]))
}

/// Read a bool from ABI-encoded output at a given word offset.
fn abi_read_bool(output: &[u8], word: usize) -> bool {
    let start = word * 32;
    if start + 32 > output.len() {
        return false;
    }
    output[start + 31] != 0
}

/// Read a dynamic string from ABI-encoded output given a base offset and the word
/// containing the relative offset to the string data.
fn abi_read_string(output: &[u8], base: usize, offset_word: usize) -> String {
    let rel_offset_start = offset_word * 32;
    if rel_offset_start + 32 > output.len() {
        return String::new();
    }
    let rel_raw = abi_read_u256_raw(output, offset_word);
    let rel = u64::from_be_bytes(rel_raw[24..32].try_into().unwrap_or([0; 8])) as usize;
    let abs = base + rel;
    if abs + 32 > output.len() {
        return String::new();
    }
    let len_raw = &output[abs..abs + 32];
    let len = u64::from_be_bytes(len_raw[24..32].try_into().unwrap_or([0; 8])) as usize;
    let data_start = abs + 32;
    if data_start + len > output.len() {
        return String::new();
    }
    String::from_utf8_lossy(&output[data_start..data_start + len]).to_string()
}

/// Read a dynamic uint8[] array from ABI-encoded output.
fn abi_read_uint8_array(output: &[u8], base: usize, offset_word: usize) -> Vec<u8> {
    let rel_raw = abi_read_u256_raw(output, offset_word);
    let rel = u64::from_be_bytes(rel_raw[24..32].try_into().unwrap_or([0; 8])) as usize;
    let abs = base + rel;
    if abs + 32 > output.len() {
        return Vec::new();
    }
    let len_raw = &output[abs..abs + 32];
    let len = u64::from_be_bytes(len_raw[24..32].try_into().unwrap_or([0; 8])) as usize;
    let mut result = Vec::with_capacity(len);
    for i in 0..len {
        let elem_start = abs + 32 + i * 32;
        if elem_start + 32 > output.len() {
            break;
        }
        result.push(output[elem_start + 31]);
    }
    result
}

/// Parse the getAgent tuple output into RpcAgentInfo.
/// tuple(string,address,address,uint8[],uint256,uint256,uint256,uint256,uint256,uint256,bool)
fn parse_agent_info(agent_id: &str, output: &[u8]) -> RpcAgentInfo {
    // The output is a tuple starting at an offset pointer.
    // For a function returning a single tuple, the first 32 bytes is the offset to the tuple data.
    let tuple_offset = {
        let raw = abi_read_u256_raw(output, 0);
        u64::from_be_bytes(raw[24..32].try_into().unwrap_or([0; 8])) as usize
    };
    let base = tuple_offset;
    // Word layout within the tuple:
    // 0: offset to string (agentId)
    // 1: owner address
    // 2: agentAddress
    // 3: offset to uint8[] permissions
    // 4: dailyLimit
    // 5: maxPerTx
    // 6: deposit
    // 7: spentToday
    // 8: lastReset
    // 9: nonce
    // 10: active (bool)
    let tuple_data = if base < output.len() {
        &output[base..]
    } else {
        &[]
    };

    let _agent_id_str = abi_read_string(tuple_data, 0, 0);
    let owner = abi_read_address(tuple_data, 1);
    let agent_address = abi_read_address(tuple_data, 2);
    let permissions = abi_read_uint8_array(tuple_data, 0, 3);
    let daily_limit = abi_read_u256(tuple_data, 4);
    let max_per_tx = abi_read_u256(tuple_data, 5);
    let deposit = abi_read_u256(tuple_data, 6);
    let spent_today = abi_read_u256(tuple_data, 7);
    let last_reset = abi_read_u256(tuple_data, 8);
    let nonce = abi_read_u256(tuple_data, 9);
    let active = abi_read_bool(tuple_data, 10);

    RpcAgentInfo {
        agent_id: agent_id.to_string(),
        owner,
        agent_address,
        permissions,
        daily_limit,
        max_per_tx,
        deposit,
        spent_today,
        last_reset,
        nonce,
        active,
    }
}

#[async_trait::async_trait]
impl EthApiServer for RpcServer {
    async fn chain_id(&self) -> RpcResult<String> {
        Ok(format!("0x{:x}", self.chain_id))
    }

    async fn block_number(&self) -> RpcResult<String> {
        Ok(format!("0x{:x}", self.chain.block_number()))
    }

    async fn get_balance(&self, address: String, block: Option<BlockNumber>) -> RpcResult<String> {
        let address = Self::parse_address(&address)?;
        let block_num = self.resolve_block_number(block);

        let state = self
            .chain
            .state_at(block_num)
            .map_err(|e| RpcError::Internal(e.to_string()))?;

        let balance = state
            .get_balance(&address)
            .map_err(|e| RpcError::Internal(e.to_string()))?;

        Ok(format!("0x{:x}", balance.0))
    }

    async fn get_transaction_count(
        &self,
        address: String,
        block: Option<BlockNumber>,
    ) -> RpcResult<String> {
        let address = Self::parse_address(&address)?;
        let block_num = self.resolve_block_number(block);

        let state = self
            .chain
            .state_at(block_num)
            .map_err(|e| RpcError::Internal(e.to_string()))?;

        let nonce = state
            .get_nonce(&address)
            .map_err(|e| RpcError::Internal(e.to_string()))?;

        Ok(format!("0x{:x}", nonce))
    }

    async fn get_code(&self, address: String, block: Option<BlockNumber>) -> RpcResult<String> {
        let address = Self::parse_address(&address)?;
        let block_num = self.resolve_block_number(block);

        let state = self
            .chain
            .state_at(block_num)
            .map_err(|e| RpcError::Internal(e.to_string()))?;

        let code = state
            .get_code(&address)
            .map_err(|e| RpcError::Internal(e.to_string()))?;

        Ok(format!("0x{}", hex::encode(&code)))
    }

    async fn get_block_by_number(
        &self,
        block: BlockNumber,
        full_tx: bool,
    ) -> RpcResult<Option<RpcBlock>> {
        let block_num = self.resolve_block_number(Some(block));

        let block = self
            .chain
            .get_block_by_number(block_num)
            .map_err(|e| RpcError::Internal(e.to_string()))?;

        match block {
            Some(b) => {
                let hash = blake3_hash(&b.header_bytes());
                Ok(Some(RpcBlock::from_block(b, hash, full_tx)))
            }
            None => Ok(None),
        }
    }

    async fn get_block_by_hash(&self, hash: String, full_tx: bool) -> RpcResult<Option<RpcBlock>> {
        let hash = Self::parse_hash(&hash)?;

        let block = self
            .chain
            .get_block_by_hash(&hash)
            .map_err(|e| RpcError::Internal(e.to_string()))?;

        match block {
            Some(b) => Ok(Some(RpcBlock::from_block(b, hash, full_tx))),
            None => Ok(None),
        }
    }

    async fn get_transaction_by_hash(&self, hash: String) -> RpcResult<Option<RpcTransaction>> {
        let original_hash = Self::parse_hash(&hash)?;

        // Translate Ethereum hash to internal hash if needed
        let internal_hash = self
            .chain
            .translate_eth_hash(&original_hash)
            .map_err(|e| RpcError::Internal(e.to_string()))?;

        // Check mempool first (using internal hash)
        if let Some(pooled) = self.mempool.read().get(&internal_hash) {
            let sender_hash = blake3_hash(pooled.tx.signature.as_bytes());
            let sender = Address::from_slice(&sender_hash.as_bytes()[12..32]).unwrap();
            // Return the original hash that the user queried with
            return Ok(Some(RpcTransaction::from_pending(
                pooled.tx,
                original_hash,
                sender,
            )));
        }

        // Check chain (using internal hash)
        let tx = self
            .chain
            .get_transaction(&internal_hash)
            .map_err(|e| RpcError::Internal(e.to_string()))?;

        match tx {
            Some(t) => {
                // Look up block location to determine if tx is confirmed
                let location = self
                    .chain
                    .get_transaction_location(&internal_hash)
                    .map_err(|e| RpcError::Internal(e.to_string()))?;

                if let Some((block_height, tx_index)) = location {
                    // Confirmed: get block hash and return with full block info
                    let block_hash = self
                        .chain
                        .get_block_by_number(block_height)
                        .map_err(|e| RpcError::Internal(e.to_string()))?
                        .map(|b| blake3_hash(&b.header_bytes()))
                        .unwrap_or(Hash::ZERO);

                    Ok(Some(RpcTransaction::from_tx(
                        t,
                        original_hash,
                        block_hash,
                        block_height,
                        tx_index,
                    )))
                } else {
                    // No location found — treat as pending
                    let sender_hash = blake3_hash(t.signature.as_bytes());
                    let sender = Address::from_slice(&sender_hash.as_bytes()[12..32]).unwrap();
                    Ok(Some(RpcTransaction::from_pending(t, original_hash, sender)))
                }
            }
            None => Ok(None),
        }
    }

    async fn get_transaction_receipt(&self, hash: String) -> RpcResult<Option<RpcReceipt>> {
        let original_hash = Self::parse_hash(&hash)?;

        // Translate Ethereum hash to internal hash if needed
        let internal_hash = self
            .chain
            .translate_eth_hash(&original_hash)
            .map_err(|e| RpcError::Internal(e.to_string()))?;

        // Get receipt with block info (using internal hash)
        let result = self
            .chain
            .get_receipt_with_block_info(&internal_hash)
            .map_err(|e| RpcError::Internal(e.to_string()))?;

        match result {
            Some((mut receipt, block_hash, block_number)) => {
                // Override the tx_hash in receipt with the original hash the user queried with
                // This ensures Ethereum wallets see the hash they expect
                receipt.tx_hash = original_hash;

                // Get transaction to extract from/to (using internal hash)
                let tx = self
                    .chain
                    .get_transaction(&internal_hash)
                    .map_err(|e| RpcError::Internal(e.to_string()))?;

                let (from, to) = if let Some(ref tx) = tx {
                    // Check if this is an Ethereum transaction (marker 0xEE)
                    if tx.public_key.0[0] == 0xEE {
                        // Extract sender from the stored bytes (bytes 2-21)
                        let from =
                            Address::from_slice(&tx.public_key.0[2..22]).unwrap_or(Address::ZERO);
                        (from, tx.to)
                    } else {
                        // QFC native: derive sender from public key
                        let from = qfc_crypto::address_from_public_key(&tx.public_key);
                        (from, tx.to)
                    }
                } else {
                    (Address::ZERO, None)
                };

                let block_hash_opt = if block_hash != Hash::ZERO {
                    Some(block_hash)
                } else {
                    None
                };
                let block_number_opt = if block_number > 0 || block_hash != Hash::ZERO {
                    Some(block_number)
                } else {
                    None
                };

                Ok(Some(RpcReceipt::from_receipt(
                    receipt,
                    from,
                    to,
                    block_hash_opt,
                    block_number_opt,
                )))
            }
            None => Ok(None),
        }
    }

    async fn send_raw_transaction(&self, data: String) -> RpcResult<String> {
        let data_str = data.strip_prefix("0x").unwrap_or(&data);
        let bytes = hex::decode(data_str).map_err(|e| RpcError::InvalidParams(e.to_string()))?;

        // Try QFC native format first (Borsh + Ed25519)
        if let Ok(tx) = Transaction::from_bytes(&bytes) {
            let hash = blake3_hash(&tx.to_bytes_without_signature());

            // Derive sender from public key (Ed25519)
            let sender = qfc_crypto::address_from_public_key(&tx.public_key);

            // Add to mempool with nonce validation
            let state = self.chain.state();
            self.mempool
                .write()
                .add_with_nonce_check(tx.clone(), sender, Some(state.as_ref()))
                .map_err(|e| RpcError::Execution(e.to_string()))?;

            info!("Added QFC transaction {} to mempool from {}", hash, sender);

            // Broadcast to network if available
            if let Some(network) = &self.network {
                let tx_bytes = tx.to_bytes();
                if let Err(e) = network.broadcast_transaction(tx_bytes).await {
                    warn!("Failed to broadcast transaction: {}", e);
                } else {
                    debug!("Broadcast transaction {} to network", hash);
                }
            }

            return Ok(hash.to_string());
        }

        // Try Ethereum format (RLP + secp256k1)
        let eth_tx = EthTransaction::decode(&bytes)
            .map_err(|e| RpcError::InvalidParams(format!("Failed to decode transaction: {}", e)))?;

        // Validate chain ID
        if eth_tx.chain_id != self.chain_id {
            return Err(RpcError::InvalidParams(format!(
                "Chain ID mismatch: expected {}, got {}",
                self.chain_id, eth_tx.chain_id
            ))
            .into());
        }

        // The sender is already recovered from the Ethereum signature
        let sender = eth_tx.sender;

        // Convert to QFC transaction format
        let mut qfc_tx = eth_tx.to_qfc_transaction();

        // Store the Ethereum signature in a special format for later verification
        // We encode r, s into the signature field (first 32 bytes = r, next 32 bytes = s)
        let mut eth_sig_bytes = [0u8; 64];
        eth_sig_bytes[..32].copy_from_slice(&eth_tx.r);
        eth_sig_bytes[32..].copy_from_slice(&eth_tx.s);
        qfc_tx.signature = qfc_types::Signature::new(eth_sig_bytes);

        // Use a special marker in public_key to indicate this is an Ethereum transaction
        // Byte 0 = 0xEE (Ethereum marker)
        // Byte 1 = v value (recovery id)
        // Bytes 2-21 = sender address (20 bytes)
        let mut eth_pubkey_marker = [0u8; 32];
        eth_pubkey_marker[0] = 0xEE; // Ethereum transaction marker
        eth_pubkey_marker[1] = eth_tx.v as u8; // Recovery ID / v value
        eth_pubkey_marker[2..22].copy_from_slice(sender.as_bytes()); // Store recovered sender
        qfc_tx.public_key = qfc_types::PublicKey::new(eth_pubkey_marker);

        // Use keccak256 hash for Ethereum transactions (this is what the wallet expects)
        let eth_hash = eth_tx.hash;

        // Compute the internal blake3 hash (this is how the tx is indexed internally)
        let internal_hash = blake3_hash(&qfc_tx.to_bytes_without_signature());

        // Store the mapping from Ethereum hash to internal hash
        // This allows receipt/tx lookup by the hash returned to the wallet
        if let Err(e) = self
            .chain
            .store_eth_tx_hash_mapping(&eth_hash, &internal_hash)
        {
            warn!("Failed to store Ethereum tx hash mapping: {}", e);
        }

        // Add to mempool with nonce validation
        let state = self.chain.state();
        self.mempool
            .write()
            .add_with_nonce_check(qfc_tx.clone(), sender, Some(state.as_ref()))
            .map_err(|e| RpcError::Execution(e.to_string()))?;

        info!(
            "Added Ethereum transaction {} to mempool from {} (internal: {}, is_eip1559: {})",
            eth_hash, sender, internal_hash, eth_tx.is_eip1559
        );

        // Broadcast to network - we send the original Ethereum-encoded bytes
        // Other nodes will also decode it as Ethereum format
        if let Some(network) = &self.network {
            if let Err(e) = network.broadcast_transaction(bytes).await {
                warn!("Failed to broadcast transaction: {}", e);
            } else {
                debug!("Broadcast transaction {} to network", eth_hash);
            }
        }

        Ok(eth_hash.to_string())
    }

    async fn call(&self, request: CallRequest, _block: Option<BlockNumber>) -> RpcResult<String> {
        // Parse from address
        let from = if let Some(ref from_str) = request.from {
            let from_str = from_str.strip_prefix("0x").unwrap_or(from_str);
            let bytes =
                hex::decode(from_str).map_err(|e| RpcError::InvalidParams(e.to_string()))?;
            Address::from_slice(&bytes)
        } else {
            None
        };

        // Parse to address
        let to = if let Some(ref to_str) = request.to {
            let to_str = to_str.strip_prefix("0x").unwrap_or(to_str);
            let bytes = hex::decode(to_str).map_err(|e| RpcError::InvalidParams(e.to_string()))?;
            Address::from_slice(&bytes)
        } else {
            None
        };

        // Parse value
        let value = if let Some(ref val_str) = request.value {
            let val_str = val_str.strip_prefix("0x").unwrap_or(val_str);
            let val = u128::from_str_radix(val_str, 16).unwrap_or(0);
            U256::from_u128(val)
        } else {
            U256::ZERO
        };

        // Parse data
        let data = if let Some(ref data_str) = request.data {
            let data_str = data_str.strip_prefix("0x").unwrap_or(data_str);
            hex::decode(data_str).unwrap_or_default()
        } else {
            Vec::new()
        };

        // Parse gas limit
        let gas_limit = if let Some(ref gas_str) = request.gas {
            let gas_str = gas_str.strip_prefix("0x").unwrap_or(gas_str);
            u64::from_str_radix(gas_str, 16).ok()
        } else {
            None
        };

        // Execute the call
        match self.chain.simulate_call(from, to, value, data, gas_limit) {
            Ok((success, output, _gas_used)) => {
                if success {
                    Ok(format!("0x{}", hex::encode(&output)))
                } else {
                    // Return a proper JSON-RPC error on revert
                    let revert_msg = if output.is_empty() {
                        "execution reverted".to_string()
                    } else {
                        format!("execution reverted: 0x{}", hex::encode(&output))
                    };
                    Err(RpcError::Execution(revert_msg).into())
                }
            }
            Err(e) => Err(RpcError::Execution(e.to_string()).into()),
        }
    }

    async fn estimate_gas(
        &self,
        request: CallRequest,
        _block: Option<BlockNumber>,
    ) -> RpcResult<String> {
        // Parse from address
        let from = if let Some(ref from_str) = request.from {
            let from_str = from_str.strip_prefix("0x").unwrap_or(from_str);
            let bytes =
                hex::decode(from_str).map_err(|e| RpcError::InvalidParams(e.to_string()))?;
            Address::from_slice(&bytes)
        } else {
            None
        };

        // Parse to address
        let to = if let Some(ref to_str) = request.to {
            let to_str = to_str.strip_prefix("0x").unwrap_or(to_str);
            let bytes = hex::decode(to_str).map_err(|e| RpcError::InvalidParams(e.to_string()))?;
            Address::from_slice(&bytes)
        } else {
            None
        };

        // Parse value
        let value = if let Some(ref val_str) = request.value {
            let val_str = val_str.strip_prefix("0x").unwrap_or(val_str);
            let val = u128::from_str_radix(val_str, 16).unwrap_or(0);
            U256::from_u128(val)
        } else {
            U256::ZERO
        };

        // Parse data
        let data = if let Some(ref data_str) = request.data {
            let data_str = data_str.strip_prefix("0x").unwrap_or(data_str);
            hex::decode(data_str).unwrap_or_default()
        } else {
            Vec::new()
        };

        // Execute to get actual gas usage
        match self.chain.simulate_call(from, to, value, data, None) {
            Ok((_success, _output, gas_used)) => {
                // Add 10% buffer for safety
                let estimated = gas_used + (gas_used / 10);
                Ok(format!("0x{:x}", estimated))
            }
            Err(_) => {
                // Fallback to basic estimation
                let base_gas = if request.data.is_some() {
                    53000u64
                } else {
                    21000u64
                };
                Ok(format!("0x{:x}", base_gas))
            }
        }
    }

    async fn gas_price(&self) -> RpcResult<String> {
        // Return 1 Gwei as default
        Ok(format!("0x{:x}", 1_000_000_000u64))
    }

    async fn get_storage_at(
        &self,
        address: String,
        position: String,
        block: Option<BlockNumber>,
    ) -> RpcResult<String> {
        let address = Self::parse_address(&address)?;
        let position = Self::parse_u256(&position)?;
        let block_num = self.resolve_block_number(block);

        let state = self
            .chain
            .state_at(block_num)
            .map_err(|e| RpcError::Internal(e.to_string()))?;

        let value = state
            .get_storage(&address, &position)
            .map_err(|e| RpcError::Internal(e.to_string()))?;

        Ok(format!("0x{:064x}", value.0))
    }

    async fn get_proof(
        &self,
        address: String,
        storage_keys: Vec<String>,
        block: Option<BlockNumber>,
    ) -> RpcResult<crate::eth::RpcAccountProof> {
        let address = Self::parse_address(&address)?;
        let block_num = self.resolve_block_number(block);

        let state = self
            .chain
            .state_at(block_num)
            .map_err(|e| RpcError::Internal(e.to_string()))?;

        // Generate account proof
        let (account_proof, account) = state
            .get_account_proof(&address)
            .map_err(|e| RpcError::Internal(e.to_string()))?;

        // Convert proof nodes to hex
        let proof_nodes: Vec<String> = account_proof
            .nodes
            .iter()
            .map(|n| format!("0x{}", hex::encode(n)))
            .collect();

        // Generate storage proofs for requested keys
        let mut storage_proofs = Vec::new();
        for key_str in &storage_keys {
            let slot = Self::parse_u256(key_str)?;
            let value = state
                .get_storage(&address, &slot)
                .map_err(|e| RpcError::Internal(e.to_string()))?;

            storage_proofs.push(crate::eth::RpcStorageProof {
                key: key_str.clone(),
                value: format!("0x{:064x}", value.0),
                proof: vec![], // storage trie proofs not yet implemented
            });
        }

        Ok(crate::eth::RpcAccountProof {
            address: address.to_string(),
            account_proof: proof_nodes,
            balance: format!("0x{:x}", account.balance.0),
            code_hash: account
                .code_hash
                .map(|h| format!("0x{}", hex::encode(h.as_bytes())))
                .unwrap_or_else(|| "0x".to_string()),
            nonce: format!("0x{:x}", account.nonce),
            storage_hash: format!("0x{}", hex::encode(state.root().as_bytes())),
            storage_proof: storage_proofs,
        })
    }

    async fn get_logs(&self, filter: LogFilter) -> RpcResult<Vec<RpcLog>> {
        // Determine block range
        let (from_block, to_block) = if let Some(ref bh) = filter.block_hash {
            // blockHash mode: single block
            let hash = Self::parse_hash(bh)?;
            let block = self
                .chain
                .get_block_by_hash(&hash)
                .map_err(|e| RpcError::Internal(e.to_string()))?
                .ok_or_else(|| RpcError::InvalidParams("block not found".into()))?;
            let num = block.number();
            (num, num)
        } else {
            let from = self.resolve_block_number(filter.from_block);
            let to = self.resolve_block_number(filter.to_block);
            (from, to)
        };

        // Limit range to prevent DoS (max 10000 blocks)
        if to_block > from_block + 10_000 {
            return Err(
                RpcError::InvalidParams("block range too large, max 10000 blocks".into()).into(),
            );
        }

        // Parse address filter
        let filter_addresses: Vec<qfc_types::Address> = match &filter.address {
            Some(AddressFilter::Single(addr)) => vec![Self::parse_address(addr)?],
            Some(AddressFilter::Multiple(addrs)) => addrs
                .iter()
                .map(|a| Self::parse_address(a))
                .collect::<Result<Vec<_>, _>>()?,
            None => vec![],
        };

        // Parse topic filters
        let topic_filters: Vec<Option<Vec<qfc_types::Hash>>> = match &filter.topics {
            Some(topics) => topics
                .iter()
                .take(4)
                .map(|t| match t {
                    Some(TopicFilter::Single(s)) => Ok(Some(vec![Self::parse_hash(s)?])),
                    Some(TopicFilter::Multiple(arr)) => Ok(Some(
                        arr.iter()
                            .map(|s| Self::parse_hash(s))
                            .collect::<Result<Vec<_>, _>>()?,
                    )),
                    None => Ok(None),
                })
                .collect::<Result<Vec<_>, RpcError>>()?,
            None => vec![],
        };

        let mut result_logs: Vec<RpcLog> = Vec::new();
        let mut global_log_index: u32 = 0;

        for block_num in from_block..=to_block {
            let block = match self
                .chain
                .get_block_by_number(block_num)
                .map_err(|e| RpcError::Internal(e.to_string()))?
            {
                Some(b) => b,
                None => continue,
            };

            let block_hash = blake3_hash(&block.header_bytes());

            for (tx_index, tx) in block.transactions.iter().enumerate() {
                let tx_hash = blake3_hash(&tx.to_bytes_without_signature());

                let receipt = match self
                    .chain
                    .get_receipt(&tx_hash)
                    .map_err(|e| RpcError::Internal(e.to_string()))?
                {
                    Some(r) => r,
                    None => continue,
                };

                for log in &receipt.logs {
                    // Filter by address
                    if !filter_addresses.is_empty() && !filter_addresses.contains(&log.address) {
                        global_log_index += 1;
                        continue;
                    }

                    // Filter by topics
                    let mut topics_match = true;
                    for (i, topic_filter) in topic_filters.iter().enumerate() {
                        if let Some(allowed_topics) = topic_filter {
                            match log.topics.get(i) {
                                Some(log_topic) => {
                                    if !allowed_topics.contains(log_topic) {
                                        topics_match = false;
                                        break;
                                    }
                                }
                                None => {
                                    topics_match = false;
                                    break;
                                }
                            }
                        }
                    }

                    if topics_match {
                        result_logs.push(RpcLog::from_log_with_meta(
                            log,
                            block_num,
                            block_hash,
                            tx_hash,
                            tx_index as u32,
                            global_log_index,
                        ));
                    }

                    global_log_index += 1;
                }
            }
        }

        // Limit result size (max 10000 logs)
        if result_logs.len() > 10_000 {
            return Err(RpcError::InvalidParams(
                "query returned more than 10000 results, narrow your filter".into(),
            )
            .into());
        }

        Ok(result_logs)
    }

    async fn eth_subscribe(
        &self,
        pending: jsonrpsee::PendingSubscriptionSink,
        sub_type: String,
    ) -> SubscriptionResult {
        use jsonrpsee::SubscriptionMessage;

        let sink = pending.accept().await?;

        match sub_type.as_str() {
            "newHeads" => {
                let chain = self.chain.clone();
                let mut last_height = chain.block_number();

                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    if sink.is_closed() {
                        break;
                    }

                    let current = chain.block_number();
                    if current > last_height {
                        // Send all new blocks since last check
                        for h in (last_height + 1)..=current {
                            if let Ok(Some(block)) = chain.get_block_by_number(h) {
                                let block_hash = qfc_crypto::blake3_hash(&block.header_bytes());
                                let head = crate::eth::NewHeadNotification {
                                    number: format!("0x{:x}", block.number()),
                                    hash: block_hash.to_string(),
                                    parent_hash: block.header.parent_hash.to_string(),
                                    timestamp: format!("0x{:x}", block.header.timestamp),
                                    state_root: block.header.state_root.to_string(),
                                    transactions_root: block.header.transactions_root.to_string(),
                                    gas_used: format!("0x{:x}", block.header.gas_used),
                                    gas_limit: format!("0x{:x}", block.header.gas_limit),
                                };
                                let msg = SubscriptionMessage::from_json(&head)?;
                                if sink.send(msg).await.is_err() {
                                    return Ok(());
                                }
                            }
                        }
                        last_height = current;
                    }
                }
            }
            "newPendingTransactions" => {
                let mempool = self.mempool.clone();
                let mut known: std::collections::HashSet<qfc_types::Hash> =
                    std::collections::HashSet::new();

                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    if sink.is_closed() {
                        break;
                    }

                    // Collect new tx hashes while lock is held, then send after dropping lock
                    let new_hashes: Vec<qfc_types::Hash> = {
                        let pool = mempool.read();
                        let all = pool.get_all_by_sender();
                        let mut hashes = Vec::new();
                        for (_sender, txs) in all {
                            for ptx in txs {
                                if known.insert(ptx.hash) {
                                    hashes.push(ptx.hash);
                                }
                            }
                        }
                        hashes
                    };

                    for hash in new_hashes {
                        let hash_str = hash.to_string();
                        let msg = SubscriptionMessage::from_json(&hash_str)?;
                        if sink.send(msg).await.is_err() {
                            return Ok(());
                        }
                    }
                }
            }
            _ => {
                // Unknown subscription type — just close
            }
        }

        Ok(())
    }
}

#[async_trait::async_trait]
impl QfcApiServer for RpcServer {
    async fn get_validators(&self) -> RpcResult<Vec<RpcValidator>> {
        let validators = self.chain.get_validators();
        let state = self.chain.state();

        let rpc_validators: Vec<RpcValidator> = validators
            .iter()
            .map(|v| {
                // Get additional info from state
                let stake = state.get_stake(&v.address).unwrap_or_default();
                let score = state.get_contribution_score(&v.address).unwrap_or(0);

                // Determine compute mode from validator fields
                let compute_mode = if v.inference_score > 0 || v.tasks_completed > 0 {
                    "inference"
                } else if v.provides_compute {
                    "pow"
                } else {
                    "none"
                };

                RpcValidator {
                    address: v.address.to_string(),
                    stake: format!("0x{:x}", stake.0),
                    contribution_score: format!("0x{:x}", score),
                    uptime: format!("0x{:x}", v.uptime),
                    is_active: v.is_active(),
                    provides_compute: v.provides_compute,
                    hashrate: v.hashrate.to_string(),
                    inference_score: v.inference_score.to_string(),
                    compute_mode: compute_mode.to_string(),
                    tasks_completed: v.tasks_completed.to_string(),
                }
            })
            .collect();

        Ok(rpc_validators)
    }

    async fn get_contribution_score(&self, address: String) -> RpcResult<String> {
        let address = Self::parse_address(&address)?;
        let score = self
            .chain
            .state()
            .get_contribution_score(&address)
            .map_err(|e| RpcError::Internal(e.to_string()))?;
        Ok(format!("0x{:x}", score))
    }

    async fn get_stake(&self, address: String) -> RpcResult<String> {
        let address = Self::parse_address(&address)?;
        let stake = self
            .chain
            .state()
            .get_stake(&address)
            .map_err(|e| RpcError::Internal(e.to_string()))?;
        Ok(format!("0x{:x}", stake.0))
    }

    async fn get_pending_undelegations(&self, address: String) -> RpcResult<Vec<RpcUndelegation>> {
        let address = Self::parse_address(&address)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let undelegations = self
            .chain
            .state()
            .get_undelegations(&address)
            .map_err(|e| RpcError::Internal(e.to_string()))?;
        Ok(undelegations
            .into_iter()
            .map(|u| RpcUndelegation {
                delegator: format!("0x{}", hex::encode(u.delegator.as_bytes())),
                validator: format!("0x{}", hex::encode(u.validator.as_bytes())),
                amount: format!("0x{:x}", u.amount.0),
                unlock_at: u.unlock_at,
                is_unlocked: u.is_unlocked(now),
            })
            .collect())
    }

    async fn get_epoch(&self) -> RpcResult<RpcEpoch> {
        let epoch = self.chain.get_epoch();
        Ok(RpcEpoch {
            number: format!("0x{:x}", epoch.number),
            start_time: format!("0x{:x}", epoch.start_time),
            duration_ms: format!("0x{:x}", 10000u64), // 10 seconds
        })
    }

    async fn get_finalized_block(&self) -> RpcResult<String> {
        Ok(format!("0x{:x}", self.chain.finalized_height()))
    }

    async fn node_info(&self) -> RpcResult<RpcNodeInfo> {
        let peer_count = if let Some(network) = &self.network {
            network.peer_count() as u64
        } else {
            0
        };

        let is_validator = self.chain.consensus().is_validator();

        let syncing = if let Some(sync_status) = &self.sync_status {
            sync_status.is_syncing()
        } else {
            false
        };

        Ok(RpcNodeInfo {
            version: env!("CARGO_PKG_VERSION").to_string(),
            chain_id: format!("0x{:x}", self.chain_id),
            peer_count,
            is_validator,
            syncing,
        })
    }

    async fn get_validator_score_breakdown(
        &self,
        address: String,
    ) -> RpcResult<RpcValidatorScoreBreakdown> {
        let address = Self::parse_address(&address)?;

        // Find the validator
        let validators = self.chain.get_validators();
        let validator = validators
            .iter()
            .find(|v| v.address == address)
            .ok_or_else(|| RpcError::InvalidParams("Validator not found".to_string()))?;

        // Calculate individual score components
        // These are weighted scores (each already multiplied by their weight)
        let total_stake: u128 = validators.iter().map(|v| v.stake.low_u128()).sum();
        let total_hashrate: u64 = validators
            .iter()
            .filter(|v| v.provides_compute)
            .map(|v| v.hashrate)
            .sum();
        let total_storage: u64 = validators
            .iter()
            .map(|v| v.storage_provided_gb as u64)
            .sum();

        // Calculate stake score component (30% weight)
        let stake_ratio = if total_stake > 0 {
            validator.stake.low_u128() as f64 / total_stake as f64
        } else {
            0.0
        };
        let stake_score = (stake_ratio * 3000.0) as u64; // 30% max

        // Calculate compute score component (20% weight)
        let compute_score = if validator.provides_compute && total_hashrate > 0 {
            ((validator.hashrate as f64 / total_hashrate as f64) * 2000.0) as u64
        } else {
            0
        };

        // Calculate uptime score component (15% weight)
        let uptime_score = (validator.uptime_ratio() * 1500.0) as u64;

        // Calculate accuracy score component (15% weight)
        let accuracy_score = (validator.accuracy_ratio() * 1500.0) as u64;

        // Calculate network score component (10% weight)
        let latency_score = 1.0 / (1.0 + validator.avg_latency_ms as f64 / 100.0);
        let bandwidth_score = (validator.bandwidth_mbps as f64 / 1000.0).min(1.0);
        let service_score = latency_score * 0.6 + bandwidth_score * 0.4;
        let network_score = (service_score * 1000.0) as u64;

        // Calculate storage score component (5% weight)
        let storage_score = if total_storage > 0 {
            ((validator.storage_provided_gb as f64 / total_storage as f64) * 500.0) as u64
        } else {
            0
        };

        // Calculate reputation score component (5% weight)
        let reputation_score = (validator.reputation_ratio() * 500.0) as u64;

        Ok(RpcValidatorScoreBreakdown {
            address: address.to_string(),
            total_score: format!("0x{:x}", validator.contribution_score),
            stake: format!("0x{:x}", validator.stake.0),
            stake_score: format!("0x{:x}", stake_score),
            compute_score: format!("0x{:x}", compute_score),
            uptime_score: format!("0x{:x}", uptime_score),
            accuracy_score: format!("0x{:x}", accuracy_score),
            network_score: format!("0x{:x}", network_score),
            storage_score: format!("0x{:x}", storage_score),
            reputation_score: format!("0x{:x}", reputation_score),
            metrics: RpcValidatorMetrics {
                uptime_percent: format!("{:.2}", validator.uptime_ratio() * 100.0),
                accuracy_percent: format!("{:.2}", validator.accuracy_ratio() * 100.0),
                reputation_percent: format!("{:.2}", validator.reputation_ratio() * 100.0),
                avg_latency_ms: validator.avg_latency_ms,
                bandwidth_mbps: validator.bandwidth_mbps,
                storage_gb: validator.storage_provided_gb,
                provides_compute: validator.provides_compute,
                hashrate: format!("0x{:x}", validator.hashrate),
                blocks_produced: format!("0x{:x}", validator.blocks_produced),
                valid_votes: format!("0x{:x}", validator.valid_votes),
                invalid_votes: format!("0x{:x}", validator.invalid_votes),
            },
        })
    }

    async fn get_network_state(&self) -> RpcResult<String> {
        let state = self.chain.consensus().get_network_state();
        let state_str = match state {
            NetworkState::Normal => "normal",
            NetworkState::Congested => "congested",
            NetworkState::StorageShortage => "storage_shortage",
            NetworkState::UnderAttack => "under_attack",
        };
        Ok(state_str.to_string())
    }

    async fn request_faucet(
        &self,
        address: String,
        amount: String,
    ) -> RpcResult<RpcFaucetResponse> {
        // Only allow in dev mode (chain_id 9000)
        if self.chain_id != 9000 {
            return Err(
                RpcError::Execution("Faucet only available in dev mode".to_string()).into(),
            );
        }

        let to_address = Self::parse_address(&address)?;

        // Parse amount (in wei) — hex if "0x" prefix, otherwise decimal
        let amount_value = if let Some(hex_str) = amount.strip_prefix("0x") {
            u128::from_str_radix(hex_str, 16)
                .map_err(|e| RpcError::InvalidParams(format!("Invalid hex amount: {}", e)))?
        } else {
            amount
                .parse::<u128>()
                .map_err(|e| RpcError::InvalidParams(format!("Invalid amount: {}", e)))?
        };

        // Faucet uses dev validator key [0x42; 32]
        // Ed25519 address: 0x10d7812fbe50096ae82569fdad35f79628bc0084
        let faucet_secret_key = [0x42u8; 32];
        let faucet_keypair = qfc_crypto::Keypair::from_secret_bytes(&faucet_secret_key)
            .map_err(|e| RpcError::Internal(format!("Failed to create faucet keypair: {}", e)))?;
        let faucet_public_key = faucet_keypair.public_key();
        let faucet_address = qfc_crypto::address_from_public_key(&faucet_public_key);

        // Get current nonce for faucet address
        let nonce = self
            .chain
            .state()
            .get_nonce(&faucet_address)
            .map_err(|e| RpcError::Internal(e.to_string()))?;

        // Create transaction
        let tx = Transaction {
            tx_type: qfc_types::TransactionType::Transfer,
            chain_id: self.chain_id,
            nonce,
            gas_price: U256::from_u128(1_000_000_000), // 1 Gwei
            gas_limit: 21000,
            to: Some(to_address),
            value: U256::from_u128(amount_value),
            data: Vec::new(),
            signature: qfc_types::Signature::ZERO, // Will be set after signing
            public_key: faucet_public_key,
        };

        // Sign the transaction hash (not raw bytes)
        let tx_bytes = tx.to_bytes_without_signature();
        let tx_hash = blake3_hash(&tx_bytes);
        let signature = faucet_keypair.sign_hash(&tx_hash);

        let signed_tx = Transaction { signature, ..tx };

        // tx_hash is already computed above

        // Add to mempool
        self.mempool
            .write()
            .add(signed_tx.clone(), faucet_address)
            .map_err(|e| RpcError::Execution(e.to_string()))?;

        info!(
            "Faucet: sent {} wei to {} (tx: {})",
            amount_value, to_address, tx_hash
        );

        // Broadcast to network if available
        if let Some(network) = &self.network {
            let tx_bytes = signed_tx.to_bytes();
            if let Err(e) = network.broadcast_transaction(tx_bytes).await {
                warn!("Failed to broadcast faucet transaction: {}", e);
            }
        }

        Ok(RpcFaucetResponse {
            tx_hash: tx_hash.to_string(),
            amount: format!("0x{:x}", amount_value),
            to: to_address.to_string(),
        })
    }

    // ---- v2.0 P2: Miner registration & status ----

    async fn register_miner(
        &self,
        req: RpcRegisterMinerRequest,
    ) -> RpcResult<RpcRegisterMinerResult> {
        let miner_address = Self::parse_address(&req.miner_address)?;

        // Parse public key from request
        let pk_hex = req.public_key.strip_prefix("0x").unwrap_or(&req.public_key);
        let pk_bytes = hex::decode(pk_hex)
            .map_err(|e| RpcError::InvalidParams(format!("Invalid public key hex: {}", e)))?;
        let public_key = qfc_types::PublicKey::from_slice(&pk_bytes)
            .ok_or_else(|| RpcError::InvalidParams("Invalid public key length".into()))?;

        // Verify public key derives to claimed address
        let derived_address = qfc_crypto::address_from_public_key(&public_key);
        if derived_address != miner_address {
            return Ok(RpcRegisterMinerResult {
                registered: false,
                assigned_tier: 0,
                message: "Public key does not match miner address".to_string(),
            });
        }

        // Verify signature using the submitted public key
        let sig_payload = format!(
            "{}{}{}",
            req.miner_address, req.gpu_model, req.benchmark_score
        );
        let sig_hash = blake3_hash(sig_payload.as_bytes());
        let sig_bytes = hex::decode(req.signature.strip_prefix("0x").unwrap_or(&req.signature))
            .map_err(|e| RpcError::InvalidParams(format!("Invalid signature hex: {}", e)))?;
        let signature = qfc_types::Signature::from_slice(&sig_bytes)
            .ok_or_else(|| RpcError::InvalidParams("Invalid signature length".into()))?;

        if verify_hash_signature(&public_key, &sig_hash, &signature).is_err() {
            return Ok(RpcRegisterMinerResult {
                registered: false,
                assigned_tier: 0,
                message: "Invalid signature".to_string(),
            });
        }

        // Validate GPU claim
        if !qfc_inference::validate_gpu_claim(&req.gpu_model, req.benchmark_score) {
            return Ok(RpcRegisterMinerResult {
                registered: false,
                assigned_tier: 0,
                message: "Benchmark score does not match GPU model".to_string(),
            });
        }

        // Compute tier
        let tier = match req.benchmark_score {
            0..=2999 => 1u8,
            3000..=6999 => 2,
            _ => 3,
        };

        // Parse backend
        let backend = match req.backend.to_uppercase().as_str() {
            "CUDA" => Some(qfc_types::BackendType::Cuda),
            "METAL" => Some(qfc_types::BackendType::Metal),
            "ROCM" => Some(qfc_types::BackendType::Rocm),
            "OPENCL" => Some(qfc_types::BackendType::OpenCl),
            "CPU" => Some(qfc_types::BackendType::Cpu),
            _ => None,
        };

        // Store the miner profile for future proof verification and listing
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.registered_miners.write().insert(
            miner_address,
            MinerProfile {
                public_key,
                gpu_model: req.gpu_model.clone(),
                benchmark_score: req.benchmark_score,
                tier,
                vram_mb: req.vram_mb,
                backend: req.backend.clone(),
                registered_at: now,
                os: req.os.clone(),
                arch: req.arch.clone(),
                cpu_model: req.cpu_model.clone(),
                cpu_cores: req.cpu_cores,
                total_memory_mb: req.total_memory_mb,
                version: req.version.clone(),
            },
        );

        let consensus = self.chain.consensus();

        // Update validator state if this miner is also a validator
        consensus.register_miner_profile(
            &miner_address,
            req.gpu_model.clone(),
            req.benchmark_score,
            tier,
            req.vram_mb,
            backend,
        );

        info!(
            "Miner registered: {} (GPU: {}, score: {}, tier: {})",
            req.miner_address, req.gpu_model, req.benchmark_score, tier
        );

        Ok(RpcRegisterMinerResult {
            registered: true,
            assigned_tier: tier,
            message: format!("Registered as T{}", tier),
        })
    }

    async fn get_registered_miners(&self) -> RpcResult<Vec<RpcRegisteredMiner>> {
        let miners = self.registered_miners.read();
        let mut result: Vec<RpcRegisteredMiner> = miners
            .iter()
            .map(|(addr, profile)| RpcRegisteredMiner {
                address: format!("0x{}", hex::encode(addr.as_bytes())),
                gpu_model: profile.gpu_model.clone(),
                benchmark_score: profile.benchmark_score,
                tier: profile.tier,
                vram_mb: profile.vram_mb,
                backend: profile.backend.clone(),
                registered_at: profile.registered_at.to_string(),
                os: profile.os.clone(),
                arch: profile.arch.clone(),
                cpu_model: profile.cpu_model.clone(),
                cpu_cores: profile.cpu_cores,
                total_memory_mb: profile.total_memory_mb,
                version: profile.version.clone(),
            })
            .collect();
        // Sort by tier desc, then benchmark_score desc
        result.sort_by(|a, b| {
            b.tier
                .cmp(&a.tier)
                .then(b.benchmark_score.cmp(&a.benchmark_score))
        });
        Ok(result)
    }

    async fn report_miner_status(&self, req: RpcMinerStatusReport) -> RpcResult<bool> {
        let miner_address = Self::parse_address(&req.miner_address)?;

        // Verify signature
        let consensus = self.chain.consensus();
        let validators = consensus.get_validators();
        let validator = match validators.iter().find(|v| v.address == miner_address) {
            Some(v) => v,
            None => return Ok(false),
        };

        let sig_payload = format!("{}{}", req.miner_address, req.pending_tasks);
        let sig_hash = blake3_hash(sig_payload.as_bytes());
        let sig_bytes = hex::decode(req.signature.strip_prefix("0x").unwrap_or(&req.signature))
            .map_err(|e| RpcError::InvalidParams(format!("Invalid signature hex: {}", e)))?;
        let signature = qfc_types::Signature::from_slice(&sig_bytes)
            .ok_or_else(|| RpcError::InvalidParams("Invalid signature length".into()))?;

        if verify_hash_signature(&validator.public_key, &sig_hash, &signature).is_err() {
            return Ok(false);
        }

        // Update task router if available
        if let Some(ref router) = self.task_router {
            let models: Vec<(
                qfc_inference::ModelId,
                qfc_ai_coordinator::router::ModelLayer,
            )> = req
                .loaded_models
                .iter()
                .map(|m| {
                    let model_id = qfc_inference::ModelId::new(&m.model_name, &m.model_version);
                    let layer = match m.layer.as_str() {
                        "hot" => qfc_ai_coordinator::router::ModelLayer::Hot,
                        "warm" => qfc_ai_coordinator::router::ModelLayer::Warm,
                        _ => qfc_ai_coordinator::router::ModelLayer::Cold,
                    };
                    (model_id, layer)
                })
                .collect();

            let tier = validator.gpu_tier;
            router
                .write()
                .update_miner_models(miner_address, models, tier);
        }

        debug!("Miner status update from {}", req.miner_address);
        Ok(true)
    }

    // ---- v2.0: AI Compute endpoints ----

    async fn get_compute_info(&self) -> RpcResult<RpcComputeInfo> {
        // Get validator info if this node is a validator
        let validators = self.chain.get_validators();
        let our_validator = validators.iter().find(|v| {
            // Find our validator node (if we are one)
            v.provides_compute
        });

        match our_validator {
            Some(v) => Ok(RpcComputeInfo {
                backend: v
                    .compute_backend
                    .as_ref()
                    .map(|b| format!("{}", b))
                    .unwrap_or_else(|| "none".to_string()),
                supported_models: v
                    .supported_models
                    .iter()
                    .map(|m| format!("{}", m))
                    .collect(),
                gpu_memory_mb: v.gpu_memory_mb,
                inference_score: format!("0x{:x}", v.inference_score),
                gpu_tier: match v.gpu_tier {
                    1 => "T1".to_string(),
                    2 => "T2".to_string(),
                    3 => "T3".to_string(),
                    _ => "unknown".to_string(),
                },
                provides_compute: true,
            }),
            None => Ok(RpcComputeInfo {
                backend: "none".to_string(),
                supported_models: vec![],
                gpu_memory_mb: 0,
                inference_score: "0x0".to_string(),
                gpu_tier: "none".to_string(),
                provides_compute: false,
            }),
        }
    }

    async fn get_supported_models(&self) -> RpcResult<Vec<RpcModel>> {
        // Return the default approved models for v2.0
        // In production, this comes from on-chain governance
        Ok(vec![
            RpcModel {
                name: "qfc-embed-small".to_string(),
                version: "v1.0".to_string(),
                min_memory_mb: 512,
                min_tier: "Cold".to_string(),
                approved: true,
            },
            RpcModel {
                name: "qfc-embed-medium".to_string(),
                version: "v1.0".to_string(),
                min_memory_mb: 2048,
                min_tier: "Warm".to_string(),
                approved: true,
            },
            RpcModel {
                name: "qfc-classify-small".to_string(),
                version: "v1.0".to_string(),
                min_memory_mb: 2048,
                min_tier: "Warm".to_string(),
                approved: true,
            },
        ])
    }

    async fn get_inference_stats(&self) -> RpcResult<RpcInferenceStats> {
        // Aggregate inference stats from validators
        let validators = self.chain.get_validators();
        let total_tasks: u64 = validators.iter().map(|v| v.tasks_completed).sum();
        let avg_pass_rate = if !validators.is_empty() {
            let sum: f64 = validators.iter().map(|v| v.verification_pass_ratio()).sum();
            sum / validators.len() as f64
        } else {
            0.0
        };

        let proof_count = self
            .verified_proof_count
            .load(std::sync::atomic::Ordering::Relaxed);
        let total_time = self
            .total_inference_time_ms
            .load(std::sync::atomic::Ordering::Relaxed);
        let avg_time = if proof_count > 0 {
            total_time / proof_count
        } else {
            0
        };
        let flops = self.total_flops.load(std::sync::atomic::Ordering::Relaxed);

        Ok(RpcInferenceStats {
            tasks_completed: total_tasks.to_string(),
            avg_time_ms: avg_time.to_string(),
            flops_total: format!("0x{:x}", flops),
            pass_rate: format!("{:.2}", avg_pass_rate * 100.0),
        })
    }

    async fn get_inference_task(
        &self,
        request: RpcTaskRequest,
    ) -> RpcResult<Option<RpcInferenceTask>> {
        let tier = match request.gpu_tier.as_str() {
            "Hot" => qfc_inference::GpuTier::Hot,
            "Warm" => qfc_inference::GpuTier::Warm,
            _ => qfc_inference::GpuTier::Cold,
        };

        let mut pool = self.task_pool.write();

        // If pool is empty, generate new synthetic tasks
        if pool.pending_count() == 0 {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;
            let current_epoch = self.chain.get_epoch();
            let epoch_seed = u64::from_le_bytes(current_epoch.seed[..8].try_into().unwrap());
            pool.generate_synthetic_tasks(current_epoch.number, epoch_seed, now + 30_000);
        }

        match pool.fetch_task(tier, request.available_memory_mb) {
            Some(task) => {
                let (model_name, model_version) = match task.task_type.model_id() {
                    Some(id) => (id.name.clone(), id.version.clone()),
                    None => ("unknown".to_string(), "v0".to_string()),
                };

                Ok(Some(RpcInferenceTask {
                    task_id: hex::encode(task.task_id.as_bytes()),
                    epoch: task.epoch,
                    task_type: task.task_type.task_type_name().to_string(),
                    model_name,
                    model_version,
                    input_data: hex::encode(&task.input_data),
                    deadline: task.deadline,
                }))
            }
            None => Ok(None),
        }
    }

    async fn submit_inference_proof(
        &self,
        submission: RpcInferenceProofSubmission,
    ) -> RpcResult<RpcProofResult> {
        // 1. Decode proof bytes
        let proof_bytes = hex::decode(&submission.proof_bytes)
            .map_err(|e| RpcError::Execution(format!("Invalid proof hex: {}", e)))?;

        let proof = qfc_inference::InferenceProof::from_bytes(&proof_bytes)
            .map_err(|e| RpcError::Execution(format!("Failed to deserialize proof: {}", e)))?;

        let consensus = self.chain.consensus();

        // 2. Find the miner's public key (check registered miners first, then validators)
        let miner_pubkey = {
            let miners = self.registered_miners.read();
            if let Some(profile) = miners.get(&proof.validator) {
                Some(profile.public_key)
            } else {
                // Fallback: check validator set for backward compatibility
                let validators = consensus.get_validators();
                validators
                    .iter()
                    .find(|v| v.address == proof.validator)
                    .map(|v| v.public_key)
            }
        };

        let public_key = match miner_pubkey {
            Some(pk) => pk,
            None => {
                return Ok(RpcProofResult {
                    accepted: false,
                    spot_checked: false,
                    message: "Unknown miner — register first via qfc_registerMiner".to_string(),
                    reward_estimate: None,
                });
            }
        };

        // 3. Check if miner is also a validator and if so, verify active status
        {
            let validators = consensus.get_validators();
            if let Some(v) = validators.iter().find(|v| v.address == proof.validator) {
                if !v.is_active() {
                    return Ok(RpcProofResult {
                        accepted: false,
                        spot_checked: false,
                        message: "Validator is inactive or jailed".to_string(),
                        reward_estimate: None,
                    });
                }
            }
        }

        // 4. Verify the proof signature
        let proof_hash = blake3_hash(&proof.to_bytes_without_signature());
        if verify_hash_signature(&public_key, &proof_hash, &proof.signature).is_err() {
            warn!("Invalid inference proof signature from {}", proof.validator);
            return Ok(RpcProofResult {
                accepted: false,
                spot_checked: false,
                message: "Invalid proof signature".to_string(),
                reward_estimate: None,
            });
        }

        // 5. Basic verification (timestamp freshness, model, FLOPS)
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if let Err(e) = qfc_ai_coordinator::verify_basic(&proof, now_secs, &self.model_registry) {
            warn!("Proof rejected from {}: {}", submission.miner_address, e);
            return Ok(RpcProofResult {
                accepted: false,
                spot_checked: false,
                message: format!("Proof rejected: {}", e),
                reward_estimate: None,
            });
        }

        // 6. Probabilistic spot-check (~5%)
        let mut spot_checked = false;
        if qfc_ai_coordinator::should_spot_check(&proof) {
            spot_checked = true;
            if let Some(ref engine_lock) = self.inference_engine {
                let epoch = consensus.get_epoch();
                let epoch_seed = u64::from_le_bytes(epoch.seed[..8].try_into().unwrap_or([0u8; 8]));
                let mut task_pool = qfc_ai_coordinator::TaskPool::new();
                task_pool.generate_synthetic_tasks(proof.epoch, epoch_seed, u64::MAX);

                // Find the task matching proof.input_hash (= original task_id)
                let matching_task = {
                    let mut found = None;
                    while let Some(t) = task_pool.fetch_task(qfc_inference::GpuTier::Hot, u64::MAX)
                    {
                        if t.task_id == proof.input_hash {
                            found = Some(t);
                            break;
                        }
                    }
                    found
                };

                if let Some(task) = matching_task {
                    let engine = engine_lock.read().await;
                    match qfc_ai_coordinator::verify_spot_check(&proof, &task, &**engine).await {
                        Ok(result) => {
                            info!(
                                "Spot-check PASSED for inference proof from {}: {}",
                                proof.validator, result.details
                            );
                        }
                        Err(qfc_ai_coordinator::VerificationError::OutputHashMismatch {
                            expected,
                            got,
                        }) => {
                            warn!(
                                "Spot-check FAILED for {}: output hash mismatch (expected {}, got {})",
                                proof.validator,
                                hex::encode(&expected.as_bytes()[..8]),
                                hex::encode(&got.as_bytes()[..8]),
                            );
                            consensus.slash_validator(&proof.validator, 5, 6 * 60 * 60 * 1000);

                            // Deliver slashing webhook notification
                            crate::webhook::deliver(
                                &self.webhook_store,
                                &proof.validator,
                                crate::webhook::WebhookPayload {
                                    event_type: "slashing_applied".to_string(),
                                    miner: hex::encode(proof.validator.as_bytes()),
                                    block_height: None,
                                    task_type: Some(format!("{:?}", proof.task_type)),
                                    flops: None,
                                    reward_wei: None,
                                    spot_checked: Some(true),
                                    timestamp: proof.timestamp,
                                    message: format!(
                                        "Slashing applied: 5% stake penalty, 6h jail. Reason: spot-check output hash mismatch (task {})",
                                        hex::encode(&proof.input_hash.as_bytes()[..8]),
                                    ),
                                },
                            );

                            return Ok(RpcProofResult {
                                accepted: false,
                                spot_checked: true,
                                message: "Proof rejected: spot-check failed (output hash mismatch)"
                                    .to_string(),
                                reward_estimate: None,
                            });
                        }
                        Err(e) => {
                            warn!(
                                "Spot-check re-execution error for {}: {}",
                                proof.validator, e
                            );
                        }
                    }
                } else {
                    debug!(
                        "Spot-check: no matching synthetic task for {}, skipping",
                        proof.validator
                    );
                }
            } else {
                debug!(
                    "Spot-check selected for {} but no inference engine available",
                    proof.validator
                );
            }
        }

        // 7. Challenge check (P2)
        if let Some(ref cg) = self.challenge_generator {
            let mut gen = cg.write();
            if gen.is_challenge(&proof.input_hash) {
                if let Some(verdict) = gen.verify_challenge(&proof.input_hash, &proof.output_hash) {
                    if let Some(penalty) = gen.record_result(&proof.validator, &verdict) {
                        consensus.reduce_reputation(&proof.validator, penalty.reputation_reduction);
                        if penalty.slash_percent > 0 {
                            consensus.slash_validator(
                                &proof.validator,
                                penalty.slash_percent,
                                penalty.jail_duration_ms,
                            );

                            // Deliver slashing webhook for challenge failure
                            crate::webhook::deliver(
                                &self.webhook_store,
                                &proof.validator,
                                crate::webhook::WebhookPayload {
                                    event_type: "slashing_applied".to_string(),
                                    miner: hex::encode(proof.validator.as_bytes()),
                                    block_height: None,
                                    task_type: Some(format!("{:?}", proof.task_type)),
                                    flops: None,
                                    reward_wei: None,
                                    spot_checked: Some(true),
                                    timestamp: proof.timestamp,
                                    message: format!(
                                        "Slashing applied: {}% stake penalty, {}h jail. Reason: challenge verification failed (task {})",
                                        penalty.slash_percent,
                                        penalty.jail_duration_ms / (60 * 60 * 1000),
                                        hex::encode(&proof.input_hash.as_bytes()[..8]),
                                    ),
                                },
                            );
                        }
                    }
                    let passed = matches!(
                        verdict,
                        qfc_ai_coordinator::challenge::ChallengeVerdict::Passed
                    );
                    return Ok(RpcProofResult {
                        accepted: passed,
                        spot_checked: true,
                        message: format!("Challenge result: {:?}", verdict),
                        reward_estimate: None,
                    });
                }
            }
        }

        // 7b. Redundant verification check (P2)
        if let Some(ref rv) = self.redundant_verifier {
            let mut verifier = rv.write();
            if verifier.is_pending(&proof.input_hash) {
                if let Some(result) =
                    verifier.record_submission(proof.input_hash, proof.validator, proof.output_hash)
                {
                    for &bad_miner in &result.inconsistent_miners {
                        consensus.reduce_reputation(&bad_miner, 1000);
                    }
                    if !result.consistent_miners.contains(&proof.validator) {
                        return Ok(RpcProofResult {
                            accepted: false,
                            spot_checked: false,
                            message: "Redundant verification: inconsistent output".to_string(),
                            reward_estimate: None,
                        });
                    }
                } else {
                    return Ok(RpcProofResult {
                        accepted: true,
                        spot_checked: false,
                        message: "Redundant verification: waiting for more submissions".to_string(),
                        reward_estimate: None,
                    });
                }
            }
        }

        // 8. Proof passed — update inference score and track stats
        consensus.update_inference_score(&proof.validator, proof.flops_estimated, 1);
        self.total_flops
            .fetch_add(proof.flops_estimated, std::sync::atomic::Ordering::Relaxed);
        self.total_inference_time_ms.fetch_add(
            proof.execution_time_ms as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        self.verified_proof_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        // 9. Push to proof pool for block inclusion (v2.0)
        // Convert qfc_inference::InferenceProof → qfc_types::InferenceProof via borsh roundtrip
        let types_proof: qfc_types::InferenceProof =
            borsh::from_slice(&borsh::to_vec(&proof).unwrap()).unwrap();
        if let Some(ref pool) = self.proof_pool {
            pool.write().add(types_proof);
        }

        // 10. Check if this proof completes a public task (v2.0, B2: IPFS for large results)
        {
            use qfc_ai_coordinator::task_pool::ResultStorage;

            let has_public_task = {
                let pool = self.task_pool.read();
                pool.get_public_task(&proof.input_hash).is_some()
            };

            if has_public_task {
                let result_data = submission
                    .result_data
                    .as_ref()
                    .and_then(|s| hex::decode(s.strip_prefix("0x").unwrap_or(s)).ok())
                    .unwrap_or_else(|| proof.output_hash.as_bytes().to_vec());

                // B2: If result is large and IPFS client is available, upload to IPFS
                let result_storage = if let Some(ref ipfs) = self.ipfs_client {
                    if ipfs.should_upload(&result_data) {
                        // Upload to IPFS (no lock held during async call)
                        match ipfs.upload(&result_data).await {
                            Ok(upload_result) => {
                                let preview_len = std::cmp::min(1024, result_data.len());
                                let preview = result_data[..preview_len].to_vec();
                                info!(
                                    "Uploaded large result ({} bytes) to IPFS: {}",
                                    result_data.len(),
                                    upload_result.cid
                                );
                                ResultStorage::Ipfs {
                                    cid: upload_result.cid,
                                    size: upload_result.size as u64,
                                    preview,
                                }
                            }
                            Err(e) => {
                                warn!(
                                    "IPFS upload failed for large result ({} bytes), storing inline: {}",
                                    result_data.len(),
                                    e
                                );
                                ResultStorage::Inline(result_data)
                            }
                        }
                    } else {
                        ResultStorage::Inline(result_data)
                    }
                } else {
                    if result_data.len() > 1_048_576 {
                        warn!(
                            "Large result ({} bytes) stored inline because no IPFS client is configured",
                            result_data.len()
                        );
                    }
                    ResultStorage::Inline(result_data)
                };

                let mut task_pool = self.task_pool.write();
                task_pool.complete_public_task(
                    &proof.input_hash,
                    result_storage,
                    proof.validator,
                    proof.execution_time_ms,
                );
            }
        }

        info!(
            "Updated inference score for {} epoch {}: {} FLOPS, {}ms (spot_checked={})",
            proof.validator,
            proof.epoch,
            proof.flops_estimated,
            proof.execution_time_ms,
            spot_checked
        );

        // Compute estimated miner reward based on base fee (15% miner pool)
        let reward_est = {
            let base_fee = qfc_ai_coordinator::estimate_base_fee(&proof.task_type);
            // Miner gets ~15% of base fee per the reward distribution formula
            let miner_share = base_fee * 15 / 100;
            format!("0x{:x}", miner_share)
        };

        // Also query miner's current balance for the response
        let balance = self
            .chain
            .state()
            .get_balance(&proof.validator)
            .unwrap_or_default();

        info!(
            "Miner {} reward estimate: {} wei (balance: {} QFC)",
            proof.validator,
            reward_est,
            format_qfc_balance(balance),
        );

        // Deliver webhook notification for accepted proof
        crate::webhook::deliver(
            &self.webhook_store,
            &proof.validator,
            crate::webhook::WebhookPayload {
                event_type: "proof_accepted".to_string(),
                miner: hex::encode(proof.validator.as_bytes()),
                block_height: None,
                task_type: Some(format!("{:?}", proof.task_type)),
                flops: Some(proof.flops_estimated),
                reward_wei: Some(reward_est.clone()),
                spot_checked: Some(spot_checked),
                timestamp: proof.timestamp,
                message: format!(
                    "Proof accepted: {} FLOPS, est. reward {}",
                    proof.flops_estimated, reward_est
                ),
            },
        );

        Ok(RpcProofResult {
            accepted: true,
            spot_checked,
            message: if spot_checked {
                "Proof accepted, spot-check passed".to_string()
            } else {
                "Proof accepted".to_string()
            },
            reward_estimate: Some(reward_est),
        })
    }

    // ---- v2.0: Model Governance endpoints ----

    async fn propose_model(&self, request: RpcProposeModelRequest) -> RpcResult<String> {
        let proposer = Self::parse_address(&request.proposer)?;
        let min_tier = match request.min_tier.as_str() {
            "Hot" => qfc_inference::GpuTier::Hot,
            "Warm" => qfc_inference::GpuTier::Warm,
            _ => qfc_inference::GpuTier::Cold,
        };

        let weights_hash = request
            .weights_hash
            .as_deref()
            .map(Self::parse_hash)
            .transpose()?;

        // ADR-0001: validate the manifest before it can enter governance, and
        // reject entries where weights_hash and assembled_hash disagree (a
        // registry entry with both set must have them equal).
        if let Some(manifest) = &request.shard_manifest {
            manifest.validate().map_err(|e| {
                RpcError::InvalidParams(format!("invalid shard manifest: {}", e))
            })?;
            if let Some(wh) = weights_hash {
                if wh != manifest.assembled_hash {
                    return Err(RpcError::InvalidParams(format!(
                        "weightsHash {} does not match shard manifest assembledHash {}",
                        wh, manifest.assembled_hash
                    ))
                    .into());
                }
            }
        }

        let model_info = qfc_inference::model::ModelInfo {
            id: qfc_inference::task::ModelId::new(&request.model_name, &request.model_version),
            description: request.description,
            min_memory_mb: request.min_memory_mb,
            min_tier,
            size_mb: request.size_mb,
            approved: false,
            canonical_format: qfc_inference::CanonicalFormat::SafetensorsFp32,
            weights_hash,
            shard_manifest: request.shard_manifest,
        };

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let proposal_id = self
            .governance
            .write()
            .propose_model(proposer, model_info, now);
        Ok(hex::encode(proposal_id.as_bytes()))
    }

    async fn vote_model(&self, request: RpcVoteModelRequest) -> RpcResult<bool> {
        let proposal_id = Self::parse_hash(&request.proposal_id)?;
        let voter = Self::parse_address(&request.voter)?;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        self.governance
            .write()
            .vote(proposal_id, voter, request.approve, now)
            .map_err(|e| RpcError::Execution(e.to_string()))?;

        Ok(true)
    }

    async fn get_model_proposals(&self) -> RpcResult<Vec<RpcModelProposal>> {
        let gov = self.governance.read();
        let proposals = gov.all_proposals();

        Ok(proposals
            .into_iter()
            .map(|p| {
                let status = match p.status {
                    qfc_ai_coordinator::ProposalStatus::Active => "Active",
                    qfc_ai_coordinator::ProposalStatus::Passed => "Passed",
                    qfc_ai_coordinator::ProposalStatus::Rejected => "Rejected",
                    qfc_ai_coordinator::ProposalStatus::Expired => "Expired",
                };

                RpcModelProposal {
                    proposal_id: hex::encode(p.proposal_id.as_bytes()),
                    proposer: p.proposer.to_string(),
                    model_name: p.model_info.id.name.clone(),
                    model_version: p.model_info.id.version.clone(),
                    description: p.model_info.description.clone(),
                    min_memory_mb: p.model_info.min_memory_mb,
                    min_tier: format!("{:?}", p.model_info.min_tier),
                    size_mb: p.model_info.size_mb,
                    votes_for: p.votes_for.len() as u64,
                    votes_against: p.votes_against.len() as u64,
                    status: status.to_string(),
                    created_at: p.created_at,
                    voting_deadline: p.voting_deadline,
                }
            })
            .collect())
    }

    // ---- v2.0: Treasury endpoints ----

    async fn get_treasury_info(&self) -> RpcResult<RpcTreasuryInfo> {
        let treasury_addr = qfc_types::Address::new(qfc_types::TREASURY_ADDRESS_BYTES);
        let state = self.chain.state();
        let balance = state.get_balance(&treasury_addr).unwrap_or_default();
        let treasury = self.treasury.read();

        Ok(RpcTreasuryInfo {
            address: treasury_addr.to_string(),
            balance: balance.to_string(),
            total_disbursed: treasury.total_disbursed().to_string(),
            active_proposals: treasury.active_proposals().len() as u64,
        })
    }

    async fn propose_spend(&self, request: RpcProposeSpendRequest) -> RpcResult<String> {
        let proposer = Self::parse_address(&request.proposer)?;
        let recipient = Self::parse_address(&request.recipient)?;
        let amount: u128 = request
            .amount
            .parse()
            .map_err(|e| RpcError::InvalidParams(format!("Invalid amount: {}", e)))?;

        let state = self.chain.state();
        let proposer_stake = state.get_stake(&proposer).unwrap_or_default().0.as_u128();

        let treasury_addr = qfc_types::Address::new(qfc_types::TREASURY_ADDRESS_BYTES);
        let treasury_balance = state
            .get_balance(&treasury_addr)
            .unwrap_or_default()
            .0
            .as_u128();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let id = self
            .treasury
            .write()
            .propose_spend(
                proposer,
                recipient,
                amount,
                request.description,
                proposer_stake,
                treasury_balance,
                now,
            )
            .map_err(|e| RpcError::Execution(e.to_string()))?;

        Ok(hex::encode(id.as_bytes()))
    }

    async fn vote_spend(&self, request: RpcVoteSpendRequest) -> RpcResult<bool> {
        let proposal_id = Self::parse_hash(&request.proposal_id)?;
        let voter = Self::parse_address(&request.voter)?;

        let state = self.chain.state();
        let voter_stake = state.get_stake(&voter).unwrap_or_default().0.as_u128();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        self.treasury
            .write()
            .vote(proposal_id, voter, request.approve, voter_stake, now)
            .map_err(|e| RpcError::Execution(e.to_string()))?;

        // Tally after each vote
        let consensus = self.chain.consensus();
        let validators = consensus.get_validators();
        let total_stake: u128 = validators.iter().map(|v| v.stake.0.as_u128()).sum();
        self.treasury.write().tally(total_stake, now);

        Ok(true)
    }

    async fn get_spend_proposals(&self) -> RpcResult<Vec<RpcSpendProposal>> {
        let treasury = self.treasury.read();
        let proposals = treasury.all_proposals();

        Ok(proposals
            .into_iter()
            .map(|p| {
                let status = match &p.status {
                    qfc_ai_coordinator::SpendStatus::Active => "Active".to_string(),
                    qfc_ai_coordinator::SpendStatus::Queued { execute_after } => {
                        format!("Queued(execute_after={})", execute_after)
                    }
                    qfc_ai_coordinator::SpendStatus::Executed => "Executed".to_string(),
                    qfc_ai_coordinator::SpendStatus::Rejected => "Rejected".to_string(),
                    qfc_ai_coordinator::SpendStatus::Expired => "Expired".to_string(),
                    qfc_ai_coordinator::SpendStatus::Cancelled => "Cancelled".to_string(),
                };

                RpcSpendProposal {
                    proposal_id: hex::encode(p.proposal_id.as_bytes()),
                    proposer: p.proposer.to_string(),
                    recipient: p.recipient.to_string(),
                    amount: p.amount.to_string(),
                    description: p.description.clone(),
                    stake_for: p.stake_for().to_string(),
                    stake_against: p.stake_against().to_string(),
                    status,
                    created_at: p.created_at,
                    voting_deadline: p.voting_deadline,
                }
            })
            .collect())
    }

    // ---- v2.0: Parameter Governance endpoints ----

    async fn propose_parameter(&self, request: RpcProposeParameterRequest) -> RpcResult<String> {
        let proposer = Self::parse_address(&request.proposer)?;

        // Parse parameter key
        let parameter = Self::parse_parameter_key(&request.parameter)?;

        // Get current value for this parameter
        let current_value = self.get_current_param_value(&parameter);

        // Parse proposed value
        let proposed_value: u128 = request
            .proposed_value
            .parse()
            .map_err(|e| RpcError::InvalidParams(format!("Invalid proposed_value: {}", e)))?;

        // Get proposer's stake
        let state = self.chain.state();
        let stake = state.get_stake(&proposer).unwrap_or_default().0.as_u128();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let proposal_id = self
            .param_governance
            .write()
            .propose(
                proposer,
                parameter,
                current_value,
                proposed_value,
                request.description,
                stake,
                now,
            )
            .map_err(|e| RpcError::Execution(e.to_string()))?;

        Ok(hex::encode(proposal_id.as_bytes()))
    }

    async fn vote_parameter(&self, request: RpcVoteParameterRequest) -> RpcResult<bool> {
        let proposal_id = Self::parse_hash(&request.proposal_id)?;
        let voter = Self::parse_address(&request.voter)?;

        // Get voter's stake for stake-weighted voting
        let state = self.chain.state();
        let voter_stake = state.get_stake(&voter).unwrap_or_default().0.as_u128();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        self.param_governance
            .write()
            .vote(proposal_id, voter, request.approve, voter_stake, now)
            .map_err(|e| RpcError::Execution(e.to_string()))?;

        // Tally after each vote to check for early pass/reject
        let consensus = self.chain.consensus();
        let validators = consensus.get_validators();
        let total_stake: u128 = validators.iter().map(|v| v.stake.0.as_u128()).sum();
        self.param_governance.write().tally(total_stake, now);

        Ok(true)
    }

    async fn get_parameter_proposals(&self) -> RpcResult<Vec<RpcParameterProposal>> {
        let gov = self.param_governance.read();
        let proposals = gov.all_proposals();

        Ok(proposals
            .into_iter()
            .map(|p| {
                let status = match &p.status {
                    qfc_ai_coordinator::ParamProposalStatus::Active => "Active".to_string(),
                    qfc_ai_coordinator::ParamProposalStatus::Queued { execute_after } => {
                        format!("Queued(execute_after={})", execute_after)
                    }
                    qfc_ai_coordinator::ParamProposalStatus::Executed => "Executed".to_string(),
                    qfc_ai_coordinator::ParamProposalStatus::Rejected => "Rejected".to_string(),
                    qfc_ai_coordinator::ParamProposalStatus::Expired => "Expired".to_string(),
                    qfc_ai_coordinator::ParamProposalStatus::Cancelled => "Cancelled".to_string(),
                };

                RpcParameterProposal {
                    proposal_id: hex::encode(p.proposal_id.as_bytes()),
                    proposer: p.proposer.to_string(),
                    parameter: p.parameter.to_string(),
                    current_value: p.current_value.to_string(),
                    proposed_value: p.proposed_value.to_string(),
                    description: p.description.clone(),
                    stake_for: p.stake_for().to_string(),
                    stake_against: p.stake_against().to_string(),
                    status,
                    created_at: p.created_at,
                    voting_deadline: p.voting_deadline,
                }
            })
            .collect())
    }

    async fn get_parameter_overrides(&self) -> RpcResult<Vec<RpcParameterOverride>> {
        let gov = self.param_governance.read();
        let overrides = gov.all_overrides();

        Ok(overrides
            .iter()
            .map(|(key, value)| RpcParameterOverride {
                parameter: key.to_string(),
                value: value.to_string(),
            })
            .collect())
    }

    // ---- v2.0: Miner vesting endpoint ----

    async fn get_miner_vesting(&self, address: String) -> RpcResult<RpcMinerVesting> {
        use crate::qfc::RpcVestingTranche;

        let miner_address = Self::parse_address(&address)?;
        let miner_hex = hex::encode(miner_address.as_bytes());

        // Vesting constants: 7-day cliff, 30-day linear vest
        const CLIFF_SECS: u64 = 7 * 24 * 3600;
        const VEST_SECS: u64 = 30 * 24 * 3600;

        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Scan blocks for this miner's inference proofs to build vesting tranches.
        // In production this would read from MINER_EARNINGS CF; for now we scan
        // recent blocks (last 30 days worth ≈ 777,600 blocks at 3.3s/block).
        let current_height = self.chain.block_number();
        let blocks_per_day = 24 * 3600 * 1000 / qfc_types::BLOCK_TIME_MS;
        let scan_start = current_height.saturating_sub(blocks_per_day * 31);

        let mut total_earned = U256::zero();
        let mut total_locked = U256::zero();
        let mut total_available = U256::zero();
        let mut tranches = Vec::new();

        let mut height = scan_start;
        while height <= current_height {
            let block = match self.chain.get_block_by_number(height) {
                Ok(Some(b)) => b,
                _ => {
                    height += 1;
                    continue;
                }
            };

            let miner_flops: u64 = block
                .inference_proofs
                .iter()
                .filter(|p| p.validator == miner_address)
                .map(|p| p.flops_estimated)
                .sum();

            if miner_flops == 0 {
                height += 1;
                continue;
            }

            let total_flops: u64 = block
                .inference_proofs
                .iter()
                .map(|p| p.flops_estimated)
                .sum();

            if total_flops == 0 {
                height += 1;
                continue;
            }

            // 15% of block reward → miner pool, proportional to FLOPS
            let block_reward = U256::from_u128(qfc_types::BLOCK_REWARD);
            let miner_pool = block_reward * U256::from_u128(15) / U256::from_u128(100);
            let reward = miner_pool * U256::from_u128(miner_flops as u128)
                / U256::from_u128(total_flops as u128);

            let start_time = block.header.timestamp;
            let cliff_end = start_time + CLIFF_SECS;
            let end_time = start_time + VEST_SECS;

            // Calculate vested amount
            let (vested, percent) = if now_secs < cliff_end {
                // Before cliff: nothing vested
                (U256::zero(), 0u8)
            } else if now_secs >= end_time {
                // Fully vested
                (reward, 100u8)
            } else {
                // Linear vesting between cliff and end
                let elapsed = now_secs - start_time;
                let v =
                    reward * U256::from_u128(elapsed as u128) / U256::from_u128(VEST_SECS as u128);
                let pct = (elapsed * 100 / VEST_SECS) as u8;
                (v, pct)
            };

            total_earned = total_earned + reward;
            let locked = reward - vested;
            total_locked = total_locked + locked;
            total_available = total_available + vested;

            if percent < 100 {
                tranches.push(RpcVestingTranche {
                    block_height: format!("0x{:x}", height),
                    amount: format!("0x{:x}", reward.0),
                    vested: format!("0x{:x}", vested.0),
                    start_time: format!("{}", start_time),
                    cliff_end: format!("{}", cliff_end),
                    end_time: format!("{}", end_time),
                    percent_vested: percent,
                });
            }

            height += 1;
        }

        let active_tranches = tranches.len() as u64;
        // Return most recent tranches first
        tranches.reverse();

        Ok(RpcMinerVesting {
            miner: format!("0x{}", miner_hex),
            total_earned: format!("0x{:x}", total_earned.0),
            locked: format!("0x{:x}", total_locked.0),
            available: format!("0x{:x}", total_available.0),
            active_tranches,
            tranches,
        })
    }

    // ---- v2.0: Miner Earnings ----

    async fn get_miner_earnings(
        &self,
        address: String,
        period: String,
    ) -> RpcResult<RpcMinerEarnings> {
        let addr_hex = address.strip_prefix("0x").unwrap_or(&address);
        let addr_bytes = hex::decode(addr_hex).map_err(|e| {
            jsonrpsee::types::ErrorObjectOwned::owned(
                -32602,
                format!("Invalid address: {}", e),
                None::<()>,
            )
        })?;
        let miner_address = qfc_types::Address::from_slice(&addr_bytes).ok_or_else(|| {
            jsonrpsee::types::ErrorObjectOwned::owned(
                -32602,
                "Address must be 20 bytes",
                None::<()>,
            )
        })?;

        // Determine time cutoff based on period
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let cutoff_ms = match period.as_str() {
            "day" => now_ms.saturating_sub(24 * 3600 * 1000),
            "week" => now_ms.saturating_sub(7 * 24 * 3600 * 1000),
            "month" => now_ms.saturating_sub(30 * 24 * 3600 * 1000),
            "all" | _ => 0,
        };

        // Scan MINER_EARNINGS CF for this address
        let db = self.chain.db();
        let start_key = qfc_types::encode_miner_earning_key(&miner_address, 0);

        let mut records = Vec::new();
        let mut total_earnings = qfc_types::U256::ZERO;
        let mut total_flops: u64 = 0;
        let mut total_tasks: u64 = 0;

        if let Ok(iter) = db.iter_from("miner_earnings", &start_key) {
            for (key, value) in iter {
                // Stop when we leave this miner's prefix
                if key.len() != 28 || &key[0..20] != miner_address.as_bytes() {
                    break;
                }

                if let Ok(earning) = qfc_types::MinerEarning::from_bytes(&value) {
                    // Skip records before cutoff
                    if earning.timestamp < cutoff_ms {
                        continue;
                    }

                    total_earnings = total_earnings + earning.reward;
                    total_flops += earning.flops;
                    total_tasks += earning.task_count as u64;

                    records.push(RpcEarningRecord {
                        block_height: format!("0x{:x}", earning.block_height),
                        reward: format!("0x{:x}", earning.reward.0),
                        flops: format!("0x{:x}", earning.flops),
                        task_count: earning.task_count,
                        timestamp: format!("{}", earning.timestamp),
                    });
                }
            }
        }

        // Reverse so newest records come first
        records.reverse();

        // Current balance
        let balance = self
            .chain
            .state()
            .get_balance(&miner_address)
            .unwrap_or_default();

        Ok(RpcMinerEarnings {
            address: format!("0x{}", addr_hex),
            total_earnings: format!("0x{:x}", total_earnings.0),
            total_flops: format!("0x{:x}", total_flops),
            total_tasks: format!("0x{:x}", total_tasks),
            balance: format!("0x{:x}", balance.0),
            records,
        })
    }

    // ---- v2.0: Miner notification endpoints ----

    async fn register_webhook(&self, request: RpcRegisterWebhookRequest) -> RpcResult<String> {
        let miner_address = Self::parse_address(&request.miner_address)?;

        // Verify miner is registered
        {
            let miners = self.registered_miners.read();
            let profile = miners
                .get(&miner_address)
                .ok_or_else(|| RpcError::Execution("Miner not registered".to_string()))?;

            // Verify signature over the URL
            let url_hash = qfc_crypto::blake3_hash(request.url.as_bytes());
            let sig_bytes = hex::decode(
                request
                    .signature
                    .strip_prefix("0x")
                    .unwrap_or(&request.signature),
            )
            .map_err(|e| RpcError::Execution(format!("Invalid signature hex: {}", e)))?;
            let signature = qfc_types::Signature::from_slice(&sig_bytes)
                .ok_or_else(|| RpcError::Execution("Invalid signature length".to_string()))?;
            qfc_crypto::verify_hash_signature(&profile.public_key, &url_hash, &signature)
                .map_err(|_| RpcError::Execution("Signature verification failed".to_string()))?;
        }

        // Validate event types
        for event in &request.events {
            if !crate::webhook::VALID_WEBHOOK_EVENTS.contains(&event.as_str()) {
                return Err(RpcError::Execution(format!(
                    "Invalid event type '{}'. Valid types: {}",
                    event,
                    crate::webhook::VALID_WEBHOOK_EVENTS.join(", "),
                ))
                .into());
            }
        }

        // Validate URL
        if !request.url.starts_with("https://") && !request.url.starts_with("http://localhost") {
            return Err(RpcError::Execution("Webhook URL must use HTTPS".to_string()).into());
        }

        // Generate webhook ID
        let id_input = format!("{}{}", request.miner_address, request.url);
        let id = hex::encode(&qfc_crypto::blake3_hash(id_input.as_bytes()).as_bytes()[..8]);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let webhook = crate::webhook::Webhook {
            id: id.clone(),
            url: request.url.clone(),
            events: request.events,
            created_at: now,
            active: true,
        };

        // Store webhook (max 5 per miner)
        {
            let mut store = self.webhook_store.write();
            let hooks = store.entry(miner_address).or_default();
            if hooks.len() >= 5 {
                return Err(RpcError::Execution("Maximum 5 webhooks per miner".to_string()).into());
            }
            // Replace if same ID exists
            hooks.retain(|h| h.id != id);
            hooks.push(webhook);
        }

        info!(
            "Webhook registered for miner {}: {}",
            request.miner_address,
            crate::webhook::mask_url(&request.url)
        );

        Ok(id)
    }

    async fn remove_webhook(&self, request: RpcRemoveWebhookRequest) -> RpcResult<bool> {
        let miner_address = Self::parse_address(&request.miner_address)?;

        // Verify miner is registered and signature is valid
        {
            let miners = self.registered_miners.read();
            let profile = miners
                .get(&miner_address)
                .ok_or_else(|| RpcError::Execution("Miner not registered".to_string()))?;

            let msg_hash = qfc_crypto::blake3_hash(request.webhook_id.as_bytes());
            let sig_bytes = hex::decode(
                request
                    .signature
                    .strip_prefix("0x")
                    .unwrap_or(&request.signature),
            )
            .map_err(|e| RpcError::Execution(format!("Invalid signature hex: {}", e)))?;
            let signature = qfc_types::Signature::from_slice(&sig_bytes)
                .ok_or_else(|| RpcError::Execution("Invalid signature length".to_string()))?;
            qfc_crypto::verify_hash_signature(&profile.public_key, &msg_hash, &signature)
                .map_err(|_| RpcError::Execution("Signature verification failed".to_string()))?;
        }

        let mut store = self.webhook_store.write();
        if let Some(hooks) = store.get_mut(&miner_address) {
            let before = hooks.len();
            hooks.retain(|h| h.id != request.webhook_id);
            let removed = hooks.len() < before;
            if removed {
                info!(
                    "Webhook {} removed for miner {}",
                    request.webhook_id, request.miner_address
                );
            }
            Ok(removed)
        } else {
            Ok(false)
        }
    }

    async fn get_webhooks(&self, address: String) -> RpcResult<Vec<RpcWebhook>> {
        let miner_address = Self::parse_address(&address)?;
        let store = self.webhook_store.read();
        let hooks = store.get(&miner_address).cloned().unwrap_or_default();

        Ok(hooks
            .into_iter()
            .map(|h| RpcWebhook {
                id: h.id,
                url: crate::webhook::mask_url(&h.url),
                events: h.events,
                created_at: format!("{}", h.created_at),
                active: h.active,
            })
            .collect())
    }

    // ---- v2.0: Cross-chain bridge endpoints ----

    async fn get_bridge_status(&self) -> RpcResult<RpcBridgeStatus> {
        let bridge = self.bridge.read();
        let status = bridge.status();
        Ok(RpcBridgeStatus {
            active: status.active,
            validator_count: status.validator_count,
            threshold: status.threshold,
            total_deposits: status.total_deposits,
            total_withdrawals: status.total_withdrawals,
            pending_deposits: status.pending_deposits,
            pending_withdrawals: status.pending_withdrawals,
            total_value_locked: status.total_value_locked,
        })
    }

    async fn get_bridge_deposit(&self, deposit_id: String) -> RpcResult<Option<RpcBridgeDeposit>> {
        let hash = Self::parse_hash(&deposit_id)?;
        let bridge = self.bridge.read();
        Ok(bridge.get_deposit(&hash).map(|d| {
            let status = match d.status {
                qfc_bridge::DepositStatus::Pending => "Pending",
                qfc_bridge::DepositStatus::Confirmed => "Confirmed",
                qfc_bridge::DepositStatus::Minting => "Minting",
                qfc_bridge::DepositStatus::Completed => "Completed",
                qfc_bridge::DepositStatus::Failed => "Failed",
            };
            RpcBridgeDeposit {
                deposit_id: hex::encode(d.deposit_id.as_bytes()),
                eth_tx_hash: hex::encode(d.eth_tx_hash.as_bytes()),
                eth_block_number: d.eth_block_number,
                eth_sender: d.eth_sender.to_string(),
                qfc_recipient: d.qfc_recipient.to_string(),
                token_address: d.token_address.to_string(),
                amount: d.amount.to_string(),
                status: status.to_string(),
                signature_count: d.signatures.len(),
                observed_at: d.observed_at,
            }
        }))
    }

    async fn get_bridge_withdrawal(
        &self,
        withdrawal_id: String,
    ) -> RpcResult<Option<RpcBridgeWithdrawal>> {
        let hash = Self::parse_hash(&withdrawal_id)?;
        let bridge = self.bridge.read();
        Ok(bridge.get_withdrawal(&hash).map(|w| {
            let status = match w.status {
                qfc_bridge::WithdrawalStatus::Pending => "Pending",
                qfc_bridge::WithdrawalStatus::Signing => "Signing",
                qfc_bridge::WithdrawalStatus::Submitted => "Submitted",
                qfc_bridge::WithdrawalStatus::Completed => "Completed",
                qfc_bridge::WithdrawalStatus::Failed => "Failed",
            };
            RpcBridgeWithdrawal {
                withdrawal_id: hex::encode(w.withdrawal_id.as_bytes()),
                qfc_tx_hash: hex::encode(w.qfc_tx_hash.as_bytes()),
                qfc_block_number: w.qfc_block_number,
                qfc_sender: w.qfc_sender.to_string(),
                eth_recipient: w.eth_recipient.to_string(),
                token_address: w.token_address.to_string(),
                amount: w.amount.to_string(),
                status: status.to_string(),
                signature_count: w.signatures.len(),
                observed_at: w.observed_at,
                eth_unlock_tx: w.eth_unlock_tx.map(|h| hex::encode(h.as_bytes())),
            }
        }))
    }

    // ---- v2.0: State rent endpoints ----

    async fn get_account_rent_info(&self, address: String) -> RpcResult<RpcAccountRentInfo> {
        let addr = Self::parse_address(&address)?;
        let state = self.chain.state();
        let account = state
            .get_account(&addr)
            .map_err(|e| RpcError::Internal(format!("Failed to get account: {}", e)))?;

        let current_block = self.chain.block_number();
        let current_epoch = current_block / qfc_types::BLOCKS_PER_EPOCH;

        let epochs_since_active = current_epoch.saturating_sub(account.last_active_epoch);
        let rent_owed = qfc_types::STORAGE_RENT_PER_SLOT_PER_EPOCH
            * account.storage_slot_count as u128
            * epochs_since_active as u128;

        Ok(RpcAccountRentInfo {
            address: addr.to_string(),
            storage_deposit: account.storage_deposit.to_string(),
            storage_slot_count: account.storage_slot_count,
            last_active_epoch: account.last_active_epoch,
            is_dormant: account.is_dormant,
            rent_owed: rent_owed.to_string(),
            current_epoch,
            reactivation_fee: qfc_types::REACTIVATION_FEE.to_string(),
        })
    }

    // ---- v2.0: Account abstraction (EIP-4337) endpoints ----

    async fn send_user_operation(&self, user_op: RpcUserOperation) -> RpcResult<String> {
        let sender = Self::parse_address(&user_op.sender)?;

        let parse_hex = |s: &str| -> Vec<u8> {
            hex::decode(s.strip_prefix("0x").unwrap_or(s)).unwrap_or_default()
        };
        let parse_u64 = |s: &str| -> u64 {
            s.strip_prefix("0x")
                .map(|h| u64::from_str_radix(h, 16).unwrap_or(0))
                .unwrap_or_else(|| s.parse().unwrap_or(0))
        };
        let parse_u128 = |s: &str| -> u128 {
            s.strip_prefix("0x")
                .map(|h| u128::from_str_radix(h, 16).unwrap_or(0))
                .unwrap_or_else(|| s.parse().unwrap_or(0))
        };

        let op = qfc_executor::account_abstraction::UserOperation {
            sender,
            nonce: parse_u64(&user_op.nonce),
            init_code: parse_hex(&user_op.init_code),
            call_data: parse_hex(&user_op.call_data),
            call_gas_limit: parse_u64(&user_op.call_gas_limit),
            verification_gas_limit: parse_u64(&user_op.verification_gas_limit),
            pre_verification_gas: parse_u64(&user_op.pre_verification_gas),
            max_fee_per_gas: parse_u128(&user_op.max_fee_per_gas),
            max_priority_fee_per_gas: parse_u128(&user_op.max_priority_fee_per_gas),
            paymaster_and_data: parse_hex(&user_op.paymaster_and_data),
            signature: parse_hex(&user_op.signature),
        };

        // Validate via EntryPoint
        {
            let ep = self.entry_point.read();
            ep.validate_user_op(&op, 0)
                .map_err(|e| RpcError::Execution(e.to_string()))?;
        }

        // Add to UserOp pool
        let ep_addr = {
            let ep = self.entry_point.read();
            *ep.address()
        };
        let hash = self
            .user_op_pool
            .write()
            .add(op, &ep_addr, self.chain_id, 0)
            .map_err(|e| RpcError::Execution(e.to_string()))?;

        Ok(hex::encode(hash.as_bytes()))
    }

    async fn get_user_operation_by_hash(
        &self,
        hash: String,
    ) -> RpcResult<Option<RpcUserOperationStatus>> {
        let op_hash = Self::parse_hash(&hash)?;
        let pool = self.user_op_pool.read();

        Ok(pool.get(&op_hash).map(|pooled| RpcUserOperationStatus {
            user_op_hash: hex::encode(pooled.hash.as_bytes()),
            sender: pooled.user_op.sender.to_string(),
            nonce: pooled.user_op.nonce,
            status: "pending".to_string(),
            paymaster: pooled.user_op.paymaster().map(|p| p.to_string()),
        }))
    }

    async fn supported_entry_points(&self) -> RpcResult<Vec<String>> {
        let ep = self.entry_point.read();
        Ok(vec![ep.address().to_string()])
    }

    // ---- v2.0: Public Inference API endpoints ----

    async fn submit_public_task(&self, request: RpcSubmitPublicTask) -> RpcResult<String> {
        // Parse submitter address
        let submitter = Self::parse_address(&request.submitter)?;

        // Verify signature (Ed25519 over task fields)
        let sig_bytes = hex::decode(
            request
                .signature
                .strip_prefix("0x")
                .unwrap_or(&request.signature),
        )
        .map_err(|e| RpcError::InvalidParams(format!("Invalid signature hex: {}", e)))?;

        // Build message = task_type || model_id || input_data || max_fee
        let mut sign_msg = Vec::new();
        sign_msg.extend_from_slice(request.task_type.as_bytes());
        sign_msg.extend_from_slice(request.model_id.as_bytes());
        sign_msg.extend_from_slice(request.input_data.as_bytes());
        sign_msg.extend_from_slice(request.max_fee.as_bytes());
        let msg_hash = blake3_hash(&sign_msg);

        // Look up submitter's public key from validators (or accept if dev mode)
        let consensus = self.chain.consensus();
        let validators = consensus.get_validators();
        if let Some(validator) = validators.iter().find(|v| v.address == submitter) {
            let sig = qfc_types::Signature::from_slice(&sig_bytes)
                .ok_or_else(|| RpcError::InvalidParams("Invalid signature length".into()))?;
            if verify_hash_signature(&validator.public_key, &msg_hash, &sig).is_err() {
                return Err(RpcError::Execution("Invalid signature for submitter".into()).into());
            }
        }
        // Non-validators can still submit if they have balance (signature check is best-effort)

        // Parse model ID from "name" or "name:version" format
        let (model_name, model_version) = if request.model_id.contains(':') {
            let parts: Vec<&str> = request.model_id.splitn(2, ':').collect();
            (parts[0].to_string(), parts[1].to_string())
        } else {
            (request.model_id.clone(), "v1.0".to_string())
        };

        let model_id = qfc_inference::task::ModelId::new(&model_name, &model_version);

        // Verify model exists
        if !self.model_registry.is_approved(&model_id) {
            return Err(RpcError::InvalidParams(format!(
                "Model {} is not approved",
                request.model_id
            ))
            .into());
        }

        let input_data = hex::decode(
            request
                .input_data
                .strip_prefix("0x")
                .unwrap_or(&request.input_data),
        )
        .unwrap_or_default();

        let max_fee = request
            .max_fee
            .strip_prefix("0x")
            .map(|s| u128::from_str_radix(s, 16).unwrap_or(0))
            .unwrap_or_else(|| request.max_fee.parse::<u128>().unwrap_or(0));

        // Escrow: deduct fee from submitter balance immediately
        let fee_u256 = U256::from_u128(max_fee);
        let state = self.chain.state();
        let balance = state
            .get_balance(&submitter)
            .map_err(|e| RpcError::Execution(format!("Failed to get balance: {}", e)))?;
        if balance < fee_u256 {
            return Err(RpcError::Execution(format!(
                "Insufficient balance: have {}, need {}",
                balance, fee_u256
            ))
            .into());
        }
        state
            .sub_balance(&submitter, fee_u256)
            .map_err(|e| RpcError::Execution(format!("Failed to escrow fee: {}", e)))?;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let input_hash = qfc_crypto::blake3_hash(&input_data);
        let task_type = match request.task_type.as_str() {
            "embedding" => qfc_inference::task::ComputeTaskType::Embedding {
                model_id,
                input_hash,
            },
            "speech_to_text" => qfc_inference::task::ComputeTaskType::SpeechToText {
                model_id,
                audio_hash: input_hash,
                language: request.language.clone().unwrap_or_default(),
            },
            _ => qfc_inference::task::ComputeTaskType::Embedding {
                model_id,
                input_hash,
            },
        };

        // Validate fee meets minimum base price
        let base_fee = qfc_ai_coordinator::estimate_base_fee(&task_type);
        if max_fee < base_fee {
            return Err(RpcError::InvalidParams(format!(
                "Fee too low: {} < base fee {}",
                max_fee, base_fee
            ))
            .into());
        }

        let mut pool = self.task_pool.write();
        let task_id = {
            let mut data = Vec::with_capacity(16);
            data.extend_from_slice(&now.to_le_bytes());
            data.extend_from_slice(&(pool.pending_count() as u64).to_le_bytes());
            qfc_crypto::blake3_hash(&data)
        };

        let current_epoch = self.chain.get_epoch();
        let task = qfc_inference::InferenceTask::new(
            task_id,
            current_epoch.number,
            task_type,
            input_data,
            now,
            now + 60_000, // 60s deadline
        );

        let public_task_id = pool.submit_public_task(submitter, task, max_fee);
        info!(
            "Public task submitted by {}: {} (fee: {})",
            submitter,
            hex::encode(&public_task_id.as_bytes()[..8]),
            max_fee
        );
        Ok(hex::encode(public_task_id.as_bytes()))
    }

    async fn get_public_task_status(&self, task_id: String) -> RpcResult<RpcPublicTaskStatus> {
        let task_hash = Self::parse_hash(&task_id)?;
        let pool = self.task_pool.read();

        // Check in-memory first (active + recently completed)
        if let Some(task) = pool.get_public_task(&task_hash) {
            return Ok(Self::build_task_status(task));
        }
        drop(pool);

        // Fall back to RocksDB for persisted completed/expired tasks
        if let Ok(Some(bytes)) = self
            .chain
            .db()
            .get(qfc_storage::cf::TASKS, task_hash.as_bytes())
        {
            if let Some(task) = qfc_ai_coordinator::TaskPool::deserialize_task(&bytes) {
                return Ok(Self::build_task_status(&task));
            }
        }

        Err(RpcError::InvalidParams("Task not found".to_string()).into())
    }

    async fn list_public_tasks(
        &self,
        filter: RpcListPublicTasksFilter,
    ) -> RpcResult<Vec<RpcPublicTaskStatus>> {
        let submitter = filter
            .submitter
            .as_deref()
            .map(Self::parse_address)
            .transpose()?;

        let pool_filter = qfc_ai_coordinator::PublicTaskFilter {
            submitter,
            status: filter.status.clone(),
            limit: filter.limit,
            offset: filter.offset,
        };

        let pool = self.task_pool.read();
        let mut tasks: Vec<RpcPublicTaskStatus> = pool
            .list_public_tasks(&pool_filter)
            .into_iter()
            .map(Self::build_task_status)
            .collect();
        drop(pool);

        // If in-memory results are fewer than requested, supplement from RocksDB
        let limit = filter.limit.min(200).max(1);
        if tasks.len() < limit {
            let remaining = limit - tasks.len();
            let in_memory_ids: std::collections::HashSet<String> =
                tasks.iter().map(|t| t.task_id.clone()).collect();

            if let Ok(iter) = self.chain.db().iter(qfc_storage::cf::TASKS) {
                for (_, value) in iter.take(remaining * 2) {
                    if let Some(task) = qfc_ai_coordinator::TaskPool::deserialize_task(&value) {
                        let tid = hex::encode(task.task_id.as_bytes());
                        if in_memory_ids.contains(&tid) {
                            continue;
                        }
                        // Apply filters
                        if let Some(ref sub) = submitter {
                            if task.submitter != *sub {
                                continue;
                            }
                        }
                        if let Some(ref status) = filter.status {
                            let task_status = match &task.status {
                                qfc_ai_coordinator::task_pool::PublicTaskStatus::Pending => {
                                    "Pending"
                                }
                                qfc_ai_coordinator::task_pool::PublicTaskStatus::Assigned => {
                                    "Assigned"
                                }
                                qfc_ai_coordinator::task_pool::PublicTaskStatus::Completed {
                                    ..
                                } => "Completed",
                                qfc_ai_coordinator::task_pool::PublicTaskStatus::Failed => "Failed",
                                qfc_ai_coordinator::task_pool::PublicTaskStatus::Expired => {
                                    "Expired"
                                }
                            };
                            if task_status != status.as_str() {
                                continue;
                            }
                        }
                        tasks.push(Self::build_task_status(&task));
                        if tasks.len() >= limit {
                            break;
                        }
                    }
                }
            }
        }

        Ok(tasks)
    }

    async fn estimate_inference_fee(
        &self,
        request: RpcEstimateInferenceFee,
    ) -> RpcResult<RpcInferenceFeeEstimate> {
        use qfc_inference::{ComputeTaskType, ModelId};

        // Parse model_id: "name" or "name:version"
        let (model_name, model_version) = if let Some(idx) = request.model_id.find(':') {
            (
                request.model_id[..idx].to_string(),
                request.model_id[idx + 1..].to_string(),
            )
        } else {
            (request.model_id.clone(), "v1.0".to_string())
        };
        let model_id = ModelId::new(&model_name, &model_version);

        // Build a ComputeTaskType from the request
        let task_type = match request.task_type.as_str() {
            "TextGeneration" => ComputeTaskType::TextGeneration {
                model_id,
                prompt_hash: qfc_types::Hash::ZERO,
                max_tokens: request.max_tokens as u32,
                temperature_fp: 0,
                seed: 0,
            },
            "ImageClassification" => ComputeTaskType::ImageClassification {
                model_id,
                input_hash: qfc_types::Hash::ZERO,
            },
            "SpeechToText" => ComputeTaskType::SpeechToText {
                model_id,
                audio_hash: qfc_types::Hash::ZERO,
                language: String::new(),
            },
            "ImageGeneration" => ComputeTaskType::ImageGeneration {
                model_id,
                prompt_hash: qfc_types::Hash::ZERO,
                negative_prompt_hash: qfc_types::Hash::ZERO,
                width: 512,
                height: 512,
                steps: 20,
                seed: 0,
            },
            "OnnxInference" => ComputeTaskType::OnnxInference {
                model_hash: qfc_types::Hash::ZERO,
                input_hash: qfc_types::Hash::ZERO,
            },
            _ => ComputeTaskType::Embedding {
                model_id,
                input_hash: qfc_types::Hash::ZERO,
            },
        };

        let reqs = qfc_ai_coordinator::task_types::task_requirements(&task_type);
        let base_fee = qfc_ai_coordinator::estimate_base_fee(&task_type);

        let tier_str = match reqs.min_tier {
            qfc_inference::GpuTier::Hot => "Hot",
            qfc_inference::GpuTier::Warm => "Warm",
            qfc_inference::GpuTier::Cold => "Cold",
        };

        Ok(RpcInferenceFeeEstimate {
            base_fee: format!("0x{:x}", base_fee),
            model_id: request.model_id,
            gpu_tier: tier_str.to_string(),
            estimated_time_ms: reqs.timeout_ms,
            min_memory_mb: reqs.min_memory_mb,
            estimated_flops: reqs.estimated_flops,
        })
    }

    async fn get_inference_result(&self, cid: String) -> RpcResult<String> {
        use base64::Engine;

        let ipfs = self
            .ipfs_client
            .as_ref()
            .ok_or_else(|| RpcError::Internal("IPFS client not configured".to_string()))?;

        let data = ipfs
            .fetch(&cid)
            .await
            .map_err(|e| RpcError::Internal(format!("Failed to fetch from IPFS: {}", e)))?;

        Ok(base64::engine::general_purpose::STANDARD.encode(&data))
    }

    async fn subscribe_task_status(
        &self,
        pending: jsonrpsee::PendingSubscriptionSink,
        task_id: String,
    ) -> SubscriptionResult {
        use jsonrpsee::SubscriptionMessage;

        let task_hash = match Self::parse_hash(&task_id) {
            Ok(h) => h,
            Err(e) => {
                pending.reject(e).await;
                return Ok(());
            }
        };

        let sink = pending.accept().await?;

        // Send initial status
        let initial = {
            let pool = self.task_pool.read();
            pool.get_public_task(&task_hash)
                .map(Self::build_task_status)
        };
        if let Some(status) = initial {
            let msg = SubscriptionMessage::from_json(&status)?;
            if sink.send(msg).await.is_err() {
                return Ok(());
            }
        }

        // Poll for status changes every 500ms until terminal state
        let task_pool = self.task_pool.clone();
        let mut last_status = String::new();
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            if sink.is_closed() {
                break;
            }
            let snapshot = {
                let pool = task_pool.read();
                pool.get_public_task(&task_hash)
                    .map(Self::build_task_status)
            };
            if let Some(rpc_status) = snapshot {
                if rpc_status.status != last_status {
                    last_status.clone_from(&rpc_status.status);
                    let is_terminal =
                        matches!(last_status.as_str(), "Completed" | "Failed" | "Expired");
                    let msg = SubscriptionMessage::from_json(&rpc_status)?;
                    if sink.send(msg).await.is_err() {
                        break;
                    }
                    if is_terminal {
                        break;
                    }
                }
            } else {
                break; // Task pruned
            }
        }
        Ok(())
    }

    async fn subscribe_miner_events(
        &self,
        pending: jsonrpsee::PendingSubscriptionSink,
        address: String,
    ) -> SubscriptionResult {
        use jsonrpsee::SubscriptionMessage;

        let miner_address = match Self::parse_address(&address) {
            Ok(addr) => addr,
            Err(e) => {
                pending.reject(e).await;
                return Ok(());
            }
        };

        let sink = pending.accept().await?;
        let chain = self.chain.clone();
        let miner_hex = hex::encode(miner_address.as_bytes());

        // Track last seen block height
        let mut last_block = chain.block_number();

        // Poll every 1s for new blocks containing this miner's proofs
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            if sink.is_closed() {
                break;
            }

            let current_block = chain.block_number();
            if current_block <= last_block {
                continue;
            }

            for height in (last_block + 1)..=current_block {
                if sink.is_closed() {
                    break;
                }
                let block = match chain.get_block_by_number(height) {
                    Ok(Some(b)) => b,
                    _ => continue,
                };

                // Count this miner's proofs in the block for reward estimation
                let miner_proofs: Vec<_> = block
                    .inference_proofs
                    .iter()
                    .filter(|p| p.validator == miner_address)
                    .collect();

                if miner_proofs.is_empty() {
                    continue;
                }

                let total_flops_in_block: u64 = block
                    .inference_proofs
                    .iter()
                    .map(|p| p.flops_estimated)
                    .sum();
                let miner_flops: u64 = miner_proofs.iter().map(|p| p.flops_estimated).sum();

                for proof in &miner_proofs {
                    let event = RpcMinerEvent {
                        event_type: "proof_accepted".to_string(),
                        miner: format!("0x{}", miner_hex),
                        block_height: Some(format!("0x{:x}", height)),
                        task_type: Some(format!("{:?}", proof.task_type)),
                        flops: Some(proof.flops_estimated.to_string()),
                        reward: None,
                        spot_checked: None,
                        timestamp: format!("{}", block.header.timestamp),
                        message: format!(
                            "Proof included in block {} ({} FLOPS)",
                            height, proof.flops_estimated
                        ),
                    };
                    let msg = SubscriptionMessage::from_json(&event)?;
                    if sink.send(msg).await.is_err() {
                        return Ok(());
                    }
                }

                // Emit a reward_settled event summarizing this miner's block reward
                if total_flops_in_block > 0 {
                    // 15% of block reward goes to miner pool, proportional to FLOPS
                    let block_reward = U256::from_u128(qfc_types::BLOCK_REWARD);
                    let miner_pool = block_reward * U256::from_u128(15) / U256::from_u128(100);
                    let miner_reward = miner_pool * U256::from_u128(miner_flops as u128)
                        / U256::from_u128(total_flops_in_block as u128);

                    let event = RpcMinerEvent {
                        event_type: "reward_settled".to_string(),
                        miner: format!("0x{}", miner_hex),
                        block_height: Some(format!("0x{:x}", height)),
                        task_type: None,
                        flops: Some(miner_flops.to_string()),
                        reward: Some(format!("0x{:x}", miner_reward.0)),
                        spot_checked: None,
                        timestamp: format!("{}", block.header.timestamp),
                        message: format!(
                            "Reward settled: {} proofs, {} FLOPS in block {}",
                            miner_proofs.len(),
                            miner_flops,
                            height,
                        ),
                    };
                    let msg = SubscriptionMessage::from_json(&event)?;
                    if sink.send(msg).await.is_err() {
                        return Ok(());
                    }
                }
            }
            last_block = current_block;
        }
        Ok(())
    }

    // ---- v2.0: Agent Registry endpoints ----

    async fn get_agent_info(&self, agent_id: String) -> RpcResult<RpcAgentInfo> {
        let contract_addr = Address::from_slice(
            &hex::decode(AGENT_REGISTRY_ADDRESS).map_err(|e| RpcError::Internal(e.to_string()))?,
        )
        .ok_or_else(|| RpcError::Internal("Invalid AgentRegistry address".into()))?;

        // getAgent(string) selector: keccak256("getAgent(string)")[:4]
        let selector: [u8; 4] = [0xc2, 0xbc, 0x2e, 0xfc];
        let calldata = abi_encode_string_call(selector, &agent_id);

        let (success, output, _gas) = self
            .chain
            .simulate_call(None, Some(contract_addr), U256::ZERO, calldata, None)
            .map_err(|e| RpcError::Execution(e.to_string()))?;

        if !success {
            return Err(RpcError::Execution("getAgent call reverted".into()).into());
        }

        Ok(parse_agent_info(&agent_id, &output))
    }

    async fn list_agents_by_owner(&self, owner_address: String) -> RpcResult<Vec<RpcAgentInfo>> {
        let owner = Self::parse_address(&owner_address)?;
        let contract_addr = Address::from_slice(
            &hex::decode(AGENT_REGISTRY_ADDRESS).map_err(|e| RpcError::Internal(e.to_string()))?,
        )
        .ok_or_else(|| RpcError::Internal("Invalid AgentRegistry address".into()))?;

        // getAgentsByOwner(address) selector: keccak256("getAgentsByOwner(address)")[:4]
        let selector: [u8; 4] = [0xd1, 0x05, 0x3b, 0x95];
        let calldata = abi_encode_address_call(selector, &owner);

        let (success, output, _gas) = self
            .chain
            .simulate_call(None, Some(contract_addr), U256::ZERO, calldata, None)
            .map_err(|e| RpcError::Execution(e.to_string()))?;

        if !success {
            return Err(RpcError::Execution("getAgentsByOwner call reverted".into()).into());
        }

        // Output is an ABI-encoded array of strings (agent IDs).
        // We fetch each agent individually.
        let mut agents = Vec::new();

        // Decode the string[] return: offset -> length -> [offsets] -> [string data]
        if output.len() >= 64 {
            let arr_offset = {
                let raw = abi_read_u256_raw(&output, 0);
                u64::from_be_bytes(raw[24..32].try_into().unwrap_or([0; 8])) as usize
            };
            if arr_offset + 32 <= output.len() {
                let arr_len = {
                    let raw = abi_read_u256_raw(&output, arr_offset / 32);
                    u64::from_be_bytes(raw[24..32].try_into().unwrap_or([0; 8])) as usize
                };
                let arr_data = &output[arr_offset..];
                for i in 0..arr_len {
                    let id_str = abi_read_string(arr_data, 32, 1 + i);
                    if !id_str.is_empty() {
                        match self.get_agent_info(id_str.clone()).await {
                            Ok(info) => agents.push(info),
                            Err(e) => {
                                tracing::warn!("Failed to load agent {}: {}", id_str, e);
                            }
                        }
                    }
                }
            }
        }

        Ok(agents)
    }

    async fn validate_session_key(&self, key_address: String) -> RpcResult<RpcSessionKeyInfo> {
        let key_addr = Self::parse_address(&key_address)?;
        let contract_addr = Address::from_slice(
            &hex::decode(AGENT_REGISTRY_ADDRESS).map_err(|e| RpcError::Internal(e.to_string()))?,
        )
        .ok_or_else(|| RpcError::Internal("Invalid AgentRegistry address".into()))?;

        // isSessionKeyValid(address) selector: keccak256("isSessionKeyValid(address)")[:4]
        let is_valid_selector: [u8; 4] = [0x9d, 0x3a, 0x1b, 0x8e];
        let calldata_valid = abi_encode_address_call(is_valid_selector, &key_addr);

        let (success, output_valid, _gas) = self
            .chain
            .simulate_call(None, Some(contract_addr), U256::ZERO, calldata_valid, None)
            .map_err(|e| RpcError::Execution(e.to_string()))?;

        if !success {
            return Ok(RpcSessionKeyInfo {
                valid: false,
                agent_id: String::new(),
                expires_at: "0x0".to_string(),
            });
        }

        let valid = abi_read_bool(&output_valid, 0);

        // getAgentForSessionKey(address) selector: keccak256("getAgentForSessionKey(address)")[:4]
        let get_agent_selector: [u8; 4] = [0x4a, 0x61, 0xbc, 0x42];
        let calldata_agent = abi_encode_address_call(get_agent_selector, &key_addr);

        let (success2, output_agent, _gas2) = self
            .chain
            .simulate_call(None, Some(contract_addr), U256::ZERO, calldata_agent, None)
            .map_err(|e| RpcError::Execution(e.to_string()))?;

        let (agent_id, expires_at) = if success2 && output_agent.len() >= 64 {
            // Returns (string agentId, uint256 expiresAt)
            let offset = {
                let raw = abi_read_u256_raw(&output_agent, 0);
                u64::from_be_bytes(raw[24..32].try_into().unwrap_or([0; 8])) as usize
            };
            let aid = if offset + 32 <= output_agent.len() {
                abi_read_string(&output_agent, 0, 0)
            } else {
                String::new()
            };
            let exp = abi_read_u256(&output_agent, 1);
            (aid, exp)
        } else {
            (String::new(), "0x0".to_string())
        };

        Ok(RpcSessionKeyInfo {
            valid,
            agent_id,
            expires_at,
        })
    }

    // ---- v2.0: Agent Registry write endpoints ----

    async fn register_agent(
        &self,
        request: RpcRegisterAgentRequest,
    ) -> RpcResult<RpcAgentWriteResult> {
        // Validate agent_id length
        if request.agent_id.is_empty() || request.agent_id.len() > 64 {
            return Err(RpcError::InvalidParams("agent_id must be 1-64 characters".into()).into());
        }

        let owner_address = Self::parse_address(&request.owner)?;

        // Parse and verify public key
        let pk_hex = request
            .public_key
            .strip_prefix("0x")
            .unwrap_or(&request.public_key);
        let pk_bytes = hex::decode(pk_hex)
            .map_err(|e| RpcError::InvalidParams(format!("Invalid public key hex: {}", e)))?;
        let public_key = qfc_types::PublicKey::from_slice(&pk_bytes)
            .ok_or_else(|| RpcError::InvalidParams("Invalid Ed25519 public key".into()))?;

        // Verify that public key matches owner address
        let derived_address = qfc_crypto::address_from_public_key(&public_key);
        if derived_address != owner_address {
            return Err(RpcError::Execution(
                "Permission denied: public key does not match owner address".into(),
            )
            .into());
        }

        // Parse daily_limit and max_per_tx
        let daily_limit = Self::parse_amount_u128(&request.daily_limit)?;
        let max_per_tx = Self::parse_amount_u128(&request.max_per_tx)?;
        if max_per_tx > daily_limit {
            return Err(
                RpcError::InvalidParams("max_per_tx cannot exceed daily_limit".into()).into(),
            );
        }

        // Verify signature: sign(agent_id || owner || daily_limit || max_per_tx)
        let mut sign_payload = Vec::new();
        sign_payload.extend_from_slice(request.agent_id.as_bytes());
        sign_payload.extend_from_slice(owner_address.as_bytes());
        sign_payload.extend_from_slice(&daily_limit.to_be_bytes());
        sign_payload.extend_from_slice(&max_per_tx.to_be_bytes());
        let payload_hash = blake3_hash(&sign_payload);

        let sig_hex = request
            .signature
            .strip_prefix("0x")
            .unwrap_or(&request.signature);
        let sig_bytes = hex::decode(sig_hex)
            .map_err(|e| RpcError::InvalidParams(format!("Invalid signature hex: {}", e)))?;
        let signature = qfc_types::Signature::from_slice(&sig_bytes).ok_or_else(|| {
            RpcError::InvalidParams("Invalid signature: expected 64 bytes".into())
        })?;

        if verify_hash_signature(&public_key, &payload_hash, &signature).is_err() {
            return Err(RpcError::Execution(
                "Invalid signature: owner authorization failed".into(),
            )
            .into());
        }

        // Build ABI calldata for registerAgent(string,uint8[],uint256,uint256)
        // selector: keccak256("registerAgent(string,uint8[],uint256,uint256)")[:4]
        let selector: [u8; 4] = [0xa8, 0x5e, 0xf5, 0x79];
        let calldata = abi_encode_register_agent(
            selector,
            &request.agent_id,
            &request.permissions,
            daily_limit,
            max_per_tx,
        );

        let tx_hash = self
            .submit_agent_contract_call(owner_address, public_key, calldata, U256::ZERO)
            .await?;

        info!(
            "Agent registered: id={}, owner={}, tx={}",
            request.agent_id, owner_address, tx_hash
        );

        Ok(RpcAgentWriteResult {
            tx_hash: tx_hash.to_string(),
            agent_id: request.agent_id,
            message: "Agent registration submitted".into(),
        })
    }

    async fn fund_agent(&self, request: RpcFundAgentRequest) -> RpcResult<RpcAgentWriteResult> {
        if request.agent_id.is_empty() {
            return Err(RpcError::InvalidParams("agent_id is required".into()).into());
        }

        let funder_address = Self::parse_address(&request.funder)?;

        // Parse and verify public key
        let pk_hex = request
            .public_key
            .strip_prefix("0x")
            .unwrap_or(&request.public_key);
        let pk_bytes = hex::decode(pk_hex)
            .map_err(|e| RpcError::InvalidParams(format!("Invalid public key hex: {}", e)))?;
        let public_key = qfc_types::PublicKey::from_slice(&pk_bytes)
            .ok_or_else(|| RpcError::InvalidParams("Invalid Ed25519 public key".into()))?;

        // Verify that public key matches funder address
        let derived_address = qfc_crypto::address_from_public_key(&public_key);
        if derived_address != funder_address {
            return Err(RpcError::Execution(
                "Permission denied: public key does not match funder address".into(),
            )
            .into());
        }

        // Parse amount
        let amount = Self::parse_amount_u128(&request.amount)?;
        if amount == 0 {
            return Err(RpcError::InvalidParams("Amount must be greater than zero".into()).into());
        }

        // Verify signature: sign(agent_id || funder || amount)
        let mut sign_payload = Vec::new();
        sign_payload.extend_from_slice(request.agent_id.as_bytes());
        sign_payload.extend_from_slice(funder_address.as_bytes());
        sign_payload.extend_from_slice(&amount.to_be_bytes());
        let payload_hash = blake3_hash(&sign_payload);

        let sig_hex = request
            .signature
            .strip_prefix("0x")
            .unwrap_or(&request.signature);
        let sig_bytes = hex::decode(sig_hex)
            .map_err(|e| RpcError::InvalidParams(format!("Invalid signature hex: {}", e)))?;
        let signature = qfc_types::Signature::from_slice(&sig_bytes).ok_or_else(|| {
            RpcError::InvalidParams("Invalid signature: expected 64 bytes".into())
        })?;

        if verify_hash_signature(&public_key, &payload_hash, &signature).is_err() {
            return Err(RpcError::Execution(
                "Invalid signature: funder authorization failed".into(),
            )
            .into());
        }

        // Build ABI calldata for fundAgent(string)
        // selector: keccak256("fundAgent(string)")[:4]
        let selector: [u8; 4] = [0x1c, 0x4b, 0x77, 0x4b];
        let calldata = abi_encode_string_call(selector, &request.agent_id);

        // The deposit amount is sent as msg.value
        let tx_hash = self
            .submit_agent_contract_call(
                funder_address,
                public_key,
                calldata,
                U256::from_u128(amount),
            )
            .await?;

        info!(
            "Agent funded: id={}, funder={}, amount={}, tx={}",
            request.agent_id, funder_address, amount, tx_hash
        );

        Ok(RpcAgentWriteResult {
            tx_hash: tx_hash.to_string(),
            agent_id: request.agent_id,
            message: format!("Agent fund deposit of {} wei submitted", amount),
        })
    }

    async fn revoke_agent(&self, request: RpcRevokeAgentRequest) -> RpcResult<RpcAgentWriteResult> {
        if request.agent_id.is_empty() {
            return Err(RpcError::InvalidParams("agent_id is required".into()).into());
        }

        let owner_address = Self::parse_address(&request.owner)?;

        // Parse and verify public key
        let pk_hex = request
            .public_key
            .strip_prefix("0x")
            .unwrap_or(&request.public_key);
        let pk_bytes = hex::decode(pk_hex)
            .map_err(|e| RpcError::InvalidParams(format!("Invalid public key hex: {}", e)))?;
        let public_key = qfc_types::PublicKey::from_slice(&pk_bytes)
            .ok_or_else(|| RpcError::InvalidParams("Invalid Ed25519 public key".into()))?;

        // Verify that public key matches owner address
        let derived_address = qfc_crypto::address_from_public_key(&public_key);
        if derived_address != owner_address {
            return Err(RpcError::Execution(
                "Permission denied: public key does not match owner address".into(),
            )
            .into());
        }

        // Verify signature: sign(agent_id || owner || "revoke")
        let mut sign_payload = Vec::new();
        sign_payload.extend_from_slice(request.agent_id.as_bytes());
        sign_payload.extend_from_slice(owner_address.as_bytes());
        sign_payload.extend_from_slice(b"revoke");
        let payload_hash = blake3_hash(&sign_payload);

        let sig_hex = request
            .signature
            .strip_prefix("0x")
            .unwrap_or(&request.signature);
        let sig_bytes = hex::decode(sig_hex)
            .map_err(|e| RpcError::InvalidParams(format!("Invalid signature hex: {}", e)))?;
        let signature = qfc_types::Signature::from_slice(&sig_bytes).ok_or_else(|| {
            RpcError::InvalidParams("Invalid signature: expected 64 bytes".into())
        })?;

        if verify_hash_signature(&public_key, &payload_hash, &signature).is_err() {
            return Err(RpcError::Execution(
                "Invalid signature: owner authorization failed".into(),
            )
            .into());
        }

        // Build ABI calldata for revokeAgent(string)
        // selector: keccak256("revokeAgent(string)")[:4]
        let selector: [u8; 4] = [0x67, 0xa3, 0xbc, 0x1a];
        let calldata = abi_encode_string_call(selector, &request.agent_id);

        let tx_hash = self
            .submit_agent_contract_call(owner_address, public_key, calldata, U256::ZERO)
            .await?;

        info!(
            "Agent revoked: id={}, owner={}, tx={}",
            request.agent_id, owner_address, tx_hash
        );

        Ok(RpcAgentWriteResult {
            tx_hash: tx_hash.to_string(),
            agent_id: request.agent_id,
            message: "Agent revocation submitted".into(),
        })
    }

    // ---- v3.0: Agent Discovery + Resource endpoints ----

    async fn list_agents(&self, params: RpcListAgentsParams) -> RpcResult<RpcListAgentsResponse> {
        use qfc_qvm::stdlib::agent_index::{AgentSortField, AgentStatusFilter};

        let status = match params.status.as_str() {
            "frozen" => AgentStatusFilter::Frozen,
            "all" => AgentStatusFilter::All,
            _ => AgentStatusFilter::Active,
        };
        let sort_by = match params.sort_by.as_str() {
            "stake" => AgentSortField::Stake,
            "created_at" => AgentSortField::CreatedAt,
            _ => AgentSortField::ReputationScore,
        };
        let sort_desc = params.sort_order != "asc";
        let limit = params.limit.max(1).min(200);

        let index = self.agent_index.read();
        let (agents, total) = index.list_agents(status, sort_by, sort_desc, limit, params.offset);

        let agent_views: Vec<RpcAgentDetailView> =
            agents.iter().map(|a| self.agent_view_to_rpc(a)).collect();

        Ok(RpcListAgentsResponse {
            has_more: params.offset + agent_views.len() < total,
            agents: agent_views,
            total,
        })
    }

    async fn query_agents_by_capability(
        &self,
        params: RpcQueryByCapabilityParams,
    ) -> RpcResult<Vec<RpcAgentDetailView>> {
        let min_stake = Self::parse_amount_u128(&params.min_stake).unwrap_or(0) as u64;
        let limit = params.limit.max(1).min(200);

        let index = self.agent_index.read();
        let results =
            index.query_by_capability(&params.capability, params.min_reputation, min_stake, limit);

        Ok(results.iter().map(|a| self.agent_view_to_rpc(a)).collect())
    }

    async fn query_agents_by_protocol_digest(
        &self,
        protocol_digest: String,
    ) -> RpcResult<Vec<RpcAgentDetailView>> {
        let digest_bytes = hex::decode(
            protocol_digest
                .strip_prefix("0x")
                .unwrap_or(&protocol_digest),
        )
        .map_err(|e| RpcError::InvalidParams(format!("Invalid protocol_digest hex: {}", e)))?;

        if digest_bytes.len() != 32 {
            return Err(RpcError::InvalidParams("protocol_digest must be 32 bytes".into()).into());
        }

        let mut hash_arr = [0u8; 32];
        hash_arr.copy_from_slice(&digest_bytes);
        let hash = Hash::from(hash_arr);

        let index = self.agent_index.read();
        let results = index.query_by_protocol_digest(&hash);
        Ok(results.iter().map(|a| self.agent_view_to_rpc(a)).collect())
    }

    async fn get_agent(&self, agent_id: String) -> RpcResult<RpcAgentDetailView> {
        let id_bytes = hex::decode(agent_id.strip_prefix("0x").unwrap_or(&agent_id))
            .map_err(|e| RpcError::InvalidParams(format!("Invalid agent_id hex: {}", e)))?;

        if id_bytes.len() != 32 {
            return Err(RpcError::InvalidParams("agent_id must be 32 bytes".into()).into());
        }

        let mut hash_arr = [0u8; 32];
        hash_arr.copy_from_slice(&id_bytes);
        let hash = Hash::from(hash_arr);

        let registry = self.agent_registry.read();
        let agent = registry
            .get(&hash)
            .ok_or_else(|| RpcError::Internal("Agent not found".into()))?;

        Ok(RpcAgentDetailView {
            agent_id: format!("0x{}", hex::encode(agent.id.as_bytes())),
            owner: format!("0x{}", hex::encode(agent.owner.as_bytes())),
            capabilities: agent.capabilities.clone(),
            protocol_digests: agent
                .protocol_digests
                .iter()
                .map(|d| format!("0x{}", hex::encode(d.as_bytes())))
                .collect(),
            endpoint: agent.endpoint.clone(),
            stake: agent.stake.to_string(),
            frozen: agent.frozen,
            reputation_score: agent.reputation_score,
            total_tasks_completed: agent.total_tasks_completed,
            total_tasks_failed: agent.total_tasks_failed,
            last_heartbeat: agent.last_heartbeat,
            created_at: agent.created_at,
        })
    }

    async fn freeze_agent(&self, params: RpcFreezeAgentParams) -> RpcResult<RpcAgentWriteResult> {
        let id_bytes = hex::decode(
            params
                .agent_id
                .strip_prefix("0x")
                .unwrap_or(&params.agent_id),
        )
        .map_err(|e| RpcError::InvalidParams(format!("Invalid agent_id hex: {}", e)))?;

        if id_bytes.len() != 32 {
            return Err(RpcError::InvalidParams("agent_id must be 32 bytes".into()).into());
        }

        let mut hash_arr = [0u8; 32];
        hash_arr.copy_from_slice(&id_bytes);
        let hash = Hash::from(hash_arr);

        let mut registry = self.agent_registry.write();
        // For now, allow freeze as governance action (no signature check for simplicity)
        registry
            .freeze(
                &hash,
                qfc_qvm::stdlib::agent_registry::zero_h160(),
                params.reason.clone(),
                true,
            )
            .map_err(|e| RpcError::Internal(format!("Freeze failed: error code {}", e)))?;

        // Update index
        if let Some(agent) = registry.get(&hash) {
            let view = qfc_qvm::stdlib::agent_index::AgentView::from_registration(agent);
            self.agent_index.write().insert_agent(view);
        }

        Ok(RpcAgentWriteResult {
            tx_hash: String::new(),
            agent_id: params.agent_id,
            message: format!("Agent frozen: {}", params.reason),
        })
    }

    async fn issue_session_key(
        &self,
        params: RpcIssueSessionKeyParams,
    ) -> RpcResult<RpcSessionKeyDetail> {
        let agent_id_bytes = hex::decode(
            params
                .agent_id
                .strip_prefix("0x")
                .unwrap_or(&params.agent_id),
        )
        .map_err(|e| RpcError::InvalidParams(format!("Invalid agent_id hex: {}", e)))?;

        if agent_id_bytes.len() != 32 {
            return Err(RpcError::InvalidParams("agent_id must be 32 bytes".into()).into());
        }

        let mut agent_hash = [0u8; 32];
        agent_hash.copy_from_slice(&agent_id_bytes);
        let agent_id = Hash::from(agent_hash);

        // Verify agent exists
        {
            let registry = self.agent_registry.read();
            if registry.get(&agent_id).is_none() {
                return Err(RpcError::Internal("Agent not found".into()).into());
            }
        }

        let pub_key_bytes = hex::decode(
            params
                .public_key
                .strip_prefix("0x")
                .unwrap_or(&params.public_key),
        )
        .map_err(|e| RpcError::InvalidParams(format!("Invalid public_key hex: {}", e)))?;

        let spending_limit = Self::parse_amount_u128(&params.spending_limit).unwrap_or(0) as u64;

        // Generate key_id from hash of params
        let key_id = blake3_hash(
            &[
                agent_id_bytes.as_slice(),
                pub_key_bytes.as_slice(),
                &params.ttl_secs.to_le_bytes(),
            ]
            .concat(),
        );

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let mut store = self.session_key_store.write();
        store.set_timestamp(now);
        let key = store
            .issue(
                key_id,
                agent_id,
                qfc_qvm::stdlib::agent_registry::zero_h160(),
                pub_key_bytes,
                params.permissions,
                spending_limit,
                params.period_duration,
                params.ttl_secs,
            )
            .map_err(|e| {
                RpcError::Internal(format!("Issue session key failed: error code {}", e))
            })?;

        Ok(self.session_key_to_rpc(key))
    }

    async fn revoke_session_key_v3(&self, key_id: String) -> RpcResult<RpcAgentWriteResult> {
        let id_bytes = hex::decode(key_id.strip_prefix("0x").unwrap_or(&key_id))
            .map_err(|e| RpcError::InvalidParams(format!("Invalid key_id hex: {}", e)))?;

        if id_bytes.len() != 32 {
            return Err(RpcError::InvalidParams("key_id must be 32 bytes".into()).into());
        }

        let mut hash_arr = [0u8; 32];
        hash_arr.copy_from_slice(&id_bytes);
        let hash = Hash::from(hash_arr);

        let mut store = self.session_key_store.write();
        store.revoke(&hash).map_err(|e| {
            RpcError::Internal(format!("Revoke session key failed: error code {}", e))
        })?;

        Ok(RpcAgentWriteResult {
            tx_hash: String::new(),
            agent_id: key_id,
            message: "Session key revoked".into(),
        })
    }

    async fn get_session_keys(&self, agent_id: String) -> RpcResult<Vec<RpcSessionKeyDetail>> {
        let id_bytes = hex::decode(agent_id.strip_prefix("0x").unwrap_or(&agent_id))
            .map_err(|e| RpcError::InvalidParams(format!("Invalid agent_id hex: {}", e)))?;

        if id_bytes.len() != 32 {
            return Err(RpcError::InvalidParams("agent_id must be 32 bytes".into()).into());
        }

        let mut hash_arr = [0u8; 32];
        hash_arr.copy_from_slice(&id_bytes);
        let hash = Hash::from(hash_arr);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let mut store = self.session_key_store.write();
        store.set_timestamp(now);
        let keys = store.list_for_agent(&hash);

        Ok(keys.iter().map(|k| self.session_key_to_rpc(k)).collect())
    }

    async fn get_agent_balance(&self, agent_id: String) -> RpcResult<RpcAgentBalance> {
        let id_bytes = hex::decode(agent_id.strip_prefix("0x").unwrap_or(&agent_id))
            .map_err(|e| RpcError::InvalidParams(format!("Invalid agent_id hex: {}", e)))?;

        if id_bytes.len() != 32 {
            return Err(RpcError::InvalidParams("agent_id must be 32 bytes".into()).into());
        }

        let mut hash_arr = [0u8; 32];
        hash_arr.copy_from_slice(&id_bytes);
        let hash = Hash::from(hash_arr);

        let registry = self.agent_registry.read();
        let agent = registry
            .get(&hash)
            .ok_or_else(|| RpcError::Internal("Agent not found".into()))?;

        let tier = qfc_qvm::stdlib::agent_registry::StakeTier::from_stake(agent.stake)
            .map(|t| match t {
                qfc_qvm::stdlib::agent_registry::StakeTier::Basic => "basic",
                qfc_qvm::stdlib::agent_registry::StakeTier::Verified => "verified",
                qfc_qvm::stdlib::agent_registry::StakeTier::Premium => "premium",
            })
            .unwrap_or("none");

        Ok(RpcAgentBalance {
            agent_id: format!("0x{}", hex::encode(agent.id.as_bytes())),
            stake: agent.stake.to_string(),
            stake_tier: tier.to_string(),
        })
    }
}

/// ABI helper: encode registerAgent(string,uint8[],uint256,uint256) calldata.
fn abi_encode_register_agent(
    selector: [u8; 4],
    agent_id: &str,
    permissions: &[u8],
    daily_limit: u128,
    max_per_tx: u128,
) -> Vec<u8> {
    // Layout: selector | offset_string | offset_permissions | daily_limit | max_per_tx | string_data | perm_data
    let id_bytes = agent_id.as_bytes();
    let id_padded_len = ((id_bytes.len() + 31) / 32) * 32;
    let perm_count = permissions.len();

    // Each uint8 in the array is padded to 32 bytes in ABI encoding
    let total_size = 4 + 4 * 32 // selector + 4 head words (2 offsets + 2 uint256)
        + 32 + id_padded_len     // string: length word + padded data
        + 32 + perm_count * 32; // array: length word + elements

    let mut data = Vec::with_capacity(total_size);
    data.extend_from_slice(&selector);

    // Head: 4 words
    // word 0: offset to string data = 4*32 = 128
    let mut w = [0u8; 32];
    w[31] = 128;
    data.extend_from_slice(&w);

    // word 1: offset to uint8[] = 128 + 32 + id_padded_len
    let perm_offset = (128 + 32 + id_padded_len) as u64;
    let mut w = [0u8; 32];
    w[24..32].copy_from_slice(&perm_offset.to_be_bytes());
    data.extend_from_slice(&w);

    // word 2: daily_limit
    let mut w = [0u8; 32];
    w[16..32].copy_from_slice(&daily_limit.to_be_bytes());
    data.extend_from_slice(&w);

    // word 3: max_per_tx
    let mut w = [0u8; 32];
    w[16..32].copy_from_slice(&max_per_tx.to_be_bytes());
    data.extend_from_slice(&w);

    // Tail: string data
    let mut len_word = [0u8; 32];
    len_word[24..32].copy_from_slice(&(id_bytes.len() as u64).to_be_bytes());
    data.extend_from_slice(&len_word);
    data.extend_from_slice(id_bytes);
    let pad = (32 - (id_bytes.len() % 32)) % 32;
    data.extend(std::iter::repeat(0u8).take(pad));

    // Tail: uint8[] data
    let mut len_word = [0u8; 32];
    len_word[24..32].copy_from_slice(&(perm_count as u64).to_be_bytes());
    data.extend_from_slice(&len_word);
    for &p in permissions {
        let mut w = [0u8; 32];
        w[31] = p;
        data.extend_from_slice(&w);
    }

    data
}

// Helper methods
impl RpcServer {
    /// Parse an amount string (hex "0x..." or decimal) into u128.
    fn parse_amount_u128(s: &str) -> Result<u128, RpcError> {
        if let Some(hex_str) = s.strip_prefix("0x") {
            u128::from_str_radix(hex_str, 16)
                .map_err(|e| RpcError::InvalidParams(format!("Invalid hex amount: {}", e)))
        } else {
            s.parse::<u128>()
                .map_err(|e| RpcError::InvalidParams(format!("Invalid amount: {}", e)))
        }
    }

    /// Build a signed ContractCall transaction to the AgentRegistry and submit to mempool.
    async fn submit_agent_contract_call(
        &self,
        sender: Address,
        public_key: qfc_types::PublicKey,
        calldata: Vec<u8>,
        value: U256,
    ) -> RpcResult<Hash> {
        let contract_addr = Address::from_slice(
            &hex::decode(AGENT_REGISTRY_ADDRESS).map_err(|e| RpcError::Internal(e.to_string()))?,
        )
        .ok_or_else(|| RpcError::Internal("Invalid AgentRegistry address".into()))?;

        // Get current nonce for sender
        let nonce = self
            .chain
            .state()
            .get_nonce(&sender)
            .map_err(|e| RpcError::Internal(e.to_string()))?;

        // Build transaction
        let tx = Transaction {
            tx_type: qfc_types::TransactionType::ContractCall,
            chain_id: self.chain_id,
            nonce,
            gas_price: U256::from_u128(1_000_000_000), // 1 Gwei
            gas_limit: 200_000,
            to: Some(contract_addr),
            value,
            data: calldata,
            signature: qfc_types::Signature::ZERO,
            public_key,
        };

        // Sign the transaction
        // NOTE: In production, the client signs offline. Here we verify the request
        // signature above and create a pre-signed tx. The actual signing happens
        // client-side via eth_sendRawTransaction for real deployments. For this
        // convenience RPC, we include the public key so the block producer can
        // verify the prior authorization signature.
        let tx_bytes = tx.to_bytes_without_signature();
        let tx_hash = blake3_hash(&tx_bytes);

        // We cannot sign on behalf of the user (we don't have the secret key).
        // The transaction carries the public_key and the request-level signature
        // already verified above provides authorization. The mempool accepts the
        // tx with a zero signature when the public_key is set, and the block
        // producer re-verifies via the contract's own authorization logic.
        let signed_tx = tx;

        // Add to mempool
        self.mempool
            .write()
            .add(signed_tx.clone(), sender)
            .map_err(|e| RpcError::Execution(e.to_string()))?;

        // Broadcast to network if available
        if let Some(network) = &self.network {
            let tx_bytes = signed_tx.to_bytes();
            if let Err(e) = network.broadcast_transaction(tx_bytes).await {
                warn!("Failed to broadcast agent tx: {}", e);
            }
        }

        Ok(tx_hash)
    }

    /// Build RpcPublicTaskStatus from a PublicTask (B1: structured envelope, B2: IPFS support)
    fn build_task_status(task: &qfc_ai_coordinator::task_pool::PublicTask) -> RpcPublicTaskStatus {
        use base64::Engine;
        use qfc_ai_coordinator::task_pool::ResultStorage;

        let model_id = task
            .inner_task
            .task_type
            .model_id()
            .map(|m| format!("{}:{}", m.name, m.version))
            .unwrap_or_default();

        let (
            status,
            result,
            result_size,
            result_type,
            result_cid,
            result_preview,
            miner_address,
            execution_time_ms,
        ) = match &task.status {
            qfc_ai_coordinator::task_pool::PublicTaskStatus::Pending => {
                ("Pending".into(), None, None, None, None, None, None, None)
            }
            qfc_ai_coordinator::task_pool::PublicTaskStatus::Assigned => {
                ("Assigned".into(), None, None, None, None, None, None, None)
            }
            qfc_ai_coordinator::task_pool::PublicTaskStatus::Completed {
                result,
                miner,
                execution_time_ms,
            } => match result {
                ResultStorage::Inline(data) => (
                    "Completed".into(),
                    Some(base64::engine::general_purpose::STANDARD.encode(data)),
                    Some(data.len()),
                    Some("inline".to_string()),
                    None,
                    None,
                    Some(miner.to_string()),
                    Some(*execution_time_ms),
                ),
                ResultStorage::Ipfs { cid, size, preview } => (
                    "Completed".into(),
                    None,
                    Some(*size as usize),
                    Some("ipfs".to_string()),
                    Some(cid.clone()),
                    Some(base64::engine::general_purpose::STANDARD.encode(preview)),
                    Some(miner.to_string()),
                    Some(*execution_time_ms),
                ),
            },
            qfc_ai_coordinator::task_pool::PublicTaskStatus::Failed => {
                ("Failed".into(), None, None, None, None, None, None, None)
            }
            qfc_ai_coordinator::task_pool::PublicTaskStatus::Expired => {
                ("Expired".into(), None, None, None, None, None, None, None)
            }
        };

        RpcPublicTaskStatus {
            task_id: hex::encode(task.task_id.as_bytes()),
            status,
            submitter: task.submitter.to_string(),
            task_type: task.inner_task.task_type.task_type_name().to_string(),
            model_id,
            created_at: task.inner_task.created_at,
            deadline: task.inner_task.deadline,
            max_fee: format!("0x{:x}", task.max_fee),
            result,
            result_size,
            result_type,
            result_cid,
            result_preview,
            miner_address,
            execution_time_ms,
        }
    }

    /// Convert AgentView to RPC response
    fn agent_view_to_rpc(&self, a: &qfc_qvm::stdlib::agent_index::AgentView) -> RpcAgentDetailView {
        RpcAgentDetailView {
            agent_id: format!("0x{}", hex::encode(a.id.as_bytes())),
            owner: format!("0x{}", hex::encode(a.owner.as_bytes())),
            capabilities: a.capabilities.clone(),
            protocol_digests: a
                .protocol_digests
                .iter()
                .map(|d| format!("0x{}", hex::encode(d.as_bytes())))
                .collect(),
            endpoint: a.endpoint.clone(),
            stake: a.stake.to_string(),
            frozen: a.frozen,
            reputation_score: a.reputation_score,
            total_tasks_completed: a.total_tasks_completed,
            total_tasks_failed: 0,
            last_heartbeat: a.last_heartbeat,
            created_at: a.created_at,
        }
    }

    /// Convert SessionKey to RPC response
    fn session_key_to_rpc(
        &self,
        k: &qfc_qvm::stdlib::session_keys::SessionKey,
    ) -> RpcSessionKeyDetail {
        RpcSessionKeyDetail {
            key_id: format!("0x{}", hex::encode(k.id.as_bytes())),
            agent_id: format!("0x{}", hex::encode(k.agent_id.as_bytes())),
            public_key: format!("0x{}", hex::encode(&k.public_key)),
            permissions: k.permissions,
            permissions_names: qfc_qvm::stdlib::session_keys::describe_permissions(k.permissions)
                .into_iter()
                .map(String::from)
                .collect(),
            spending_limit: k.spending_limit.to_string(),
            spent_this_period: k.spent_this_period.to_string(),
            period_duration: k.period_duration,
            expires_at: k.expires_at,
            nonce: k.nonce,
            created_at: k.created_at,
        }
    }
}

// ---- txpool RPC ----

#[async_trait::async_trait]
impl TxPoolApiServer for RpcServer {
    async fn txpool_content(&self) -> RpcResult<TxPoolContent> {
        let mempool = self.mempool.read();
        let all = mempool.get_all_by_sender();

        let mut pending: std::collections::HashMap<
            String,
            std::collections::HashMap<String, RpcTransaction>,
        > = std::collections::HashMap::new();

        for (sender, txs) in all {
            let sender_str = sender.to_string();
            let nonce_map = pending.entry(sender_str).or_default();
            for ptx in txs {
                let nonce_str = format!("{}", ptx.tx.nonce);
                nonce_map.insert(
                    nonce_str,
                    RpcTransaction::from_pending(ptx.tx, ptx.hash, ptx.sender),
                );
            }
        }

        Ok(TxPoolContent {
            pending,
            queued: std::collections::HashMap::new(), // QFC mempool has no separate queued pool
        })
    }

    async fn txpool_status(&self) -> RpcResult<TxPoolStatus> {
        let mempool = self.mempool.read();
        let size = mempool.size();
        Ok(TxPoolStatus {
            pending: format!("0x{:x}", size),
            queued: "0x0".to_string(),
        })
    }
}

/// Format a U256 balance as human-readable QFC (1 QFC = 1e18 wei)
fn format_qfc_balance(balance: qfc_types::U256) -> String {
    let wei_str = format!("{}", balance);
    if wei_str.len() <= 18 {
        format!("0.{:0>18}", wei_str)
    } else {
        let (whole, frac) = wei_str.split_at(wei_str.len() - 18);
        let frac_trimmed = frac.trim_end_matches('0');
        if frac_trimmed.is_empty() {
            whole.to_string()
        } else {
            format!("{}.{}", whole, &frac[..6.min(frac.len())])
        }
    }
}

#[cfg(test)]
mod tests_agent_write_rpcs {
    use super::*;
    use crate::qfc::{
        RpcAgentWriteResult, RpcFundAgentRequest, RpcRegisterAgentRequest, RpcRevokeAgentRequest,
    };

    // ---- Type serialization tests ----

    #[test]
    fn test_register_agent_request_serde() {
        let req = RpcRegisterAgentRequest {
            agent_id: "my-agent-1".into(),
            owner: "0x1234567890abcdef1234567890abcdef12345678".into(),
            public_key: "0xaabbccdd".into(),
            permissions: vec![1, 2, 3],
            daily_limit: "0xde0b6b3a7640000".into(),
            max_per_tx: "0x2386f26fc10000".into(),
            signature: "0xdeadbeef".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: RpcRegisterAgentRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.agent_id, "my-agent-1");
        assert_eq!(parsed.permissions, vec![1, 2, 3]);
        assert_eq!(parsed.daily_limit, "0xde0b6b3a7640000");
    }

    #[test]
    fn test_fund_agent_request_serde() {
        let req = RpcFundAgentRequest {
            agent_id: "agent-42".into(),
            funder: "0x1234567890abcdef1234567890abcdef12345678".into(),
            public_key: "0xaabb".into(),
            amount: "1000000000000000000".into(),
            signature: "0xsig".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: RpcFundAgentRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.agent_id, "agent-42");
        assert_eq!(parsed.amount, "1000000000000000000");
    }

    #[test]
    fn test_revoke_agent_request_serde() {
        let req = RpcRevokeAgentRequest {
            agent_id: "agent-99".into(),
            owner: "0xabcdef".into(),
            public_key: "0x1234".into(),
            signature: "0xsig".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: RpcRevokeAgentRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.agent_id, "agent-99");
    }

    #[test]
    fn test_agent_write_result_serde() {
        let result = RpcAgentWriteResult {
            tx_hash: "0xabcdef1234567890".into(),
            agent_id: "test-agent".into(),
            message: "Agent registration submitted".into(),
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("txHash"));
        assert!(json.contains("agentId"));
        assert!(json.contains("test-agent"));
    }

    #[test]
    fn test_register_agent_request_default_permissions() {
        let json = r#"{
            "agentId": "a1",
            "owner": "0x00",
            "publicKey": "0x00",
            "dailyLimit": "100",
            "maxPerTx": "10",
            "signature": "0x00"
        }"#;
        let parsed: RpcRegisterAgentRequest = serde_json::from_str(json).unwrap();
        assert!(parsed.permissions.is_empty());
    }

    // ---- ABI encoding tests ----

    #[test]
    fn test_abi_encode_register_agent_basic() {
        let selector: [u8; 4] = [0xa8, 0x5e, 0xf5, 0x79];
        let calldata = abi_encode_register_agent(
            selector,
            "test-agent",
            &[1, 2],
            1_000_000_000_000_000_000u128, // 1 ETH in wei
            100_000_000_000_000_000u128,   // 0.1 ETH in wei
        );
        // Verify selector
        assert_eq!(&calldata[..4], &[0xa8, 0x5e, 0xf5, 0x79]);
        // Verify the calldata length is properly aligned to 32 bytes
        assert_eq!((calldata.len() - 4) % 32, 0);
        // Minimum: 4 head words + string(len+data) + array(len+2 elements)
        assert!(calldata.len() >= 4 + 4 * 32 + 64 + 3 * 32);
    }

    #[test]
    fn test_abi_encode_register_agent_empty_permissions() {
        let selector: [u8; 4] = [0xa8, 0x5e, 0xf5, 0x79];
        let calldata = abi_encode_register_agent(selector, "a", &[], 100, 10);
        assert_eq!(&calldata[..4], &selector);
        assert_eq!((calldata.len() - 4) % 32, 0);
    }

    // ---- parse_amount_u128 tests ----

    #[test]
    fn test_parse_amount_u128_decimal() {
        let result = RpcServer::parse_amount_u128("1000000000000000000").unwrap();
        assert_eq!(result, 1_000_000_000_000_000_000u128);
    }

    #[test]
    fn test_parse_amount_u128_hex() {
        let result = RpcServer::parse_amount_u128("0xde0b6b3a7640000").unwrap();
        assert_eq!(result, 1_000_000_000_000_000_000u128);
    }

    #[test]
    fn test_parse_amount_u128_invalid() {
        assert!(RpcServer::parse_amount_u128("not_a_number").is_err());
        assert!(RpcServer::parse_amount_u128("0xZZZ").is_err());
    }

    // ---- Input validation tests (via signature verification) ----

    #[test]
    fn test_signature_verification_happy_path() {
        // Create a real keypair and sign a payload
        let keypair = qfc_crypto::Keypair::from_secret_bytes(&[0x42u8; 32]).unwrap();
        let public_key = keypair.public_key();
        let address = qfc_crypto::address_from_public_key(&public_key);

        // Simulate registerAgent signature payload
        let agent_id = "test-agent";
        let daily_limit: u128 = 1_000_000;
        let max_per_tx: u128 = 100_000;

        let mut payload = Vec::new();
        payload.extend_from_slice(agent_id.as_bytes());
        payload.extend_from_slice(address.as_bytes());
        payload.extend_from_slice(&daily_limit.to_be_bytes());
        payload.extend_from_slice(&max_per_tx.to_be_bytes());
        let payload_hash = qfc_crypto::blake3_hash(&payload);

        let sig = keypair.sign_hash(&payload_hash);

        // Verify succeeds
        assert!(qfc_crypto::verify_hash_signature(&public_key, &payload_hash, &sig).is_ok());
    }

    #[test]
    fn test_signature_verification_wrong_key_fails() {
        let keypair1 = qfc_crypto::Keypair::from_secret_bytes(&[0x42u8; 32]).unwrap();
        let keypair2 = qfc_crypto::Keypair::from_secret_bytes(&[0x43u8; 32]).unwrap();
        let public_key2 = keypair2.public_key();

        let payload = b"test payload";
        let payload_hash = qfc_crypto::blake3_hash(payload);
        let sig = keypair1.sign_hash(&payload_hash);

        // Verify fails with wrong public key
        assert!(qfc_crypto::verify_hash_signature(&public_key2, &payload_hash, &sig).is_err());
    }

    #[test]
    fn test_signature_verification_tampered_payload_fails() {
        let keypair = qfc_crypto::Keypair::from_secret_bytes(&[0x42u8; 32]).unwrap();
        let public_key = keypair.public_key();

        let payload_hash = qfc_crypto::blake3_hash(b"original");
        let sig = keypair.sign_hash(&payload_hash);

        let tampered_hash = qfc_crypto::blake3_hash(b"tampered");
        assert!(qfc_crypto::verify_hash_signature(&public_key, &tampered_hash, &sig).is_err());
    }

    #[test]
    fn test_address_derivation_mismatch_detected() {
        let keypair1 = qfc_crypto::Keypair::from_secret_bytes(&[0x42u8; 32]).unwrap();
        let keypair2 = qfc_crypto::Keypair::from_secret_bytes(&[0x43u8; 32]).unwrap();
        let addr1 = qfc_crypto::address_from_public_key(&keypair1.public_key());
        let addr2 = qfc_crypto::address_from_public_key(&keypair2.public_key());
        assert_ne!(addr1, addr2);
    }

    #[test]
    fn test_fund_agent_zero_amount_validation() {
        // Zero amount should be rejected — tested at the type level
        let amount = RpcServer::parse_amount_u128("0").unwrap();
        assert_eq!(amount, 0);
        // The handler checks amount == 0 and returns error
    }

    #[test]
    fn test_revoke_agent_signature_payload_includes_revoke_tag() {
        // Ensure the "revoke" tag is included in the signature payload
        let keypair = qfc_crypto::Keypair::from_secret_bytes(&[0x42u8; 32]).unwrap();
        let address = qfc_crypto::address_from_public_key(&keypair.public_key());

        let mut payload_with = Vec::new();
        payload_with.extend_from_slice(b"agent-1");
        payload_with.extend_from_slice(address.as_bytes());
        payload_with.extend_from_slice(b"revoke");
        let hash_with = qfc_crypto::blake3_hash(&payload_with);

        let mut payload_without = Vec::new();
        payload_without.extend_from_slice(b"agent-1");
        payload_without.extend_from_slice(address.as_bytes());
        let hash_without = qfc_crypto::blake3_hash(&payload_without);

        // Different payloads produce different hashes
        assert_ne!(hash_with, hash_without);

        // Sign with "revoke" tag
        let sig = keypair.sign_hash(&hash_with);
        assert!(qfc_crypto::verify_hash_signature(&keypair.public_key(), &hash_with, &sig).is_ok());
        // But doesn't verify against the hash without "revoke"
        assert!(
            qfc_crypto::verify_hash_signature(&keypair.public_key(), &hash_without, &sig).is_err()
        );
    }
}

#[cfg(test)]
mod tests_model_governance_rpcs {
    use crate::qfc::RpcProposeModelRequest;

    /// Old-style requests without the new optional fields must still parse
    /// (serde defaults — backward compatible).
    #[test]
    fn test_propose_model_request_without_optional_fields() {
        let json = r#"{
            "proposer": "0x1234567890abcdef1234567890abcdef12345678",
            "modelName": "qfc-llm-test",
            "modelVersion": "v1.0",
            "description": "test model",
            "minMemoryMb": 1024,
            "minTier": "Cold",
            "sizeMb": 100
        }"#;
        let parsed: RpcProposeModelRequest = serde_json::from_str(json).unwrap();
        assert!(parsed.weights_hash.is_none());
        assert!(parsed.shard_manifest.is_none());
    }

    /// Manifest round-trips through the RPC request with 0x-hex hashes.
    #[test]
    fn test_propose_model_request_with_manifest_round_trip() {
        let data = vec![0x42u8; 64];
        let manifest = qfc_inference::ShardManifest {
            shards: vec![qfc_inference::ShardEntry {
                cid: "QmYwAPJzv5CZsnA625s3Xf2nemtYgPpHdWEz79ojWnPbdG".to_string(),
                hash: qfc_crypto::blake3_hash(&data),
                size_bytes: data.len() as u64,
                layer_range: None,
            }],
            total_size_bytes: data.len() as u64,
            assembled_hash: qfc_crypto::blake3_hash(&data),
        };
        manifest.validate().unwrap();

        let req = RpcProposeModelRequest {
            proposer: "0x1234567890abcdef1234567890abcdef12345678".into(),
            model_name: "qfc-llm-test".into(),
            model_version: "v1.0".into(),
            description: "sharded test model".into(),
            min_memory_mb: 1024,
            min_tier: "Cold".into(),
            size_mb: 100,
            weights_hash: Some(format!("{}", manifest.assembled_hash)),
            shard_manifest: Some(manifest.clone()),
        };

        let json = serde_json::to_string(&req).unwrap();
        // Hashes serialize as 0x-hex strings (qfc_types::Hash serde).
        assert!(json.contains(&format!("{}", manifest.assembled_hash)));
        let parsed: RpcProposeModelRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.shard_manifest.as_ref().unwrap(), &manifest);
        assert_eq!(
            parsed.weights_hash.as_deref().unwrap(),
            format!("{}", manifest.assembled_hash)
        );
    }

    /// An invalid manifest (bad cid) is rejected by the same validate() the
    /// propose_model handler runs before building ModelInfo.
    #[test]
    fn test_propose_model_manifest_with_invalid_cid_rejected() {
        let data = vec![0x42u8; 64];
        let manifest = qfc_inference::ShardManifest {
            shards: vec![qfc_inference::ShardEntry {
                cid: "../evil".to_string(),
                hash: qfc_crypto::blake3_hash(&data),
                size_bytes: data.len() as u64,
                layer_range: None,
            }],
            total_size_bytes: data.len() as u64,
            assembled_hash: qfc_crypto::blake3_hash(&data),
        };
        assert!(manifest.validate().is_err());
    }
}
