//! State pruning for removing old state data

use crate::error::Result;
use parking_lot::RwLock;
use qfc_storage::{cf, Database, WriteBatch};
use qfc_trie::TrieNode;
use qfc_types::Hash;
use std::collections::{HashSet, VecDeque};
use tracing::{debug, info};

/// Configuration for state pruning
#[derive(Clone, Debug)]
pub struct PruningConfig {
    /// Number of recent blocks to keep full state for
    pub keep_recent: u64,
    /// Whether to keep finalized block states
    pub keep_finalized: bool,
    /// Batch size for deletion operations
    pub batch_size: usize,
    /// Minimum interval between pruning operations (in blocks)
    pub prune_interval: u64,
}

impl Default for PruningConfig {
    fn default() -> Self {
        Self {
            keep_recent: 128,
            keep_finalized: true,
            batch_size: 1000,
            prune_interval: 100,
        }
    }
}

/// State pruner for removing old state data
pub struct StatePruner {
    db: Database,
    config: PruningConfig,
    /// State roots that must be preserved
    preserved_roots: RwLock<HashSet<Hash>>,
    /// Block heights mapped to their state roots
    block_roots: RwLock<VecDeque<(u64, Hash)>>,
    /// Last pruned block height
    last_pruned_height: RwLock<u64>,
}

impl StatePruner {
    /// Create a new state pruner
    pub fn new(db: Database, config: PruningConfig) -> Self {
        Self {
            db,
            config,
            preserved_roots: RwLock::new(HashSet::new()),
            block_roots: RwLock::new(VecDeque::new()),
            last_pruned_height: RwLock::new(0),
        }
    }

    /// Create with default configuration
    pub fn with_defaults(db: Database) -> Self {
        Self::new(db, PruningConfig::default())
    }

    /// Register a new block's state root
    pub fn register_block(&self, height: u64, state_root: Hash) {
        let mut block_roots = self.block_roots.write();
        block_roots.push_back((height, state_root));

        // Remove old entries beyond what we need to track
        let max_tracked = self.config.keep_recent * 2;
        while block_roots.len() > max_tracked as usize {
            block_roots.pop_front();
        }
    }

    /// Mark a state root as finalized (should be preserved)
    pub fn mark_finalized(&self, state_root: Hash) {
        if self.config.keep_finalized && state_root != Hash::ZERO {
            self.preserved_roots.write().insert(state_root);
            debug!("Marked state root as finalized: {}", state_root);
        }
    }

    /// Check if pruning should be performed
    pub fn should_prune(&self, current_height: u64) -> bool {
        let last_pruned = *self.last_pruned_height.read();
        current_height >= last_pruned + self.config.prune_interval
    }

    /// Perform state pruning
    ///
    /// Returns the number of nodes pruned
    pub fn prune(&self, current_height: u64) -> Result<usize> {
        if current_height < self.config.keep_recent {
            debug!("Not enough blocks for pruning");
            return Ok(0);
        }

        let prune_before = current_height.saturating_sub(self.config.keep_recent);

        info!(
            "Starting state pruning for blocks before height {}",
            prune_before
        );

        // Collect roots to keep
        let roots_to_keep = self.collect_roots_to_keep(current_height)?;

        if roots_to_keep.is_empty() {
            debug!("No roots to preserve, skipping pruning");
            return Ok(0);
        }

        // Mark all reachable nodes from preserved roots
        let reachable_nodes = self.mark_reachable_nodes(&roots_to_keep)?;

        info!(
            "Marked {} reachable nodes from {} preserved roots",
            reachable_nodes.len(),
            roots_to_keep.len()
        );

        // Collect and delete unreachable nodes
        let pruned_count = self.delete_unreachable_nodes(&reachable_nodes, prune_before)?;

        *self.last_pruned_height.write() = current_height;

        info!("Pruning complete: {} nodes removed", pruned_count);

        Ok(pruned_count)
    }

    /// Collect all state roots that should be preserved
    fn collect_roots_to_keep(&self, current_height: u64) -> Result<HashSet<Hash>> {
        let mut roots = HashSet::new();

        // Add preserved (finalized) roots
        roots.extend(self.preserved_roots.read().iter().cloned());

        // Add recent block roots
        let block_roots = self.block_roots.read();
        let keep_from = current_height.saturating_sub(self.config.keep_recent);

        for (height, root) in block_roots.iter() {
            if *height >= keep_from && *root != Hash::ZERO {
                roots.insert(*root);
            }
        }

        // Remove ZERO hash if present
        roots.remove(&Hash::ZERO);

        Ok(roots)
    }

