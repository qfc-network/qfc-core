//! Blockchain management

use crate::error::{ChainError, Result};
use crate::genesis::{genesis_hash, GenesisConfig};
use parking_lot::RwLock;
use qfc_consensus::ConsensusEngine;
use qfc_crypto::blake3_hash;
use qfc_executor::Executor;
use qfc_state::StateDB;
use qfc_storage::{cf, encode_block_number, Database, WriteBatch};
use qfc_types::{
    block_reward_for_year, Address, Block, BlockBody, BlockHeader, Epoch, EthTxMeta, Hash, Receipt,
    SealedBlock, Transaction, ValidatorNode, BLOCK_INTERVAL_MS, FEE_PRODUCER_PERCENT,
    FEE_TREASURY_PERCENT, U256,
};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::mpsc::Sender;
use tracing::{debug, info, warn};

/// Maximum reorg depth: a fork whose common ancestor is deeper than this
/// below the new tip is never adopted (spec §4 of ADR-0012).
pub const MAX_REORG_DEPTH: u64 = 64;

/// The result of executing a block body against its parent state.
#[derive(Clone, Debug)]
pub struct ExecutionOutcome {
    /// Post-execution state root
    pub state_root: Hash,
    /// Receipts for the executed transactions
    pub receipts: Vec<Receipt>,
    /// Total gas used
    pub gas_used: u64,
}

/// Chain configuration
#[derive(Clone, Debug)]
pub struct ChainConfig {
    /// Chain ID
    pub chain_id: u64,
    /// Genesis configuration
    pub genesis: GenesisConfig,
    /// ADR-0017 hardfork gate for the EVM opcode fixes (BLOCKHASH /
    /// EXTCODEHASH / PREVRANDAO). This is a CONSENSUS CONSTANT: every node
    /// must use [`qfc_types::EVM_OPCODE_ACTIVATION_HEIGHT`] (the default) —
    /// a per-node value is a silent consensus fork. Overridable ONLY so
    /// tests can exercise post-activation semantics on short chains.
    pub evm_opcode_activation_height: u64,
}

impl Default for ChainConfig {
    fn default() -> Self {
        Self {
            chain_id: qfc_types::DEFAULT_CHAIN_ID,
            genesis: GenesisConfig::testnet(),
            evm_opcode_activation_height: qfc_types::EVM_OPCODE_ACTIVATION_HEIGHT,
        }
    }
}

/// Blockchain state and management
pub struct Chain {
    /// Database
    db: Database,
    /// State database
    state: Arc<StateDB>,
    /// Transaction executor
    executor: Executor,
    /// Consensus engine
    consensus: Arc<ConsensusEngine>,
    /// Chain configuration
    config: ChainConfig,
    /// Current head block
    head: RwLock<Option<SealedBlock>>,
    /// Genesis hash
    genesis_hash: RwLock<Option<Hash>>,
    /// Serializes ALL canonical-chain mutations: gossip imports, catch-up
    /// imports, backward-walk imports, pending rescans AND
    /// `store_produced_block`. Kills the shared-StateDB import races (D10).
    /// tokio Mutex so sync paths (Phase B gate, catch-up loop) can hold it
    /// across awaits.
    import_lock: tokio::sync::Mutex<()>,
    /// Last finalized (height, hash). Reorgs never cross it. Starts at
    /// (0, genesis) until finality votes land.
    finalized: RwLock<(u64, Hash)>,
    /// Optional sink for transactions displaced by a reorg. When a reorg
    /// abandons a canonical block, every tx it carried that is NOT present
    /// on the winning branch is sent here so the node can re-inject it into
    /// the mempool — otherwise a deploy/transfer orphaned by a fork is lost
    /// forever (phantom-receipt / reorg-cleanup fix, ADR-0013).
    reorg_tx_sink: RwLock<Option<Sender<Transaction>>>,
}

impl Chain {
    /// Create a new chain
    pub fn new(db: Database, config: ChainConfig, consensus: Arc<ConsensusEngine>) -> Result<Self> {
        let state = Arc::new(StateDB::new(db.clone()));
        let executor = Executor::new(config.chain_id);

        let chain = Self {
            db,
            state,
            executor,
            consensus,
            config,
            head: RwLock::new(None),
            genesis_hash: RwLock::new(None),
            import_lock: tokio::sync::Mutex::new(()),
            finalized: RwLock::new((0, Hash::ZERO)),
            reorg_tx_sink: RwLock::new(None),
        };

        // Initialize genesis if needed
        chain.init_genesis()?;

        Ok(chain)
    }

    /// Initialize genesis block
    fn init_genesis(&self) -> Result<()> {
        // Refuse to open a database created for a different chain (§6):
        // a chain_id mismatch silently forks execution (EVM chain id,
        // signature domains), so it must be a hard startup error.
        if let Some(stored) = self.db.get(cf::METADATA, qfc_storage::meta::CHAIN_ID)? {
            if stored.len() == 8 {
                let stored = u64::from_le_bytes(stored.try_into().unwrap());
                if stored != self.config.chain_id {
                    return Err(ChainError::ChainIdMismatch {
                        stored,
                        configured: self.config.chain_id,
                    });
                }
            }
        }

        // Check if genesis already exists
        if let Some(genesis_hash) = self.db.get(cf::METADATA, qfc_storage::meta::GENESIS_HASH)? {
            let hash = Hash::from_slice(&genesis_hash).ok_or_else(|| {
                ChainError::Storage("Invalid genesis hash in database".to_string())
            })?;

            *self.genesis_hash.write() = Some(hash);
            *self.finalized.write() = (0, hash);

            // Anchor the deterministic epoch-seed derivation BEFORE anything
            // can run a producer/miner/sync task (D11: genesis_seed race).
            let mut seed = [0u8; 32];
            seed.copy_from_slice(hash.as_bytes());
            self.consensus.set_genesis_seed(seed);

            // Load head block
            if let Some(head_bytes) = self
                .db
                .get(cf::METADATA, qfc_storage::meta::LATEST_BLOCK_NUMBER)?
            {
                if head_bytes.len() == 8 {
                    let height = u64::from_le_bytes(head_bytes.try_into().unwrap());
                    if let Some(block) = self.get_block_by_number(height)? {
                        let block_hash = blake3_hash(&block.header_bytes());
                        *self.head.write() = Some(SealedBlock::new(block_hash, block));
                    }
                }
            }

            // Restore state root from the latest block
            if let Some(state_root_bytes) = self
                .db
                .get(cf::METADATA, qfc_storage::meta::LATEST_STATE_ROOT)?
            {
                if let Some(state_root) = Hash::from_slice(&state_root_bytes) {
                    self.state.set_root(state_root);
                    info!("Restored state root: {}", state_root);
                }
            } else if let Some(head) = self.head.read().as_ref() {
                // Fallback: use head block's state root
                let state_root = head.block.state_root();
                self.state.set_root(state_root);
                info!("Restored state root from head block: {}", state_root);
            }

            // Restore consensus state (validators, epoch, finalized height)
            // from the latest checkpoint. Only fall back to re-deriving from
            // genesis when no usable checkpoint exists — registering genesis
            // validators after a successful restore would clobber the
            // checkpointed state (stake, scores, jail status, epoch).
            if !self.load_validator_checkpoint() {
                self.register_genesis_validators();
            }

            info!("Loaded chain with genesis: {}", hash);
            return Ok(());
        }

        info!("Initializing genesis block");

        // Build genesis block
        let mut genesis = self.config.genesis.build_genesis_block();

        // Apply allocations
        for (address, balance) in self.config.genesis.parse_allocations() {
            self.state.set_balance(&address, balance)?;
            debug!("Genesis allocation: {} = {}", address, balance);
        }

        // Apply validators
        for (address, _public_key, stake) in self.config.genesis.parse_validators() {
            self.state.set_stake(&address, stake)?;
            debug!("Genesis validator: {} stake = {}", address, stake);
        }

        // Commit state and get root
        let state_root = self.state.commit()?;
        genesis.header.state_root = state_root;

        // Compute genesis hash
        let hash = genesis_hash(&genesis);

        // Store genesis block, chain metadata, and head metadata in a single
        // atomic batch so a crash during init can never leave a partially
        // initialized chain (e.g. a stored genesis block without GENESIS_HASH).
        let mut batch = WriteBatch::new();
        Self::append_block_to_batch(&mut batch, &genesis);
        Self::append_receipts_and_head_to_batch(&mut batch, &genesis, &[], &state_root);
        batch.put(
            cf::METADATA,
            qfc_storage::meta::GENESIS_HASH.to_vec(),
            hash.as_bytes().to_vec(),
        );
        batch.put(
            cf::METADATA,
            qfc_storage::meta::CHAIN_ID.to_vec(),
            self.config.chain_id.to_le_bytes().to_vec(),
        );
        self.db.write_batch_sync(batch)?;

        *self.genesis_hash.write() = Some(hash);
        *self.finalized.write() = (0, hash);
        *self.head.write() = Some(SealedBlock::new(hash, genesis));

        // Anchor the deterministic epoch-seed derivation (D11) — set exactly
        // once, before any producer/miner/sync task can observe the engine.
        let mut seed = [0u8; 32];
        seed.copy_from_slice(hash.as_bytes());
        self.consensus.set_genesis_seed(seed);

        // Register genesis validators with consensus engine
        self.register_genesis_validators();

        info!("Genesis block created: {}", hash);

        Ok(())
    }

