//! Sync manager - handles incoming network messages

use parking_lot::RwLock;
use qfc_chain::Chain;
use qfc_crypto::blake3_hash;
use qfc_mempool::Mempool;
use qfc_network::NetworkMessage;
use qfc_types::Block;
use std::sync::Arc;
use tracing::{debug, info, warn};

/// Sync manager handles incoming blocks and transactions from the network
pub struct SyncManager {
    chain: Arc<Chain>,
    #[allow(dead_code)] // Will be used when transaction sync is implemented
    mempool: Arc<RwLock<Mempool>>,
}

impl SyncManager {
    /// Create a new sync manager
    pub fn new(chain: Arc<Chain>, mempool: Arc<RwLock<Mempool>>) -> Self {
        Self { chain, mempool }
    }

    /// Handle an incoming network message
    pub async fn handle_message(&self, msg: NetworkMessage) {
        match msg {
            NetworkMessage::NewBlock(data) => {
                self.handle_block(data).await;
            }
            NetworkMessage::NewTransaction(data) => {
                self.handle_transaction(data).await;
            }
            NetworkMessage::Vote(data) => {
                self.handle_vote(data).await;
            }
            NetworkMessage::ValidatorMsg(data) => {
                self.handle_validator_msg(data).await;
            }
        }
    }

    /// Handle an incoming block
    async fn handle_block(&self, data: Vec<u8>) {
        let block: Block = match borsh::from_slice(&data) {
            Ok(b) => b,
            Err(e) => {
                warn!("Failed to decode block: {}", e);
                return;
            }
        };

        let block_hash = blake3_hash(&block.header_bytes());
        let block_number = block.number();

        debug!(
            "Received block #{} ({})",
            block_number,
            hex::encode(&block_hash.as_bytes()[..8])
        );

        // Try to import the block
        match self.chain.import_block(block) {
            Ok(_) => {
                info!(
                    "Imported block #{} from network",
                    block_number
                );
            }
            Err(qfc_chain::ChainError::BlockAlreadyKnown) => {
                debug!("Block #{} already known", block_number);
            }
            Err(e) => {
                warn!("Failed to import block #{}: {}", block_number, e);
            }
        }
    }

    /// Handle an incoming transaction
    async fn handle_transaction(&self, data: Vec<u8>) {
        // TODO: Transactions need sender public key for verification
        // For now, just log that we received one
        debug!("Received transaction ({} bytes) - pending sender verification", data.len());
    }

    /// Handle an incoming vote
    async fn handle_vote(&self, data: Vec<u8>) {
        // TODO: Implement vote handling for finality
        debug!("Received vote ({} bytes)", data.len());
    }

    /// Handle a validator message
    async fn handle_validator_msg(&self, data: Vec<u8>) {
        // TODO: Implement validator message handling
        debug!("Received validator message ({} bytes)", data.len());
    }
}
