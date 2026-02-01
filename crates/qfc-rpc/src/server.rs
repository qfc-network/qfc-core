//! RPC server implementation

use crate::error::RpcError;
use crate::eth::EthApiServer;
use crate::qfc::{QfcApiServer, RpcEpoch, RpcNodeInfo, RpcValidator};
use crate::types::{BlockNumber, BlockTag, CallRequest, RpcBlock, RpcReceipt, RpcTransaction};
use jsonrpsee::core::RpcResult;
use jsonrpsee::server::{ServerBuilder, ServerHandle};
use parking_lot::RwLock;
use qfc_chain::Chain;
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

impl Default for RpcConfig {
    fn default() -> Self {
        Self {
            http_addr: "127.0.0.1:8545".parse().unwrap(),
            http_enabled: true,
        }
    }
}

/// RPC server
#[derive(Clone)]
pub struct RpcServer {
    /// Chain
    chain: Arc<Chain>,
    /// Mempool
    mempool: Arc<RwLock<Mempool>>,
    /// Network service (optional, for broadcasting)
    network: Option<Arc<NetworkService>>,
    /// Chain ID
    chain_id: u64,
}

impl RpcServer {
    /// Create a new RPC server
    pub fn new(chain: Arc<Chain>, mempool: Arc<RwLock<Mempool>>, chain_id: u64) -> Self {
        Self {
            chain,
            mempool,
            network: None,
            chain_id,
        }
    }

    /// Set the network service for transaction broadcasting
    pub fn with_network(mut self, network: Arc<NetworkService>) -> Self {
        self.network = Some(network);
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

        let receipt = self
            .chain
            .get_receipt(&hash)
            .map_err(|e| RpcError::Internal(e.to_string()))?;

        match receipt {
            Some(r) => {
                // TODO: Get actual from/to
                let from = Address::ZERO;
                let to = None;
                Ok(Some(RpcReceipt::from_receipt(r, from, to)))
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

        // Derive sender from signature (placeholder)
        let sender_hash = blake3_hash(tx.signature.as_bytes());
        let sender = Address::from_slice(&sender_hash.as_bytes()[12..32]).unwrap();

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
        // TODO: Implement
        Ok(Vec::new())
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
        // TODO: Implement
        Ok(RpcEpoch {
            number: "0x0".to_string(),
            start_time: "0x0".to_string(),
            duration_ms: "0x2710".to_string(), // 10000ms
        })
    }

    async fn get_finalized_block(&self) -> RpcResult<String> {
        Ok(format!("0x{:x}", self.chain.block_number()))
    }

    async fn node_info(&self) -> RpcResult<RpcNodeInfo> {
        Ok(RpcNodeInfo {
            version: "0.1.0".to_string(),
            chain_id: format!("0x{:x}", self.chain_id),
            peer_count: 0,
            is_validator: false,
            syncing: false,
        })
    }
}