    /// Register genesis validators with the consensus engine
    fn register_genesis_validators(&self) {
        let validators: Vec<ValidatorNode> = self
            .config
            .genesis
            .parse_validators()
            .into_iter()
            .map(|(address, public_key, stake)| {
                let mut v = ValidatorNode::default();
                v.address = address;
                v.public_key = public_key;
                v.stake = stake;
                v.contribution_score = 1000; // Default contribution score
                info!(
                    "Registering genesis validator: {} (pubkey set: {})",
                    address,
                    public_key != qfc_types::PublicKey::ZERO
                );
                v
            })
            .collect();

        if !validators.is_empty() {
            self.consensus.update_validators(validators);
        }
    }

    /// Load the latest validator checkpoint from storage and restore
    /// consensus state from it. Returns `true` if a checkpoint was restored.
    fn load_validator_checkpoint(&self) -> bool {
        match self.consensus.load_latest_checkpoint(&self.db) {
            Ok(Some(checkpoint)) => {
                self.consensus.restore_from_checkpoint(&checkpoint);
                info!(
                    "Loaded validator checkpoint: epoch={}, height={}, validators={}",
                    checkpoint.epoch,
                    checkpoint.block_height,
                    checkpoint.validators.len()
                );
                true
            }
            Ok(None) => {
                debug!("No validator checkpoint found, using genesis validators");
                false
            }
            Err(e) => {
                warn!(
                    "Failed to load validator checkpoint, using genesis validators: {}",
                    e
                );
                false
            }
        }
    }

    /// Create checkpoint if at epoch boundary
    pub fn maybe_create_checkpoint(&self, block_height: u64) -> Result<()> {
        // Check if at epoch boundary
        let blocks_per_epoch = qfc_types::BLOCKS_PER_EPOCH;
        if block_height % blocks_per_epoch != 0 {
            return Ok(());
        }

        match self.consensus.create_checkpoint(&self.db, block_height) {
            Ok(checkpoint) => {
                info!(
                    "Created checkpoint at epoch {} height {}",
                    checkpoint.epoch, checkpoint.block_height
                );
            }
            Err(e) => {
                warn!("Failed to create checkpoint: {}", e);
            }
        }

        Ok(())
    }

    /// Get genesis hash
    pub fn genesis_hash(&self) -> Option<Hash> {
        *self.genesis_hash.read()
    }

    /// Get current head block
    pub fn head(&self) -> Option<SealedBlock> {
        self.head.read().clone()
    }

    /// Get current block number
    pub fn block_number(&self) -> u64 {
        self.head.read().as_ref().map(|h| h.number()).unwrap_or(0)
    }

    /// Get state root
    pub fn state_root(&self) -> Hash {
        self.state.root()
    }

    /// Get a block by number
    pub fn get_block_by_number(&self, number: u64) -> Result<Option<Block>> {
        let key = encode_block_number(number);

        // Get header
        let header_bytes = match self.db.get(cf::BLOCK_HEADERS, &key)? {
            Some(b) => b,
            None => return Ok(None),
        };

        let header: BlockHeader =
            borsh::from_slice(&header_bytes).map_err(|e| ChainError::Storage(e.to_string()))?;

        // Get body
        let body_bytes = match self.db.get(cf::BLOCK_BODIES, &key)? {
            Some(b) => b,
            None => return Ok(None),
        };

        let body: BlockBody =
            borsh::from_slice(&body_bytes).map_err(|e| ChainError::Storage(e.to_string()))?;

        Ok(Some(Block {
            header,
            transactions: body.transactions,
            votes: body.votes,
            inference_proofs: body.inference_proofs,
            signature: body.signature,
        }))
    }

    /// Get a block by hash. Finds canonical AND side-branch blocks — the
    /// hash-keyed store holds every block ever imported or produced.
    pub fn get_block_by_hash(&self, hash: &Hash) -> Result<Option<Block>> {
        if let Some(bytes) = self.db.get(cf::BLOCKS_BY_HASH, hash.as_bytes())? {
            let block: Block =
                borsh::from_slice(&bytes).map_err(|e| ChainError::Storage(e.to_string()))?;
            return Ok(Some(block));
        }

        // Legacy fallback (databases written before the hash-keyed store):
        // resolve through the canonical number index.
        let number_bytes = match self.db.get(cf::BLOCK_HASH_INDEX, hash.as_bytes())? {
            Some(b) => b,
            None => return Ok(None),
        };

        if number_bytes.len() != 8 {
            return Ok(None);
        }

        let number = u64::from_be_bytes(number_bytes.try_into().unwrap());
        self.get_block_by_number(number)
    }

    /// Whether `hash` is the canonical block at `number`.
    pub fn is_canonical(&self, hash: &Hash, number: u64) -> Result<bool> {
        match self.get_block_by_number(number)? {
            Some(b) => Ok(blake3_hash(&b.header_bytes()) == *hash),
            None => Ok(false),
        }
    }

    /// Get a transaction by hash
    pub fn get_transaction(&self, hash: &Hash) -> Result<Option<Transaction>> {
        let tx_bytes = match self.db.get(cf::TRANSACTIONS, hash.as_bytes())? {
            Some(b) => b,
            None => return Ok(None),
        };

        let tx: Transaction =
            borsh::from_slice(&tx_bytes).map_err(|e| ChainError::Storage(e.to_string()))?;

        Ok(Some(tx))
    }

    /// Get a receipt by transaction hash
    pub fn get_receipt(&self, hash: &Hash) -> Result<Option<Receipt>> {
        let receipt_bytes = match self.db.get(cf::RECEIPTS, hash.as_bytes())? {
            Some(b) => b,
            None => return Ok(None),
        };

        let receipt: Receipt =
            borsh::from_slice(&receipt_bytes).map_err(|e| ChainError::Storage(e.to_string()))?;

        Ok(Some(receipt))
    }

    /// Get transaction location (block_height, tx_index) by hash
    pub fn get_transaction_location(&self, hash: &Hash) -> Result<Option<(u64, u32)>> {
        let location_bytes = match self.db.get(cf::TX_INDEX, hash.as_bytes())? {
            Some(b) => b,
            None => return Ok(None),
        };

        Ok(qfc_storage::decode_tx_location(&location_bytes))
    }

    /// Store Ethereum transaction hash mapping (keccak256 -> blake3)
    /// This allows looking up transactions/receipts by the hash returned to Ethereum wallets
    ///
    /// Note: this is intentionally *not* part of the atomic block-commit batch
    /// ([`Self::commit_block`]). Its only caller is the RPC server's
    /// `eth_sendRawTransaction` path, which records the mapping at submission
    /// time — before the transaction is in any block — so there is no block
    /// commit to be atomic with. Losing this mapping in a crash only degrades
    /// hash translation for a not-yet-mined tx; it cannot orphan block data.
    pub fn store_eth_tx_hash_mapping(&self, eth_hash: &Hash, internal_hash: &Hash) -> Result<()> {
        self.db.put(
            cf::ETH_TX_INDEX,
            eth_hash.as_bytes(),
            internal_hash.as_bytes(),
        )?;
        Ok(())
    }

