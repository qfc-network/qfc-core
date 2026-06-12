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
    Address, Block, BlockBody, BlockHeader, Epoch, Hash, Receipt, SealedBlock, Transaction,
    ValidatorNode, U256,
};
use std::sync::Arc;
use tracing::{debug, info, warn};

/// Chain configuration
#[derive(Clone, Debug)]
pub struct ChainConfig {
    /// Chain ID
    pub chain_id: u64,
    /// Genesis configuration
    pub genesis: GenesisConfig,
}

impl Default for ChainConfig {
    fn default() -> Self {
        Self {
            chain_id: qfc_types::DEFAULT_CHAIN_ID,
            genesis: GenesisConfig::testnet(),
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
        };

        // Initialize genesis if needed
        chain.init_genesis()?;

        Ok(chain)
    }

    /// Initialize genesis block
    fn init_genesis(&self) -> Result<()> {
        // Check if genesis already exists
        if let Some(genesis_hash) = self.db.get(cf::METADATA, qfc_storage::meta::GENESIS_HASH)? {
            let hash = Hash::from_slice(&genesis_hash).ok_or_else(|| {
                ChainError::Storage("Invalid genesis hash in database".to_string())
            })?;

            *self.genesis_hash.write() = Some(hash);

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
        *self.head.write() = Some(SealedBlock::new(hash, genesis));

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

    /// Get a block by hash
    pub fn get_block_by_hash(&self, hash: &Hash) -> Result<Option<Block>> {
        // Look up block number from hash index
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

    /// Translate Ethereum hash to internal hash if it exists
    /// Returns the internal hash if this is an Ethereum transaction, otherwise returns the original hash
    pub fn translate_eth_hash(&self, hash: &Hash) -> Result<Hash> {
        match self.db.get(cf::ETH_TX_INDEX, hash.as_bytes())? {
            Some(internal_bytes) => Hash::from_slice(&internal_bytes)
                .ok_or_else(|| ChainError::Storage("Invalid internal hash".to_string())),
            None => Ok(*hash), // Not an Ethereum tx, return as-is
        }
    }

    /// Get receipt with block info
    pub fn get_receipt_with_block_info(&self, hash: &Hash) -> Result<Option<(Receipt, Hash, u64)>> {
        let receipt = match self.get_receipt(hash)? {
            Some(r) => r,
            None => return Ok(None),
        };

        // Get transaction location
        let (block_height, _tx_index) = match self.get_transaction_location(hash)? {
            Some(loc) => loc,
            None => return Ok(Some((receipt, Hash::ZERO, 0))),
        };

        // Get block hash
        let block = match self.get_block_by_number(block_height)? {
            Some(b) => b,
            None => return Ok(Some((receipt, Hash::ZERO, block_height))),
        };

        let block_hash = blake3_hash(&block.header_bytes());
        Ok(Some((receipt, block_hash, block_height)))
    }

    /// Import a block
    pub fn import_block(&self, block: Block) -> Result<Hash> {
        let block_hash = blake3_hash(&block.header_bytes());

        // Check if block already exists
        if self
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

        // Get parent block
        let parent = self
            .get_block_by_hash(&block.parent_hash())?
            .ok_or_else(|| ChainError::InvalidParent {
                expected: "existing block".to_string(),
                actual: block.parent_hash().to_string(),
            })?;

        // Validate block
        self.consensus.validate_block(&block, &parent)?;

        // Process mature undelegations before executing transactions
        self.executor.process_mature_undelegations(&self.state);

        // Execute transactions
        let producer = block.producer();
        let (receipts, gas_used) =
            self.executor
                .execute_transactions(&block.transactions, &self.state, &producer);

        // Verify state root
        let state_root = self.state.commit()?;
        if state_root != block.state_root() {
            return Err(ChainError::InvalidBlock("State root mismatch".to_string()));
        }

        // Verify gas used
        if gas_used != block.gas_used() {
            return Err(ChainError::InvalidBlock("Gas used mismatch".to_string()));
        }

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

        // Apply inference scores from block proofs (v2.0 on-chain state)
        for proof in &block.inference_proofs {
            self.consensus
                .update_inference_score(&proof.validator, proof.flops_estimated, 1);
        }

        // Commit block, receipts, and head metadata in a single atomic batch
        self.commit_block(&block, &receipts, &state_root)?;

        // Update in-memory head (only after the durable commit succeeded)
        *self.head.write() = Some(SealedBlock::new(block_hash, block.clone()));

        // Record block production in consensus engine for PoC scoring
        self.consensus.record_block_produced(&producer);

        // Maybe create checkpoint at epoch boundary
        let _ = self.maybe_create_checkpoint(block.number());

        info!("Imported block {} at height {}", block_hash, block.number());

        Ok(block_hash)
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

        // Store hash index
        batch.put(
            cf::BLOCK_HASH_INDEX,
            block_hash.as_bytes().to_vec(),
            key.to_vec(),
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

    /// Get current epoch
    pub fn get_epoch(&self) -> Epoch {
        self.consensus.get_epoch()
    }

    /// Get finalized block height
    pub fn finalized_height(&self) -> u64 {
        self.consensus.finalized_height()
    }

    /// Store a block that we produced (skip validation since we created it)
    pub fn store_produced_block(&self, block: &Block, receipts: &[Receipt]) -> Result<()> {
        // Commit block, receipts, and head metadata in a single atomic batch
        let block_hash = self.commit_block(block, receipts, &block.state_root())?;

        // Update in-memory head (only after the durable commit succeeded)
        *self.head.write() = Some(SealedBlock::new(block_hash, block.clone()));

        // Maybe create checkpoint at epoch boundary — the producer path must
        // checkpoint too, otherwise a block-producing node never persists
        // consensus state and every restart re-derives from genesis.
        let _ = self.maybe_create_checkpoint(block.number());

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
        let block_timestamp = self.head().map(|b| b.block.timestamp()).unwrap_or(0);

        // Take a snapshot so we can revert any state changes
        let snapshot = self.state.snapshot();

        // Give sender enough balance for gas (simulation only)
        let gas_cost = U256::from_u64(gas) * U256::from_u64(1_000_000_000);
        let total_needed = gas_cost + value;
        let _ = self.state.add_balance(&sender, total_needed);

        let evm_executor = qfc_executor::EvmExecutor::new(
            &self.state,
            self.config.chain_id,
            block_number,
            block_timestamp,
            Address::ZERO,
            qfc_types::DEFAULT_BLOCK_GAS_LIMIT,
        );

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

        // Revert state changes
        let _ = self.state.revert(snapshot);

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

        // header + body + hash index + 1 tx + 1 tx location + 1 receipt
        // + latest_block_number + latest_state_root = 8 ops, one batch
        assert_eq!(batch.len(), 8);

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

        chain.store_produced_block(&block, &receipts).unwrap();

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
    /// (every BLOCKS_PER_EPOCH blocks) is written by the producer path.
    fn produce_blocks(chain: &Chain, from: u64, to: u64) {
        let mut parent_hash = chain.head().unwrap().hash;
        for number in from..=to {
            let mut block = Block::default();
            block.header.number = number;
            block.header.parent_hash = parent_hash;
            block.header.state_root = chain.state_root();
            chain.store_produced_block(&block, &[]).unwrap();
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
