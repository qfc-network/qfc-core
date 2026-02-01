//! Sync manager - handles incoming network messages and block synchronization

use libp2p::PeerId;
use parking_lot::RwLock;
use qfc_chain::Chain;
use qfc_crypto::blake3_hash;
use qfc_mempool::Mempool;
use qfc_network::{NetworkMessage, NetworkService, SyncEvent, SyncRequest, SyncResponse};
use qfc_rpc::SyncStatusProvider;
use qfc_types::{Block, Hash};
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use tracing::{debug, error, info, warn};

/// Maximum number of blocks to request at once
const MAX_BLOCKS_PER_REQUEST: u64 = 32;

/// Sync state information
#[derive(Clone, Debug, Default)]
#[allow(dead_code)]
pub struct SyncState {
    /// Highest block number known from peers
    pub highest_peer_block: u64,
    /// Whether we're actively syncing
    pub is_syncing: bool,
    /// Number of pending blocks waiting for parents
    pub pending_count: usize,
}

/// Sync manager handles incoming blocks and transactions from the network
#[derive(Clone)]
pub struct SyncManager {
    chain: Arc<Chain>,
    #[allow(dead_code)] // Will be used when transaction sync is implemented
    mempool: Arc<RwLock<Mempool>>,
    network: Arc<NetworkService>,
    /// Blocks we're waiting for (parent hash -> child blocks waiting)
    pending_blocks: Arc<RwLock<VecDeque<Block>>>,
    /// Hashes we've already requested
    requested_hashes: Arc<RwLock<HashSet<Hash>>>,
    /// Highest known block from peers
    highest_peer_block: Arc<RwLock<u64>>,
}

impl SyncManager {
    /// Create a new sync manager
    pub fn new(
        chain: Arc<Chain>,
        mempool: Arc<RwLock<Mempool>>,
        network: Arc<NetworkService>,
    ) -> Self {
        Self {
            chain,
            mempool,
            network,
            pending_blocks: Arc::new(RwLock::new(VecDeque::new())),
            requested_hashes: Arc::new(RwLock::new(HashSet::new())),
            highest_peer_block: Arc::new(RwLock::new(0)),
        }
    }

    /// Get the current sync state
    pub fn sync_state(&self) -> SyncState {
        let highest_peer = *self.highest_peer_block.read();
        let our_height = self.chain.block_number();
        let pending_count = self.pending_blocks.read().len();

        // We're syncing if we're more than 2 blocks behind the highest known peer
        // and we have pending blocks or requested hashes
        let is_syncing = highest_peer > 0
            && our_height + 2 < highest_peer
            && (pending_count > 0 || !self.requested_hashes.read().is_empty());

        SyncState {
            highest_peer_block: highest_peer,
            is_syncing,
            pending_count,
        }
    }

    /// Check if we're currently syncing
    #[allow(dead_code)]
    pub fn is_syncing(&self) -> bool {
        self.sync_state().is_syncing
    }