    /// Store render-only Ethereum tx metadata keyed by the internal BLAKE3 hash.
    ///
    /// Companion to [`Self::store_eth_tx_hash_mapping`]: that records the
    /// forward eth->internal hash index used to *find* a tx; this records the
    /// reverse data needed to *render* it back to a wallet (canonical keccak
    /// hash, full-width `v`, EIP-2718 envelope type, EIP-1559 fees). Like the
    /// forward mapping it is written at RPC submission time and is explicitly
    /// NOT part of the atomic block-commit batch or any state root.
    pub fn store_eth_tx_meta(&self, internal_hash: &Hash, meta: &EthTxMeta) -> Result<()> {
        self.db
            .put(cf::ETH_TX_META, internal_hash.as_bytes(), &meta.to_bytes())?;
        Ok(())
    }

    /// Fetch render-only Ethereum tx metadata by internal hash, if present.
    /// Absent for native QFC transactions (and for eth txs whose submission
    /// node was a different peer), in which case callers fall back to the
    /// internal hash and a legacy envelope.
    pub fn get_eth_tx_meta(&self, internal_hash: &Hash) -> Result<Option<EthTxMeta>> {
        match self.db.get(cf::ETH_TX_META, internal_hash.as_bytes())? {
            Some(bytes) => Ok(EthTxMeta::from_bytes(&bytes)),
            None => Ok(None),
        }
    }

    /// Translate Ethereum hash to internal hash if it exists
    /// Returns the internal hash if this is an Ethereum transaction, otherwise returns the original hash
    pub fn translate_eth_hash(&self, hash: &Hash) -> Result<Hash> {
        match self.db.get(cf::ETH_TX_INDEX, hash.as_bytes())? {
            Some(internal_bytes) => Hash::from_slice(&internal_bytes)
                .ok_or_else(|| ChainError::Storage("Invalid internal hash".to_string())),
            None => Ok(*hash), // Not an Ethereum tx, return as-is
        }
    }

    /// Resolve a tx hash to its CANONICAL block + position, or `None`.
    ///
    /// This is the single source of truth for "is this tx actually on the
    /// canonical chain right now?". A reorg can leave a stale `TX_INDEX` /
    /// `RECEIPTS` row for a tx that was only ever in an abandoned block
    /// (phantom receipt). Rather than trust the recorded location, we load
    /// the canonical block at that height and verify the tx really sits at
    /// the recorded index by re-hashing it. A mismatch (or a missing block /
    /// out-of-range index) means the row is stale and the tx is NOT
    /// canonical — callers then report it as pending, which is correct: the
    /// client keeps waiting / resubmits.
    pub fn canonical_tx_at(&self, hash: &Hash) -> Result<Option<(Block, usize)>> {
        let (block_height, tx_index) = match self.get_transaction_location(hash)? {
            Some(loc) => loc,
            None => return Ok(None),
        };
        let block = match self.get_block_by_number(block_height)? {
            Some(b) => b,
            None => return Ok(None),
        };
        let idx = tx_index as usize;
        match block.transactions.get(idx) {
            Some(tx) if blake3_hash(&tx.to_bytes_without_signature()) == *hash => {
                Ok(Some((block, idx)))
            }
            _ => Ok(None),
        }
    }

    /// Get receipt with block info.
    ///
    /// Returns `None` for a tx that is not canonically at its recorded
    /// location (a phantom receipt left behind by a reorg): the receipt is
    /// stale, so the RPC reports the tx as pending.
    pub fn get_receipt_with_block_info(&self, hash: &Hash) -> Result<Option<(Receipt, Hash, u64)>> {
        // Verify the tx is actually on the canonical chain at its recorded
        // position before trusting the receipt (guards against phantom
        // receipts from abandoned branches).
        let block = match self.canonical_tx_at(hash)? {
            Some((block, _idx)) => block,
            None => return Ok(None),
        };

        let receipt = match self.get_receipt(hash)? {
            Some(r) => r,
            None => return Ok(None),
        };

        let block_hash = blake3_hash(&block.header_bytes());
        Ok(Some((receipt, block_hash, block.number())))
    }

    /// Install the sink that receives transactions displaced by a reorg
    /// (see [`Self::reorg_tx_sink`]). The node wires this to a task that
    /// re-injects them into the mempool.
    pub fn set_reorg_tx_sink(&self, sink: Sender<Transaction>) {
        *self.reorg_tx_sink.write() = Some(sink);
    }

    // ============ Shared deterministic state transition (spec §1) ============

    /// Execute a block body against an explicit parent state root, on a
    /// SCRATCH state instance — the live head state is never touched, so a
    /// failed import can never poison it (D10). This is THE single state
    /// transition, used byte-identically by block production and block
    /// import (D7): mature undelegations (block-timestamp clock), then
    /// transactions, then the deterministic reward application.
    ///
    /// Note: committing the scratch trie writes content-addressed trie nodes
    /// to storage even when the resulting root is later rejected; unreferenced
    /// nodes are harmless garbage (pruning collects them).
    pub fn execute_at(
        &self,
        parent_state_root: Hash,
        parent_hash: Hash,
        block_number: u64,
        timestamp_ms: u64,
        producer: &Address,
        transactions: &[Transaction],
    ) -> Result<ExecutionOutcome> {
        let scratch = StateDB::with_root(self.db.clone(), parent_state_root);

        // Per-execution executor with the block context set: the EVM's
        // TIMESTAMP/NUMBER opcodes and every timestamp-derived transition
        // (undelegation `unlock_at`) see the block's own values instead of
        // zeros or the local wall clock. A local instance keeps this method
        // `&self` and race-free against concurrent executions.
        // `parent_hash` (the executing block's parent) anchors the ADR-0017
        // opcode context: PREVRANDAO and the BLOCKHASH ancestor walk.
        let mut executor = Executor::new(self.config.chain_id);
        executor.set_block_context(
            block_number,
            timestamp_ms,
            qfc_types::DEFAULT_BLOCK_GAS_LIMIT,
            parent_hash,
        );
        executor.set_block_hash_lookup(self.block_hash_lookup());
        executor.set_evm_opcode_activation_height(self.config.evm_opcode_activation_height);

        // 1. Mature undelegations — maturity clock is the BLOCK timestamp,
        //    never the local wall clock (D12).
        executor.process_mature_undelegations(&scratch, timestamp_ms / 1000);

        // 2. Execute transactions.
        let (receipts, gas_used) = executor.execute_transactions(transactions, &scratch, producer);

        // 3. Deterministic rewards — pure function of the block contents.
        let total_fees = Self::total_fees(transactions, &receipts);
        Self::apply_block_rewards(&scratch, block_number, producer, total_fees)?;

        let state_root = scratch.commit()?;
        Ok(ExecutionOutcome {
            state_root,
            receipts,
            gas_used,
        })
    }

    /// Execute `block` against `parent_state_root` (see [`Self::execute_at`]).
    pub fn execute_block(
        &self,
        block: &Block,
        parent_state_root: Hash,
    ) -> Result<ExecutionOutcome> {
        self.execute_at(
            parent_state_root,
            block.parent_hash(),
            block.number(),
            block.timestamp(),
            &block.producer(),
            &block.transactions,
        )
    }

