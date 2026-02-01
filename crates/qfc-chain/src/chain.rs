//! Blockchain management

use crate::error::{ChainError, Result};
use crate::genesis::{genesis_hash, GenesisConfig};
use parking_lot::RwLock;
use qfc_consensus::ConsensusEngine;
use qfc_crypto::{blake3_hash, merkle_root};
use qfc_executor::Executor;
use qfc_state::StateDB;
use qfc_storage::{cf, encode_block_number, Database, WriteBatch};
use qfc_types::{
    Account, Address, Block, BlockBody, BlockHeader, Hash, Receipt, SealedBlock, Transaction, U256,
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
    pub fn new(
        db: Database,
        config: ChainConfig,
        consensus: Arc<ConsensusEngine>,
    ) -> Result<Self> {
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
            if let Some(head_bytes) =
                self.db.get(cf::METADATA, qfc_storage::meta::LATEST_BLOCK_NUMBER)?
            {
                if head_bytes.len() == 8 {
                    let height = u64::from_le_bytes(head_bytes.try_into().unwrap());
                    if let Some(block) = self.get_block_by_number(height)? {
                        let block_hash = blake3_hash(&block.header_bytes());
                        *self.head.write() = Some(SealedBlock::new(block_hash, block));
                    }
                }
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
        self.db.put(cf::METADATA, qfc_storage::meta::GENESIS_HASH, hash.as_bytes())?;
        self.db.put(
            cf::METADATA,
            qfc_storage::meta::CHAIN_ID,
            &self.config.chain_id.to_le_bytes(),
        )?;

        *self.genesis_hash.write() = Some(hash);
        *self.head.write() = Some(SealedBlock::new(hash, genesis));

        info!("Genesis block created: {}", hash);

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

        let header: BlockHeader = borsh::from_slice(&header_bytes)
            .map_err(|e| ChainError::Storage(e.to_string()))?;

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
            signature: qfc_types::Signature::ZERO, // TODO: Store signature
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

    /// Import a block
    pub fn import_block(&self, block: Block) -> Result<Hash> {
        let block_hash = blake3_hash(&block.header_bytes());

        // Check if block already exists
        if self.db.get(cf::BLOCK_HASH_INDEX, block_hash.as_bytes())?.is_some() {
            return Err(ChainError::BlockAlreadyKnown);
        }

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

        info!(
            "Imported block {} at height {}",
            block_hash,
            block.number()
        );

        Ok(block_hash)
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
        batch.put(cf::BLOCK_BODIES, key.to_vec(), borsh::to_vec(&body).unwrap());

        // Store hash index
        batch.put(
            cf::BLOCK_HASH_INDEX,
            block_hash.as_bytes().to_vec(),
            key.to_vec(),
        );

        // Store transactions
        for tx in &block.transactions {
            let tx_hash = blake3_hash(&tx.to_bytes_without_signature());
            batch.put(
                cf::TRANSACTIONS,
                tx_hash.as_bytes().to_vec(),
                tx.to_bytes(),
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
