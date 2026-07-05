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
    WorkProof, BLOCK_INTERVAL_MS, JAIL_CENSORSHIP_MS, JAIL_DOUBLE_SIGN_MS, JAIL_FALSE_VOTE_MS,
    JAIL_INVALID_BLOCK_MS, JAIL_INVALID_INFERENCE_MS, JAIL_OFFLINE_MS, SLASH_CENSORSHIP_PERCENT,
    SLASH_DOUBLE_SIGN_PERCENT, SLASH_FALSE_VOTE_PERCENT, SLASH_INVALID_BLOCK_PERCENT,
    SLASH_INVALID_INFERENCE_PERCENT, SLASH_OFFLINE_PERCENT,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, error, info, warn};

/// Maximum number of blocks to request at once
const MAX_BLOCKS_PER_REQUEST: u64 = 32;

/// Maximum number of pending blocks waiting for parents (prevents memory exhaustion).
/// Only the gossip backward-walk uses this, for small reorg gaps — deep
/// catch-up goes through the forward range sync (`run_catch_up_loop`), which
/// imports in order and never queues out-of-order blocks.
const MAX_PENDING_BLOCKS: usize = 1_000;

/// How often the catch-up loop checks whether we've fallen behind peers.
const CATCH_UP_INTERVAL_SECS: u64 = 5;

/// Begin a forward catch-up once we are more than this many blocks behind the
/// highest block height seen from peers.
const CATCH_UP_LAG_THRESHOLD: u64 = 2;

/// How often the active peer-status poll loop ticks. Each connected peer
/// without a recent status is polled every tick, so a freshly connected peer
/// has a verified status within ~2s (spec §5: poll at connect + every ~2s
/// until fresh).
const STATUS_POLL_INTERVAL_MS: u64 = 2_000;

/// Re-poll a peer whose status is older than this (one slot), keeping the
/// gate's view continuously fresh.
const STATUS_REFRESH_MS: u64 = BLOCK_INTERVAL_MS;

/// A peer status older than this (~3 slots) is stale: the produce gate and
/// `is_syncing` ignore it entirely. Combined with the 2s poll cadence a
/// status only ever goes stale when the peer stops answering GetStatus.
pub(crate) const STATUS_STALE_MS: u64 = 3 * BLOCK_INTERVAL_MS;