    /// Build the BLOCKHASH resolver for the EVM (ADR-0017): resolves
    /// `wanted` by walking parent hashes from `start` down through the
    /// HASH-KEYED block store (`cf::BLOCKS_BY_HASH`) — deliberately NEVER
    /// through the canonical number index, which during `reorg_to`'s branch
    /// re-execution still holds the OLD branch (the atomic batch swaps it
    /// only after the whole branch re-executed). Walking the executing
    /// block's own ancestor chain gives the correct hashes on every path:
    /// produce, import, reorg re-execution and simulation.
    ///
    /// The walk is lazy (only runs when a contract actually executes
    /// BLOCKHASH) and bounded by the opcode's 256-block window plus the
    /// executor-side range check. Every block within that window of a
    /// post-activation block is present in `BLOCKS_BY_HASH` (all recent
    /// binaries write it in the commit batch; fresh syncs re-write history).
    fn block_hash_lookup(&self) -> qfc_executor::BlockHashLookup {
        let db = self.db.clone();
        Arc::new(move |start: &Hash, wanted: u64| -> Option<Hash> {
            let mut cursor = *start;
            loop {
                let bytes = db.get(cf::BLOCKS_BY_HASH, cursor.as_bytes()).ok()??;
                let block: Block = borsh::from_slice(&bytes).ok()?;
                match block.number() {
                    n if n == wanted => return Some(cursor),
                    // Walked past the target (or reached genesis) without
                    // finding it — out of range for this ancestor chain.
                    n if n < wanted || n == 0 => return None,
                    _ => cursor = block.parent_hash(),
                }
            }
        })
    }

    /// Total transaction fees: Σ gas_used × gas_price.
    fn total_fees(transactions: &[Transaction], receipts: &[Receipt]) -> U256 {
        let mut total = U256::ZERO;
        for (tx, receipt) in transactions.iter().zip(receipts.iter()) {
            total += U256::from_u64(receipt.gas_used) * tx.gas_price;
        }
        total
    }

    /// Apply block rewards deterministically (spec §1):
    /// - producer: full year-halved block reward + FEE_PRODUCER_PERCENT of fees
    /// - treasury: FEE_TREASURY_PERCENT of fees
    /// - the remaining fee share is burned (senders already paid; simply not
    ///   re-credited)
    ///
    /// Deliberately REMOVED from the block path (they were node-local and
    /// therefore forked every import; see ADR-0012):
    /// - voter split (blocks carry no votes; "all locally-active validators"
    ///   is not deterministic) — restore when votes are in-block
    /// - inference fee settlement (TaskPool is node-local) — moves on-chain
    ///   later; RPC/TaskPool behavior is otherwise unchanged
    /// - dynamic network-state multipliers (no production setter exists);
    ///   NetworkState::Normal is hard-coded by using none at all
    fn apply_block_rewards(
        state: &StateDB,
        block_number: u64,
        producer: &Address,
        total_fees: U256,
    ) -> Result<()> {
        // Halving year from the REAL block interval (§6 — the old math used
        // a stale 3333 ms constant).
        let blocks_per_year = 365 * 24 * 60 * 60 * 1000 / BLOCK_INTERVAL_MS;
        let year = block_number / blocks_per_year;
        let block_reward = block_reward_for_year(year);

        let producer_fee_share =
            total_fees * U256::from_u64(FEE_PRODUCER_PERCENT) / U256::from_u64(100);
        state.add_balance(producer, block_reward + producer_fee_share)?;

        let treasury_fee = total_fees * U256::from_u64(FEE_TREASURY_PERCENT) / U256::from_u64(100);
        if !treasury_fee.is_zero() {
            let treasury_addr = Address::new(qfc_types::TREASURY_ADDRESS_BYTES);
            state.add_balance(&treasury_addr, treasury_fee)?;
        }

        Ok(())
    }

    // ============ Import + fork choice (spec §4) ============

    /// Import a block (gossip, catch-up, backward-walk, pending rescans).
    ///
    /// All imports and `store_produced_block` serialize through one lock, so
    /// concurrent paths can never interleave commits (D10).
    pub async fn import_block(&self, block: Block) -> Result<Hash> {
        let _guard = self.import_lock.lock().await;
        self.import_block_locked(block)
    }

    /// Blocking variant of [`Self::import_block`] for non-async contexts
    /// (tests, tools). Panics if called from within an async runtime.
    pub fn import_block_blocking(&self, block: Block) -> Result<Hash> {
        let _guard = self.import_lock.blocking_lock();
        self.import_block_locked(block)
    }

    fn import_block_locked(&self, block: Block) -> Result<Hash> {
        let block_hash = blake3_hash(&block.header_bytes());

        // Check if block already exists (canonical or side branch)
        if self
            .db
            .get(cf::BLOCKS_BY_HASH, block_hash.as_bytes())?
            .is_some()
            || self
                .db
                .get(cf::BLOCK_HASH_INDEX, block_hash.as_bytes())?
                .is_some()
        {
            return Err(ChainError::BlockAlreadyKnown);
        }

        // Check for double-sign before processing
        if let Some(evidence) = self.consensus.check_double_sign(&block) {
            // Process the evidence (slash the validator)
            if let Err(e) = self
                .consensus
                .process_double_sign_evidence(&evidence, &self.db)
            {
                debug!("Failed to process double-sign evidence: {}", e);
            }
            // Store evidence for later broadcast
            self.store_double_sign_evidence(&evidence);
            // Block from double-signer should still be rejected if invalid
        }

        // Cache block for future double-sign detection
        self.consensus.cache_block(&block);

        // Get parent block (canonical or side branch)
        let parent = self
            .get_block_by_hash(&block.parent_hash())?
            .ok_or_else(|| ChainError::InvalidParent {
                expected: "existing block".to_string(),
                actual: block.parent_hash().to_string(),
            })?;

        // Validate block (self-contained: schedule, VRF, timestamps, roots)
        self.consensus.validate_block(&block, &parent)?;

        // Verify inference proofs root (v2.0)
        if block.header.version >= 2 || !block.inference_proofs.is_empty() {
            let proof_hashes: Vec<Hash> = block
                .inference_proofs
                .iter()
                .map(|p| blake3_hash(&p.to_bytes_without_signature()))
                .collect();
            let expected_proofs_root = qfc_crypto::merkle_root(&proof_hashes);
            if block.header.proofs_root != expected_proofs_root {
                return Err(ChainError::InvalidBlock(
                    "Inference proofs root mismatch".to_string(),
                ));
            }
        }

        // Execute against the PARENT's state root on a scratch state — the
        // live head state is untouched until the block is accepted (D10).
        let outcome = self.execute_block(&block, parent.state_root())?;
        if outcome.state_root != block.state_root() {
            return Err(ChainError::InvalidBlock("State root mismatch".to_string()));
        }
        if outcome.gas_used != block.gas_used() {
            return Err(ChainError::InvalidBlock("Gas used mismatch".to_string()));
        }

        // Apply inference scores from block proofs (observability scoring)
        for proof in &block.inference_proofs {
            self.consensus
                .update_inference_score(&proof.validator, proof.flops_estimated, 1);
        }

        // Fork choice: canonical = highest number, tie-break lowest hash.
        let head = self
            .head
            .read()
            .clone()
            .ok_or(ChainError::GenesisNotFound)?;

        if block.parent_hash() == head.hash {
            // Fast path: extends the canonical head.
            self.commit_canonical(&block, &outcome.receipts, &outcome.state_root)?;
            info!("Imported block {} at height {}", block_hash, block.number());
            return Ok(block_hash);
        }

        // Side branch: persist by hash only (branch store), then decide.
        self.db.put(
            cf::BLOCKS_BY_HASH,
            block_hash.as_bytes(),
            &borsh::to_vec(&block).map_err(|e| ChainError::Storage(e.to_string()))?,
        )?;

        let better = block.number() > head.number()
            || (block.number() == head.number() && block_hash < head.hash);
        if better {
            match self.reorg_to(block_hash, &block) {
                Ok(_displaced) => {
                    info!(
                        "Reorged to block {} at height {} (old head {} at {})",
                        block_hash,
                        block.number(),
                        head.hash,
                        head.number()
                    );
                }
                Err(e) => {
                    // The block itself is valid and stored on its branch; a
                    // refused reorg (finality guard, missing ancestor, failed
                    // branch re-execution) keeps the current head — accurate
                    // now that the canonical rewrite is a single atomic batch
                    // performed only after the whole branch re-executed.
                    warn!(
                        "Reorg to block {} at height {} refused: {}",
                        block_hash,
                        block.number(),
                        e
                    );
                }
            }
        } else {
            debug!(
                "Stored side-branch block {} at height {} (head {} at {})",
                block_hash,
                block.number(),
                head.hash,
                head.number()
            );
        }

        Ok(block_hash)
    }

