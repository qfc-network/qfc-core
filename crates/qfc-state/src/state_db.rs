//! State database for managing accounts and storage

use crate::error::{Result, StateError};
use lru::LruCache;
use parking_lot::RwLock;
use qfc_crypto::blake3_hash;
use qfc_storage::{cf, Database};
use qfc_trie::Trie;
use qfc_types::{Account, Address, Hash, Undelegation, U256};
use std::collections::HashMap;
use std::num::NonZeroUsize;
use tracing::debug;

/// Maximum number of accounts in the LRU cache
const ACCOUNT_CACHE_SIZE: usize = 10_000;
/// Maximum number of addresses with cached storage slots
const STORAGE_CACHE_SIZE: usize = 5_000;
/// Maximum number of code entries in the LRU cache
const CODE_CACHE_SIZE: usize = 1_000;

/// State database for managing blockchain state
pub struct StateDB {
    /// The underlying database
    db: Database,
    /// Account state trie
    trie: RwLock<Trie>,
    /// Account cache (LRU-bounded)
    account_cache: RwLock<LruCache<Address, Account>>,
    /// Storage cache: address -> (slot -> value) (LRU-bounded by address)
    storage_cache: RwLock<LruCache<Address, HashMap<U256, U256>>>,
    /// Code cache: hash -> code (LRU-bounded)
    code_cache: RwLock<LruCache<Hash, Vec<u8>>>,
    /// Current state root
    root: RwLock<Hash>,
}

impl StateDB {
    /// Create a new state database with empty state
    pub fn new(db: Database) -> Self {
        let trie = Trie::new(db.clone());
        Self {
            db,
            trie: RwLock::new(trie),
            account_cache: RwLock::new(LruCache::new(
                NonZeroUsize::new(ACCOUNT_CACHE_SIZE).unwrap(),
            )),
            storage_cache: RwLock::new(LruCache::new(
                NonZeroUsize::new(STORAGE_CACHE_SIZE).unwrap(),
            )),
            code_cache: RwLock::new(LruCache::new(NonZeroUsize::new(CODE_CACHE_SIZE).unwrap())),
            root: RwLock::new(Hash::ZERO),
        }
    }

    /// Create a state database at a specific root
    pub fn with_root(db: Database, root: Hash) -> Self {
        let trie = Trie::with_root(db.clone(), root);
        Self {
            db,
            trie: RwLock::new(trie),
            account_cache: RwLock::new(LruCache::new(
                NonZeroUsize::new(ACCOUNT_CACHE_SIZE).unwrap(),
            )),
            storage_cache: RwLock::new(LruCache::new(
                NonZeroUsize::new(STORAGE_CACHE_SIZE).unwrap(),
            )),
            code_cache: RwLock::new(LruCache::new(NonZeroUsize::new(CODE_CACHE_SIZE).unwrap())),
            root: RwLock::new(root),
        }
    }

    /// Restore state to a specific root (used on chain restart)
    pub fn set_root(&self, root: Hash) {
        *self.trie.write() = Trie::with_root(self.db.clone(), root);
        *self.root.write() = root;
        self.account_cache.write().clear();
        self.storage_cache.write().clear();
        // Code is content-addressed (immutable), safe to keep across root changes
    }

    /// Get the current state root
    pub fn root(&self) -> Hash {
        *self.root.read()
    }

    /// Get an account (returns default if not exists)
    pub fn get_account(&self, address: &Address) -> Result<Account> {
        // Check cache first (LruCache::get requires &mut self)
        if let Some(account) = self.account_cache.write().get(address) {
            return Ok(account.clone());
        }

        // Load from trie
        let key = address.as_bytes();
        let trie = self.trie.read();

        match trie.get(key)? {
            Some(data) => {
                let account = Account::from_bytes(&data)
                    .map_err(|e| StateError::Serialization(e.to_string()))?;
                // Cache it
                self.account_cache.write().put(*address, account.clone());
                Ok(account)
            }
            None => Ok(Account::new_eoa()),
        }
    }

    /// Set an account
    pub fn set_account(&self, address: &Address, account: &Account) -> Result<()> {
        // Update cache
        self.account_cache.write().put(*address, account.clone());

        // Update trie
        let key = address.as_bytes();
        let value = account.to_bytes();
        self.trie.write().insert(key, value)?;

        Ok(())
    }

    /// Check if an account exists
    pub fn exists(&self, address: &Address) -> Result<bool> {
        let account = self.get_account(address)?;
        Ok(!account.is_empty())
    }

