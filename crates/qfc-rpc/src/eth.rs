//! Ethereum-compatible RPC methods

use crate::types::{
    BlockNumber, CallRequest, LogFilter, RpcBlock, RpcFeeHistory, RpcLog, RpcReceipt,
    RpcTransaction,
};
use jsonrpsee::core::{RpcResult, SubscriptionResult};
use jsonrpsee::proc_macros::rpc;
use serde::{Deserialize, Serialize};

/// Subscription event for newHeads
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewHeadNotification {
    pub number: String,
    pub hash: String,
    pub parent_hash: String,
    pub timestamp: String,
    pub state_root: String,
    pub transactions_root: String,
    pub gas_used: String,
    pub gas_limit: String,
}

/// Ethereum RPC API trait
#[rpc(server, namespace = "eth")]
pub trait EthApi {
    /// Returns the chain ID
    #[method(name = "chainId")]
    async fn chain_id(&self) -> RpcResult<String>;

    /// Returns the current block number
    #[method(name = "blockNumber")]
    async fn block_number(&self) -> RpcResult<String>;

    /// Returns the balance of the account
    #[method(name = "getBalance")]
    async fn get_balance(&self, address: String, block: Option<BlockNumber>) -> RpcResult<String>;

    /// Returns the number of transactions sent from an address
    #[method(name = "getTransactionCount")]
    async fn get_transaction_count(
        &self,
        address: String,
        block: Option<BlockNumber>,
    ) -> RpcResult<String>;

    /// Returns code at a given address
    #[method(name = "getCode")]
    async fn get_code(&self, address: String, block: Option<BlockNumber>) -> RpcResult<String>;

    /// Returns block by number
    #[method(name = "getBlockByNumber")]
    async fn get_block_by_number(
        &self,
        block: BlockNumber,
        full_tx: bool,
    ) -> RpcResult<Option<RpcBlock>>;

    /// Returns block by hash
    #[method(name = "getBlockByHash")]
    async fn get_block_by_hash(&self, hash: String, full_tx: bool) -> RpcResult<Option<RpcBlock>>;

    /// Returns transaction by hash
    #[method(name = "getTransactionByHash")]
    async fn get_transaction_by_hash(&self, hash: String) -> RpcResult<Option<RpcTransaction>>;

    /// Returns transaction receipt
    #[method(name = "getTransactionReceipt")]
    async fn get_transaction_receipt(&self, hash: String) -> RpcResult<Option<RpcReceipt>>;

    /// Sends a raw transaction
    #[method(name = "sendRawTransaction")]
    async fn send_raw_transaction(&self, data: String) -> RpcResult<String>;

    /// Executes a call without creating a transaction
    #[method(name = "call")]
    async fn call(&self, request: CallRequest, block: Option<BlockNumber>) -> RpcResult<String>;

    /// Estimates gas for a transaction
    #[method(name = "estimateGas")]
    async fn estimate_gas(
        &self,
        request: CallRequest,
        block: Option<BlockNumber>,
    ) -> RpcResult<String>;

    /// Returns the current gas price
    #[method(name = "gasPrice")]
    async fn gas_price(&self) -> RpcResult<String>;

    /// Returns storage at a given position
    #[method(name = "getStorageAt")]
    async fn get_storage_at(
        &self,
        address: String,
        position: String,
        block: Option<BlockNumber>,
    ) -> RpcResult<String>;

    /// Returns the account and storage values with Merkle proof
    #[method(name = "getProof")]
    async fn get_proof(
        &self,
        address: String,
        storage_keys: Vec<String>,
        block: Option<BlockNumber>,
    ) -> RpcResult<RpcAccountProof>;

    /// Returns logs matching a filter
    #[method(name = "getLogs")]
    async fn get_logs(&self, filter: LogFilter) -> RpcResult<Vec<RpcLog>>;

    /// Returns a suggested priority fee (tip). QFC has no priority fee.
    #[method(name = "maxPriorityFeePerGas")]
    async fn max_priority_fee_per_gas(&self) -> RpcResult<String>;

    /// Returns historical gas fee data for EIP-1559 fee estimation.
    #[method(name = "feeHistory")]
    async fn fee_history(
        &self,
        block_count: serde_json::Value,
        newest_block: BlockNumber,
        reward_percentiles: Option<Vec<f64>>,
    ) -> RpcResult<RpcFeeHistory>;

    /// Returns the node's sync status (`false` when caught up).
    #[method(name = "syncing")]
    async fn syncing(&self) -> RpcResult<serde_json::Value>;

    /// Returns the list of addresses owned by the node (always empty).
    #[method(name = "accounts")]
    async fn accounts(&self) -> RpcResult<Vec<String>>;

    /// Returns the number of transactions in a block by number.
    #[method(name = "getBlockTransactionCountByNumber")]
    async fn get_block_transaction_count_by_number(
        &self,
        block: BlockNumber,
    ) -> RpcResult<Option<String>>;

    /// Returns the number of transactions in a block by hash.
    #[method(name = "getBlockTransactionCountByHash")]
    async fn get_block_transaction_count_by_hash(&self, hash: String) -> RpcResult<Option<String>>;

    /// Returns a transaction by block number and index.
    #[method(name = "getTransactionByBlockNumberAndIndex")]
    async fn get_transaction_by_block_number_and_index(
        &self,
        block: BlockNumber,
        index: String,
    ) -> RpcResult<Option<RpcTransaction>>;

    /// Returns a transaction by block hash and index.
    #[method(name = "getTransactionByBlockHashAndIndex")]
    async fn get_transaction_by_block_hash_and_index(
        &self,
        hash: String,
        index: String,
    ) -> RpcResult<Option<RpcTransaction>>;

    /// Subscribe to new block headers
    #[subscription(name = "subscribe" => "subscription", unsubscribe = "unsubscribe", item = serde_json::Value)]
    async fn eth_subscribe(&self, sub_type: String) -> SubscriptionResult;
}

/// Account proof (EIP-1186)
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcAccountProof {
    /// Address
    pub address: String,
    /// Account proof nodes (hex-encoded)
    pub account_proof: Vec<String>,
    /// Account balance
    pub balance: String,
    /// Code hash
    pub code_hash: String,
    /// Account nonce
    pub nonce: String,
    /// Storage hash (state root)
    pub storage_hash: String,
    /// Storage proofs
    pub storage_proof: Vec<RpcStorageProof>,
}

/// Storage slot proof
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcStorageProof {
    /// Storage key
    pub key: String,
    /// Storage value
    pub value: String,
    /// Proof nodes (hex-encoded)
    pub proof: Vec<String>,
}
