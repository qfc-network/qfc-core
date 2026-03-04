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
    Address, Block, BlockBody, BlockHeader, Epoch, Hash, Receipt, SealedBlock, Signature,
    Transaction, TransactionType, ValidatorNode, U256,
};
use std::sync::Arc;
use tracing::{debug, info};

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

            // Try to load validators from checkpoint
            self.load_validator_checkpoint();

            // Register genesis validators with consensus engine (as fallback)
            self.register_genesis_validators();

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
        for (address, stake) in self.config.genesis.parse_validators() {
            self.state.set_stake(&address, stake)?;
            debug!("Genesis validator: {} stake = {}", address, stake);
        }

        // Commit state and get root
        let state_root = self.state.commit()?;
        genesis.header.state_root = state_root;

        // Compute genesis hash
        let hash = genesis_hash(&genesis);

        // Store genesis
        self.store_block(&genesis)?;

        // Update metadata
        self.db.put(
            cf::METADATA,
            qfc_storage::meta::GENESIS_HASH,
            hash.as_bytes(),
        )?;
        self.db.put(
            cf::METADATA,
            qfc_storage::meta::CHAIN_ID,
            &self.config.chain_id.to_le_bytes(),
        )?;

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
            .map(|(address, stake)| {
                let mut v = ValidatorNode::default();
                v.address = address;
                v.stake = stake;
                v.contribution_score = 1000; // Default contribution score
                info!("Registering genesis validator: {}", address);
                v
            })
            .collect();

        if !validators.is_empty() {
            self.consensus.update_validators(validators);
        }
    }

    /// Load validator checkpoint from storage
    fn load_validator_checkpoint(&self) {
        match self.consensus.load_latest_checkpoint(&self.db) {
            Ok(Some(checkpoint)) => {
                self.consensus.restore_from_checkpoint(&checkpoint);
                info!(
                    "Loaded validator checkpoint: epoch={}, height={}",
                    checkpoint.epoch, checkpoint.block_height
                );
            }
            Ok(None) => {
                debug!("No validator checkpoint found, using genesis validators");
            }
            Err(e) => {
                debug!("Failed to load validator checkpoint: {}", e);
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
                debug!("Failed to create checkpoint: {}", e);
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

        // Store block
        self.store_block(&block)?;

        // Store receipts
        for receipt in &receipts {
            self.db.put(
                cf::RECEIPTS,
                receipt.tx_hash.as_bytes(),
                &borsh::to_vec(&receipt).unwrap(),
            )?;
        }

        // Update head
        *self.head.write() = Some(SealedBlock::new(block_hash, block.clone()));

        // Update metadata
        self.db.put(
            cf::METADATA,
            qfc_storage::meta::LATEST_BLOCK_NUMBER,
            &block.number().to_le_bytes(),
        )?;
        self.db.put(
            cf::METADATA,
            qfc_storage::meta::LATEST_STATE_ROOT,
            state_root.as_bytes(),
        )?;

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

    /// Store a block in the database
    fn store_block(&self, block: &Block) -> Result<()> {
        let key = encode_block_number(block.number());
        let block_hash = blake3_hash(&block.header_bytes());

        let mut batch = WriteBatch::new();

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

        self.db.write_batch(batch)?;

        Ok(())
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
        let block_hash = blake3_hash(&block.header_bytes());

        // Store block
        self.store_block(block)?;

        // Store receipts
        for receipt in receipts {
            self.db.put(
                cf::RECEIPTS,
                receipt.tx_hash.as_bytes(),
                &borsh::to_vec(receipt).unwrap(),
            )?;
        }

        // Update head
        *self.head.write() = Some(SealedBlock::new(block_hash, block.clone()));

        // Update metadata
        self.db.put(
            cf::METADATA,
            qfc_storage::meta::LATEST_BLOCK_NUMBER,
            &block.number().to_le_bytes(),
        )?;
        self.db.put(
            cf::METADATA,
            qfc_storage::meta::LATEST_STATE_ROOT,
            block.state_root().as_bytes(),
        )?;

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
        // Use a default sender if not specified
        let sender = from.unwrap_or_else(|| Address::ZERO);

        // Create a simulated transaction
        let tx_type = if to.is_some() {
            if data.is_empty() {
                TransactionType::Transfer
            } else {
                TransactionType::ContractCall
            }
        } else {
            TransactionType::ContractCreate
        };

        let gas = gas_limit.unwrap_or(qfc_types::DEFAULT_BLOCK_GAS_LIMIT);

        let tx = Transaction {
            tx_type,
            chain_id: self.config.chain_id,
            nonce: self.state.get_nonce(&sender).unwrap_or(0),
            to,
            value,
            data,
            gas_limit: gas,
            gas_price: U256::from_u64(1), // Minimal gas price for simulation
            public_key: qfc_types::PublicKey::ZERO,
            signature: Signature::ZERO,
        };

        // Take a snapshot
        let snapshot = self.state.snapshot();

        // Give sender enough balance for gas (simulation only)
        let gas_cost = U256::from_u64(gas) * U256::from_u64(1); // gas * gas_price
        let total_needed = gas_cost + value;
        let _ = self.state.add_balance(&sender, total_needed);

        // Create a signed transaction (we skip validation for simulation)
        let tx_hash = blake3_hash(&tx.to_bytes_without_signature());
        let signed_tx = qfc_types::SignedTransaction::new(tx, tx_hash, sender);

        // Execute
        let result = self
            .executor
            .execute(&signed_tx, &self.state, &Address::ZERO);

        // Revert state changes
        let _ = self.state.revert(snapshot);

        match result {
            Ok(exec_result) => {
                // Return error message as output if failed
                let output = if exec_result.success {
                    Vec::new() // No EVM output yet
                } else {
                    exec_result.error.unwrap_or_default().into_bytes()
                };
                Ok((exec_result.success, output, exec_result.gas_used))
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
}
