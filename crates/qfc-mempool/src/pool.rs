//! Transaction pool implementation

use crate::error::{MempoolError, Result};
use dashmap::DashMap;
use parking_lot::RwLock;
use qfc_crypto::blake3_hash;
use qfc_types::{Address, Hash, Transaction, U256};
use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};
use tracing::{debug, trace};

/// Mempool configuration
#[derive(Clone, Debug)]
pub struct MempoolConfig {
    /// Maximum number of transactions
    pub max_size: usize,
    /// Maximum transactions per account
    pub max_per_account: usize,
    /// Minimum gas price
    pub min_gas_price: U256,
    /// Transaction lifetime
    pub tx_lifetime: Duration,
}

impl Default for MempoolConfig {
    fn default() -> Self {
        Self {
            max_size: 10000,
            max_per_account: 64,
            min_gas_price: U256::from_u64(1_000_000_000), // 1 Gwei
            tx_lifetime: Duration::from_secs(3600),       // 1 hour
        }
    }
}

/// Transaction in the pool
#[derive(Clone, Debug)]
pub struct PooledTransaction {
    /// The transaction
    pub tx: Transaction,
    /// Transaction hash
    pub hash: Hash,
    /// Sender address
    pub sender: Address,
    /// When the transaction was added
    pub added_at: Instant,
}

impl PooledTransaction {
    pub fn new(tx: Transaction, hash: Hash, sender: Address) -> Self {
        Self {
            tx,
            hash,
            sender,
            added_at: Instant::now(),
        }
    }

    pub fn is_expired(&self, lifetime: Duration) -> bool {
        self.added_at.elapsed() > lifetime
    }
}

/// Transaction pool
pub struct Mempool {
    /// Configuration
    config: MempoolConfig,
    /// Transactions by hash
    by_hash: DashMap<Hash, PooledTransaction>,
    /// Transactions by sender -> nonce -> hash
    by_sender: DashMap<Address, BTreeMap<u64, Hash>>,
    /// Transactions sorted by gas price (for selection)
    by_price: RwLock<BTreeMap<(U256, Hash), Hash>>,
    /// Current pool size
    size: RwLock<usize>,
}

impl Mempool {
    /// Create a new mempool
    pub fn new(config: MempoolConfig) -> Self {
        Self {
            config,
            by_hash: DashMap::new(),
            by_sender: DashMap::new(),
            by_price: RwLock::new(BTreeMap::new()),
            size: RwLock::new(0),
        }
    }

    /// Create with default configuration
    pub fn default_pool() -> Self {
        Self::new(MempoolConfig::default())
    }

    /// Add a transaction to the pool
    pub fn add(&self, tx: Transaction, sender: Address) -> Result<Hash> {
        let hash = blake3_hash(&tx.to_bytes_without_signature());

        // Check if already known
        if self.by_hash.contains_key(&hash) {
            return Err(MempoolError::AlreadyKnown);
        }

        // Check gas price
        if tx.gas_price < self.config.min_gas_price {
            return Err(MempoolError::GasPriceTooLow {
                minimum: self.config.min_gas_price.to_string(),
                provided: tx.gas_price.to_string(),
            });
        }

        // Check pool capacity
        if *self.size.read() >= self.config.max_size {
            // Try to evict lowest gas price transaction
            if !self.evict_one() {
                return Err(MempoolError::PoolFull);
            }
        }

        // Check per-account limit
        let sender_count = self
            .by_sender
            .get(&sender)
            .map(|m| m.len())
            .unwrap_or(0);

        if sender_count >= self.config.max_per_account {
            return Err(MempoolError::AccountPoolFull);
        }

        // Add to pool
        let pooled = PooledTransaction::new(tx.clone(), hash, sender);

        self.by_hash.insert(hash, pooled);

        self.by_sender
            .entry(sender)
            .or_default()
            .insert(tx.nonce, hash);

        self.by_price
            .write()
            .insert((tx.gas_price, hash), hash);

        *self.size.write() += 1;

        debug!(
            "Added tx {} from {} nonce={} gas_price={}",
            hash, sender, tx.nonce, tx.gas_price
        );

        Ok(hash)
    }