    /// Reorganize the canonical chain onto the branch ending at `tip`.
    ///
    /// Walks parents (all by hash) to the first canonical ancestor, refuses
    /// any reorg whose resulting chain would not contain the finalized
    /// (height, hash), re-executes the ENTIRE new branch on scratch state,
    /// and only then rewrites the canonical index + head metadata in a
    /// single atomic batch — an `Err` anywhere leaves the old canonical
    /// chain fully intact.
    ///
    /// Depth policy (review fix 5): reorgs deeper than [`MAX_REORG_DEPTH`]
    /// are permitted — logged at WARN — as long as the new chain retains
    /// the finalized (height, hash); the cap remains a hard limit for
    /// branches that do NOT (those are refused at any depth, since with
    /// review fix 3 finality only ever advances on the canonical chain and
    /// an honest majority branch therefore always qualifies).
    ///
    /// Returns the transactions displaced by the reorg: every tx carried by
    /// an abandoned canonical block that is NOT present on the winning
    /// branch. Their stale `RECEIPTS` / `TX_INDEX` / `TRANSACTIONS` rows are
    /// deleted in the same atomic batch (before the new branch's writes, so
    /// a tx in BOTH branches keeps its correct new location), and each is
    /// forwarded to [`Self::reorg_tx_sink`] for mempool re-injection.
    fn reorg_to(&self, tip_hash: Hash, tip: &Block) -> Result<Vec<Transaction>> {
        let old_head = self
            .head
            .read()
            .clone()
            .ok_or(ChainError::GenesisNotFound)?;
        let (recorded_fin_height, recorded_fin_hash) = *self.finalized.read();
        let finalized_height = self.consensus.finalized_height().max(recorded_fin_height);

        // Collect the non-canonical branch, tip first. (No depth abort here:
        // the depth decision needs the finalized-containment answer below.)
        let mut branch: Vec<(Hash, Block)> = Vec::new();
        let mut cursor_hash = tip_hash;
        let mut cursor = tip.clone();
        loop {
            if self.is_canonical(&cursor_hash, cursor.number())? {
                break; // `cursor` is the common ancestor
            }
            branch.push((cursor_hash, cursor.clone()));
            let parent_hash = cursor.parent_hash();
            cursor =
                self.get_block_by_hash(&parent_hash)?
                    .ok_or_else(|| ChainError::InvalidParent {
                        expected: "existing branch ancestor".to_string(),
                        actual: parent_hash.to_string(),
                    })?;
            cursor_hash = parent_hash;
        }
        let ancestor = cursor;

        // Does the post-reorg canonical chain still contain the finalized
        // (height, hash)? Either the divergence point is at/above it (the
        // shared prefix keeps it — the finalized block is canonical, review
        // fix 3c) or the new branch itself carries that exact block at that
        // exact height (walk the NEW branch for the finalized hash, review
        // fix 3a).
        let retains_finalized = ancestor.number() >= finalized_height
            || (recorded_fin_height == finalized_height
                && branch.iter().any(|(hash, block)| {
                    block.number() == finalized_height && *hash == recorded_fin_hash
                }));

        if !retains_finalized {
            return Err(ChainError::InvalidBlock(format!(
                "reorg would abandon finalized height {finalized_height} (ancestor at {}, \
                 new branch does not contain the finalized hash)",
                ancestor.number()
            )));
        }

        if branch.len() as u64 > MAX_REORG_DEPTH {
            warn!(
                "Deep reorg: adopting a branch {} blocks deep (cap {} waived — the new \
                 chain retains the finalized height {})",
                branch.len(),
                MAX_REORG_DEPTH,
                finalized_height
            );
        }

        // Phase 1 — re-execute the ENTIRE new branch on scratch state first.
        // Roots were already verified when the branch blocks were imported;
        // this regenerates receipts and guards against storage rot. Nothing
        // canonical is written until every block has succeeded.
        let mut parent_root = ancestor.state_root();
        let mut executed: Vec<(&Hash, &Block, ExecutionOutcome)> = Vec::new();
        for (hash, block) in branch.iter().rev() {
            let outcome = self.execute_block(block, parent_root)?;
            if outcome.state_root != block.state_root() {
                return Err(ChainError::InvalidBlock(format!(
                    "state root mismatch re-executing branch block {hash}"
                )));
            }
            parent_root = outcome.state_root;
            executed.push((hash, block, outcome));
        }

        // Every tx hash carried by the winning branch — a tx present in BOTH
        // the abandoned and the new branch must KEEP its (new) receipt/index,
        // so it is excluded from the phantom-row deletion below.
        let new_branch_tx_hashes: HashSet<Hash> = executed
            .iter()
            .flat_map(|(_, block, _)| block.transactions.iter())
            .map(|tx| blake3_hash(&tx.to_bytes_without_signature()))
            .collect();

        // Phase 2 — single atomic batch: drop the old branch's canonical
        // hash-index entries AND the phantom receipt/tx-index/tx rows for the
        // txs it carried that are not on the new branch (its blocks stay
        // available in the hash-keyed branch store), write every new branch
        // block canonically, and repoint the head metadata. RocksDB applies
        // batch ops in insertion order, so the new-branch writes below land
        // AFTER these deletes — a tx in both branches ends up with its
        // correct new receipt/location. The whole batch is atomic, so a
        // crash or error can never leave a half-rewritten index.
        let mut batch = WriteBatch::new();
        let mut displaced: Vec<Transaction> = Vec::new();
        for number in (ancestor.number() + 1)..=old_head.number() {
            if let Some(old) = self.get_block_by_number(number)? {
                let old_hash = blake3_hash(&old.header_bytes());
                batch.delete(cf::BLOCK_HASH_INDEX, old_hash.as_bytes().to_vec());

                for tx in &old.transactions {
                    let tx_hash = blake3_hash(&tx.to_bytes_without_signature());
                    if new_branch_tx_hashes.contains(&tx_hash) {
                        continue; // survives on the new branch — keep its rows
                    }
                    batch.delete(cf::RECEIPTS, tx_hash.as_bytes().to_vec());
                    batch.delete(cf::TX_INDEX, tx_hash.as_bytes().to_vec());
                    batch.delete(cf::TRANSACTIONS, tx_hash.as_bytes().to_vec());
                    displaced.push(tx.clone());
                }
            }
        }
        for (_, block, outcome) in &executed {
            Self::append_block_to_batch(&mut batch, block);
            Self::append_receipts_and_head_to_batch(
                &mut batch,
                block,
                &outcome.receipts,
                &outcome.state_root,
            );
        }
        self.db.write_batch_sync(batch)?;

        // Durable commit succeeded: adopt the new tip in memory and run the
        // per-block consensus hooks.
        let tip_root = executed
            .last()
            .map(|(_, _, o)| o.state_root)
            .unwrap_or_else(|| ancestor.state_root());
        self.state.set_root(tip_root);
        *self.head.write() = Some(SealedBlock::new(tip_hash, tip.clone()));
        for (_, block, _) in &executed {
            self.consensus.record_block_produced(&block.producer());
            let _ = self.maybe_create_checkpoint(block.number());
        }

        // Forward displaced txs for mempool re-injection (best effort: a
        // missing/closed sink just means no re-injection is wired). The
        // channel is bounded; under a reorg storm `try_send` drops rather
        // than growing memory — a dropped tx just isn't re-proposed by this
        // node (peers still hold it, and the client can resubmit).
        if !displaced.is_empty() {
            if let Some(sink) = self.reorg_tx_sink.read().as_ref() {
                for tx in &displaced {
                    let _ = sink.try_send(tx.clone());
                }
            }
        }

        Ok(displaced)
    }

