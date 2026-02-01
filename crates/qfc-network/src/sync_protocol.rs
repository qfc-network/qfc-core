//! Block sync request-response protocol
//!
//! Implements a simple request-response protocol for fetching blocks.

use async_trait::async_trait;
use borsh::{BorshDeserialize, BorshSerialize};
use futures::prelude::*;
use libp2p::request_response;
use libp2p::StreamProtocol;
use qfc_types::Hash;
use std::io;

/// Protocol name for block sync
pub const SYNC_PROTOCOL: StreamProtocol = StreamProtocol::new("/qfc/sync/1");

/// Sync request types
#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub enum SyncRequest {
    /// Request a block by hash
    GetBlockByHash(Hash),
    /// Request a block by number
    GetBlockByNumber(u64),
    /// Request multiple blocks by number range
    GetBlockRange { start: u64, end: u64 },
    /// Request the current head block info
    GetStatus,
}

/// Sync response types
#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub enum SyncResponse {
    /// Block data (serialized Block)
    Block(Vec<u8>),
    /// Multiple blocks
    Blocks(Vec<Vec<u8>>),
    /// Block not found
    NotFound,
    /// Status response
    Status {
        /// Current block number
        block_number: u64,
        /// Current block hash
        block_hash: Hash,
        /// Genesis hash
        genesis_hash: Hash,
    },
    /// Error response
    Error(String),
}

/// Codec for sync protocol
#[derive(Debug, Clone, Default)]
pub struct SyncCodec;

#[async_trait]
impl request_response::Codec for SyncCodec {
    type Protocol = StreamProtocol;
    type Request = SyncRequest;
    type Response = SyncResponse;

    async fn read_request<T>(
        &mut self,
        _protocol: &Self::Protocol,
        io: &mut T,
    ) -> io::Result<Self::Request>
    where
        T: AsyncRead + Unpin + Send,
    {
        // Read length prefix (4 bytes)
        let mut len_buf = [0u8; 4];
        io.read_exact(&mut len_buf).await?;
        let len = u32::from_be_bytes(len_buf) as usize;

        if len > 1024 * 1024 {
            // Max 1MB
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Request too large"));
        }

        // Read data
        let mut buf = vec![0u8; len];
        io.read_exact(&mut buf).await?;

        // Deserialize
        SyncRequest::try_from_slice(&buf)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    async fn read_response<T>(
        &mut self,
        _protocol: &Self::Protocol,
        io: &mut T,
    ) -> io::Result<Self::Response>
    where
        T: AsyncRead + Unpin + Send,
    {
        // Read length prefix (4 bytes)
        let mut len_buf = [0u8; 4];
        io.read_exact(&mut len_buf).await?;
        let len = u32::from_be_bytes(len_buf) as usize;

        if len > 10 * 1024 * 1024 {
            // Max 10MB for responses (can contain multiple blocks)
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Response too large",
            ));
        }

        // Read data
        let mut buf = vec![0u8; len];
        io.read_exact(&mut buf).await?;

        // Deserialize
        SyncResponse::try_from_slice(&buf)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    async fn write_request<T>(
        &mut self,
        _protocol: &Self::Protocol,
        io: &mut T,
        req: Self::Request,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        let data =
            borsh::to_vec(&req).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        // Write length prefix
        let len = data.len() as u32;
        io.write_all(&len.to_be_bytes()).await?;

        // Write data
        io.write_all(&data).await?;
        io.flush().await?;

        Ok(())
    }

    async fn write_response<T>(
        &mut self,
        _protocol: &Self::Protocol,
        io: &mut T,
        res: Self::Response,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        let data =
            borsh::to_vec(&res).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        // Write length prefix
        let len = data.len() as u32;
        io.write_all(&len.to_be_bytes()).await?;

        // Write data
        io.write_all(&data).await?;
        io.flush().await?;

        Ok(())
    }
}