    /// Get a transaction by hash
    pub fn get(&self, hash: &Hash) -> Option<PooledTransaction> {
        self.by_hash.get(hash).map(|r| r.clone())
    }

    /// Check if a transaction exists
    pub fn contains(&self, hash: &Hash) -> bool {
        self.by_hash.contains_key(hash)
    }

    /// Get all transactions for an account
    pub fn get_by_sender(&self, sender: &Address) -> Vec<PooledTransaction> {
        self.by_sender
            .get(sender)
            .map(|nonce_map| {
                nonce_map
                    .values()
                    .filter_map(|hash| self.by_hash.get(hash).map(|r| r.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Remove a transaction from the pool
    pub fn remove(&self, hash: &Hash) -> Option<PooledTransaction> {
        if let Some((_, pooled)) = self.by_hash.remove(hash) {
            // Remove from by_sender
            if let Some(mut nonce_map) = self.by_sender.get_mut(&pooled.sender) {
                nonce_map.remove(&pooled.tx.nonce);
            }

            // Remove from by_price
            self.by_price
                .write()
                .remove(&(pooled.tx.gas_price, *hash));

            *self.size.write() -= 1;

            trace!("Removed tx {}", hash);
            return Some(pooled);
        }
        None
    }

    /// Remove multiple transactions
    pub fn remove_many(&self, hashes: &[Hash]) {
        for hash in hashes {
            self.remove(hash);
        }
    }

    /// Remove transactions by sender with nonce <= given nonce
    pub fn remove_confirmed(&self, sender: &Address, confirmed_nonce: u64) {
        if let Some(mut nonce_map) = self.by_sender.get_mut(sender) {
            let to_remove: Vec<u64> = nonce_map
                .range(..=confirmed_nonce)
                .map(|(nonce, _)| *nonce)
                .collect();

            for nonce in to_remove {
                if let Some(hash) = nonce_map.remove(&nonce) {
                    if let Some((_, pooled)) = self.by_hash.remove(&hash) {
                        self.by_price
                            .write()
                            .remove(&(pooled.tx.gas_price, hash));
                        *self.size.write() -= 1;
                    }
                }
            }
        }
    }

    /// Select transactions for a block
    pub fn select(&self, max_gas: u64, max_count: usize) -> Vec<Transaction> {
        let mut selected = Vec::new();
        let mut gas_used = 0u64;
        let mut seen_senders: HashMap<Address, u64> = HashMap::new();

        // Iterate by gas price (highest first)
        let by_price = self.by_price.read();
        for ((_gas_price, hash), _) in by_price.iter().rev() {
            if selected.len() >= max_count {
                break;
            }

            if let Some(pooled) = self.by_hash.get(hash) {
                let tx = &pooled.tx;

                // Check gas limit
                if gas_used + tx.gas_limit > max_gas {
                    continue;
                }

                // Check if expired
                if pooled.is_expired(self.config.tx_lifetime) {
                    continue;
                }

                // Check nonce ordering
                let _expected_nonce = seen_senders
                    .get(&pooled.sender)
                    .map(|n| n + 1)
                    .unwrap_or(0); // TODO: Get from state

                // For now, just accept any nonce (simplified)
                // In production, we'd check against state
                seen_senders.insert(pooled.sender, tx.nonce);

                selected.push(tx.clone());
                gas_used += tx.gas_limit;
            }
        }

        selected
    }

    /// Evict the lowest gas price transaction
    fn evict_one(&self) -> bool {
        let by_price = self.by_price.read();
        if let Some(((_, hash), _)) = by_price.iter().next() {
            let hash = *hash;
            drop(by_price);
            self.remove(&hash);
            return true;
        }
        false
    }

    /// Remove expired transactions
    pub fn remove_expired(&self) -> usize {
        let mut removed = 0;

        let expired: Vec<Hash> = self
            .by_hash
            .iter()
            .filter(|r| r.is_expired(self.config.tx_lifetime))
            .map(|r| r.hash)
            .collect();

        for hash in expired {
            self.remove(&hash);
            removed += 1;
        }

        if removed > 0 {
            debug!("Removed {} expired transactions", removed);
        }

        removed
    }

    /// Get current pool size
    pub fn size(&self) -> usize {
        *self.size.read()
    }

    /// Check if pool is empty
    pub fn is_empty(&self) -> bool {
        self.size() == 0
    }

    /// Clear the pool
    pub fn clear(&self) {
        self.by_hash.clear();
        self.by_sender.clear();
        self.by_price.write().clear();
        *self.size.write() = 0;
    }

    /// Get pool statistics
    pub fn stats(&self) -> MempoolStats {
        MempoolStats {
            size: self.size(),
            max_size: self.config.max_size,
            unique_senders: self.by_sender.len(),
        }
    }
}

/// Mempool statistics
#[derive(Clone, Debug)]
pub struct MempoolStats {
    pub size: usize,
    pub max_size: usize,
    pub unique_senders: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use qfc_types::TransactionType;

    fn create_test_tx(nonce: u64, gas_price: u64) -> Transaction {
        Transaction {
            tx_type: TransactionType::Transfer,
            chain_id: 9000,
            nonce,
            to: Some(Address::new([0x22; 20])),
            value: U256::from_u64(1000),
            data: Vec::new(),
            gas_limit: 21000,
            gas_price: U256::from_u64(gas_price),
            public_key: Default::default(),
            signature: Default::default(),
        }
    }

    #[test]
    fn test_add_and_get() {
        let pool = Mempool::default_pool();
        let sender = Address::new([0x11; 20]);
        let tx = create_test_tx(0, 2_000_000_000);

        let hash = pool.add(tx.clone(), sender).unwrap();
        let pooled = pool.get(&hash).unwrap();

        assert_eq!(pooled.tx.nonce, 0);
        assert_eq!(pooled.sender, sender);
    }

    #[test]
    fn test_duplicate() {
        let pool = Mempool::default_pool();
        let sender = Address::new([0x11; 20]);
        let tx = create_test_tx(0, 2_000_000_000);

        pool.add(tx.clone(), sender).unwrap();
        let result = pool.add(tx, sender);

        assert!(matches!(result, Err(MempoolError::AlreadyKnown)));
    }

    #[test]
    fn test_gas_price_too_low() {
        let pool = Mempool::default_pool();
        let sender = Address::new([0x11; 20]);
        let tx = create_test_tx(0, 100); // Very low gas price

        let result = pool.add(tx, sender);
        assert!(matches!(result, Err(MempoolError::GasPriceTooLow { .. })));
    }

    #[test]
    fn test_remove() {
        let pool = Mempool::default_pool();
        let sender = Address::new([0x11; 20]);
        let tx = create_test_tx(0, 2_000_000_000);

        let hash = pool.add(tx, sender).unwrap();
        assert!(pool.contains(&hash));

        pool.remove(&hash);
        assert!(!pool.contains(&hash));
    }

    #[test]
    fn test_select() {
        let pool = Mempool::default_pool();
        let sender = Address::new([0x11; 20]);

        // Add transactions with different gas prices
        pool.add(create_test_tx(0, 1_000_000_000), sender).unwrap();
        pool.add(create_test_tx(1, 3_000_000_000), sender).unwrap();
        pool.add(create_test_tx(2, 2_000_000_000), sender).unwrap();

        let selected = pool.select(100000, 10);

        // Should get transactions sorted by gas price (highest first)
        assert_eq!(selected.len(), 3);
        assert_eq!(selected[0].gas_price, U256::from_u64(3_000_000_000));
    }
}
