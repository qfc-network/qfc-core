//! Snap sync support for fast state synchronization
//!
//! Snap sync allows new nodes to quickly synchronize by downloading
//! state snapshots rather than replaying all transactions from genesis.

use crate::error::Result;
use crate::StateDB;
use parking_lot::RwLock;
use qfc_storage::{cf, Database, WriteBatch};
use qfc_trie::TrieNode;
use qfc_types::{Account, Address, Hash, U256};
use std::collections::VecDeque;
use tracing::{debug, info, warn};

/// Snap sync configuration
#[derive(Clone, Debug)]
pub struct SnapSyncConfig {
    /// Target state root to sync to
    pub target_root: Hash,
    /// Block number of the target state
    pub target_block: u64,
    /// Maximum concurrent range requests
    pub max_concurrent_requests: usize,
}

/// Account range for snap sync
#[derive(Clone, Debug)]
pub struct AccountRange {
    /// Starting address (inclusive)
    pub start: Address,
    /// Ending address (exclusive)
    pub end: Address,
    /// Accounts in this range
    pub accounts: Vec<(Address, Account)>,
    /// Proof for the range
    pub proof: Vec<Vec<u8>>,
}

/// Storage range for snap sync
#[derive(Clone, Debug)]
pub struct StorageRange {
    /// Account address
    pub address: Address,
    /// Starting slot (inclusive)
    pub start: U256,
    /// Ending slot (exclusive)
    pub end: U256,
    /// Storage slots in this range
    pub slots: Vec<(U256, U256)>,
    /// Proof for the range
    pub proof: Vec<Vec<u8>>,
}

/// Trie node batch for healing
#[derive(Clone, Debug)]
pub struct TrieNodeBatch {
    /// Nodes by their hash
    pub nodes: Vec<(Hash, Vec<u8>)>,
}

/// Snap sync progress
#[derive(Clone, Debug, Default)]
pub struct SnapSyncProgress {
    /// Number of accounts synced
    pub accounts_synced: u64,
    /// Number of storage slots synced
    pub storage_slots_synced: u64,
    /// Number of trie nodes healed
    pub nodes_healed: u64,
    /// Current phase
    pub phase: SnapSyncPhase,
    /// Last synced address (for resumption)
    pub last_address: Option<Address>,
    /// Estimated completion percentage
    pub completion_percentage: f32,
}

/// Snap sync phases
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum SnapSyncPhase {
    #[default]
    NotStarted,
    /// Downloading account ranges
    AccountSync,
    /// Downloading storage ranges
    StorageSync,
    /// Healing missing trie nodes
    TrieHealing,
    /// Verifying final state
    Verification,
    /// Sync complete
    Complete,
}

/// Snap sync state manager
pub struct SnapSyncManager {
    db: Database,
    config: Option<SnapSyncConfig>,
    progress: RwLock<SnapSyncProgress>,
    /// Accounts pending storage sync
    pending_storage: RwLock<VecDeque<Address>>,
    /// Missing trie nodes for healing
    missing_nodes: RwLock<Vec<Hash>>,
}

impl SnapSyncManager {
    /// Create a new snap sync manager
    pub fn new(db: Database) -> Self {
        Self {
            db,
            config: None,
            progress: RwLock::new(SnapSyncProgress::default()),
            pending_storage: RwLock::new(VecDeque::new()),
            missing_nodes: RwLock::new(Vec::new()),
        }
    }

    /// Start snap sync to a target state
    pub fn start(&mut self, config: SnapSyncConfig) {
        info!(
            "Starting snap sync to block {} with root {}",
            config.target_block, config.target_root
        );

        self.config = Some(config);
        *self.progress.write() = SnapSyncProgress {
            phase: SnapSyncPhase::AccountSync,
            ..Default::default()
        };
    }

    /// Get current progress
    pub fn progress(&self) -> SnapSyncProgress {
        self.progress.read().clone()
    }

    /// Check if snap sync is active
    pub fn is_active(&self) -> bool {
        let phase = self.progress.read().phase.clone();
        phase != SnapSyncPhase::NotStarted && phase != SnapSyncPhase::Complete
    }

