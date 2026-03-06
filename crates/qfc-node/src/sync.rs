//! Sync manager - handles incoming network messages and block synchronization

use libp2p::PeerId;
use parking_lot::RwLock;
use qfc_chain::Chain;
use qfc_crypto::{blake3_hash, verify_hash_signature};
use qfc_mempool::Mempool;
use qfc_network::{NetworkMessage, NetworkService, SyncEvent, SyncRequest, SyncResponse};
use qfc_rpc::SyncStatusProvider;
use qfc_types::{
    Block, Hash, Heartbeat, InferenceProof, SlashingEvidence, ValidatorMessage, Vote, VoteDecision,
    WorkProof,
};
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
    /// Inference engine for spot-check re-execution (v2.0)
    inference_engine: Option<Arc<tokio::sync::RwLock<Box<dyn qfc_inference::InferenceEngine>>>>,
    /// Approved model registry for proof validation (v2.0)
    model_registry: Arc<qfc_inference::model::ModelRegistry>,
    /// v2.0: Pool of verified inference proofs awaiting block inclusion
    proof_pool: Option<Arc<RwLock<qfc_ai_coordinator::ProofPool>>>,
    /// v2.0 P2: Challenge generator
    challenge_generator: Option<Arc<RwLock<qfc_ai_coordinator::challenge::ChallengeGenerator>>>,
    /// v2.0 P2: Redundant verifier
    redundant_verifier: Option<Arc<RwLock<qfc_ai_coordinator::redundant::RedundantVerifier>>>,
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
            inference_engine: None,
            model_registry: Arc::new(qfc_inference::model::ModelRegistry::default_v2()),
            proof_pool: None,
            challenge_generator: None,
            redundant_verifier: None,
        }
    }

    /// Attach an inference engine for spot-check verification (v2.0)
    pub fn with_inference_engine(
        mut self,
        engine: Box<dyn qfc_inference::InferenceEngine>,
    ) -> Self {
        self.inference_engine = Some(Arc::new(tokio::sync::RwLock::new(engine)));
        self
    }

    /// Set the shared proof pool (v2.0)
    pub fn with_proof_pool(mut self, pool: Arc<RwLock<qfc_ai_coordinator::ProofPool>>) -> Self {
        self.proof_pool = Some(pool);
        self
    }

    /// Set the challenge generator (P2)
    pub fn with_challenge_generator(
        mut self,
        gen: Arc<RwLock<qfc_ai_coordinator::challenge::ChallengeGenerator>>,
    ) -> Self {
        self.challenge_generator = Some(gen);
        self
    }

    /// Set the redundant verifier (P2)
    pub fn with_redundant_verifier(
        mut self,
        rv: Arc<RwLock<qfc_ai_coordinator::redundant::RedundantVerifier>>,
    ) -> Self {
        self.redundant_verifier = Some(rv);
        self
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
            SyncRequest::GetBlockByHash(hash) => match self.chain.get_block_by_hash(&hash) {
                Ok(Some(block)) => {
                    let data = borsh::to_vec(&block).unwrap();
                    SyncResponse::Block(data)
                }
                Ok(None) => SyncResponse::NotFound,
                Err(e) => SyncResponse::Error(e.to_string()),
            },
            SyncRequest::GetBlockByNumber(number) => match self.chain.get_block_by_number(number) {
                Ok(Some(block)) => {
                    let data = borsh::to_vec(&block).unwrap();
                    SyncResponse::Block(data)
                }
                Ok(None) => SyncResponse::NotFound,
                Err(e) => SyncResponse::Error(e.to_string()),
            },
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

                // If we're a validator, cast our vote for this block
                if self.chain.consensus().is_validator() {
                    self.cast_vote_for_block(&block).await;
                }
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

    /// Cast a vote for a successfully imported block
    async fn cast_vote_for_block(&self, block: &Block) {
        let consensus = self.chain.consensus();
        let block_number = block.number();

        // Create an accept vote (we validated the block during import)
        let vote = match consensus.vote(block, true) {
            Ok(v) => v,
            Err(e) => {
                warn!("Failed to create vote for block #{}: {}", block_number, e);
                return;
            }
        };

        // Broadcast our vote
        let vote_data = vote.to_bytes();
        if let Err(e) = self.network.broadcast_vote(vote_data).await {
            warn!(
                "Failed to broadcast vote for block #{}: {}",
                block_number, e
            );
        } else {
            info!(
                "Broadcast accept vote for block #{} from {}",
                block_number,
                consensus.our_address().unwrap_or_default()
            );
        }

        // Add our vote to pending votes
        consensus.add_vote(vote);
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
            info!(
                "Fetching block {} from peer {}",
                hex::encode(&missing_parent.as_bytes()[..8]),
                peer
            );
            match self_clone
                .network
                .request_block_by_hash(peer, missing_parent)
                .await
            {
                Ok(SyncResponse::Block(data)) => {
                    info!("Received block data ({} bytes)", data.len());
                    // Parse and try to import the block
                    match borsh::from_slice::<Block>(&data) {
                        Ok(block) => {
                            let block_number = block.number();
                            let block_parent = block.parent_hash();
                            info!(
                                "Parsed block #{}, parent: {}",
                                block_number,
                                hex::encode(&block_parent.as_bytes()[..8])
                            );

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
                                    warn!(
                                        "Failed to import fetched block #{}: {}",
                                        block_number, e
                                    );
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
        // 1. Deserialize the vote
        let vote: Vote = match borsh::from_slice(&data) {
            Ok(v) => v,
            Err(e) => {
                warn!("Failed to decode vote: {}", e);
                return;
            }
        };

        debug!(
            "Received vote for block #{} from {}",
            vote.block_height, vote.voter
        );

        // 2. Get consensus engine and validators
        let consensus = self.chain.consensus();
        let validators = consensus.get_validators();

        // 3. Find the voter in the validator set
        let voter_validator = match validators.iter().find(|v| v.address == vote.voter) {
            Some(v) => v,
            None => {
                warn!("Vote from unknown validator: {}", vote.voter);
                return;
            }
        };

        // 4. Check if voter is active
        if !voter_validator.is_active() {
            warn!("Vote from inactive/jailed validator: {}", vote.voter);
            return;
        }

        // 5. Verify the vote signature
        let vote_hash = blake3_hash(&vote.to_bytes_without_signature());
        if verify_hash_signature(&voter_validator.public_key, &vote_hash, &vote.signature).is_err()
        {
            warn!("Invalid vote signature from {}", vote.voter);
            // Record invalid vote for slashing consideration
            consensus.record_vote(&vote.voter, false);
            return;
        }

        // 6. Verify the vote is for a known block
        let block_exists = self
            .chain
            .get_block_by_hash(&vote.block_hash)
            .ok()
            .flatten()
            .is_some();

        if !block_exists {
            debug!(
                "Vote for unknown block {}, storing anyway",
                hex::encode(&vote.block_hash.as_bytes()[..8])
            );
        }

        // 7. Record the vote as valid
        let is_accept = vote.decision == VoteDecision::Accept;
        consensus.record_vote(&vote.voter, true);

        // 8. Add vote to pending votes
        consensus.add_vote(vote.clone());

        info!(
            "Added {} vote from {} for block #{}",
            if is_accept { "accept" } else { "reject" },
            vote.voter,
            vote.block_height
        );

        // 9. Check if block has reached finality
        if consensus.check_finality(&vote.block_hash) {
            let current_finalized = consensus.finalized_height();
            if vote.block_height > current_finalized {
                consensus.set_finalized_height(vote.block_height);
                info!("Block #{} finalized!", vote.block_height);

                // Prune old votes
                consensus.prune_old_votes(vote.block_height);
            }
        }

        // 10. If we're a validator and haven't voted yet, cast our vote
        if consensus.is_validator() {
            self.maybe_cast_vote(&vote.block_hash, vote.block_height)
                .await;
        }
    }

    /// Cast our own vote for a block if we haven't already
    async fn maybe_cast_vote(&self, block_hash: &Hash, block_height: u64) {
        let consensus = self.chain.consensus();

        // Check if we've already voted for this block
        // (A more robust implementation would track our own votes)
        let our_address = match consensus.our_address() {
            Some(addr) => addr,
            None => return,
        };

        // Get the block to validate
        let block = match self.chain.get_block_by_hash(block_hash) {
            Ok(Some(b)) => b,
            _ => {
                debug!("Cannot vote: block not found");
                return;
            }
        };

        // Get the parent block for validation
        let parent = match self.chain.get_block_by_hash(&block.parent_hash()) {
            Ok(Some(p)) => p,
            _ => {
                debug!("Cannot vote: parent block not found");
                return;
            }
        };

        // Validate the block and decide our vote
        let accept = consensus.validate_block(&block, &parent).is_ok();

        // Create and sign our vote
        let vote = match consensus.vote(&block, accept) {
            Ok(v) => v,
            Err(e) => {
                warn!("Failed to create vote: {}", e);
                return;
            }
        };

        // Broadcast our vote
        let vote_data = vote.to_bytes();
        if let Err(e) = self.network.broadcast_vote(vote_data).await {
            warn!("Failed to broadcast vote: {}", e);
        } else {
            info!(
                "Broadcast {} vote for block #{} from {}",
                if accept { "accept" } else { "reject" },
                block_height,
                our_address
            );
        }

        // Add our own vote to pending votes
        consensus.add_vote(vote);
    }

    /// Handle a validator message
    async fn handle_validator_msg(&self, data: Vec<u8>) {
        // Deserialize the validator message
        let msg: ValidatorMessage = match borsh::from_slice(&data) {
            Ok(m) => m,
            Err(e) => {
                warn!("Failed to decode validator message: {}", e);
                return;
            }
        };

        match msg {
            ValidatorMessage::Heartbeat(heartbeat) => {
                self.handle_heartbeat(heartbeat).await;
            }
            ValidatorMessage::EpochAnnouncement(announcement) => {
                self.handle_epoch_announcement(announcement).await;
            }
            ValidatorMessage::SlashingEvidence(evidence) => {
                self.handle_slashing_evidence(evidence).await;
            }
            ValidatorMessage::WorkProof(proof) => {
                self.handle_work_proof(proof).await;
            }
            ValidatorMessage::InferenceProof(proof) => {
                self.handle_inference_proof(proof).await;
            }
        }
    }

    /// Handle a validator heartbeat
    async fn handle_heartbeat(&self, heartbeat: Heartbeat) {
        let consensus = self.chain.consensus();
        let validators = consensus.get_validators();

        // Find the validator
        let validator = match validators.iter().find(|v| v.address == heartbeat.validator) {
            Some(v) => v,
            None => {
                debug!("Heartbeat from unknown validator: {}", heartbeat.validator);
                return;
            }
        };

        // Verify signature
        let heartbeat_hash = blake3_hash(&heartbeat.to_bytes_without_signature());
        if verify_hash_signature(&validator.public_key, &heartbeat_hash, &heartbeat.signature)
            .is_err()
        {
            warn!("Invalid heartbeat signature from {}", heartbeat.validator);
            return;
        }

        // Update peer height if they report a higher block
        if heartbeat.block_height > self.chain.block_number() {
            self.update_peer_height(heartbeat.block_height);
        }

        // Calculate latency (rough estimate based on timestamp difference)
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        if now > heartbeat.timestamp {
            let latency_ms = (now - heartbeat.timestamp) as u32;
            // Only record reasonable latencies (< 30 seconds)
            if latency_ms < 30_000 {
                consensus.record_latency(&heartbeat.validator, latency_ms);
            }
        }

        debug!(
            "Heartbeat from {} at block #{}",
            heartbeat.validator, heartbeat.block_height
        );
    }

    /// Handle an epoch announcement
    async fn handle_epoch_announcement(&self, announcement: qfc_types::EpochAnnouncement) {
        let consensus = self.chain.consensus();
        let validators = consensus.get_validators();

        // Find the announcer
        let announcer = match validators
            .iter()
            .find(|v| v.address == announcement.announcer)
        {
            Some(v) => v,
            None => {
                warn!(
                    "Epoch announcement from unknown validator: {}",
                    announcement.announcer
                );
                return;
            }
        };

        // Verify signature
        let announcement_hash = blake3_hash(&announcement.to_bytes_without_signature());
        if verify_hash_signature(
            &announcer.public_key,
            &announcement_hash,
            &announcement.signature,
        )
        .is_err()
        {
            warn!(
                "Invalid epoch announcement signature from {}",
                announcement.announcer
            );
            return;
        }

        // Check if this is a new epoch
        let current_epoch = consensus.get_epoch();
        if announcement.epoch_number <= current_epoch.number {
            debug!(
                "Ignoring old epoch announcement: {} (current: {})",
                announcement.epoch_number, current_epoch.number
            );
            return;
        }

        // Start the new epoch
        info!(
            "Received epoch {} announcement from {}",
            announcement.epoch_number, announcement.announcer
        );
        consensus.start_epoch(announcement.epoch_number, announcement.seed);
    }

    /// Handle slashing evidence
    async fn handle_slashing_evidence(&self, evidence: SlashingEvidence) {
        let consensus = self.chain.consensus();
        let validators = consensus.get_validators();

        // Find the reporter
        let reporter = match validators.iter().find(|v| v.address == evidence.reporter) {
            Some(v) => v,
            None => {
                warn!(
                    "Slashing evidence from unknown validator: {}",
                    evidence.reporter
                );
                return;
            }
        };

        // Verify signature
        let evidence_hash = blake3_hash(&evidence.to_bytes_without_signature());
        if verify_hash_signature(&reporter.public_key, &evidence_hash, &evidence.signature).is_err()
        {
            warn!(
                "Invalid slashing evidence signature from {}",
                evidence.reporter
            );
            return;
        }

        // Check if the offender exists
        if !validators.iter().any(|v| v.address == evidence.offender) {
            warn!(
                "Slashing evidence for unknown validator: {}",
                evidence.offender
            );
            return;
        }

        info!(
            "Received slashing evidence against {} for {:?} from {}",
            evidence.offender, evidence.offense, evidence.reporter
        );

        // Determine slash parameters based on offense type
        let (slash_percent, jail_duration_ms) = match evidence.offense {
            qfc_types::SlashableOffense::DoubleSign => (10, 24 * 60 * 60 * 1000), // 10%, 24 hours
            qfc_types::SlashableOffense::InvalidBlock => (5, 12 * 60 * 60 * 1000), // 5%, 12 hours
            qfc_types::SlashableOffense::Censorship => (3, 6 * 60 * 60 * 1000),   // 3%, 6 hours
            qfc_types::SlashableOffense::Offline => (1, 1 * 60 * 60 * 1000),      // 1%, 1 hour
            qfc_types::SlashableOffense::FalseVote => (2, 2 * 60 * 60 * 1000),    // 2%, 2 hours
            qfc_types::SlashableOffense::InvalidInference => (5, 6 * 60 * 60 * 1000), // 5%, 6 hours
        };

        // Apply the slash
        consensus.slash_validator(&evidence.offender, slash_percent, jail_duration_ms);

        info!(
            "Slashed validator {} by {}%, jailed for {}ms",
            evidence.offender, slash_percent, jail_duration_ms
        );
    }

    /// Handle a work proof from mining
    async fn handle_work_proof(&self, proof: WorkProof) {
        let consensus = self.chain.consensus();
        let validators = consensus.get_validators();

        // Find the validator who submitted the proof
        let validator = match validators.iter().find(|v| v.address == proof.validator) {
            Some(v) => v,
            None => {
                debug!("Work proof from unknown validator: {}", proof.validator);
                return;
            }
        };

        // Check if validator is active
        if !validator.is_active() {
            debug!(
                "Work proof from inactive/jailed validator: {}",
                proof.validator
            );
            return;
        }

        // Verify the proof signature
        let proof_hash = blake3_hash(&proof.to_bytes_without_signature());
        if verify_hash_signature(&validator.public_key, &proof_hash, &proof.signature).is_err() {
            warn!("Invalid work proof signature from {}", proof.validator);
            return;
        }

        // Get current epoch to construct mining task for hashrate calculation
        let _epoch = consensus.get_epoch();

        // Calculate hashrate from the proof
        // Note: We use a simplified calculation here since we don't have the exact task
        // that was used. The work_count and epoch_duration are sufficient.
        let epoch_duration_secs = 10; // Default epoch duration
        let estimated_hashrate = if epoch_duration_secs > 0 {
            // Rough estimate: work_count * some factor / duration
            // This is a simplified estimate since we don't have full task info
            proof.work_count.saturating_mul(65536) / epoch_duration_secs
        } else {
            0
        };

        // Update the validator's hashrate and mark as compute provider
        consensus.update_hashrate(&proof.validator, estimated_hashrate);
        if estimated_hashrate > 0 {
            consensus.set_provides_compute(&proof.validator, true);
        }

        info!(
            "Received work proof from {} for epoch {}: {} valid hashes, ~{} H/s",
            proof.validator, proof.epoch, proof.work_count, estimated_hashrate
        );
    }

    /// Handle an inference proof from an AI compute miner (v2.0)
    async fn handle_inference_proof(&self, proof: InferenceProof) {
        let consensus = self.chain.consensus();
        let validators = consensus.get_validators();

        // 1. Find the validator who submitted the proof
        let validator = match validators.iter().find(|v| v.address == proof.validator) {
            Some(v) => v,
            None => {
                debug!(
                    "Inference proof from unknown validator: {}",
                    proof.validator
                );
                return;
            }
        };

        // 2. Check if validator is active
        if !validator.is_active() {
            debug!(
                "Inference proof from inactive/jailed validator: {}",
                proof.validator
            );
            return;
        }

        // 3. Verify the proof signature
        let proof_hash = blake3_hash(&proof.to_bytes_without_signature());
        if verify_hash_signature(&validator.public_key, &proof_hash, &proof.signature).is_err() {
            warn!("Invalid inference proof signature from {}", proof.validator);
            return;
        }

        // 4. Convert qfc_types::InferenceProof → qfc_inference::InferenceProof via borsh roundtrip
        let proof_bytes = borsh::to_vec(&proof).unwrap();
        let inference_proof: qfc_inference::InferenceProof = match borsh::from_slice(&proof_bytes) {
            Ok(p) => p,
            Err(e) => {
                warn!("Failed to convert inference proof: {}", e);
                return;
            }
        };

        // 5. Run basic verification (epoch, model, FLOPS)
        // Advance epoch if expired before checking, so we use a fresh epoch number
        let head_hash = self.chain.head().map(|h| h.hash).unwrap_or_default();
        let epoch_number = consensus.maybe_advance_epoch(
            qfc_types::EPOCH_DURATION_SECS * 1000,
            head_hash,
        );
        if let Err(e) = qfc_ai_coordinator::verify_basic(
            &inference_proof,
            epoch_number,
            &self.model_registry,
        ) {
            warn!(
                "Inference proof from {} failed basic verification: {}",
                proof.validator, e
            );
            return;
        }

        // 6. Probabilistic spot-check (~5%)
        if qfc_ai_coordinator::should_spot_check(&inference_proof) {
            if let Some(ref engine_lock) = self.inference_engine {
                // Regenerate synthetic tasks to find the original task with correct
                // input_data. This ensures the spot-check uses identical task_id +
                // input_data as the miner, preventing false-positive fraud detection.
                let epoch = consensus.get_epoch();
                let epoch_seed = u64::from_le_bytes(epoch.seed[..8].try_into().unwrap_or([0u8; 8]));
                let mut task_pool = qfc_ai_coordinator::TaskPool::new();
                task_pool.generate_synthetic_tasks(proof.epoch, epoch_seed, u64::MAX);

                // Find the task matching proof.input_hash (= original task_id)
                let matching_task = {
                    let mut found = None;
                    while let Some(t) = task_pool.fetch_task(qfc_inference::GpuTier::Hot, u64::MAX)
                    {
                        if t.task_id == proof.input_hash {
                            found = Some(t);
                            break;
                        }
                    }
                    found
                };

                if let Some(task) = matching_task {
                    let engine = engine_lock.read().await;
                    match qfc_ai_coordinator::verify_spot_check(&inference_proof, &task, &**engine)
                        .await
                    {
                        Ok(result) => {
                            info!(
                                "Spot-check PASSED for inference proof from {}: {}",
                                proof.validator, result.details
                            );
                        }
                        Err(qfc_ai_coordinator::VerificationError::OutputHashMismatch {
                            expected,
                            got,
                        }) => {
                            warn!(
                                "Spot-check FAILED for {}: output hash mismatch (expected {}, got {})",
                                proof.validator,
                                hex::encode(&expected.as_bytes()[..8]),
                                hex::encode(&got.as_bytes()[..8]),
                            );
                            // Slash the miner for fraud: 5% stake, 6 hours jail
                            consensus.slash_validator(&proof.validator, 5, 6 * 60 * 60 * 1000);
                            return;
                        }
                        Err(e) => {
                            // Re-execution failure is not necessarily fraud; log and skip
                            warn!(
                                "Spot-check re-execution error for {}: {}",
                                proof.validator, e
                            );
                        }
                    }
                } else {
                    debug!(
                        "Spot-check: no matching synthetic task for {}, skipping",
                        proof.validator
                    );
                }
            } else {
                debug!(
                    "Spot-check selected for {} but no inference engine available",
                    proof.validator
                );
            }
        }

        // 7. Challenge check (P2): if this is a challenge task, verify and return early
        if let Some(ref cg) = self.challenge_generator {
            let mut gen = cg.write();
            if gen.is_challenge(&proof.input_hash) {
                if let Some(verdict) = gen.verify_challenge(&proof.input_hash, &proof.output_hash) {
                    if let Some(penalty) = gen.record_result(&proof.validator, &verdict) {
                        consensus.reduce_reputation(&proof.validator, penalty.reputation_reduction);
                        if penalty.slash_percent > 0 {
                            consensus.slash_validator(
                                &proof.validator,
                                penalty.slash_percent,
                                penalty.jail_duration_ms,
                            );
                        }
                        if !matches!(
                            verdict,
                            qfc_ai_coordinator::challenge::ChallengeVerdict::Passed
                        ) {
                            warn!("Challenge failed for {}: {:?}", proof.validator, verdict);
                        }
                    }
                    if matches!(
                        verdict,
                        qfc_ai_coordinator::challenge::ChallengeVerdict::Passed
                    ) {
                        debug!("Challenge passed for {}", proof.validator);
                    }
                }
                // Challenges don't go to proof pool — return early
                return;
            }
        }

        // 7b. Redundant verification check (P2)
        if let Some(ref rv) = self.redundant_verifier {
            let mut verifier = rv.write();
            if verifier.is_pending(&proof.input_hash) {
                if let Some(result) =
                    verifier.record_submission(proof.input_hash, proof.validator, proof.output_hash)
                {
                    // Penalize inconsistent miners
                    for &bad_miner in &result.inconsistent_miners {
                        consensus.reduce_reputation(&bad_miner, 1000);
                        info!(
                            "Redundant verification: inconsistent miner {} penalized",
                            bad_miner
                        );
                    }
                    // Only consistent proofs proceed
                    if !result.consistent_miners.contains(&proof.validator) {
                        return;
                    }
                } else {
                    // Still waiting for more submissions
                    return;
                }
            }
        }

        // 8. Proof passed — update inference score
        consensus.update_inference_score(
            &proof.validator,
            proof.flops_estimated,
            1, // single task completed
        );

        // 9. Push to proof pool for block inclusion (v2.0)
        if let Some(ref pool) = self.proof_pool {
            pool.write().add(proof.clone());
        }

        info!(
            "Accepted inference proof from {} for epoch {}: {} FLOPS, {}ms",
            proof.validator, proof.epoch, proof.flops_estimated, proof.execution_time_ms
        );
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

            info!(
                "Requesting blocks {}..{} from peer {}",
                current, request_end, peer_id
            );

            match self
                .network
                .request_block_range(peer_id, current, request_end)
                .await
            {
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
