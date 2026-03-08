//! txpool RPC methods (Geth-compatible)

use jsonrpsee::core::RpcResult;
use jsonrpsee::proc_macros::rpc;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::types::RpcTransaction;

/// txpool_content response
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TxPoolContent {
    pub pending: HashMap<String, HashMap<String, RpcTransaction>>,
    pub queued: HashMap<String, HashMap<String, RpcTransaction>>,
}

/// txpool_status response
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TxPoolStatus {
    pub pending: String,
    pub queued: String,
}

/// Geth-compatible txpool RPC API
#[rpc(server, namespace = "txpool")]
pub trait TxPoolApi {
    /// Returns all pending and queued transactions grouped by sender
    #[method(name = "content")]
    async fn txpool_content(&self) -> RpcResult<TxPoolContent>;

    /// Returns the count of pending and queued transactions
    #[method(name = "status")]
    async fn txpool_status(&self) -> RpcResult<TxPoolStatus>;
}