    /// Get next account range to request
    pub fn next_account_range(&self) -> Option<(Address, Address)> {
        if self.progress.read().phase != SnapSyncPhase::AccountSync {
            return None;
        }

        let last = self.progress.read().last_address;
        let start = last.map(|a| increment_address(a)).unwrap_or(Address::ZERO);

        if start == Address::MAX {
            return None;
        }

        // Request up to 1/16 of address space at a time
        let end = add_address_offset(start, 1u128 << 124);

        Some((start, end))
    }

    /// Process received account range
    pub fn process_account_range(&self, range: AccountRange) -> Result<()> {
        let mut batch = WriteBatch::new();
        let mut progress = self.progress.write();

        for (address, account) in &range.accounts {
            // Store account data
            let account_bytes = account.to_bytes();
            batch.put(cf::STATE, Self::account_key(address), account_bytes);

            progress.accounts_synced += 1;

            // Queue for storage sync if has storage
            if account.storage_root.is_some() && account.storage_root != Some(Hash::ZERO) {
                self.pending_storage.write().push_back(*address);
            }
        }

        // Update last synced address
        if let Some((last_addr, _)) = range.accounts.last() {
            progress.last_address = Some(*last_addr);
        }

        // Store proof nodes
        for proof_data in range.proof {
            if proof_data.len() >= 32 {
                if let Ok(node) = TrieNode::from_bytes(&proof_data) {
                    let hash = node.hash();
                    batch.put(cf::STATE, hash.as_bytes().to_vec(), proof_data);
                }
            }
        }

        self.db.write_batch(batch)?;

        // Check if account sync is complete
        if range.end == Address::MAX || range.accounts.is_empty() {
            progress.phase = SnapSyncPhase::StorageSync;
            info!(
                "Account sync complete, {} accounts synced",
                progress.accounts_synced
            );
        }

        // Update completion estimate
        if let Some(last) = progress.last_address {
            let progress_bytes = last.as_bytes();
            let first_byte = progress_bytes[0] as f32;
            progress.completion_percentage = (first_byte / 255.0) * 100.0 * 0.5;
            // Accounts = 50%
        }

        Ok(())
    }

    /// Get next storage range to request
    pub fn next_storage_range(&self) -> Option<(Address, U256, U256)> {
        if self.progress.read().phase != SnapSyncPhase::StorageSync {
            return None;
        }

        let address = self.pending_storage.read().front().cloned()?;

        // Request full storage range for simplicity
        Some((address, U256::ZERO, U256::MAX))
    }

    /// Process received storage range
    pub fn process_storage_range(&self, range: StorageRange) -> Result<()> {
        let mut batch = WriteBatch::new();
        let mut progress = self.progress.write();

        for (slot, value) in &range.slots {
            // Store storage slot
            let key = Self::storage_key(&range.address, slot);
            batch.put(cf::STATE, key, value.to_be_bytes().to_vec());
            progress.storage_slots_synced += 1;
        }

        // Store proof nodes
        for proof_data in range.proof {
            if proof_data.len() >= 32 {
                if let Ok(node) = TrieNode::from_bytes(&proof_data) {
                    let hash = node.hash();
                    batch.put(cf::STATE, hash.as_bytes().to_vec(), proof_data);
                }
            }
        }

        self.db.write_batch(batch)?;

        // Remove from pending if complete
        if range.end >= U256::MAX || range.slots.is_empty() {
            self.pending_storage.write().pop_front();
        }

        // Check if storage sync is complete
        if self.pending_storage.read().is_empty() {
            progress.phase = SnapSyncPhase::TrieHealing;
            info!(
                "Storage sync complete, {} slots synced",
                progress.storage_slots_synced
            );
        }

        // Update completion estimate
        let pending = self.pending_storage.read().len();
        let total = progress.accounts_synced as usize;
        if total > 0 {
            let storage_progress = 1.0 - (pending as f32 / total as f32);
            progress.completion_percentage = 50.0 + storage_progress * 40.0; // Storage = 40%
        }

        Ok(())
    }