    /// Commit a block as the new canonical head: header/body/indexes/receipts
    /// and head metadata land in a single atomic batch, then the live state
    /// adopts the block's root and the in-memory head advances.
    fn commit_canonical(
        &self,
        block: &Block,
        receipts: &[Receipt],
        state_root: &Hash,
    ) -> Result<Hash> {
        let block_hash = self.commit_block(block, receipts, state_root)?;

        // The live state view (RPC/mempool reads) adopts the committed root.
        self.state.set_root(*state_root);

        // Update in-memory head (only after the durable commit succeeded)
        *self.head.write() = Some(SealedBlock::new(block_hash, block.clone()));

        // Record block production in consensus engine for PoC scoring
        self.consensus.record_block_produced(&block.producer());

        // Maybe create checkpoint at epoch boundary
        let _ = self.maybe_create_checkpoint(block.number());

        Ok(block_hash)
    }

    /// Record a finalized (height, hash). Reorgs never cross it.
    ///
    /// Only records when `hash` is canonical locally at `height` (review
    /// fix 3c): finalizing a side-branch or unknown block would wedge every
    /// future reorg — the finality guard would then demand a hash the
    /// canonical chain can never contain.
    pub fn record_finalized(&self, height: u64, hash: Hash) {
        match self.is_canonical(&hash, height) {
            Ok(true) => {}
            _ => {
                debug!(
                    "Ignoring finalization of non-canonical block {} at height {}",
                    hash, height
                );
                return;
            }
        }
        let mut fin = self.finalized.write();
        if height > fin.0 {
            *fin = (height, hash);
        }
        self.consensus
            .set_finalized_height(height.max(self.consensus.finalized_height()));
    }

    /// Last finalized (height, hash).
    pub fn finalized(&self) -> (u64, Hash) {
        *self.finalized.read()
    }

    /// Store double-sign evidence for later broadcast
    fn store_double_sign_evidence(&self, evidence: &qfc_types::DoubleSignEvidence) {
        let key = format!("double_sign:{}:{}", evidence.height, evidence.validator);
        if let Err(e) = self
            .db
            .put(cf::METADATA, key.as_bytes(), &evidence.to_bytes())
        {
            debug!("Failed to store double-sign evidence: {}", e);
        }
    }

    /// Get pending double-sign evidence (for broadcast)
    pub fn get_pending_double_sign_evidence(&self) -> Vec<qfc_types::DoubleSignEvidence> {
        // In a full implementation, we would scan for evidence to broadcast
        Vec::new()
    }

    /// Append all storage writes for a block (header, body, hash index,
    /// transactions, tx locations) to a batch. Returns the block hash.
    fn append_block_to_batch(batch: &mut WriteBatch, block: &Block) -> Hash {
        let key = encode_block_number(block.number());
        let block_hash = blake3_hash(&block.header_bytes());

        // Store header
        batch.put(
            cf::BLOCK_HEADERS,
            key.to_vec(),
            borsh::to_vec(&block.header).unwrap(),
        );

        // Store body
        let body = BlockBody::from_block(block);
        batch.put(
            cf::BLOCK_BODIES,
            key.to_vec(),
            borsh::to_vec(&body).unwrap(),
        );

        // Store hash index (canonical membership)
        batch.put(
            cf::BLOCK_HASH_INDEX,
            block_hash.as_bytes().to_vec(),
            key.to_vec(),
        );

        // Store the full block in the hash-keyed branch store (fork choice
        // resolves parents by hash across branches)
        batch.put(
            cf::BLOCKS_BY_HASH,
            block_hash.as_bytes().to_vec(),
            borsh::to_vec(block).unwrap(),
        );

        // Store transactions and their locations
        for (index, tx) in block.transactions.iter().enumerate() {
            let tx_hash = blake3_hash(&tx.to_bytes_without_signature());

            // Store transaction data
            batch.put(cf::TRANSACTIONS, tx_hash.as_bytes().to_vec(), tx.to_bytes());

            // Store transaction location index (block_height, tx_index)
            let tx_location = qfc_storage::encode_tx_location(block.number(), index as u32);
            batch.put(
                cf::TX_INDEX,
                tx_hash.as_bytes().to_vec(),
                tx_location.to_vec(),
            );
        }

        block_hash
    }

    /// Append the block's receipts and the canonical-head metadata
    /// (`latest_block_number` / `latest_state_root`) to a batch.
    fn append_receipts_and_head_to_batch(
        batch: &mut WriteBatch,
        block: &Block,
        receipts: &[Receipt],
        state_root: &Hash,
    ) {
        for receipt in receipts {
            batch.put(
                cf::RECEIPTS,
                receipt.tx_hash.as_bytes().to_vec(),
                borsh::to_vec(receipt).unwrap(),
            );
        }

        batch.put(
            cf::METADATA,
            qfc_storage::meta::LATEST_BLOCK_NUMBER.to_vec(),
            block.number().to_le_bytes().to_vec(),
        );
        batch.put(
            cf::METADATA,
            qfc_storage::meta::LATEST_STATE_ROOT.to_vec(),
            state_root.as_bytes().to_vec(),
        );
    }

    /// Atomically commit a canonical block.
    ///
    /// Header, body, hash index, transactions, tx locations, receipts, and
    /// head metadata (`latest_block_number` / `latest_state_root`) all land in
    /// a single WriteBatch, so a crash can never leave a stored block without
    /// its receipts or a head pointer ahead of the stored data.
    ///
    /// The batch is written with `WriteOptions::set_sync(true)`; see
    /// `docs/adr/0001-block-commit-durability.md` for the durability policy.
    fn commit_block(&self, block: &Block, receipts: &[Receipt], state_root: &Hash) -> Result<Hash> {
        let mut batch = WriteBatch::new();
        let block_hash = Self::append_block_to_batch(&mut batch, block);
        Self::append_receipts_and_head_to_batch(&mut batch, block, receipts, state_root);
        self.db.write_batch_sync(batch)?;
        Ok(block_hash)
    }

    /// Get state at a specific block
    pub fn state_at(&self, block_number: u64) -> Result<StateDB> {
        let block = self
            .get_block_by_number(block_number)?
            .ok_or_else(|| ChainError::BlockNotFound(block_number.to_string()))?;

        Ok(StateDB::with_root(self.db.clone(), block.state_root()))
    }

    /// Get account balance
    pub fn get_balance(&self, address: &Address) -> Result<U256> {
        Ok(self.state.get_balance(address)?)
    }

    /// Get account nonce
    pub fn get_nonce(&self, address: &Address) -> Result<u64> {
        Ok(self.state.get_nonce(address)?)
    }

    /// Get contract code
    pub fn get_code(&self, address: &Address) -> Result<Vec<u8>> {
        Ok(self.state.get_code(address)?)
    }

    /// Get storage value
    pub fn get_storage(&self, address: &Address, slot: &U256) -> Result<U256> {
        Ok(self.state.get_storage(address, slot)?)
    }

    /// Get the executor
    pub fn executor(&self) -> &Executor {
        &self.executor
    }

    /// Get the state
    pub fn state(&self) -> &Arc<StateDB> {
        &self.state
    }

    /// Get the database
    pub fn db(&self) -> &Database {
        &self.db
    }

    /// Get the consensus engine
    pub fn consensus(&self) -> &ConsensusEngine {
        &self.consensus
    }

    /// Get current validators
    pub fn get_validators(&self) -> Vec<ValidatorNode> {
        self.consensus.get_validators()
    }

    /// Get current epoch.
    ///
    /// Advances the tracked epoch to the wall-clock one first (guarded on
    /// the genesis seed being anchored): non-validator/full nodes run no
    /// producer or miner loop, so without this qfc_getEpoch, task
    /// submission and synthetic-task generation were stuck on the boot
    /// epoch forever (review fix 10). Epochs are a pure function of
    /// wall-clock time; validation never reads this state.
    pub fn get_epoch(&self) -> Epoch {
        if self.consensus.has_genesis_seed() {
            self.consensus.maybe_advance_epoch();
        }
        self.consensus.get_epoch()
    }