    /// Get account balance
    pub fn get_balance(&self, address: &Address) -> Result<U256> {
        Ok(self.get_account(address)?.balance)
    }

    /// Set account balance
    pub fn set_balance(&self, address: &Address, balance: U256) -> Result<()> {
        let mut account = self.get_account(address)?;
        account.balance = balance;
        self.set_account(address, &account)
    }

    /// Add to account balance
    pub fn add_balance(&self, address: &Address, amount: U256) -> Result<()> {
        let mut account = self.get_account(address)?;
        account.add_balance(amount);
        self.set_account(address, &account)
    }

    /// Subtract from account balance
    pub fn sub_balance(&self, address: &Address, amount: U256) -> Result<()> {
        let mut account = self.get_account(address)?;
        if !account.sub_balance(amount) {
            return Err(StateError::InsufficientBalance {
                need: amount.to_string(),
                have: account.balance.to_string(),
            });
        }
        self.set_account(address, &account)
    }

    /// Transfer balance from one account to another
    pub fn transfer(&self, from: &Address, to: &Address, amount: U256) -> Result<()> {
        if amount.is_zero() {
            return Ok(());
        }

        // Check balance
        let from_account = self.get_account(from)?;
        if from_account.balance < amount {
            return Err(StateError::InsufficientBalance {
                need: amount.to_string(),
                have: from_account.balance.to_string(),
            });
        }

        // Perform transfer
        self.sub_balance(from, amount)?;
        self.add_balance(to, amount)?;

        Ok(())
    }

    /// Get account nonce
    pub fn get_nonce(&self, address: &Address) -> Result<u64> {
        Ok(self.get_account(address)?.nonce)
    }

    /// Set account nonce
    pub fn set_nonce(&self, address: &Address, nonce: u64) -> Result<()> {
        let mut account = self.get_account(address)?;
        account.nonce = nonce;
        self.set_account(address, &account)
    }

    /// Increment account nonce
    pub fn increment_nonce(&self, address: &Address) -> Result<u64> {
        let mut account = self.get_account(address)?;
        account.increment_nonce();
        self.set_account(address, &account)?;
        Ok(account.nonce)
    }

    /// Get contract code
    pub fn get_code(&self, address: &Address) -> Result<Vec<u8>> {
        let account = self.get_account(address)?;

        match account.code_hash {
            Some(hash) => {
                // Check cache
                if let Some(code) = self.code_cache.write().get(&hash) {
                    return Ok(code.clone());
                }

                // Load from database
                match self.db.get(cf::CODE, hash.as_bytes())? {
                    Some(code) => {
                        self.code_cache.write().put(hash, code.clone());
                        Ok(code)
                    }
                    None => Ok(Vec::new()),
                }
            }
            None => Ok(Vec::new()),
        }
    }

    /// Set contract code
    pub fn set_code(&self, address: &Address, code: Vec<u8>) -> Result<Hash> {
        let code_hash = blake3_hash(&code);

        // Store code in database
        self.db.put(cf::CODE, code_hash.as_bytes(), &code)?;

        // Update code cache
        self.code_cache.write().put(code_hash, code);

        // Update account
        let mut account = self.get_account(address)?;
        account.code_hash = Some(code_hash);
        self.set_account(address, &account)?;

        Ok(code_hash)
    }

    /// Get code hash
    pub fn get_code_hash(&self, address: &Address) -> Result<Option<Hash>> {
        Ok(self.get_account(address)?.code_hash)
    }

    /// Get storage value
    pub fn get_storage(&self, address: &Address, slot: &U256) -> Result<U256> {
        // Check cache
        if let Some(storage) = self.storage_cache.write().get(address) {
            if let Some(value) = storage.get(slot) {
                return Ok(*value);
            }
        }

        // Get account's storage root
        let account = self.get_account(address)?;
        let storage_root = account.storage_root.unwrap_or(Hash::ZERO);

        if storage_root == Hash::ZERO {
            return Ok(U256::ZERO);
        }

        // Load from storage trie
        let storage_trie = Trie::with_root(self.db.clone(), storage_root);
        let key = slot.to_be_bytes();

        match storage_trie.get(&key)? {
            Some(data) if data.len() == 32 => {
                let value = U256::from_be_bytes(&data.try_into().unwrap());
                // Cache it
                let mut cache = self.storage_cache.write();
                if let Some(slots) = cache.get_mut(address) {
                    slots.insert(*slot, value);
                } else {
                    let mut slots = HashMap::new();
                    slots.insert(*slot, value);
                    cache.put(*address, slots);
                }
                Ok(value)
            }
            _ => Ok(U256::ZERO),
        }
    }