    /// Get missing trie nodes for healing
    pub fn get_missing_nodes(&self, limit: usize) -> Vec<Hash> {
        if self.progress.read().phase != SnapSyncPhase::TrieHealing {
            return Vec::new();
        }

        self.missing_nodes
            .read()
            .iter()
            .take(limit)
            .cloned()
            .collect()
    }

    /// Process received trie nodes
    pub fn process_trie_nodes(&self, batch: TrieNodeBatch) -> Result<()> {
        let mut write_batch = WriteBatch::new();
        let mut progress = self.progress.write();

        let received: std::collections::HashSet<Hash> =
            batch.nodes.iter().map(|(h, _)| *h).collect();

        for (hash, data) in batch.nodes {
            write_batch.put(cf::STATE, hash.as_bytes().to_vec(), data);
            progress.nodes_healed += 1;
        }

        self.db.write_batch(write_batch)?;

        // Remove received nodes from missing list
        self.missing_nodes.write().retain(|h| !received.contains(h));

        // Check if healing is complete
        if self.missing_nodes.read().is_empty() {
            progress.phase = SnapSyncPhase::Verification;
            info!(
                "Trie healing complete, {} nodes healed",
                progress.nodes_healed
            );
        }

        // Update completion estimate
        progress.completion_percentage = 90.0
            + (progress.nodes_healed as f32
                / (progress.nodes_healed + self.missing_nodes.read().len() as u64) as f32)
                * 10.0;

        Ok(())
    }