    /// Get finalized block height
    pub fn finalized_height(&self) -> u64 {
        self.consensus.finalized_height()
    }

    /// Store a block that we produced ourselves.
    ///
    /// Runs the SAME validate + execute path as import (self-validation,
    /// spec §4): a node must never commit a block that its peers would
    /// reject, so the producer path re-derives the schedule, VRF and state
    /// root exactly like an importer would. Serializes on the import lock.
    pub async fn store_produced_block(&self, block: &Block) -> Result<()> {
        let _guard = self.import_lock.lock().await;
        self.store_produced_block_locked(block)
    }

    /// Blocking variant of [`Self::store_produced_block`] for non-async
    /// contexts. Panics if called from within an async runtime.
    pub fn store_produced_block_blocking(&self, block: &Block) -> Result<()> {
        let _guard = self.import_lock.blocking_lock();
        self.store_produced_block_locked(block)
    }

    fn store_produced_block_locked(&self, block: &Block) -> Result<()> {
        let head = self
            .head
            .read()
            .clone()
            .ok_or(ChainError::GenesisNotFound)?;

        // A produced block must extend the current canonical head — if the
        // head moved (a concurrent import won the lock first), drop it.
        if block.parent_hash() != head.hash {
            return Err(ChainError::InvalidParent {
                expected: head.hash.to_string(),
                actual: block.parent_hash().to_string(),
            });
        }

        // Self-validation: same checks every importer applies.
        self.consensus.validate_block(block, &head.block)?;

        // Same deterministic execution against the parent state root.
        let outcome = self.execute_block(block, head.block.state_root())?;
        if outcome.state_root != block.state_root() {
            return Err(ChainError::InvalidBlock(
                "produced block state root mismatch (self-validation)".to_string(),
            ));
        }
        if outcome.gas_used != block.gas_used() {
            return Err(ChainError::InvalidBlock(
                "produced block gas used mismatch (self-validation)".to_string(),
            ));
        }

        let block_hash = self.commit_canonical(block, &outcome.receipts, &outcome.state_root)?;

        debug!(
            "Stored produced block {} at height {}",
            block_hash,
            block.number()
        );

        Ok(())
    }

