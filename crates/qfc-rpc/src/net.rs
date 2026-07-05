//! `net_*` and `web3_*` RPC namespaces.
//!
//! These are non-consensus informational methods that MetaMask, ethers and
//! block explorers probe during connection setup.

use jsonrpsee::core::RpcResult;
use jsonrpsee::proc_macros::rpc;

/// `net_*` network status methods.
#[rpc(server, namespace = "net")]
pub trait NetApi {
    /// Returns the current network id as a decimal string (the chain id).
    #[method(name = "version")]
    async fn version(&self) -> RpcResult<String>;

    /// Returns `true` if the client is actively listening for connections.
    #[method(name = "listening")]
    async fn listening(&self) -> RpcResult<bool>;

    /// Returns the number of connected peers as a hex quantity.
    #[method(name = "peerCount")]
    async fn peer_count(&self) -> RpcResult<String>;
}

/// `web3_*` client utility methods.
#[rpc(server, namespace = "web3")]
pub trait Web3Api {
    /// Returns the client software version string.
    #[method(name = "clientVersion")]
    async fn client_version(&self) -> RpcResult<String>;
}