    /// Identify missing trie nodes by traversing from root
    pub fn identify_missing_nodes(&self) -> Result<usize> {
        let config = match &self.config {
            Some(c) => c,
            None => return Ok(0),
        };

        let mut missing = Vec::new();
        let mut queue: VecDeque<Hash> = VecDeque::new();
        queue.push_back(config.target_root);

        while let Some(hash) = queue.pop_front() {
            if hash == Hash::ZERO {
                continue;
            }

            match self.db.get(cf::STATE, hash.as_bytes())? {
                Some(data) => {
                    // Node exists, check children
                    if let Ok(node) = TrieNode::from_bytes(&data) {
                        match node {
                            TrieNode::Extension { child, .. } => {
                                if child != Hash::ZERO {
                                    queue.push_back(child);
                                }
                            }
                            TrieNode::Branch { children, .. } => {
                                for child in children.iter().flatten() {
                                    if *child != Hash::ZERO {
                                        queue.push_back(*child);
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                None => {
                    // Node is missing
                    missing.push(hash);
                }
            }
        }

        let count = missing.len();
        *self.missing_nodes.write() = missing;

        debug!("Identified {} missing trie nodes", count);
        Ok(count)
    }

    /// Verify the synced state matches target root
    pub fn verify(&self) -> Result<bool> {
        let config = match &self.config {
            Some(c) => c,
            None => return Ok(false),
        };

        // Create state DB at target root
        let state = StateDB::with_root(self.db.clone(), config.target_root);

        // Try to get the root - if successful, state is consistent
        let computed_root = state.root();

        let verified = computed_root == config.target_root;

        if verified {
            let mut progress = self.progress.write();
            progress.phase = SnapSyncPhase::Complete;
            progress.completion_percentage = 100.0;
            info!(
                "Snap sync verification passed, state root: {}",
                computed_root
            );
        } else {
            warn!(
                "Snap sync verification failed: expected {}, got {}",
                config.target_root, computed_root
            );
        }

        Ok(verified)
    }

    /// Complete snap sync and return final state
    pub fn complete(&self) -> Result<Option<StateDB>> {
        let config = match &self.config {
            Some(c) => c.clone(),
            None => return Ok(None),
        };

        if self.progress.read().phase != SnapSyncPhase::Complete {
            return Ok(None);
        }

        Ok(Some(StateDB::with_root(
            self.db.clone(),
            config.target_root,
        )))
    }

    /// Create account key for storage
    fn account_key(address: &Address) -> Vec<u8> {
        let mut key = Vec::with_capacity(21);
        key.push(b'a'); // Prefix for account
        key.extend_from_slice(address.as_bytes());
        key
    }

    /// Create storage key
    fn storage_key(address: &Address, slot: &U256) -> Vec<u8> {
        let mut key = Vec::with_capacity(53);
        key.push(b's'); // Prefix for storage
        key.extend_from_slice(address.as_bytes());
        key.extend_from_slice(&slot.to_be_bytes());
        key
    }
}

/// Increment an address by 1
fn increment_address(addr: Address) -> Address {
    let mut bytes = *addr.as_bytes();
    for i in (0..20).rev() {
        if bytes[i] < 255 {
            bytes[i] += 1;
            return Address::new(bytes);
        }
        bytes[i] = 0;
    }
    Address::MAX
}

/// Add offset to address
fn add_address_offset(addr: Address, offset: u128) -> Address {
    let mut bytes = *addr.as_bytes();

    // Convert last 16 bytes to u128, add offset, convert back
    let value = u128::from_be_bytes(bytes[4..20].try_into().unwrap());
    let (new_value, overflow) = value.overflowing_add(offset);

    if overflow {
        return Address::MAX;
    }

    bytes[4..20].copy_from_slice(&new_value.to_be_bytes());
    Address::new(bytes)
}

/// Snap sync request types for network protocol
#[derive(Clone, Debug)]
pub enum SnapSyncRequest {
    /// Request account range
    GetAccountRange {
        root: Hash,
        start: Address,
        end: Address,
        limit: usize,
    },
    /// Request storage range
    GetStorageRange {
        root: Hash,
        accounts: Vec<Address>,
        start: U256,
        end: U256,
        limit: usize,
    },
    /// Request trie nodes
    GetTrieNodes { root: Hash, paths: Vec<Vec<u8>> },
}

/// Snap sync response types for network protocol
#[derive(Clone, Debug)]
pub enum SnapSyncResponse {
    AccountRange(AccountRange),
    StorageRange(StorageRange),
    TrieNodes(TrieNodeBatch),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_db() -> Database {
        Database::open_temp().unwrap()
    }

    #[test]
    fn test_snap_sync_manager_creation() {
        let db = create_test_db();
        let manager = SnapSyncManager::new(db);
        assert!(!manager.is_active());
    }

    #[test]
    fn test_snap_sync_start() {
        let db = create_test_db();
        let mut manager = SnapSyncManager::new(db);

        let config = SnapSyncConfig {
            target_root: Hash::new([1; 32]),
            target_block: 1000,
            max_concurrent_requests: 4,
        };

        manager.start(config);

        assert!(manager.is_active());
        assert_eq!(manager.progress().phase, SnapSyncPhase::AccountSync);
    }

    #[test]
    fn test_next_account_range() {
        let db = create_test_db();
        let mut manager = SnapSyncManager::new(db);

        let config = SnapSyncConfig {
            target_root: Hash::new([1; 32]),
            target_block: 1000,
            max_concurrent_requests: 4,
        };

        manager.start(config);

        let range = manager.next_account_range();
        assert!(range.is_some());

        let (start, end) = range.unwrap();
        assert_eq!(start, Address::ZERO);
        assert_ne!(end, start);
    }

    #[test]
    fn test_increment_address() {
        let addr = Address::new([0; 20]);
        let next = increment_address(addr);
        assert_eq!(next.as_bytes()[19], 1);

        let max_minus_one = Address::new([
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xfe,
        ]);
        let should_be_max = increment_address(max_minus_one);
        assert_eq!(should_be_max.as_bytes()[19], 0xff);
    }

    #[test]
    fn test_progress_phases() {
        let progress = SnapSyncProgress::default();
        assert_eq!(progress.phase, SnapSyncPhase::NotStarted);
        assert_eq!(progress.accounts_synced, 0);
        assert_eq!(progress.completion_percentage, 0.0);
    }
}