    /// Mark all nodes reachable from the given roots
    fn mark_reachable_nodes(&self, roots: &HashSet<Hash>) -> Result<HashSet<Hash>> {
        let mut reachable = HashSet::new();
        let mut queue: Vec<Hash> = roots.iter().cloned().collect();

        while let Some(hash) = queue.pop() {
            if hash == Hash::ZERO || reachable.contains(&hash) {
                continue;
            }

            // Try to load node from database
            if let Some(bytes) = self.db.get(cf::STATE, hash.as_bytes())? {
                reachable.insert(hash);

                // Parse node and add children to queue
                if let Ok(node) = TrieNode::from_bytes(&bytes) {
                    match node {
                        TrieNode::Empty => {}
                        TrieNode::Leaf { .. } => {}
                        TrieNode::Extension { child, .. } => {
                            if child != Hash::ZERO {
                                queue.push(child);
                            }
                        }
                        TrieNode::Branch { children, .. } => {
                            for child in children.iter().flatten() {
                                if *child != Hash::ZERO {
                                    queue.push(*child);
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(reachable)
    }

    /// Delete nodes that are not in the reachable set
    fn delete_unreachable_nodes(
        &self,
        reachable: &HashSet<Hash>,
        _prune_before: u64,
    ) -> Result<usize> {
        let mut deleted: usize = 0;
        let mut batch = WriteBatch::new();

        // Iterate through all state nodes
        let iter = self.db.iter(cf::STATE)?;

        for (key, _value) in iter {
            if key.len() != 32 {
                continue;
            }

            // Parse hash from key
            let hash = match Hash::from_slice(&key) {
                Some(h) => h,
                None => continue,
            };

            if !reachable.contains(&hash) {
                batch.delete(cf::STATE, key.to_vec());
                deleted += 1;

                // Write batch when it gets large
                if deleted % self.config.batch_size == 0 {
                    self.db.write_batch(std::mem::take(&mut batch))?;
                    batch = WriteBatch::new();
                    debug!("Deleted {} nodes so far", deleted);
                }
            }
        }

        // Write any remaining deletions
        if !batch.is_empty() {
            self.db.write_batch(batch)?;
        }

        Ok(deleted)
    }

    /// Get pruning statistics
    pub fn stats(&self) -> PruningStats {
        PruningStats {
            preserved_roots: self.preserved_roots.read().len(),
            tracked_blocks: self.block_roots.read().len(),
            last_pruned_height: *self.last_pruned_height.read(),
            config: self.config.clone(),
        }
    }

    /// Clear all preserved roots (use with caution)
    pub fn clear_preserved_roots(&self) {
        self.preserved_roots.write().clear();
    }

    /// Estimate the number of pruneable nodes without actually pruning
    pub fn estimate_pruneable(&self, current_height: u64) -> Result<usize> {
        if current_height < self.config.keep_recent {
            return Ok(0);
        }

        let roots_to_keep = self.collect_roots_to_keep(current_height)?;
        let reachable = self.mark_reachable_nodes(&roots_to_keep)?;

        let mut total_nodes: usize = 0;
        let iter = self.db.iter(cf::STATE)?;

        for (key, _) in iter {
            if key.len() == 32 {
                total_nodes += 1;
            }
        }

        Ok(total_nodes.saturating_sub(reachable.len()))
    }
}

/// Statistics about pruning state
#[derive(Clone, Debug)]
pub struct PruningStats {
    /// Number of preserved (finalized) roots
    pub preserved_roots: usize,
    /// Number of tracked block-to-root mappings
    pub tracked_blocks: usize,
    /// Height of last pruning operation
    pub last_pruned_height: u64,
    /// Current configuration
    pub config: PruningConfig,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_db() -> Database {
        Database::open_temp().unwrap()
    }

    #[test]
    fn test_pruning_config_default() {
        let config = PruningConfig::default();
        assert_eq!(config.keep_recent, 128);
        assert!(config.keep_finalized);
        assert_eq!(config.batch_size, 1000);
    }

    #[test]
    fn test_register_block() {
        let db = create_test_db();
        let pruner = StatePruner::with_defaults(db);

        let hash1 = Hash::new([1; 32]);
        let hash2 = Hash::new([2; 32]);

        pruner.register_block(1, hash1);
        pruner.register_block(2, hash2);

        let stats = pruner.stats();
        assert_eq!(stats.tracked_blocks, 2);
    }

    #[test]
    fn test_mark_finalized() {
        let db = create_test_db();
        let pruner = StatePruner::with_defaults(db);

        let hash = Hash::new([1; 32]);
        pruner.mark_finalized(hash);

        let stats = pruner.stats();
        assert_eq!(stats.preserved_roots, 1);
    }

    #[test]
    fn test_should_prune() {
        let db = create_test_db();
        let config = PruningConfig {
            prune_interval: 10,
            ..Default::default()
        };
        let pruner = StatePruner::new(db, config);

        assert!(!pruner.should_prune(5));
        assert!(pruner.should_prune(10));
        assert!(pruner.should_prune(15));
    }

    #[test]
    fn test_prune_too_early() {
        let db = create_test_db();
        let pruner = StatePruner::with_defaults(db);

        // Not enough blocks yet
        let result = pruner.prune(50);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 0);
    }

    #[test]
    fn test_collect_roots_to_keep() {
        let db = create_test_db();
        let config = PruningConfig {
            keep_recent: 10,
            ..Default::default()
        };
        let pruner = StatePruner::new(db, config);

        // Register some blocks
        for i in 1..=20 {
            let hash = Hash::new([i as u8; 32]);
            pruner.register_block(i as u64, hash);
        }

        // Mark one as finalized
        let finalized = Hash::new([5; 32]);
        pruner.mark_finalized(finalized);

        let roots = pruner.collect_roots_to_keep(20).unwrap();

        // Should include finalized root and recent 10 blocks (10-20)
        assert!(roots.contains(&finalized));
        assert!(roots.contains(&Hash::new([20; 32])));
        assert!(roots.contains(&Hash::new([11; 32])));
        // Block 9 should not be included (keep_recent = 10, current = 20, keep_from = 10)
        assert!(!roots.contains(&Hash::new([9; 32])));
    }

    #[test]
    fn test_pruning_stats() {
        let db = create_test_db();
        let pruner = StatePruner::with_defaults(db);

        let stats = pruner.stats();
        assert_eq!(stats.preserved_roots, 0);
        assert_eq!(stats.tracked_blocks, 0);
        assert_eq!(stats.last_pruned_height, 0);
    }
}