    /// Set storage value
    pub fn set_storage(&self, address: &Address, slot: U256, value: U256) -> Result<()> {
        // Update cache
        {
            let mut cache = self.storage_cache.write();
            if let Some(slots) = cache.get_mut(address) {
                slots.insert(slot, value);
            } else {
                let mut slots = HashMap::new();
                slots.insert(slot, value);
                cache.put(*address, slots);
            }
        }

        // Get current storage root
        let account = self.get_account(address)?;
        let storage_root = account.storage_root.unwrap_or(Hash::ZERO);

        // Update storage trie
        let mut storage_trie = Trie::with_root(self.db.clone(), storage_root);
        let key = slot.to_be_bytes();

        if value.is_zero() {
            storage_trie.delete(&key)?;
        } else {
            storage_trie.insert(&key, value.to_be_bytes().to_vec())?;
        }

        let new_storage_root = storage_trie.commit()?;

        // Update account's storage root
        let mut account = self.get_account(address)?;
        account.storage_root = if new_storage_root == Hash::ZERO {
            None
        } else {
            Some(new_storage_root)
        };
        self.set_account(address, &account)?;

        Ok(())
    }

    /// Get stake amount for a validator
    pub fn get_stake(&self, address: &Address) -> Result<U256> {
        Ok(self.get_account(address)?.get_stake())
    }

    /// Generate a Merkle proof for an account against the current state root.
    /// Returns the proof and the account data (if it exists).
    pub fn get_account_proof(&self, address: &Address) -> Result<(qfc_trie::MerkleProof, Account)> {
        let key = address.as_bytes();
        let trie = self.trie.read();
        let proof = trie
            .get_proof(key)
            .map_err(|e| StateError::Storage(format!("Failed to generate proof: {}", e)))?;
        let account = self.get_account(address)?;
        Ok((proof, account))
    }

    /// Set stake amount for a validator
    pub fn set_stake(&self, address: &Address, stake: U256) -> Result<()> {
        let mut account = self.get_account(address)?;
        account.set_stake(stake);
        self.set_account(address, &account)
    }

    /// Get contribution score for a validator
    pub fn get_contribution_score(&self, address: &Address) -> Result<u64> {
        Ok(self.get_account(address)?.get_contribution_score())
    }

    /// Set contribution score for a validator
    pub fn set_contribution_score(&self, address: &Address, score: u64) -> Result<()> {
        let mut account = self.get_account(address)?;
        account.set_contribution_score(score);
        self.set_account(address, &account)
    }

    // ============ Delegation Methods ============

    /// Get delegation info for a delegator
    pub fn get_delegation(&self, delegator: &Address) -> Result<(Option<Address>, U256)> {
        let account = self.get_account(delegator)?;
        Ok((account.get_delegated_to(), account.get_delegated_amount()))
    }

    /// Get delegation amount for a specific delegator->validator pair
    pub fn get_delegation_amount(&self, delegator: &Address, validator: &Address) -> Result<U256> {
        let account = self.get_account(delegator)?;
        match account.get_delegated_to() {
            Some(v) if v == *validator => Ok(account.get_delegated_amount()),
            _ => Ok(U256::ZERO),
        }
    }

    /// Set delegation for a delegator to a validator
    pub fn set_delegation(
        &self,
        delegator: &Address,
        validator: &Address,
        amount: U256,
    ) -> Result<()> {
        let mut account = self.get_account(delegator)?;
        account.set_delegation(*validator, amount);
        self.set_account(delegator, &account)
    }

    /// Clear delegation for a delegator
    pub fn clear_delegation(&self, delegator: &Address) -> Result<()> {
        let mut account = self.get_account(delegator)?;
        account.clear_delegation();
        self.set_account(delegator, &account)
    }

    /// Add to delegated amount for a delegator
    pub fn add_delegation_amount(&self, delegator: &Address, amount: U256) -> Result<()> {
        let mut account = self.get_account(delegator)?;
        account.add_delegated_amount(amount);
        self.set_account(delegator, &account)
    }

    /// Subtract from delegated amount (for undelegation)
    pub fn sub_delegation_amount(&self, delegator: &Address, amount: U256) -> Result<bool> {
        let mut account = self.get_account(delegator)?;
        let success = account.sub_delegated_amount(amount);
        if success {
            self.set_account(delegator, &account)?;
        }
        Ok(success)
    }

