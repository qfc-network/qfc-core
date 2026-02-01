//! RPC error types

use jsonrpsee::types::ErrorObjectOwned;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RpcError {
    #[error("Invalid params: {0}")]
    InvalidParams(String),

    #[error("Block not found")]
    BlockNotFound,

    #[error("Transaction not found")]
    TransactionNotFound,

    #[error("Account not found")]
    AccountNotFound,

    #[error("Execution error: {0}")]
    Execution(String),

    #[error("Internal error: {0}")]
    Internal(String),
}

impl From<RpcError> for ErrorObjectOwned {
    fn from(e: RpcError) -> Self {
        match e {
            RpcError::InvalidParams(msg) => ErrorObjectOwned::owned(-32602, msg, None::<()>),
            RpcError::BlockNotFound => ErrorObjectOwned::owned(-32001, "Block not found", None::<()>),
            RpcError::TransactionNotFound => {
                ErrorObjectOwned::owned(-32002, "Transaction not found", None::<()>)
            }
            RpcError::AccountNotFound => {
                ErrorObjectOwned::owned(-32003, "Account not found", None::<()>)
            }
            RpcError::Execution(msg) => ErrorObjectOwned::owned(-32000, msg, None::<()>),
            RpcError::Internal(msg) => ErrorObjectOwned::owned(-32603, msg, None::<()>),
        }
    }
}

pub type Result<T> = std::result::Result<T, RpcError>;
