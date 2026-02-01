//! Ethereum-compatible RPC methods

use crate::error::RpcError;
use crate::types::{BlockNumber, BlockTag, CallRequest, RpcBlock, RpcReceipt, RpcTransaction};
use jsonrpsee::core::RpcResult;
use jsonrpsee::proc_macros::rpc;

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
}