    /// Check if an address has an active delegation
    pub fn has_delegation(&self, delegator: &Address) -> Result<bool> {
        let account = self.get_account(delegator)?;
        Ok(account.has_delegation())
    }

    /// Get total stake for a validator (direct stake + delegated stake)
    /// Note: This requires validator state from consensus engine
    /// This method only returns the direct stake from account state
    pub fn get_direct_stake(&self, address: &Address) -> Result<U256> {
        self.get_stake(address)
    }

    // ============ Undelegation Methods ============

    /// Store a pending undelegation record
    pub fn store_undelegation(&self, undelegation: &Undelegation) -> Result<()> {
        let key = Undelegation::storage_key(
            &undelegation.delegator,
            &undelegation.validator,
            undelegation.unlock_at,
        );
        self.db
            .put(cf::UNDELEGATIONS, &key, &undelegation.to_bytes())
            .map_err(|e| StateError::Storage(e.to_string()))
    }

    /// Get all pending undelegations for a delegator
    pub fn get_undelegations(&self, delegator: &Address) -> Result<Vec<Undelegation>> {
        let prefix = delegator.as_bytes();
        let mut results = Vec::new();
        let iter = self
            .db
            .iter_from(cf::UNDELEGATIONS, prefix)
            .map_err(|e| StateError::Storage(e.to_string()))?;
        for (key, value) in iter {
            if !key.starts_with(prefix) {
                break;
            }
            if let Ok(u) = Undelegation::from_bytes(&value) {
                results.push(u);
            }
        }
        Ok(results)
    }

    /// Get all undelegations that have matured (unlock_at <= current_time)
    pub fn get_mature_undelegations(&self, current_time: u64) -> Result<Vec<Undelegation>> {
        let mut results = Vec::new();
        let iter = self
            .db
            .iter(cf::UNDELEGATIONS)
            .map_err(|e| StateError::Storage(e.to_string()))?;
        for (_key, value) in iter {
            if let Ok(u) = Undelegation::from_bytes(&value) {
                if u.is_unlocked(current_time) {
                    results.push(u);
                }
            }
        }
        Ok(results)
    }

    /// Delete a completed undelegation record
    pub fn delete_undelegation(
        &self,
        delegator: &Address,
        validator: &Address,
        unlock_at: u64,
    ) -> Result<()> {
        let key = Undelegation::storage_key(delegator, validator, unlock_at);
        self.db
            .delete(cf::UNDELEGATIONS, &key)
            .map_err(|e| StateError::Storage(e.to_string()))
    }

    /// Commit all changes and return new state root
    pub fn commit(&self) -> Result<Hash> {
        let new_root = self.trie.write().commit()?;
        *self.root.write() = new_root;
        debug!("State committed, new root: {}", new_root);
        Ok(new_root)
    }

    /// Clear all caches
    pub fn clear_cache(&self) {
        self.account_cache.write().clear();
        self.storage_cache.write().clear();
        // Keep code cache as code is immutable
    }

    /// Create a snapshot of the current state
    pub fn snapshot(&self) -> StateSnapshot {
        // Extract current cache contents into HashMaps for the snapshot
        let accounts: HashMap<Address, Account> = self
            .account_cache
            .read()
            .iter()
            .map(|(k, v)| (*k, v.clone()))
            .collect();
        let storage: HashMap<Address, HashMap<U256, U256>> = self
            .storage_cache
            .read()
            .iter()
            .map(|(k, v)| (*k, v.clone()))
            .collect();
        StateSnapshot {
            root: self.root(),
            accounts,
            storage,
        }
    }

    /// Revert to a snapshot
    pub fn revert(&self, snapshot: StateSnapshot) -> Result<()> {
        *self.root.write() = snapshot.root;

        // Rebuild LRU caches from snapshot
        let mut account_cache = LruCache::new(NonZeroUsize::new(ACCOUNT_CACHE_SIZE).unwrap());
        for (k, v) in snapshot.accounts {
            account_cache.put(k, v);
        }
        *self.account_cache.write() = account_cache;

        let mut storage_cache = LruCache::new(NonZeroUsize::new(STORAGE_CACHE_SIZE).unwrap());
        for (k, v) in snapshot.storage {
            storage_cache.put(k, v);
        }
        *self.storage_cache.write() = storage_cache;

        // Recreate trie at snapshot root
        *self.trie.write() = Trie::with_root(self.db.clone(), snapshot.root);

        Ok(())
    }
}