/// A peer's last actively-polled `GetStatus` result (spec §5). This map —
/// NOT passive gossip/heartbeat heights — is the produce gate's input:
/// request/response works on silent peers, and gossip heights are both
/// unauthenticated and absent exactly when every validator is gated.
#[derive(Clone, Debug)]
struct PeerStatusEntry {
    /// The peer's reported canonical head height.
    head: u64,
    /// The peer's genesis hash (entries with a foreign genesis are kept for
    /// diagnostics but never feed the gate or peer selection).
    genesis_hash: Hash,
    /// When the status response was received.
    last_seen: Instant,
}

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
    /// Per-peer verified statuses from ACTIVE GetStatus polling (gate input).
    peer_statuses: Arc<RwLock<HashMap<PeerId, PeerStatusEntry>>>,
    /// Peers with a GetStatus request currently in flight (prevents the 2s
    /// poll loop from stacking requests behind a slow/dead peer's timeout).
    status_inflight: Arc<RwLock<HashSet<PeerId>>>,
    /// True while `sync_with_peer` is running (forward catch-up). Part of
    /// `is_syncing()` — forward catch-up was previously invisible to it.
    catching_up: Arc<AtomicBool>,
    /// Rotation cursor for catch-up peer selection: 0 = the peer with the
    /// highest verified head; bumped on a failed sync to try the next-best.
    sync_peer_cursor: Arc<AtomicUsize>,
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
    /// v2.1 E3: Arbitration manager for multi-validator dispute resolution
    arbitration_manager: Arc<RwLock<qfc_ai_coordinator::ArbitrationManager>>,
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
            peer_statuses: Arc::new(RwLock::new(HashMap::new())),
            status_inflight: Arc::new(RwLock::new(HashSet::new())),
            catching_up: Arc::new(AtomicBool::new(false)),
            sync_peer_cursor: Arc::new(AtomicUsize::new(0)),
            inference_engine: None,
            model_registry: Arc::new(qfc_inference::model::ModelRegistry::default_v2()),
            proof_pool: None,
            challenge_generator: None,
            redundant_verifier: None,
            arbitration_manager: Arc::new(RwLock::new(
                qfc_ai_coordinator::ArbitrationManager::new(),
            )),
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

    /// Get the current sync state.
    ///
    /// `is_syncing` (spec §5) = active forward catch-up (`catching_up`) OR
    /// pending/backward-walk activity OR a verified (status-confirmed,
    /// genesis-matching, fresh) peer head more than the lag threshold ahead
    /// of us. The old definition required pending-queue activity, so the
    /// forward range catch-up — the main sync path — reported "not syncing".
    pub fn sync_state(&self) -> SyncState {
        let highest_peer = *self.highest_peer_block.read();
        let our_height = self.chain.block_number();
        let pending_count = self.pending_blocks.read().len();

        let verified_highest = self
            .fresh_verified_peer_heads()
            .into_iter()
            .map(|(_, head)| head)
            .max();

        let is_syncing = self.catching_up.load(Ordering::Relaxed)
            || pending_count > 0
            || !self.requested_hashes.read().is_empty()
            || verified_highest.is_some_and(|h| h > our_height + CATCH_UP_LAG_THRESHOLD);

        SyncState {
            highest_peer_block: highest_peer.max(verified_highest.unwrap_or(0)),
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

    /// Whether a node at `our_height` should start a forward catch-up given the
    /// highest block height seen from peers. Pure; split out for testing.
    fn should_catch_up(our_height: u64, highest_peer: u64) -> bool {
        highest_peer > our_height + CATCH_UP_LAG_THRESHOLD
    }

    /// Record an actively-polled peer status. Genesis-matching heads also
    /// feed `highest_peer_block` (they are the *authenticated* height signal).
    fn record_peer_status(&self, peer: PeerId, head: u64, genesis_hash: Hash) {
        let ours = self.chain.genesis_hash().unwrap_or_default();
        self.peer_statuses.write().insert(
            peer,
            PeerStatusEntry {
                head,
                genesis_hash,
                last_seen: Instant::now(),
            },
        );
        if genesis_hash == ours {
            self.update_peer_height(head);
        } else {
            warn!(
                "Peer {} reports a different genesis hash — excluded from gate/sync",
                peer
            );
        }
    }

    /// Connected peers with a fresh, genesis-matching status: `(peer, head)`.
    fn fresh_verified_peer_heads(&self) -> Vec<(PeerId, u64)> {
        let connected: HashSet<PeerId> = self.network.peers().into_iter().collect();
        let ours = self.chain.genesis_hash().unwrap_or_default();
        self.peer_statuses
            .read()
            .iter()
            .filter(|(peer, entry)| {
                connected.contains(peer)
                    && entry.genesis_hash == ours
                    && (entry.last_seen.elapsed().as_millis() as u64) <= STATUS_STALE_MS
            })
            .map(|(peer, entry)| (*peer, entry.head))
            .collect()
    }

    /// The produce gate's view of the network (spec §5):
    /// `(connected_peer_count, max fresh genesis-matching verified head)`.
    /// `None` means "peers may exist but none has a fresh verified status".
    pub fn gate_peer_view(&self) -> (usize, Option<u64>) {
        let connected = self.network.peer_count();
        let max_head = self
            .fresh_verified_peer_heads()
            .into_iter()
            .map(|(_, head)| head)
            .max();
        (connected, max_head)
    }

    /// Active peer-status poll loop (spec §5). Every ~2s:
    /// - drop statuses of disconnected peers (expire on disconnect);
    /// - poll every connected peer that has no status yet (new connection)
    ///   or whose status is older than one slot.
    ///
    /// Each GetStatus is spawned so one dead peer's 30s request timeout can
    /// never stall polling of the others; `status_inflight` prevents
    /// stacking duplicate requests on the same peer. Spawn once at startup.
    pub async fn run_status_poll_loop(self: Arc<Self>) {
        let mut ticker =
            tokio::time::interval(std::time::Duration::from_millis(STATUS_POLL_INTERVAL_MS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;

            let connected: HashSet<PeerId> = self.network.peers().into_iter().collect();

            // Expire entries for peers that disconnected.
            self.peer_statuses
                .write()
                .retain(|peer, _| connected.contains(peer));

            for peer in connected {
                let needs_poll = match self.peer_statuses.read().get(&peer) {
                    None => true,
                    Some(entry) => {
                        (entry.last_seen.elapsed().as_millis() as u64) >= STATUS_REFRESH_MS
                    }
                };
                if !needs_poll || !self.status_inflight.write().insert(peer) {
                    continue;
                }
                let sm = self.clone();
                tokio::spawn(async move {
                    sm.poll_peer_status(peer).await;
                    sm.status_inflight.write().remove(&peer);
                });
            }
        }
    }

    /// Send one GetStatus to `peer` and record the result.
    async fn poll_peer_status(&self, peer: PeerId) {
        match self.network.request_status(peer).await {
            Ok(SyncResponse::Status {
                block_number,
                block_hash: _,
                genesis_hash,
            }) => {
                debug!("Status from {}: head #{}", peer, block_number);
                self.record_peer_status(peer, block_number, genesis_hash);
            }
            Ok(other) => {
                debug!("Unexpected status response from {}: {:?}", peer, other);
            }
            Err(e) => {
                debug!("Status poll of {} failed: {}", peer, e);
            }
        }
    }

    /// Pick the sync peer: the `cursor`-th candidate of the list ordered by
    /// verified head (desc), tie-broken by peer id for determinism. Pure;
    /// split out for testing. `cursor` rotates on failed syncs so a stuck
    /// best-head peer cannot monopolize catch-up.
    fn pick_sync_peer(mut candidates: Vec<(PeerId, u64)>, cursor: usize) -> Option<PeerId> {
        if candidates.is_empty() {
            return None;
        }
        candidates.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        Some(candidates[cursor % candidates.len()].0)
    }

    /// The peer to catch up from: highest verified (status-confirmed,
    /// genesis-matching) head, rotated on failure. Never `peers().first()` —
    /// that picked an arbitrary peer, including genesis-foreign ones.
    fn best_sync_peer(&self) -> Option<PeerId> {
        Self::pick_sync_peer(
            self.fresh_verified_peer_heads(),
            self.sync_peer_cursor.load(Ordering::Relaxed),
        )
    }

    /// Run one sync attempt against the best verified peer, advancing the
    /// rotation cursor on failure. Returns false when no verified peer is
    /// available or the sync failed.
    pub async fn catch_up_with_best_peer(&self) -> bool {
        let Some(peer) = self.best_sync_peer() else {
            debug!("Catch-up requested but no verified peer available yet");
            return false;
        };
        let ok = self.sync_with_peer(peer).await;
        if ok {
            self.sync_peer_cursor.store(0, Ordering::Relaxed);
        } else {
            self.sync_peer_cursor.fetch_add(1, Ordering::Relaxed);
        }
        ok
    }

    /// Forced catch-up for the produce gate (spec §5): when a validator has
    /// been gated strictly-behind for more than 2 slots, it must sync even
    /// inside the lag 1–2 "dead zone" below `CATCH_UP_LAG_THRESHOLD`, or a
    /// 1-block lag would gate it forever. No-op while a sync is running.
    pub fn spawn_forced_catch_up(self: &Arc<Self>) {
        if self.catching_up.load(Ordering::Relaxed) {
            return;
        }
        let sm = self.clone();
        tokio::spawn(async move {
            info!("Producer gated while behind — forcing a catch-up attempt");
            sm.catch_up_with_best_peer().await;
        });
    }

    /// Periodic forward catch-up loop.
    ///
    /// When we fall behind the highest known peer, download the missing blocks
    /// **in order by number** (range requests) and import them sequentially.
    /// In-order import never hits a missing parent, so this catches a node up
    /// from any depth — including a fresh node syncing from genesis. The gossip
    /// backward-walk (`request_missing_blocks`) is kept only for small reorg
    /// gaps; its bounded pending buffer cannot bridge a large lag, which is why
    /// behind nodes previously got stuck re-requesting forever.
    ///
    /// Runs sequentially (each tick awaits the full sync), so catch-ups never
    /// overlap. Spawn once at startup.
    pub async fn run_catch_up_loop(self: Arc<Self>) {
        let mut ticker =
            tokio::time::interval(std::time::Duration::from_secs(CATCH_UP_INTERVAL_SECS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;

            let our_height = self.chain.block_number();
            // Trigger ONLY on fresh verified (status-confirmed,
            // genesis-matching) peer heads — never `highest_peer_block`,
            // which gossip/heartbeats feed and which a single bogus claim
            // could ratchet to keep this loop spinning forever (review fix
            // 15). `highest_peer_block` remains observability-only.
            let highest_peer = self
                .fresh_verified_peer_heads()
                .into_iter()
                .map(|(_, head)| head)
                .max()
                .unwrap_or(0);
            if !Self::should_catch_up(our_height, highest_peer) {
                continue;
            }

            info!(
                "Catch-up: {} blocks behind (us {}, best peer head {})",
                highest_peer.saturating_sub(our_height),
                our_height,
                highest_peer,
            );
            // sync_with_peer re-verifies the peer's status/genesis, then
            // range-syncs forward in order via sync_blocks_from_peer.
            self.catch_up_with_best_peer().await;
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

            SyncRequest::GetHeaderRange { start, end } => {
                let mut headers = Vec::new();
                let end = end.min(start + MAX_BLOCKS_PER_REQUEST);

                for num in start..=end {
                    match self.chain.get_block_by_number(num) {
                        Ok(Some(block)) => {
                            headers.push(borsh::to_vec(&block.header).unwrap());
                        }
                        Ok(None) => break,
                        Err(_) => break,
                    }
                }

                if headers.is_empty() {
                    SyncResponse::NotFound
                } else {
                    SyncResponse::Headers(headers)
                }
            }

            SyncRequest::GetStateProof {
                address,
                block_number,
            } => {
                let addr = qfc_types::Address::new(address);
                match self.chain.state_at(block_number) {
                    Ok(state) => match state.get_account_proof(&addr) {
                        Ok((proof, account)) => SyncResponse::StateProof {
                            proof: proof.to_bytes(),
                            account: borsh::to_vec(&account).unwrap(),
                        },
                        Err(e) => SyncResponse::Error(e.to_string()),
                    },
                    Err(e) => SyncResponse::Error(e.to_string()),
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

        // Try to import the block
        match self.chain.import_block(block.clone()).await {
            Ok(_) => {
                info!("Imported block #{} from network", block_number);
                // Only a VALIDATED height may ratchet highest_peer_block
                // (review fix 15): pre-validation gossip heights let a bogus
                // claim drive the catch-up loop forever.
                self.update_peer_height(block_number);
                // Process any pending blocks that might now be importable
                self.process_pending_blocks().await;

                // If we're a validator, cast our vote — but ONLY for a block
                // that became canonical at its height (review fix 3b):
                // side-branch stores must never attract accept votes, or
                // finality can wedge on a hash the canonical chain lacks.
                if self.chain.consensus().is_validator()
                    && self
                        .chain
                        .is_canonical(&block_hash, block_number)
                        .unwrap_or(false)
                {
                    self.cast_vote_for_block(&block).await;
                }

                // Votes may have arrived BEFORE the block (stored in
                // pending_votes); now that the block is imported, re-check
                // finality so an early quorum is not missed at this height.
                self.try_finalize(&block_hash, block_number);
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
                // Add to pending and request missing blocks (bounded)
                {
                    let mut pending = self.pending_blocks.write();
                    if pending.len() >= MAX_PENDING_BLOCKS {
                        warn!("Pending blocks queue full ({MAX_PENDING_BLOCKS}), dropping oldest");
                        pending.pop_front();
                    }
                    pending.push_back(block);
                }
                self.request_missing_blocks(parent_hash);
            }
            Err(e) => {
                warn!("Failed to import block #{}: {}", block_number, e);
            }
        }
    }

    /// Single writer for the finalized pointer (review-fix hardening).
    ///
    /// Finality moves only for a block we hold CANONICALLY at that height
    /// (review fixes 3c/3d): votes for unknown or side-branch blocks are
    /// stored but must never move the finalized pointer, or every future
    /// reorg wedges on a hash the canonical chain cannot contain.
    /// `Chain::record_finalized` re-checks canonicity and is the ONLY
    /// place that raises the engine's finalized height, so a height is
    /// never recorded without its hash.
    fn try_finalize(&self, block_hash: &Hash, height: u64) {
        let consensus = self.chain.consensus();
        let before = self.chain.finalized().0;
        if height <= before {
            return;
        }
        if !self.chain.is_canonical(block_hash, height).unwrap_or(false) {
            return;
        }
        if !consensus.check_finality(block_hash) {
            return;
        }
        self.chain.record_finalized(height, *block_hash);
        if self.chain.finalized().0 > before {
            info!("Block #{} finalized!", height);
            consensus.prune_old_votes(height);
        }
    }

    /// Cast a vote for a successfully imported block
    async fn cast_vote_for_block(&self, block: &Block) {
        let consensus = self.chain.consensus();
        let block_number = block.number();
        let block_hash = blake3_hash(&block.header_bytes());

        // Never vote twice at a height / for a block (review fix 2a).
        if !consensus.try_record_own_vote(block_number, block_hash) {
            debug!("Already voted at height {}, not voting again", block_number);
            return;
        }

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

        // Request from the status-verified peer with the highest head
        // (same rotation machinery as the forward catch-up) — never an
        // arbitrary HashSet-ordered peer, which could be genesis-foreign or
        // permanently behind (review fix 6).
        let Some(peer) = self.best_sync_peer() else {
            warn!("No status-verified peer available to request blocks from");
            self.requested_hashes.write().remove(&missing_parent);
            return;
        };
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

                            match self_clone.chain.import_block(block.clone()).await {
                                Ok(_) => {
                                    info!("Imported fetched block #{}", block_number);
                                    // Try to process pending blocks
                                    self_clone.process_pending_blocks().await;
                                }
                                Err(qfc_chain::ChainError::InvalidParent { .. }) => {
                                    // Need to request even earlier blocks
                                    info!("Block #{} still missing parent, queuing", block_number);
                                    let mut pending = self_clone.pending_blocks.write();
                                    if pending.len() < MAX_PENDING_BLOCKS {
                                        pending.push_front(block);
                                    } else {
                                        warn!(
                                            "Pending blocks queue full, dropping block #{}",
                                            block_number
                                        );
                                    }
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

    /// Try to import pending blocks. Drains the queue, imports what it can
    /// (each import serializes on the chain-wide import lock), and requeues
    /// blocks still missing a parent. The queue lock is never held across an
    /// await.
    async fn process_pending_blocks(&self) {
        loop {
            let drained: Vec<Block> = {
                let mut pending = self.pending_blocks.write();
                pending.drain(..).collect()
            };
            if drained.is_empty() {
                return;
            }

            let mut imported = false;
            let mut to_retry: VecDeque<Block> = VecDeque::new();

            for block in drained {
                let block_number = block.number();
                match self.chain.import_block(block.clone()).await {
                    Ok(_) => {
                        info!("Imported pending block #{}", block_number);
                        imported = true;
                        // Early-stored votes for this block may already form
                        // a quorum — re-check now that it is importable.
                        let hash = blake3_hash(&block.header_bytes());
                        self.try_finalize(&hash, block_number);
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

            let done = to_retry.is_empty();
            {
                // Prepend the retries; new arrivals may have queued meanwhile.
                let mut pending = self.pending_blocks.write();
                while let Some(block) = to_retry.pop_back() {
                    pending.push_front(block);
                }
            }

            // Nothing imported this pass -> another pass cannot make progress.
            if !imported || done {
                return;
            }
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

        // Add to mempool with nonce validation
        let state = self.chain.state();
        match self
            .mempool
            .write()
            .add_with_nonce_check(tx, sender, Some(state.as_ref()))
        {
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

        // 4. NOTE: no is_active() gate here. Jail status is node-local
        // (gossip-driven) state; filtering a consensus input (finality votes)
        // by it would let nodes disagree on which votes count — the same
        // class of divergence review fix 4 removed from the election.
        // Registry membership + signature verification suffice.

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

        // 8. Add vote to pending votes (dedups by voter address)
        consensus.add_vote(vote.clone());

        info!(
            "Added {} vote from {} for block #{}",
            if is_accept { "accept" } else { "reject" },
            vote.voter,
            vote.block_height
        );

        // 9. Check if the block has reached finality (see try_finalize for
        // the canonicality rules).
        if block_exists {
            self.try_finalize(&vote.block_hash, vote.block_height);
        }

        // 10. If we're a validator and haven't voted at this height yet,
        // cast our vote. Receiving a vote must never unconditionally emit a
        // new one (review fix 2b) — maybe_cast_vote is a no-op once we have
        // voted at the height, so vote traffic converges instead of echoing.
        if consensus.is_validator() {
            self.maybe_cast_vote(&vote.block_hash, vote.block_height)
                .await;
        }
    }

    /// Cast our own vote for a block if we haven't already voted at its
    /// height and the block is canonical locally (review fixes 2a/3b).
    async fn maybe_cast_vote(&self, block_hash: &Hash, block_height: u64) {
        let consensus = self.chain.consensus();

        let our_address = match consensus.our_address() {
            Some(addr) => addr,
            None => return,
        };

        // Only vote for blocks that are canonical at their height —
        // side-branch blocks must not attract votes (review fix 3b).
        if !self
            .chain
            .is_canonical(block_hash, block_height)
            .unwrap_or(false)
        {
            debug!(
                "Not voting: block {} is not canonical at height {}",
                hex::encode(&block_hash.as_bytes()[..8]),
                block_height
            );
            return;
        }

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

        // Never vote twice at a height / for a block (review fix 2a).
        if !consensus.try_record_own_vote(block_height, *block_hash) {
            debug!("Already voted at height {}, not voting again", block_height);
            return;
        }

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

        // VERIFY-OR-IGNORE ONLY (ADR-0012, D11): epochs are a pure function
        // of wall-clock time and the genesis-anchored seed derivation, so a
        // received announcement can never change our epoch state. Adopting
        // (epoch, seed) pairs from a single validator signature let any peer
        // re-anchor a node's schedule and fork it. We verify the announced
        // seed against our own derivation purely as a health signal.
        match consensus.derive_epoch_seed(announcement.epoch_number) {
            Ok(expected_seed) if expected_seed == announcement.seed => {
                debug!(
                    "Epoch {} announcement from {} matches our derivation (ignored)",
                    announcement.epoch_number, announcement.announcer
                );
            }
            Ok(_) => {
                warn!(
                    "Epoch {} announcement from {} carries a seed that does NOT match \
                     our genesis-anchored derivation — ignoring (possible fork or \
                     misconfigured peer)",
                    announcement.epoch_number, announcement.announcer
                );
            }
            Err(_) => {
                debug!(
                    "Ignoring epoch {} announcement from {}: our genesis seed is unset",
                    announcement.epoch_number, announcement.announcer
                );
            }
        }
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

        // InvalidTraining is settled ONLY via the A5 absolute-slash path
        // (`SlashResult.slashed_amount` = `slash_multiple × per_step_reward`,
        // ADR-0009 Decision 4) and applied by the A6 absolute-amount slash
        // entry point in consensus — it MUST NOT flow through the
        // percent-of-stake `slash_validator(percent, ..)` path below. Routing
        // it here would (a) silently drop the absolute-40r deterrent (a 0%
        // slash) and (b) let any known validator jail an arbitrary victim for
        // free by signing `SlashingEvidence{offense: InvalidTraining}`. Reject
        // it explicitly: no state change, no jail.
        if evidence.offense == qfc_types::SlashableOffense::InvalidTraining {
            warn!(
                "Rejecting InvalidTraining slashing evidence against {} from {}: \
                 InvalidTraining is settled via the A5 absolute-slash path \
                 (SlashResult.slashed_amount, ADR-0009 D4) and MUST NOT be applied \
                 through the percent slash path (A6)",
                evidence.offender, evidence.reporter
            );
            return;
        }

        // Determine slash parameters based on the constants.rs single source.
        let (slash_percent, jail_duration_ms) = match evidence.offense {
            qfc_types::SlashableOffense::DoubleSign => {
                (SLASH_DOUBLE_SIGN_PERCENT as u8, JAIL_DOUBLE_SIGN_MS)
            }
            qfc_types::SlashableOffense::InvalidBlock => {
                (SLASH_INVALID_BLOCK_PERCENT as u8, JAIL_INVALID_BLOCK_MS)
            }
            qfc_types::SlashableOffense::Censorship => {
                (SLASH_CENSORSHIP_PERCENT as u8, JAIL_CENSORSHIP_MS)
            }
            qfc_types::SlashableOffense::Offline => (SLASH_OFFLINE_PERCENT as u8, JAIL_OFFLINE_MS),
            qfc_types::SlashableOffense::FalseVote => {
                (SLASH_FALSE_VOTE_PERCENT as u8, JAIL_FALSE_VOTE_MS)
            }
            qfc_types::SlashableOffense::InvalidInference => (
                SLASH_INVALID_INFERENCE_PERCENT as u8,
                JAIL_INVALID_INFERENCE_MS,
            ),
            // InvalidTraining is handled by the early-return guard above
            // (A5 absolute-slash path, ADR-0009 D4 + A6) and never reaches here.
            qfc_types::SlashableOffense::InvalidTraining => {
                unreachable!("InvalidTraining must be rejected before the percent-slash dispatch")
            }
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

        // 5. Run basic verification (timestamp freshness, model, FLOPS)
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if let Err(e) =
            qfc_ai_coordinator::verify_basic(&inference_proof, now_secs, &self.model_registry)
        {
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
                                "Spot-check FAILED for {}: output hash mismatch (expected {}, got {}). Opening arbitration.",
                                proof.validator,
                                hex::encode(&expected.as_bytes()[..8]),
                                hex::encode(&got.as_bytes()[..8]),
                            );
                            // E3: Open multi-validator arbitration instead of immediate slash
                            let mut arb = self.arbitration_manager.write();
                            if arb.open_dispute(
                                proof.input_hash,
                                proof.validator,
                                proof.output_hash,
                            ) {
                                // Add our own vote (the spot-check re-execution result)
                                if let Some(our_addr) = consensus.our_address() {
                                    arb.submit_vote(
                                        &proof.input_hash,
                                        qfc_ai_coordinator::ArbitrationVote {
                                            validator: our_addr,
                                            output_hash: expected,
                                            execution_time_ms: 0,
                                        },
                                    );
                                }
                                info!(
                                    "Arbitration panel opened for task {} (miner {})",
                                    hex::encode(&proof.input_hash.as_bytes()[..8]),
                                    proof.validator,
                                );
                            }
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

        // 8b. E3: If this task has an open arbitration panel, submit as a vote
        {
            let mut arb = self.arbitration_manager.write();
            if arb.get_panel(&proof.input_hash).is_some() {
                arb.submit_vote(
                    &proof.input_hash,
                    qfc_ai_coordinator::ArbitrationVote {
                        validator: proof.validator,
                        output_hash: proof.output_hash,
                        execution_time_ms: proof.execution_time_ms,
                    },
                );
            }

            // Resolve any panels that have quorum
            let outcomes = arb.resolve_ready();
            for (task_id, outcome, miner) in outcomes {
                match outcome {
                    qfc_ai_coordinator::ArbitrationOutcome::MinerVindicated => {
                        info!(
                            "Arbitration: miner {} vindicated for task {}",
                            miner,
                            hex::encode(&task_id.as_bytes()[..8]),
                        );
                    }
                    qfc_ai_coordinator::ArbitrationOutcome::MinerFaulted {
                        agree_count,
                        total_count,
                        ..
                    } => {
                        warn!(
                            "Arbitration: miner {} faulted for task {} ({}/{} validators disagree). Slashing.",
                            miner,
                            hex::encode(&task_id.as_bytes()[..8]),
                            agree_count,
                            total_count,
                        );
                        consensus.slash_validator(
                            &miner,
                            SLASH_INVALID_INFERENCE_PERCENT as u8,
                            JAIL_INVALID_INFERENCE_MS,
                        );
                    }
                    qfc_ai_coordinator::ArbitrationOutcome::Inconclusive => {
                        info!(
                            "Arbitration: inconclusive for task {} (miner {}), no action taken",
                            hex::encode(&task_id.as_bytes()[..8]),
                            miner,
                        );
                    }
                }
            }
        }

        // 9. Push to proof pool for block inclusion (v2.0)
        if let Some(ref pool) = self.proof_pool {
            pool.write().add(proof.clone());
        }

        info!(
            "Accepted inference proof from {} for epoch {}: {} FLOPS, {}ms",
            proof.validator, proof.epoch, proof.flops_estimated, proof.execution_time_ms
        );
    }

    /// Initiate sync with a peer: check its status, and if it is ahead,
    /// range-sync the missing blocks forward in order. Driven by
    /// [`run_catch_up_loop`] and the gate's forced catch-up.
    ///
    /// Sets `catching_up` for the duration (feeds `is_syncing()`). Returns
    /// false when the peer was unusable (status failure, foreign genesis) or
    /// the range sync stopped on a hard failure — callers rotate to the
    /// next-best peer on false.
    pub async fn sync_with_peer(&self, peer_id: PeerId) -> bool {
        self.catching_up.store(true, Ordering::Relaxed);
        let result = self.sync_with_peer_inner(peer_id).await;
        self.catching_up.store(false, Ordering::Relaxed);
        result
    }

    async fn sync_with_peer_inner(&self, peer_id: PeerId) -> bool {
        info!("Starting sync with peer {}", peer_id);

        // First, get peer's status
        match self.network.request_status(peer_id).await {
            Ok(SyncResponse::Status {
                block_number,
                block_hash: _,
                genesis_hash,
            }) => {
                // Keep the gate's status map fresh with this response too.
                self.record_peer_status(peer_id, block_number, genesis_hash);

                let our_genesis = self.chain.genesis_hash().unwrap_or_default();
                if genesis_hash != our_genesis {
                    warn!("Peer {} has different genesis hash!", peer_id);
                    return false;
                }

                let our_block_number = self.chain.block_number();
                if block_number > our_block_number {
                    info!(
                        "Peer {} is ahead: {} vs our {}",
                        peer_id, block_number, our_block_number
                    );
                    // Request blocks we're missing
                    self.sync_blocks_from_peer(peer_id, our_block_number + 1, block_number)
                        .await
                } else {
                    debug!("We're up to date with peer {}", peer_id);
                    true
                }
            }
            Ok(other) => {
                warn!("Unexpected status response from peer: {:?}", other);
                false
            }
            Err(e) => {
                error!("Failed to get status from peer {}: {}", peer_id, e);
                false
            }
        }
    }

    /// Sync blocks from a peer. Returns true when the whole `start..=end`
    /// range was walked without a hard failure.
    async fn sync_blocks_from_peer(&self, peer_id: PeerId, start: u64, end: u64) -> bool {
        let mut current = start;
        let mut clean = true;

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
                    let mut hard_failure = false;
                    for block_data in blocks {
                        if let Ok(block) = borsh::from_slice::<Block>(&block_data) {
                            let block_number = block.number();
                            let parent_hash = block.parent_hash();
                            match self.chain.import_block(block.clone()).await {
                                Ok(_) => {
                                    info!("Synced block #{}", block_number);
                                }
                                Err(qfc_chain::ChainError::BlockAlreadyKnown) => {
                                    debug!("Block #{} already known", block_number);
                                }
                                Err(qfc_chain::ChainError::InvalidParent { .. }) => {
                                    // Fork healing: the peer's branch diverges
                                    // below this range. Queue the block and walk
                                    // backwards by hash to the common ancestor;
                                    // the fork-choice import will reorg once the
                                    // branch connects. Stop hammering the rest of
                                    // the batch — it all descends from this block.
                                    info!(
                                        "Synced block #{} does not connect; walking back to common ancestor",
                                        block_number
                                    );
                                    {
                                        let mut pending = self.pending_blocks.write();
                                        if pending.len() < MAX_PENDING_BLOCKS {
                                            pending.push_back(block);
                                        }
                                    }
                                    self.request_missing_blocks(parent_hash);
                                    hard_failure = true;
                                    break;
                                }
                                Err(e) => {
                                    // Hard failure (validation/execution): the
                                    // rest of the batch builds on this block, so
                                    // importing it would fail identically —
                                    // break instead of hammering.
                                    warn!("Failed to import synced block #{}: {}", block_number, e);
                                    hard_failure = true;
                                    break;
                                }
                            }
                        }
                    }
                    if hard_failure {
                        clean = false;
                        break;
                    }
                    current = request_end + 1;
                }
                Ok(SyncResponse::NotFound) => {
                    debug!("No more blocks available from peer");
                    clean = false;
                    break;
                }
                Ok(other) => {
                    warn!("Unexpected response: {:?}", other);
                    clean = false;
                    break;
                }
                Err(e) => {
                    error!("Sync failed: {}", e);
                    clean = false;
                    break;
                }
            }
        }
        clean
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

#[cfg(test)]
mod tests {
    use super::SyncManager;
    use libp2p::PeerId;

    /// Peer selection prefers the highest verified head; the cursor rotates
    /// through candidates (next-best first) on failed syncs; empty candidate
    /// lists select nobody (never an arbitrary `peers().first()`).
    #[test]
    fn pick_sync_peer_prefers_highest_head_and_rotates() {
        let a = PeerId::random();
        let b = PeerId::random();
        let c = PeerId::random();
        let candidates = vec![(a, 5), (b, 9), (c, 7)];

        assert_eq!(SyncManager::pick_sync_peer(candidates.clone(), 0), Some(b));
        assert_eq!(SyncManager::pick_sync_peer(candidates.clone(), 1), Some(c));
        assert_eq!(SyncManager::pick_sync_peer(candidates.clone(), 2), Some(a));
        // Cursor wraps.
        assert_eq!(SyncManager::pick_sync_peer(candidates, 3), Some(b));
        // No verified candidates -> no sync target.
        assert_eq!(SyncManager::pick_sync_peer(Vec::new(), 0), None);
        assert_eq!(SyncManager::pick_sync_peer(Vec::new(), 5), None);
    }

    /// Equal heads tie-break deterministically by peer id.
    #[test]
    fn pick_sync_peer_tie_breaks_by_peer_id() {
        let mut ids = [PeerId::random(), PeerId::random()];
        ids.sort();
        let candidates = vec![(ids[1], 4), (ids[0], 4)];
        assert_eq!(SyncManager::pick_sync_peer(candidates, 0), Some(ids[0]));
    }

    /// Catch-up triggers only once we are more than the lag threshold (2)
    /// behind the highest known peer — never when level or only marginally
    /// behind (avoids thrashing on normal gossip latency).
    #[test]
    fn should_catch_up_threshold() {
        assert!(!SyncManager::should_catch_up(100, 0)); // no peer height yet
        assert!(!SyncManager::should_catch_up(100, 100)); // level
        assert!(!SyncManager::should_catch_up(100, 101)); // 1 behind
        assert!(!SyncManager::should_catch_up(100, 102)); // 2 behind (== threshold)
        assert!(SyncManager::should_catch_up(100, 103)); // 3 behind → catch up
        assert!(SyncManager::should_catch_up(0, 500_000)); // fresh node from genesis
    }
}
