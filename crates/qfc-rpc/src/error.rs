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

    /// T5: AI task-pool quota rejection. Carries the violated limit
    /// (`reason` ∈ pool_pressure | qps | in_flight | flops_budget) and a
    /// retry-after hint; surfaced as JSON-RPC code -32029 with structured
    /// `data`.
    #[error("Quota exceeded ({reason}): {message}")]
    QuotaExceeded {
        reason: &'static str,
        message: String,
        retry_after_ms: u64,
    },
}

impl From<qfc_ai_coordinator::QuotaError> for RpcError {
    fn from(e: qfc_ai_coordinator::QuotaError) -> Self {
        RpcError::QuotaExceeded {
            reason: e.reason(),
            message: e.to_string(),
            retry_after_ms: e.retry_after_ms(),
        }
    }
}

impl From<RpcError> for ErrorObjectOwned {
    fn from(e: RpcError) -> Self {
        match e {
            RpcError::InvalidParams(msg) => ErrorObjectOwned::owned(-32602, msg, None::<()>),
            RpcError::BlockNotFound => {
                ErrorObjectOwned::owned(-32001, "Block not found", None::<()>)
            }
            RpcError::TransactionNotFound => {
                ErrorObjectOwned::owned(-32002, "Transaction not found", None::<()>)
            }
            RpcError::AccountNotFound => {
                ErrorObjectOwned::owned(-32003, "Account not found", None::<()>)
            }
            RpcError::Execution(msg) => ErrorObjectOwned::owned(-32000, msg, None::<()>),
            RpcError::Internal(msg) => ErrorObjectOwned::owned(-32603, msg, None::<()>),
            RpcError::QuotaExceeded {
                reason,
                message,
                retry_after_ms,
            } => ErrorObjectOwned::owned(
                -32029,
                format!("Quota exceeded ({reason}): {message}"),
                Some(serde_json::json!({
                    "reason": reason,
                    "retryAfterMs": retry_after_ms,
                })),
            ),
        }
    }
}

pub type Result<T> = std::result::Result<T, RpcError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_error_maps_to_distinct_code_with_retry_hint() {
        let quota_err = qfc_ai_coordinator::QuotaError::QpsExceeded {
            tenant: qfc_types::Address::new([1; 20]),
            limit: 5.0,
            retry_after_ms: 200,
        };
        let rpc_err: RpcError = quota_err.into();
        let obj: ErrorObjectOwned = rpc_err.into();
        assert_eq!(obj.code(), -32029);
        assert!(obj.message().contains("qps"), "message: {}", obj.message());
        assert!(obj.message().contains("retry after 200ms"));
        let data = obj.data().expect("structured data").to_string();
        assert!(data.contains("\"retryAfterMs\":200"));
        assert!(data.contains("\"reason\":\"qps\""));
    }
}