    /// Simulate a call without modifying state (for eth_call)
    pub fn simulate_call(
        &self,
        from: Option<Address>,
        to: Option<Address>,
        value: U256,
        data: Vec<u8>,
        gas_limit: Option<u64>,
    ) -> Result<(bool, Vec<u8>, u64)> {
        let sender = from.unwrap_or_else(|| Address::ZERO);
        let gas = gas_limit.unwrap_or(qfc_types::DEFAULT_BLOCK_GAS_LIMIT);

        // Use EvmExecutor directly instead of routing through the full transaction
        // execution pipeline. The old path went through execute_contract_call() which
        // checks state.get_code() and silently returns empty output if code is not
        // found — breaking eth_call for deployed contracts.
        let block_number = self.block_number();
        let head = self.head();
        let block_timestamp = head.as_ref().map(|b| b.block.timestamp()).unwrap_or(0);
        // ADR-0017 context for simulation: anchor the BLOCKHASH walk /
        // PREVRANDAO at the current head, matching an eth_call executed in
        // the head block's context.
        let parent_hash = head.as_ref().map(|b| b.hash).unwrap_or(Hash::ZERO);

        // Simulate on a SCRATCH state at the current root — never
        // snapshot/revert the shared live instance, which raced concurrent
        // imports adopting a new root between the snapshot and the revert
        // (review fix 9). The scratch is dropped without commit; nothing it
        // writes is ever referenced.
        let scratch = StateDB::with_root(self.db.clone(), self.state_root());

        // Give sender enough balance for gas (simulation only)
        let gas_cost = U256::from_u64(gas) * U256::from_u64(1_000_000_000);
        let total_needed = gas_cost + value;
        let _ = scratch.add_balance(&sender, total_needed);

        let evm_executor = qfc_executor::EvmExecutor::new(
            &scratch,
            self.config.chain_id,
            block_number,
            block_timestamp,
            Address::ZERO,
            qfc_types::DEFAULT_BLOCK_GAS_LIMIT,
            parent_hash,
            Some(self.block_hash_lookup()),
        )
        .with_activation_height(self.config.evm_opcode_activation_height);

        let result = if let Some(to_addr) = to {
            if value.is_zero() {
                // View/pure function call — use static_call (no state changes)
                evm_executor.static_call(Some(&sender), &to_addr, data, gas)
            } else {
                // Call with value — use regular call
                evm_executor.call(&sender, &to_addr, data, value, gas)
            }
        } else {
            // Contract creation
            evm_executor.create(&sender, data, value, gas)
        };

        match result {
            Ok(evm_result) => {
                let output = if evm_result.success {
                    evm_result.output
                } else {
                    if evm_result.output.is_empty() {
                        evm_result.error.unwrap_or_default().into_bytes()
                    } else {
                        evm_result.output
                    }
                };
                Ok((evm_result.success, output, evm_result.gas_used))
            }
            Err(e) => Err(ChainError::Executor(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qfc_consensus::ConsensusConfig;

    fn create_test_chain() -> Chain {
        let db = Database::open_temp().unwrap();
        let consensus = Arc::new(ConsensusEngine::new(ConsensusConfig::default()));
        Chain::new(db, ChainConfig::default(), consensus).unwrap()
    }

    #[test]
    fn test_chain_creation() {
        let chain = create_test_chain();
        assert!(chain.genesis_hash().is_some());
        assert_eq!(chain.block_number(), 0);
    }

    #[test]
    fn test_get_genesis_block() {
        let chain = create_test_chain();
        let genesis = chain.get_block_by_number(0).unwrap();
        assert!(genesis.is_some());
        assert!(genesis.unwrap().is_genesis());
    }

    #[test]
    fn test_genesis_allocations() {
        let chain = create_test_chain();

        // Check that genesis allocation was applied
        let addr = Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        let balance = chain.get_balance(&addr).unwrap();

        assert!(balance > U256::ZERO);
    }

    /// The whole block commit (header, body, hash index, tx, tx location,
    /// receipts, head metadata) must be assembled into a single WriteBatch —
    /// that batch is what RocksDB applies atomically.
    #[test]
    fn test_block_commit_assembles_single_atomic_batch() {
        let mut block = Block::default();
        block.header.number = 1;
        block.transactions = vec![Transaction::default()];

        let tx_hash = blake3_hash(&block.transactions[0].to_bytes_without_signature());
        let receipts = vec![Receipt {
            tx_hash,
            ..Default::default()
        }];

        let mut batch = WriteBatch::new();
        Chain::append_block_to_batch(&mut batch, &block);
        Chain::append_receipts_and_head_to_batch(&mut batch, &block, &receipts, &Hash::ZERO);

        // header + body + hash index + block-by-hash + 1 tx + 1 tx location
        // + 1 receipt + latest_block_number + latest_state_root = 9 ops,
        // one batch
        assert_eq!(batch.len(), 9);

        let cfs: std::collections::HashSet<&str> = batch
            .ops()
            .iter()
            .map(|op| match op {
                qfc_storage::BatchOp::Put { cf, .. } => cf.as_str(),
                qfc_storage::BatchOp::Delete { cf, .. } => cf.as_str(),
            })
            .collect();
        for expected in [
            cf::BLOCK_HEADERS,
            cf::BLOCK_BODIES,
            cf::BLOCK_HASH_INDEX,
            cf::BLOCKS_BY_HASH,
            cf::TRANSACTIONS,
            cf::TX_INDEX,
            cf::RECEIPTS,
            cf::METADATA,
        ] {
            assert!(cfs.contains(expected), "batch missing CF {expected}");
        }
    }

    /// After a block commit, receipts and head metadata must be present for
    /// the stored block — a stored block can never be observed without them.
    #[test]
    fn test_commit_leaves_no_partial_block() {
        let chain = create_test_chain();
        let genesis = chain.get_block_by_number(0).unwrap().unwrap();

        let mut block = Block::default();
        block.header.number = 1;
        block.header.parent_hash = blake3_hash(&genesis.header_bytes());
        block.header.state_root = chain.state_root();
        block.transactions = vec![Transaction::default()];

        let tx_hash = blake3_hash(&block.transactions[0].to_bytes_without_signature());
        let receipts = vec![Receipt {
            tx_hash,
            ..Default::default()
        }];

        // Commit through the canonical path directly: this test exercises
        // storage atomicity, not consensus validation (the produced-block
        // path now self-validates schedule + VRF + state root).
        chain
            .commit_canonical(&block, &receipts, &block.state_root())
            .unwrap();

        // Block, transaction, location, and receipt are all readable
        assert!(chain.get_block_by_number(1).unwrap().is_some());
        assert!(chain.get_transaction(&tx_hash).unwrap().is_some());
        assert_eq!(
            chain.get_transaction_location(&tx_hash).unwrap(),
            Some((1, 0))
        );
        assert!(chain.get_receipt(&tx_hash).unwrap().is_some());

        // Head metadata committed in the same batch
        let latest = chain
            .db()
            .get(cf::METADATA, qfc_storage::meta::LATEST_BLOCK_NUMBER)
            .unwrap()
            .expect("latest_block_number must be set");
        assert_eq!(u64::from_le_bytes(latest.try_into().unwrap()), 1);

        let root = chain
            .db()
            .get(cf::METADATA, qfc_storage::meta::LATEST_STATE_ROOT)
            .unwrap()
            .expect("latest_state_root must be set");
        assert_eq!(root, block.state_root().as_bytes().to_vec());

        assert_eq!(chain.block_number(), 1);
    }

    /// Genesis init must also commit head metadata atomically with the block.
    #[test]
    fn test_genesis_commits_head_metadata() {
        let chain = create_test_chain();

        let latest = chain
            .db()
            .get(cf::METADATA, qfc_storage::meta::LATEST_BLOCK_NUMBER)
            .unwrap()
            .expect("genesis must set latest_block_number");
        assert_eq!(u64::from_le_bytes(latest.try_into().unwrap()), 0);

        assert!(chain
            .db()
            .get(cf::METADATA, qfc_storage::meta::LATEST_STATE_ROOT)
            .unwrap()
            .is_some());
    }

    // ============ Checkpoint fast restart ============

    fn storage_config(path: &std::path::Path) -> qfc_storage::StorageConfig {
        qfc_storage::StorageConfig {
            path: path.to_path_buf(),
            create_if_missing: true,
            ..Default::default()
        }
    }

    /// Produce dummy blocks up to `height` so an epoch-boundary checkpoint
    /// (every BLOCKS_PER_EPOCH blocks) is written by the commit path.
    /// (Commits directly through the canonical path — these dummy blocks
    /// exercise persistence, not consensus validation.)
    fn produce_blocks(chain: &Chain, from: u64, to: u64) {
        let mut parent_hash = chain.head().unwrap().hash;
        for number in from..=to {
            let mut block = Block::default();
            block.header.number = number;
            block.header.parent_hash = parent_hash;
            block.header.state_root = chain.state_root();
            chain
                .commit_canonical(&block, &[], &block.state_root())
                .unwrap();
            parent_hash = blake3_hash(&block.header_bytes());
        }
    }

    fn test_validator_set() -> Vec<ValidatorNode> {
        (1..=3u8)
            .map(|i| {
                let mut v = ValidatorNode::default();
                v.address = Address::new([i; 20]);
                v.stake = U256::from_u64(10_000 * i as u64);
                v.contribution_score = 4242;
                v
            })
            .collect()
    }

    /// Full restart path: build consensus state, write the epoch-boundary
    /// checkpoint via block production, drop everything, reopen the same DB,
    /// and assert epoch / validators / finalized height are restored from
    /// the checkpoint instead of being re-derived from genesis.
    #[test]
    fn test_restart_restores_consensus_state_from_checkpoint() {
        let dir = tempfile::tempdir().unwrap();

        {
            let db = Database::open(storage_config(dir.path())).unwrap();
            let consensus = Arc::new(ConsensusEngine::new(ConsensusConfig::default()));
            let chain = Chain::new(db, ChainConfig::default(), consensus.clone()).unwrap();

            // Consensus state that genesis init could never produce.
            // (start_epoch first: it recalculates contribution scores, which
            // would overwrite the sentinel score set below.)
            consensus.start_epoch(7, [0xab; 32]);
            consensus.update_validators(test_validator_set());
            consensus.set_finalized_height(2);

            // Height 3 = BLOCKS_PER_EPOCH boundary -> checkpoint written
            produce_blocks(&chain, 1, qfc_types::BLOCKS_PER_EPOCH);
        } // chain + db dropped, releasing the RocksDB lock

        let db = Database::open(storage_config(dir.path())).unwrap();
        let consensus = Arc::new(ConsensusEngine::new(ConsensusConfig::default()));
        let chain = Chain::new(db, ChainConfig::default(), consensus.clone()).unwrap();

        // Restored from checkpoint, not re-derived from genesis
        assert_eq!(consensus.get_epoch().number, 7);
        assert_eq!(consensus.get_epoch().seed, [0xab; 32]);
        assert_eq!(consensus.finalized_height(), 2);

        let validators = consensus.get_validators();
        assert_eq!(validators.len(), 3);
        assert_eq!(validators[0].address, Address::new([1; 20]));
        assert_eq!(validators[0].contribution_score, 4242);
        assert_eq!(validators[2].stake, U256::from_u64(30_000));

        // Chain head itself was restored as before
        assert_eq!(chain.block_number(), qfc_types::BLOCKS_PER_EPOCH);
    }

    /// If the newest checkpoint entry is corrupt, restart must fall back to
    /// the previous good checkpoint instead of panicking or losing state.
    #[test]
    fn test_restart_falls_back_on_corrupt_latest_checkpoint() {
        let dir = tempfile::tempdir().unwrap();

        {
            let db = Database::open(storage_config(dir.path())).unwrap();
            let consensus = Arc::new(ConsensusEngine::new(ConsensusConfig::default()));
            let chain = Chain::new(db.clone(), ChainConfig::default(), consensus.clone()).unwrap();

            consensus.start_epoch(7, [0xab; 32]);
            consensus.update_validators(test_validator_set());
            produce_blocks(&chain, 1, qfc_types::BLOCKS_PER_EPOCH);

            // Corrupt entry at a newer epoch than the good checkpoint
            db.put(cf::CHECKPOINTS, &8u64.to_be_bytes(), b"corrupt checkpoint")
                .unwrap();
        }

        let db = Database::open(storage_config(dir.path())).unwrap();
        let consensus = Arc::new(ConsensusEngine::new(ConsensusConfig::default()));
        let _chain = Chain::new(db, ChainConfig::default(), consensus.clone()).unwrap();

        // Fell back to the good epoch-7 checkpoint
        assert_eq!(consensus.get_epoch().number, 7);
        assert_eq!(consensus.get_validators().len(), 3);
    }

    /// Without any checkpoint, restart keeps the previous behavior: genesis
    /// validators are registered as fallback.
    #[test]
    fn test_restart_without_checkpoint_uses_genesis_validators() {
        let dir = tempfile::tempdir().unwrap();

        let genesis_validators;
        {
            let db = Database::open(storage_config(dir.path())).unwrap();
            let consensus = Arc::new(ConsensusEngine::new(ConsensusConfig::default()));
            let _chain = Chain::new(db, ChainConfig::default(), consensus.clone()).unwrap();
            genesis_validators = consensus.get_validators();
            // No blocks produced -> no checkpoint written
        }

        let db = Database::open(storage_config(dir.path())).unwrap();
        let consensus = Arc::new(ConsensusEngine::new(ConsensusConfig::default()));
        let _chain = Chain::new(db, ChainConfig::default(), consensus.clone()).unwrap();

        assert_eq!(consensus.get_epoch().number, 0);
        let restored = consensus.get_validators();
        assert_eq!(restored.len(), genesis_validators.len());
        for (a, b) in restored.iter().zip(genesis_validators.iter()) {
            assert_eq!(a.address, b.address);
        }
    }
}