/// State snapshot for reverting changes
#[derive(Clone)]
pub struct StateSnapshot {
    pub root: Hash,
    pub accounts: HashMap<Address, Account>,
    pub storage: HashMap<Address, HashMap<U256, U256>>,
}

impl Clone for StateDB {
    fn clone(&self) -> Self {
        // Rebuild LRU caches from source
        let mut account_cache = LruCache::new(NonZeroUsize::new(ACCOUNT_CACHE_SIZE).unwrap());
        for (k, v) in self.account_cache.read().iter() {
            account_cache.put(*k, v.clone());
        }
        let mut storage_cache = LruCache::new(NonZeroUsize::new(STORAGE_CACHE_SIZE).unwrap());
        for (k, v) in self.storage_cache.read().iter() {
            storage_cache.put(*k, v.clone());
        }
        let mut code_cache = LruCache::new(NonZeroUsize::new(CODE_CACHE_SIZE).unwrap());
        for (k, v) in self.code_cache.read().iter() {
            code_cache.put(*k, v.clone());
        }
        Self {
            db: self.db.clone(),
            trie: RwLock::new(Trie::with_root(self.db.clone(), self.root())),
            account_cache: RwLock::new(account_cache),
            storage_cache: RwLock::new(storage_cache),
            code_cache: RwLock::new(code_cache),
            root: RwLock::new(self.root()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_state() -> StateDB {
        let db = Database::open_temp().unwrap();
        StateDB::new(db)
    }

    #[test]
    fn test_new_state() {
        let state = create_test_state();
        assert_eq!(state.root(), Hash::ZERO);
    }

    #[test]
    fn test_get_nonexistent_account() {
        let state = create_test_state();
        let addr = Address::new([0x11; 20]);
        let account = state.get_account(&addr).unwrap();
        assert!(account.is_empty());
    }

    #[test]
    fn test_set_and_get_balance() {
        let state = create_test_state();
        let addr = Address::new([0x11; 20]);

        state.set_balance(&addr, U256::from_u64(1000)).unwrap();
        assert_eq!(state.get_balance(&addr).unwrap(), U256::from_u64(1000));
    }

    #[test]
    fn test_transfer() {
        let state = create_test_state();
        let from = Address::new([0x11; 20]);
        let to = Address::new([0x22; 20]);

        state.set_balance(&from, U256::from_u64(1000)).unwrap();
        state.transfer(&from, &to, U256::from_u64(400)).unwrap();

        assert_eq!(state.get_balance(&from).unwrap(), U256::from_u64(600));
        assert_eq!(state.get_balance(&to).unwrap(), U256::from_u64(400));
    }

    #[test]
    fn test_transfer_insufficient_balance() {
        let state = create_test_state();
        let from = Address::new([0x11; 20]);
        let to = Address::new([0x22; 20]);

        state.set_balance(&from, U256::from_u64(100)).unwrap();
        let result = state.transfer(&from, &to, U256::from_u64(200));

        assert!(matches!(
            result,
            Err(StateError::InsufficientBalance { .. })
        ));
    }

    #[test]
    fn test_nonce() {
        let state = create_test_state();
        let addr = Address::new([0x11; 20]);

        assert_eq!(state.get_nonce(&addr).unwrap(), 0);
        state.increment_nonce(&addr).unwrap();
        assert_eq!(state.get_nonce(&addr).unwrap(), 1);
    }

    #[test]
    fn test_code() {
        let state = create_test_state();
        let addr = Address::new([0x11; 20]);
        let code = vec![0x60, 0x00, 0x60, 0x00, 0xf3]; // Simple bytecode

        let hash = state.set_code(&addr, code.clone()).unwrap();
        assert_eq!(state.get_code(&addr).unwrap(), code);
        assert_eq!(state.get_code_hash(&addr).unwrap(), Some(hash));
    }

    #[test]
    fn test_storage() {
        let state = create_test_state();
        let addr = Address::new([0x11; 20]);
        let slot = U256::from_u64(1);
        let value = U256::from_u64(12345);

        state.set_storage(&addr, slot, value).unwrap();
        assert_eq!(state.get_storage(&addr, &slot).unwrap(), value);
    }

    #[test]
    fn test_commit() {
        let db = Database::open_temp().unwrap();
        let root = {
            let state = StateDB::new(db.clone());
            let addr = Address::new([0x11; 20]);
            state.set_balance(&addr, U256::from_u64(1000)).unwrap();
            state.commit().unwrap()
        };

        // Reopen with same root
        let state = StateDB::with_root(db, root);
        let addr = Address::new([0x11; 20]);
        assert_eq!(state.get_balance(&addr).unwrap(), U256::from_u64(1000));
    }

    #[test]
    fn test_snapshot_revert() {
        let state = create_test_state();
        let addr = Address::new([0x11; 20]);

        state.set_balance(&addr, U256::from_u64(1000)).unwrap();
        let snapshot = state.snapshot();

        state.set_balance(&addr, U256::from_u64(2000)).unwrap();
        assert_eq!(state.get_balance(&addr).unwrap(), U256::from_u64(2000));

        state.revert(snapshot).unwrap();
        assert_eq!(state.get_balance(&addr).unwrap(), U256::from_u64(1000));
    }

    #[test]
    fn test_delegation() {
        let state = create_test_state();
        let delegator = Address::new([0x11; 20]);
        let validator = Address::new([0x22; 20]);

        // Initially no delegation
        assert!(!state.has_delegation(&delegator).unwrap());
        let (target, amount) = state.get_delegation(&delegator).unwrap();
        assert!(target.is_none());
        assert_eq!(amount, U256::ZERO);

        // Set delegation
        state
            .set_delegation(&delegator, &validator, U256::from_u64(1000))
            .unwrap();

        assert!(state.has_delegation(&delegator).unwrap());
        let (target, amount) = state.get_delegation(&delegator).unwrap();
        assert_eq!(target, Some(validator));
        assert_eq!(amount, U256::from_u64(1000));

        // Get delegation amount for specific validator
        assert_eq!(
            state.get_delegation_amount(&delegator, &validator).unwrap(),
            U256::from_u64(1000)
        );

        // Other validator returns zero
        let other_validator = Address::new([0x33; 20]);
        assert_eq!(
            state
                .get_delegation_amount(&delegator, &other_validator)
                .unwrap(),
            U256::ZERO
        );
    }

    #[test]
    fn test_add_delegation_amount() {
        let state = create_test_state();
        let delegator = Address::new([0x11; 20]);
        let validator = Address::new([0x22; 20]);

        state
            .set_delegation(&delegator, &validator, U256::from_u64(1000))
            .unwrap();
        state
            .add_delegation_amount(&delegator, U256::from_u64(500))
            .unwrap();

        let (_, amount) = state.get_delegation(&delegator).unwrap();
        assert_eq!(amount, U256::from_u64(1500));
    }

    #[test]
    fn test_sub_delegation_amount() {
        let state = create_test_state();
        let delegator = Address::new([0x11; 20]);
        let validator = Address::new([0x22; 20]);

        state
            .set_delegation(&delegator, &validator, U256::from_u64(1000))
            .unwrap();

        // Partial withdrawal
        assert!(state
            .sub_delegation_amount(&delegator, U256::from_u64(400))
            .unwrap());
        let (_, amount) = state.get_delegation(&delegator).unwrap();
        assert_eq!(amount, U256::from_u64(600));

        // Full withdrawal clears delegation
        assert!(state
            .sub_delegation_amount(&delegator, U256::from_u64(600))
            .unwrap());
        assert!(!state.has_delegation(&delegator).unwrap());
    }

    #[test]
    fn test_sub_delegation_amount_insufficient() {
        let state = create_test_state();
        let delegator = Address::new([0x11; 20]);
        let validator = Address::new([0x22; 20]);

        state
            .set_delegation(&delegator, &validator, U256::from_u64(1000))
            .unwrap();

        // Cannot withdraw more than delegated
        assert!(!state
            .sub_delegation_amount(&delegator, U256::from_u64(1500))
            .unwrap());
        let (_, amount) = state.get_delegation(&delegator).unwrap();
        assert_eq!(amount, U256::from_u64(1000));
    }

    #[test]
    fn test_clear_delegation() {
        let state = create_test_state();
        let delegator = Address::new([0x11; 20]);
        let validator = Address::new([0x22; 20]);

        state
            .set_delegation(&delegator, &validator, U256::from_u64(1000))
            .unwrap();
        assert!(state.has_delegation(&delegator).unwrap());

        state.clear_delegation(&delegator).unwrap();
        assert!(!state.has_delegation(&delegator).unwrap());
    }
}
