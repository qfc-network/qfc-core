//! IPFS client for uploading and fetching large inference results.
//!
//! Uses a local Kubo API (default `http://127.0.0.1:5001`) via curl subprocess,
//! following the same HTTP pattern as `qfc-miner/src/submit.rs`.

use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

/// IPFS configuration
#[derive(Clone, Debug)]
pub struct IpfsConfig {
    /// Kubo API URL (default: http://127.0.0.1:5001)
    pub api_url: String,
    /// Public gateway URL for fetching (default: http://127.0.0.1:8080)
    pub gateway_url: String,
    /// Result size threshold in bytes (default: 1MB)
    pub size_threshold: usize,
    /// Pin TTL in seconds (default: 7 days)
    pub pin_ttl_secs: u64,
    /// Upload timeout
    pub timeout: Duration,
}

impl Default for IpfsConfig {
    fn default() -> Self {
        Self {
            api_url: "http://127.0.0.1:5001".into(),
            gateway_url: "http://127.0.0.1:8080".into(),
            size_threshold: 1_048_576,   // 1MB
            pin_ttl_secs: 7 * 24 * 3600, // 7 days
            timeout: Duration::from_secs(30),
        }
    }
}

/// Result of uploading to IPFS
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IpfsUploadResult {
    pub cid: String,
    pub size: usize,
}

/// IPFS client for uploading inference results
#[derive(Clone)]
pub struct IpfsClient {
    config: IpfsConfig,
}

impl IpfsClient {
    pub fn new(config: IpfsConfig) -> Self {
        Self { config }
    }

    /// Check if data exceeds the size threshold for IPFS storage
    pub fn should_upload(&self, data: &[u8]) -> bool {
        data.len() > self.config.size_threshold
    }

    /// Upload data to IPFS via Kubo API (`/api/v0/add`)
    /// Returns CID on success
    pub async fn upload(&self, data: &[u8]) -> Result<IpfsUploadResult, IpfsError> {
        // Build multipart form body
        let boundary = "----QFCBoundary";
        let mut body = Vec::new();
        body.extend_from_slice(format!("--{}\r\n", boundary).as_bytes());
        body.extend_from_slice(
            b"Content-Disposition: form-data; name=\"file\"; filename=\"result\"\r\n",
        );
        body.extend_from_slice(b"Content-Type: application/octet-stream\r\n\r\n");
        body.extend_from_slice(data);
        body.extend_from_slice(format!("\r\n--{}--\r\n", boundary).as_bytes());

        let url = format!("{}/api/v0/add?pin=true", self.config.api_url);

        let mut child = tokio::process::Command::new("curl")
            .args([
                "-s",
                "-X",
                "POST",
                "-H",
                &format!("Content-Type: multipart/form-data; boundary={}", boundary),
                "--data-binary",
                "@-",
                "--max-time",
                &self.config.timeout.as_secs().to_string(),
                &url,
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| IpfsError::ConnectionFailed(e.to_string()))?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(&body)
                .await
                .map_err(|e| IpfsError::UploadFailed(e.to_string()))?;
        }

        let output = child
            .wait_with_output()
            .await
            .map_err(|e| IpfsError::ConnectionFailed(e.to_string()))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(IpfsError::UploadFailed(format!("curl failed: {}", stderr)));
        }

        let response_str =
            String::from_utf8(output.stdout).map_err(|e| IpfsError::ParseError(e.to_string()))?;

        // Kubo returns JSON: {"Name":"result","Hash":"Qm...","Size":"123"}
        let parsed: KuboAddResponse = serde_json::from_str(&response_str)
            .map_err(|e| IpfsError::ParseError(format!("Parse IPFS response: {}", e)))?;

        Ok(IpfsUploadResult {
            cid: parsed.hash,
            size: data.len(),
        })
    }

    /// Fetch data from IPFS via gateway
    pub async fn fetch(&self, cid: &str) -> Result<Vec<u8>, IpfsError> {
        let url = format!("{}/ipfs/{}", self.config.gateway_url, cid);

        let output = tokio::process::Command::new("curl")
            .args([
                "-s",
                "--max-time",
                &self.config.timeout.as_secs().to_string(),
                &url,
            ])
            .output()
            .await
            .map_err(|e| IpfsError::ConnectionFailed(e.to_string()))?;

        if !output.status.success() {
            return Err(IpfsError::FetchFailed(format!(
                "Failed to fetch CID: {}",
                cid
            )));
        }

        Ok(output.stdout)
    }

    /// Unpin content from IPFS
    pub async fn unpin(&self, cid: &str) -> Result<(), IpfsError> {
        let url = format!("{}/api/v0/pin/rm?arg={}", self.config.api_url, cid);

        let output = tokio::process::Command::new("curl")
            .args(["-s", "-X", "POST", &url])
            .output()
            .await
            .map_err(|e| IpfsError::ConnectionFailed(e.to_string()))?;

        if !output.status.success() {
            return Err(IpfsError::UnpinFailed(cid.to_string()));
        }

        Ok(())
    }

    /// Build a public gateway URL for a CID
    pub fn gateway_url(&self, cid: &str) -> String {
        format!("{}/ipfs/{}", self.config.gateway_url, cid)
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct KuboAddResponse {
    hash: String,
    #[allow(dead_code)]
    name: String,
    #[allow(dead_code)]
    size: String,
}

/// Errors that can occur during IPFS operations
#[derive(Debug, thiserror::Error)]
pub enum IpfsError {
    #[error("IPFS connection failed: {0}")]
    ConnectionFailed(String),
    #[error("IPFS upload failed: {0}")]
    UploadFailed(String),
    #[error("IPFS fetch failed: {0}")]
    FetchFailed(String),
    #[error("IPFS unpin failed: {0}")]
    UnpinFailed(String),
    #[error("IPFS parse error: {0}")]
    ParseError(String),
}
