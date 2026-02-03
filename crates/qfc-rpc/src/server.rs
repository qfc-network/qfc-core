//! RPC server implementation

use crate::error::RpcError;
use crate::eth::EthApiServer;
use crate::qfc::{
    QfcApiServer, RpcEpoch, RpcFaucetResponse, RpcNodeInfo, RpcValidator, RpcValidatorMetrics,
    RpcValidatorScoreBreakdown,
};
use crate::types::{BlockNumber, BlockTag, CallRequest, RpcBlock, RpcReceipt, RpcTransaction};
use jsonrpsee::core::RpcResult;
use jsonrpsee::server::{ServerBuilder, ServerHandle};
use parking_lot::RwLock;
use qfc_chain::Chain;
use qfc_consensus::NetworkState;
use qfc_crypto::blake3_hash;
use qfc_mempool::Mempool;
use qfc_network::NetworkService;
use qfc_types::{Address, Hash, Transaction, U256};
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
}

impl Clone for RpcServer {
    fn clone(&self) -> Self {
        Self {
            chain: self.chain.clone(),
            mempool: self.mempool.clone(),
            network: self.network.clone(),
            sync_status: self.sync_status.clone(),
            chain_id: self.chain_id,
        }
    }
}

impl RpcServer {
    /// Create a new RPC server
    pub fn new(chain: Arc<Chain>, mempool: Arc<RwLock<Mempool>>, chain_id: u64) -> Self {
        Self {
            chain,
            mempool,
            network: None,
            sync_status: None,
            chain_id,
        }
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

    /// Start the RPC server
    pub async fn start(self, config: RpcConfig) -> Result<ServerHandle, Box<dyn std::error::Error + Send + Sync>> {
        if !config.http_enabled {
            return Err("HTTP not enabled".into());
        }

        info!("Starting RPC server on {}", config.http_addr);

        let server = ServerBuilder::default()
            .build(config.http_addr)
            .await?;

        // Merge both RPC modules
        let mut eth_module = EthApiServer::into_rpc(self.clone());
        let qfc_module = QfcApiServer::into_rpc(self);
        eth_module.merge(qfc_module).expect("Failed to merge RPC modules");

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
        let hash = Self::parse_hash(&hash)?;

        // Check mempool first
        if let Some(pooled) = self.mempool.read().get(&hash) {
            let sender_hash = blake3_hash(pooled.tx.signature.as_bytes());
            let sender = Address::from_slice(&sender_hash.as_bytes()[12..32]).unwrap();
            return Ok(Some(RpcTransaction::from_pending(pooled.tx, hash, sender)));
        }

        // Check chain
        let tx = self
            .chain
            .get_transaction(&hash)
            .map_err(|e| RpcError::Internal(e.to_string()))?;

        match tx {
            Some(t) => {
                let sender_hash = blake3_hash(t.signature.as_bytes());
                let sender = Address::from_slice(&sender_hash.as_bytes()[12..32]).unwrap();
                Ok(Some(RpcTransaction::from_pending(t, hash, sender)))
            }
            None => Ok(None),
        }
    }

    async fn get_transaction_receipt(&self, hash: String) -> RpcResult<Option<RpcReceipt>> {
        let hash = Self::parse_hash(&hash)?;

        // Get receipt with block info
        let result = self
            .chain
            .get_receipt_with_block_info(&hash)
            .map_err(|e| RpcError::Internal(e.to_string()))?;

        match result {
            Some((receipt, block_hash, block_number)) => {
                // Get transaction to extract from/to
                let tx = self
                    .chain
                    .get_transaction(&hash)
                    .map_err(|e| RpcError::Internal(e.to_string()))?;

                let (from, to) = if let Some(tx) = tx {
                    // Derive sender from public key
                    let from = qfc_crypto::address_from_public_key(&tx.public_key);
                    (from, tx.to)
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

        let tx =
            Transaction::from_bytes(&bytes).map_err(|e| RpcError::InvalidParams(e.to_string()))?;

        let hash = blake3_hash(&tx.to_bytes_without_signature());

        // Derive sender from public key (Ed25519)
        let sender = qfc_crypto::address_from_public_key(&tx.public_key);

        // Add to mempool
        self.mempool
            .write()
            .add(tx.clone(), sender)
            .map_err(|e| RpcError::Execution(e.to_string()))?;

        info!("Added transaction {} to mempool from {}", hash, sender);

        // Broadcast to network if available
        if let Some(network) = &self.network {
            let tx_bytes = tx.to_bytes();
            if let Err(e) = network.broadcast_transaction(tx_bytes).await {
                warn!("Failed to broadcast transaction: {}", e);
            } else {
                debug!("Broadcast transaction {} to network", hash);
            }
        }

        Ok(hash.to_string())
    }

    async fn call(&self, request: CallRequest, _block: Option<BlockNumber>) -> RpcResult<String> {
        // Parse from address
        let from = if let Some(ref from_str) = request.from {
            let from_str = from_str.strip_prefix("0x").unwrap_or(from_str);
            let bytes = hex::decode(from_str).map_err(|e| RpcError::InvalidParams(e.to_string()))?;
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
            let bytes = hex::decode(from_str).map_err(|e| RpcError::InvalidParams(e.to_string()))?;
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

                RpcValidator {
                    address: v.address.to_string(),
                    stake: format!("0x{:x}", stake.0),
                    contribution_score: format!("0x{:x}", score),
                    uptime: format!("0x{:x}", v.uptime),
                    is_active: v.is_active(),
                    provides_compute: v.provides_compute,
                    hashrate: v.hashrate.to_string(),
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
        let total_storage: u64 = validators.iter().map(|v| v.storage_provided_gb as u64).sum();

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

    async fn request_faucet(&self, address: String, amount: String) -> RpcResult<RpcFaucetResponse> {
        // Only allow in dev mode (chain_id 9000)
        if self.chain_id != 9000 {
            return Err(RpcError::Execution("Faucet only available in dev mode".to_string()).into());
        }

        let to_address = Self::parse_address(&address)?;

        // Parse amount (in wei)
        let amount_str = amount.strip_prefix("0x").unwrap_or(&amount);
        let amount_value = u128::from_str_radix(amount_str, 16)
            .or_else(|_| amount_str.parse::<u128>())
            .map_err(|e| RpcError::InvalidParams(format!("Invalid amount: {}", e)))?;

        // Faucet uses dev validator key [0x42; 32]
        // Ed25519 address: 0x10d7812fbe50096ae82569fdad35f79628bc0084
        let faucet_secret_key = [0x42u8; 32];
        let faucet_keypair = qfc_crypto::Keypair::from_secret_bytes(&faucet_secret_key)
            .map_err(|e| RpcError::Internal(format!("Failed to create faucet keypair: {}", e)))?;
        let faucet_public_key = faucet_keypair.public_key();
        let faucet_address = qfc_crypto::address_from_public_key(&faucet_public_key);

        // Get current nonce for faucet address
        let nonce = self.chain.state()
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

        let signed_tx = Transaction {
            signature,
            ..tx
        };

        // tx_hash is already computed above

        // Add to mempool
        self.mempool
            .write()
            .add(signed_tx.clone(), faucet_address)
            .map_err(|e| RpcError::Execution(e.to_string()))?;

        info!("Faucet: sent {} wei to {} (tx: {})", amount_value, to_address, tx_hash);

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
}