    /// Update highest known peer block
    pub fn update_peer_height(&self, height: u64) {
        let mut highest = self.highest_peer_block.write();
        if height > *highest {
            *highest = height;
        }
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

    /// Handle a sync event (incoming sync request)
    pub async fn handle_sync_event(&self, event: SyncEvent) {
        match event {
            SyncEvent::Request {
                peer_id,
                request,
                response_tx,
            } => {
                info!("Handling sync request from {}: {:?}", peer_id, request);
                let response = self.handle_sync_request(request).await;
                info!("Sending sync response: {:?}", response);
                if response_tx.send(response).is_err() {
                    warn!("Failed to send sync response through channel");
                }
            }
        }
    }

    /// Handle a sync request and return a response
    async fn handle_sync_request(&self, request: SyncRequest) -> SyncResponse {
        match request {
            SyncRequest::GetBlockByHash(hash) => {
                match self.chain.get_block_by_hash(&hash) {
                    Ok(Some(block)) => {
                        let data = borsh::to_vec(&block).unwrap();
                        SyncResponse::Block(data)
                    }
                    Ok(None) => SyncResponse::NotFound,
                    Err(e) => SyncResponse::Error(e.to_string()),
                }
            }
            SyncRequest::GetBlockByNumber(number) => {
                match self.chain.get_block_by_number(number) {
                    Ok(Some(block)) => {
                        let data = borsh::to_vec(&block).unwrap();
                        SyncResponse::Block(data)
                    }
                    Ok(None) => SyncResponse::NotFound,
                    Err(e) => SyncResponse::Error(e.to_string()),
                }
            }
            SyncRequest::GetBlockRange { start, end } => {
                let mut blocks = Vec::new();
                let end = end.min(start + MAX_BLOCKS_PER_REQUEST);

                for num in start..=end {
                    match self.chain.get_block_by_number(num) {
                        Ok(Some(block)) => {
                            blocks.push(borsh::to_vec(&block).unwrap());
                        }
                        Ok(None) => break,
                        Err(_) => break,
                    }
                }

                if blocks.is_empty() {
                    SyncResponse::NotFound
                } else {
                    SyncResponse::Blocks(blocks)
                }
            }
            SyncRequest::GetStatus => {
                let block_number = self.chain.block_number();
                let genesis_hash = self.chain.genesis_hash().unwrap_or_default();
                let block_hash = self
                    .chain
                    .head()
                    .map(|h| blake3_hash(&h.block.header_bytes()))
                    .unwrap_or_default();

                SyncResponse::Status {
                    block_number,
                    block_hash,
                    genesis_hash,
                }
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
        let parent_hash = block.parent_hash();

        debug!(
            "Received block #{} ({})",
            block_number,
            hex::encode(&block_hash.as_bytes()[..8])
        );

        // Update highest known peer block
        self.update_peer_height(block_number);

        // Try to import the block
        match self.chain.import_block(block.clone()) {
            Ok(_) => {
                info!("Imported block #{} from network", block_number);
                // Process any pending blocks that might now be importable
                self.process_pending_blocks().await;
            }
            Err(qfc_chain::ChainError::BlockAlreadyKnown) => {
                debug!("Block #{} already known", block_number);
            }
            Err(qfc_chain::ChainError::InvalidParent { .. }) => {
                debug!(
                    "Block #{} missing parent {}, requesting sync",
                    block_number,
                    hex::encode(&parent_hash.as_bytes()[..8])
                );
                // Add to pending and request missing blocks
                self.pending_blocks.write().push_back(block);
                self.request_missing_blocks(parent_hash);
            }
            Err(e) => {
                warn!("Failed to import block #{}: {}", block_number, e);
            }
        }
    }

    /// Request missing blocks from peers
    fn request_missing_blocks(&self, missing_parent: Hash) {
        // Check if we've already requested this
        {
            let mut requested = self.requested_hashes.write();
            if requested.contains(&missing_parent) {
                return;
            }
            requested.insert(missing_parent);
        }

        // Get a peer to request from
        let peers = self.network.peers();
        if peers.is_empty() {
            warn!("No peers available to request blocks from");
            self.requested_hashes.write().remove(&missing_parent);
            return;
        }

        // Try to request from the first peer
        let peer = peers[0];
        let self_clone = self.clone();

        info!(
            "Requesting block {} from peer {}",
            hex::encode(&missing_parent.as_bytes()[..8]),
            peer
        );

        // Spawn the request to avoid recursion issues
        tokio::spawn(async move {
            info!("Fetching block {} from peer {}", hex::encode(&missing_parent.as_bytes()[..8]), peer);
            match self_clone.network.request_block_by_hash(peer, missing_parent).await {
                Ok(SyncResponse::Block(data)) => {
                    info!("Received block data ({} bytes)", data.len());
                    // Parse and try to import the block
                    match borsh::from_slice::<Block>(&data) {
                        Ok(block) => {
                            let block_number = block.number();
                            let block_parent = block.parent_hash();
                            info!("Parsed block #{}, parent: {}", block_number, hex::encode(&block_parent.as_bytes()[..8]));

                            match self_clone.chain.import_block(block.clone()) {
                                Ok(_) => {
                                    info!("Imported fetched block #{}", block_number);
                                    // Try to process pending blocks
                                    self_clone.process_pending_blocks_sync();
                                }
                                Err(qfc_chain::ChainError::InvalidParent { .. }) => {
                                    // Need to request even earlier blocks
                                    info!("Block #{} still missing parent, queuing", block_number);
                                    self_clone.pending_blocks.write().push_front(block);
                                    // Request parent
                                    self_clone.request_missing_blocks(block_parent);
                                }
                                Err(e) => {
                                    warn!("Failed to import fetched block #{}: {}", block_number, e);
                                }
                            }
                        }
                        Err(e) => {
                            warn!("Failed to parse block: {}", e);
                        }
                    }
                }
                Ok(SyncResponse::NotFound) => {
                    info!("Block not found on peer");
                }
                Ok(other) => {
                    warn!("Unexpected sync response: {:?}", other);
                }
                Err(e) => {
                    error!("Failed to request block from peer: {}", e);
                }
            }

            // Clean up requested hash
            self_clone.requested_hashes.write().remove(&missing_parent);
        });
    }

    /// Try to import pending blocks (async version)
    async fn process_pending_blocks(&self) {
        self.process_pending_blocks_sync();
    }

    /// Try to import pending blocks (sync version for use in spawned tasks)
    fn process_pending_blocks_sync(&self) {
        let mut imported = true;

        while imported {
            imported = false;
            let mut pending = self.pending_blocks.write();
            let mut to_retry = VecDeque::new();

            while let Some(block) = pending.pop_front() {
                let block_number = block.number();
                match self.chain.import_block(block.clone()) {
                    Ok(_) => {
                        info!("Imported pending block #{}", block_number);
                        imported = true;
                    }
                    Err(qfc_chain::ChainError::BlockAlreadyKnown) => {
                        // Already imported, skip
                    }
                    Err(qfc_chain::ChainError::InvalidParent { .. }) => {
                        // Still missing parent, keep in queue
                        to_retry.push_back(block);
                    }
                    Err(e) => {
                        warn!("Failed to import pending block #{}: {}", block_number, e);
                    }
                }
            }

            *pending = to_retry;
        }
    }

    /// Handle an incoming transaction
    async fn handle_transaction(&self, data: Vec<u8>) {
        // Parse transaction
        let tx = match qfc_types::Transaction::from_bytes(&data) {
            Ok(t) => t,
            Err(e) => {
                warn!("Failed to decode transaction: {}", e);
                return;
            }
        };

        let tx_hash = blake3_hash(&tx.to_bytes_without_signature());

        // Derive sender from signature (placeholder - proper verification would use public key recovery)
        let sender_hash = blake3_hash(tx.signature.as_bytes());
        let sender = match qfc_types::Address::from_slice(&sender_hash.as_bytes()[12..32]) {
            Some(addr) => addr,
            None => {
                warn!("Failed to derive sender address");
                return;
            }
        };

        // Add to mempool
        match self.mempool.write().add(tx, sender) {
            Ok(_) => {
                info!(
                    "Added transaction {} from network (sender: {})",
                    hex::encode(&tx_hash.as_bytes()[..8]),
                    sender
                );
            }
            Err(e) => {
                debug!("Failed to add transaction to mempool: {}", e);
            }
        }
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

    /// Initiate sync with a peer
    #[allow(dead_code)] // Will be used when initial sync is implemented
    pub async fn sync_with_peer(&self, peer_id: PeerId) {
        info!("Starting sync with peer {}", peer_id);

        // First, get peer's status
        match self.network.request_status(peer_id).await {
            Ok(SyncResponse::Status {
                block_number,
                block_hash: _,
                genesis_hash,
            }) => {
                let our_genesis = self.chain.genesis_hash().unwrap_or_default();
                if genesis_hash != our_genesis {
                    warn!("Peer {} has different genesis hash!", peer_id);
                    return;
                }

                let our_block_number = self.chain.block_number();
                if block_number > our_block_number {
                    info!(
                        "Peer {} is ahead: {} vs our {}",
                        peer_id, block_number, our_block_number
                    );
                    // Request blocks we're missing
                    self.sync_blocks_from_peer(peer_id, our_block_number + 1, block_number)
                        .await;
                } else {
                    debug!("We're up to date with peer {}", peer_id);
                }
            }
            Ok(other) => {
                warn!("Unexpected status response from peer: {:?}", other);
            }
            Err(e) => {
                error!("Failed to get status from peer {}: {}", peer_id, e);
            }
        }
    }

    /// Sync blocks from a peer
    async fn sync_blocks_from_peer(&self, peer_id: PeerId, start: u64, end: u64) {
        let mut current = start;

        while current <= end {
            let request_end = (current + MAX_BLOCKS_PER_REQUEST - 1).min(end);

            info!("Requesting blocks {}..{} from peer {}", current, request_end, peer_id);

            match self.network.request_block_range(peer_id, current, request_end).await {
                Ok(SyncResponse::Blocks(blocks)) => {
                    for block_data in blocks {
                        if let Ok(block) = borsh::from_slice::<Block>(&block_data) {
                            let block_number = block.number();
                            match self.chain.import_block(block) {
                                Ok(_) => {
                                    info!("Synced block #{}", block_number);
                                }
                                Err(qfc_chain::ChainError::BlockAlreadyKnown) => {
                                    debug!("Block #{} already known", block_number);
                                }
                                Err(e) => {
                                    warn!("Failed to import synced block #{}: {}", block_number, e);
                                }
                            }
                        }
                    }
                    current = request_end + 1;
                }
                Ok(SyncResponse::NotFound) => {
                    debug!("No more blocks available from peer");
                    break;
                }
                Ok(other) => {
                    warn!("Unexpected response: {:?}", other);
                    break;
                }
                Err(e) => {
                    error!("Sync failed: {}", e);
                    break;
                }
            }
        }
    }
}

impl SyncStatusProvider for SyncManager {
    fn is_syncing(&self) -> bool {
        self.sync_state().is_syncing
    }

    fn highest_peer_block(&self) -> u64 {
        *self.highest_peer_block.read()
    }

    fn pending_count(&self) -> usize {
        self.pending_blocks.read().len()
    }
}
