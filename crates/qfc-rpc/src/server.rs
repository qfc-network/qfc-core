//! RPC server implementation

use crate::error::RpcError;
use crate::eth::EthApiServer;
use crate::qfc::{
    QfcApiServer, RpcComputeInfo, RpcEpoch, RpcFaucetResponse, RpcInferenceProofSubmission,
    RpcInferenceStats, RpcInferenceTask, RpcMinerStatusReport, RpcModel, RpcModelProposal,
    RpcNodeInfo, RpcProofResult, RpcProposeModelRequest, RpcPublicTaskStatus,
    RpcRegisterMinerRequest, RpcRegisterMinerResult, RpcSubmitPublicTask, RpcTaskRequest,
    RpcValidator, RpcValidatorMetrics, RpcValidatorScoreBreakdown, RpcVoteModelRequest,
};
use crate::types::{BlockNumber, BlockTag, CallRequest, RpcBlock, RpcReceipt, RpcTransaction};
use jsonrpsee::core::RpcResult;
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
            inference_engine: self.inference_engine.clone(),
            proof_pool: self.proof_pool.clone(),
            challenge_generator: self.challenge_generator.clone(),
            redundant_verifier: self.redundant_verifier.clone(),
            task_router: self.task_router.clone(),
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
            inference_engine: None,
            proof_pool: None,
            challenge_generator: None,
            redundant_verifier: None,
            task_router: None,
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

        // Merge both RPC modules
        let mut eth_module = EthApiServer::into_rpc(self.clone());
        let qfc_module = QfcApiServer::into_rpc(self);
        eth_module
            .merge(qfc_module)
            .expect("Failed to merge RPC modules");

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

            // Add to mempool
            self.mempool
                .write()
                .add(tx.clone(), sender)
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

        // Add to mempool
        self.mempool
            .write()
            .add(qfc_tx.clone(), sender)
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
                    // Return the error message if available
                    if output.is_empty() {
                        Ok("0x".to_string())
                    } else {
                        Ok(format!("0x{}", hex::encode(&output)))
                    }
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

        // Verify signature
        let consensus = self.chain.consensus();
        let validators = consensus.get_validators();
        let validator = match validators.iter().find(|v| v.address == miner_address) {
            Some(v) => v,
            None => {
                return Ok(RpcRegisterMinerResult {
                    registered: false,
                    assigned_tier: 0,
                    message: "Unknown validator address".to_string(),
                });
            }
        };

        let sig_payload = format!(
            "{}{}{}",
            req.miner_address, req.gpu_model, req.benchmark_score
        );
        let sig_hash = blake3_hash(sig_payload.as_bytes());
        let sig_bytes = hex::decode(req.signature.strip_prefix("0x").unwrap_or(&req.signature))
            .map_err(|e| RpcError::InvalidParams(format!("Invalid signature hex: {}", e)))?;
        let signature = qfc_types::Signature::from_slice(&sig_bytes)
            .ok_or_else(|| RpcError::InvalidParams("Invalid signature length".into()))?;

        if verify_hash_signature(&validator.public_key, &sig_hash, &signature).is_err() {
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
            "CPU" => Some(qfc_types::BackendType::Cpu),
            _ => None,
        };

        // Update validator state
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
                gpu_tier: "unknown".to_string(), // TODO: derive from hardware
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

        Ok(RpcInferenceStats {
            tasks_completed: total_tasks.to_string(),
            avg_time_ms: "0".to_string(), // TODO: track average
            flops_total: "0".to_string(), // TODO: accumulate
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
            let epoch_seed =
                qfc_crypto::blake3_hash(now.to_le_bytes().as_ref()).as_bytes()[0] as u64;
            pool.generate_synthetic_tasks(now / 10_000, epoch_seed, now + 30_000);
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

        // 2. Find the validator
        let validators = consensus.get_validators();
        let validator = match validators.iter().find(|v| v.address == proof.validator) {
            Some(v) => v,
            None => {
                return Ok(RpcProofResult {
                    accepted: false,
                    spot_checked: false,
                    message: "Unknown validator".to_string(),
                });
            }
        };

        // 3. Check if validator is active
        if !validator.is_active() {
            return Ok(RpcProofResult {
                accepted: false,
                spot_checked: false,
                message: "Validator is inactive or jailed".to_string(),
            });
        }

        // 4. Verify the proof signature
        let proof_hash = blake3_hash(&proof.to_bytes_without_signature());
        if verify_hash_signature(&validator.public_key, &proof_hash, &proof.signature).is_err() {
            warn!("Invalid inference proof signature from {}", proof.validator);
            return Ok(RpcProofResult {
                accepted: false,
                spot_checked: false,
                message: "Invalid proof signature".to_string(),
            });
        }

        // 5. Basic verification (epoch, model, FLOPS)
        let current_epoch = consensus.get_epoch();
        if let Err(e) =
            qfc_ai_coordinator::verify_basic(&proof, current_epoch.number, &self.model_registry)
        {
            warn!("Proof rejected from {}: {}", submission.miner_address, e);
            return Ok(RpcProofResult {
                accepted: false,
                spot_checked: false,
                message: format!("Proof rejected: {}", e),
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
                            return Ok(RpcProofResult {
                                accepted: false,
                                spot_checked: true,
                                message: "Proof rejected: spot-check failed (output hash mismatch)"
                                    .to_string(),
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
                        });
                    }
                } else {
                    return Ok(RpcProofResult {
                        accepted: true,
                        spot_checked: false,
                        message: "Redundant verification: waiting for more submissions".to_string(),
                    });
                }
            }
        }

        // 8. Proof passed — update inference score
        consensus.update_inference_score(&proof.validator, proof.flops_estimated, 1);

        // 9. Push to proof pool for block inclusion (v2.0)
        // Convert qfc_inference::InferenceProof → qfc_types::InferenceProof via borsh roundtrip
        let types_proof: qfc_types::InferenceProof =
            borsh::from_slice(&borsh::to_vec(&proof).unwrap()).unwrap();
        if let Some(ref pool) = self.proof_pool {
            pool.write().add(types_proof);
        }

        // 10. Check if this proof completes a public task (v2.0)
        {
            let mut task_pool = self.task_pool.write();
            if task_pool.get_public_task(&proof.input_hash).is_some() {
                let result_data = submission
                    .result_data
                    .as_ref()
                    .and_then(|s| hex::decode(s.strip_prefix("0x").unwrap_or(s)).ok())
                    .unwrap_or_else(|| proof.output_hash.as_bytes().to_vec());
                task_pool.complete_public_task(
                    &proof.input_hash,
                    result_data,
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

        Ok(RpcProofResult {
            accepted: true,
            spot_checked,
            message: if spot_checked {
                "Proof accepted, spot-check passed".to_string()
            } else {
                "Proof accepted".to_string()
            },
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

        let model_info = qfc_inference::model::ModelInfo {
            id: qfc_inference::task::ModelId::new(&request.model_name, &request.model_version),
            description: request.description,
            min_memory_mb: request.min_memory_mb,
            min_tier,
            size_mb: request.size_mb,
            approved: false,
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

        let task_type = match request.task_type.as_str() {
            "embedding" => qfc_inference::task::ComputeTaskType::Embedding {
                model_id,
                input_hash: qfc_crypto::blake3_hash(&input_data),
            },
            _ => qfc_inference::task::ComputeTaskType::Embedding {
                model_id,
                input_hash: qfc_crypto::blake3_hash(&input_data),
            },
        };

        let mut pool = self.task_pool.write();
        let task_id = {
            let mut data = Vec::with_capacity(16);
            data.extend_from_slice(&now.to_le_bytes());
            data.extend_from_slice(&(pool.pending_count() as u64).to_le_bytes());
            qfc_crypto::blake3_hash(&data)
        };

        let task = qfc_inference::InferenceTask::new(
            task_id,
            now / 10_000,
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

        match pool.get_public_task(&task_hash) {
            Some(task) => {
                let (status, result_data, miner_address, execution_time_ms, fee) =
                    match &task.status {
                        qfc_ai_coordinator::task_pool::PublicTaskStatus::Pending => {
                            ("Pending".to_string(), None, None, None, None)
                        }
                        qfc_ai_coordinator::task_pool::PublicTaskStatus::Assigned => {
                            ("Assigned".to_string(), None, None, None, None)
                        }
                        qfc_ai_coordinator::task_pool::PublicTaskStatus::Completed {
                            result_data,
                            miner,
                            execution_time_ms,
                        } => (
                            "Completed".to_string(),
                            Some(hex::encode(result_data)),
                            Some(miner.to_string()),
                            Some(*execution_time_ms),
                            Some(format!("0x{:x}", task.max_fee)),
                        ),
                        qfc_ai_coordinator::task_pool::PublicTaskStatus::Failed => {
                            ("Failed".to_string(), None, None, None, None)
                        }
                        qfc_ai_coordinator::task_pool::PublicTaskStatus::Expired => {
                            ("Expired".to_string(), None, None, None, None)
                        }
                    };

                Ok(RpcPublicTaskStatus {
                    task_id: hex::encode(task.task_id.as_bytes()),
                    status,
                    result_data,
                    miner_address,
                    execution_time_ms,
                    fee,
                })
            }
            None => Err(RpcError::InvalidParams("Task not found".to_string()).into()),
        }
    }
}
