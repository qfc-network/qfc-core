//! Merkle Patricia Trie implementation

use crate::error::{Result, TrieError};
use crate::nibbles::NibbleSlice;
use crate::node::TrieNode;
use lru::LruCache;
use parking_lot::RwLock;
use qfc_storage::{cf, Database};
use qfc_types::Hash;
use std::collections::HashMap;
use std::num::NonZeroUsize;

/// Trie configuration
#[derive(Clone, Debug)]
pub struct TrieConfig {
    /// Cache size for nodes
    pub cache_size: usize,
}

impl Default for TrieConfig {
    fn default() -> Self {
        Self { cache_size: 10000 }
    }
}

/// Merkle Patricia Trie
pub struct Trie {
    /// Database for persistent storage
    db: Database,
    /// Current root hash
    root: Hash,
    /// Node cache
    cache: RwLock<LruCache<Hash, TrieNode>>,
    /// Dirty nodes (modified but not committed)
    dirty: RwLock<HashMap<Hash, TrieNode>>,
}

impl Trie {
    /// Create a new trie with empty root
    pub fn new(db: Database) -> Self {
        Self::new_with_config(db, TrieConfig::default())
    }

    /// Create a new trie with configuration
    pub fn new_with_config(db: Database, config: TrieConfig) -> Self {
        Self {
            db,
            root: Hash::ZERO,
            cache: RwLock::new(LruCache::new(
                NonZeroUsize::new(config.cache_size).unwrap(),
            )),
            dirty: RwLock::new(HashMap::new()),
        }
    }

    /// Create a trie with an existing root
    pub fn with_root(db: Database, root: Hash) -> Self {
        let mut trie = Self::new(db);
        trie.root = root;
        trie
    }

    /// Get the current root hash
    pub fn root(&self) -> Hash {
        self.root
    }

    /// Get a value from the trie
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        if self.root == Hash::ZERO {
            return Ok(None);
        }

        let nibbles = NibbleSlice::from_bytes(key);
        self.get_at(&self.root, &nibbles)
    }

    fn get_at(&self, node_hash: &Hash, key: &NibbleSlice) -> Result<Option<Vec<u8>>> {
        if *node_hash == Hash::ZERO {
            return Ok(None);
        }

        let node = self.get_node(node_hash)?;

        match node {
            TrieNode::Empty => Ok(None),

            TrieNode::Leaf {
                key: leaf_key,
                value,
            } => {
                let leaf_nibbles = NibbleSlice::from_nibbles(&leaf_key);
                if key.to_nibbles() == leaf_nibbles.to_nibbles() {
                    Ok(Some(value))
                } else {
                    Ok(None)
                }
            }

            TrieNode::Extension {
                key: ext_key,
                child,
            } => {
                let ext_nibbles = NibbleSlice::from_nibbles(&ext_key);
                if key.starts_with(&ext_nibbles) {
                    let remaining = key.offset(ext_nibbles.len());
                    self.get_at(&child, &remaining)
                } else {
                    Ok(None)
                }
            }

            TrieNode::Branch { children, value } => {
                if key.is_empty() {
                    Ok(value)
                } else {
                    let index = key.at(0) as usize;
                    match children[index] {
                        Some(child_hash) => {
                            let remaining = key.offset(1);
                            self.get_at(&child_hash, &remaining)
                        }
                        None => Ok(None),
                    }
                }
            }
        }
    }

    /// Insert a value into the trie
    pub fn insert(&mut self, key: &[u8], value: Vec<u8>) -> Result<()> {
        let nibbles = NibbleSlice::from_bytes(key);
        let new_root = self.insert_at(&self.root, &nibbles, value)?;
        self.root = new_root;
        Ok(())
    }

    fn insert_at(&self, node_hash: &Hash, key: &NibbleSlice, value: Vec<u8>) -> Result<Hash> {
        if *node_hash == Hash::ZERO {
            // Create a new leaf
            let node = TrieNode::leaf(key.clone(), value);
            return self.store_node(node);
        }

        let node = self.get_node(node_hash)?;

        match node {
            TrieNode::Empty => {
                let node = TrieNode::leaf(key.clone(), value);
                self.store_node(node)
            }

            TrieNode::Leaf {
                key: leaf_key,
                value: leaf_value,
            } => {
                let leaf_nibbles = NibbleSlice::from_nibbles(&leaf_key);

                if key.to_nibbles() == leaf_nibbles.to_nibbles() {
                    // Replace existing value
                    let node = TrieNode::leaf(key.clone(), value);
                    self.store_node(node)
                } else {
                    // Split into branch
                    let common_len = key.common_prefix_len(&leaf_nibbles);

                    // Create branch node
                    let mut children = [None; 16];

                    if common_len < leaf_nibbles.len() {
                        // Old leaf goes into branch
                        let old_nibble = leaf_nibbles.at(common_len) as usize;
                        let old_remaining = leaf_nibbles.offset(common_len + 1);
                        if old_remaining.is_empty() {
                            // Old value is exactly at branch
                            let old_leaf = TrieNode::leaf(NibbleSlice::from_nibbles(&[]), leaf_value);
                            children[old_nibble] = Some(self.store_node(old_leaf)?);
                        } else {
                            let old_leaf = TrieNode::leaf(old_remaining, leaf_value);
                            children[old_nibble] = Some(self.store_node(old_leaf)?);
                        }
                    }

                    let branch_value = if common_len < key.len() {
                        // New value goes into branch child
                        let new_nibble = key.at(common_len) as usize;
                        let new_remaining = key.offset(common_len + 1);
                        if new_remaining.is_empty() {
                            let new_leaf = TrieNode::leaf(NibbleSlice::from_nibbles(&[]), value.clone());
                            children[new_nibble] = Some(self.store_node(new_leaf)?);
                        } else {
                            let new_leaf = TrieNode::leaf(new_remaining, value.clone());
                            children[new_nibble] = Some(self.store_node(new_leaf)?);
                        }
                        None
                    } else {
                        // New key ends at branch
                        Some(value)
                    };

                    let branch = TrieNode::branch(children, branch_value);
                    let branch_hash = self.store_node(branch)?;

                    if common_len > 0 {
                        // Need extension node
                        let ext = TrieNode::extension(key.prefix(common_len), branch_hash);
                        self.store_node(ext)
                    } else {
                        Ok(branch_hash)
                    }
                }
            }

            TrieNode::Extension {
                key: ext_key,
                child,
            } => {
                let ext_nibbles = NibbleSlice::from_nibbles(&ext_key);
                let common_len = key.common_prefix_len(&ext_nibbles);

                if common_len == ext_nibbles.len() {
                    // Key starts with extension, recurse into child
                    let remaining = key.offset(common_len);
                    let new_child = self.insert_at(&child, &remaining, value)?;
                    let new_ext = TrieNode::extension(ext_nibbles, new_child);
                    self.store_node(new_ext)
                } else {
                    // Split the extension
                    let mut children = [None; 16];

                    // Old extension's remaining part
                    let old_remaining = ext_nibbles.offset(common_len + 1);
                    let old_nibble = ext_nibbles.at(common_len) as usize;

                    if old_remaining.is_empty() {
                        children[old_nibble] = Some(child);
                    } else {
                        let old_ext = TrieNode::extension(old_remaining, child);
                        children[old_nibble] = Some(self.store_node(old_ext)?);
                    }

                    // New value
                    let branch_value = if common_len < key.len() {
                        let new_nibble = key.at(common_len) as usize;
                        let new_remaining = key.offset(common_len + 1);
                        let new_leaf = TrieNode::leaf(new_remaining, value.clone());
                        children[new_nibble] = Some(self.store_node(new_leaf)?);
                        None
                    } else {
                        Some(value)
                    };

                    let branch = TrieNode::branch(children, branch_value);
                    let branch_hash = self.store_node(branch)?;

                    if common_len > 0 {
                        let ext = TrieNode::extension(key.prefix(common_len), branch_hash);
                        self.store_node(ext)
                    } else {
                        Ok(branch_hash)
                    }
                }
            }

            TrieNode::Branch { mut children, value: branch_value } => {
                if key.is_empty() {
                    // Update branch value
                    let new_branch = TrieNode::branch(children, Some(value));
                    self.store_node(new_branch)
                } else {
                    // Recurse into appropriate child
                    let nibble = key.at(0) as usize;
                    let remaining = key.offset(1);
                    let child_hash = children[nibble].unwrap_or(Hash::ZERO);
                    let new_child = self.insert_at(&child_hash, &remaining, value)?;
                    children[nibble] = Some(new_child);
                    let new_branch = TrieNode::branch(children, branch_value);
                    self.store_node(new_branch)
                }
            }
        }
    }

    /// Delete a value from the trie
    pub fn delete(&mut self, key: &[u8]) -> Result<bool> {
        if self.root == Hash::ZERO {
            return Ok(false);
        }

        let nibbles = NibbleSlice::from_bytes(key);
        match self.delete_at(&self.root, &nibbles)? {
            Some(new_root) => {
                self.root = new_root;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    fn delete_at(&self, node_hash: &Hash, key: &NibbleSlice) -> Result<Option<Hash>> {
        if *node_hash == Hash::ZERO {
            return Ok(None);
        }

        let node = self.get_node(node_hash)?;

        match node {
            TrieNode::Empty => Ok(None),

            TrieNode::Leaf { key: leaf_key, .. } => {
                let leaf_nibbles = NibbleSlice::from_nibbles(&leaf_key);
                if key.to_nibbles() == leaf_nibbles.to_nibbles() {
                    Ok(Some(Hash::ZERO))
                } else {
                    Ok(None)
                }
            }

            TrieNode::Extension { key: ext_key, child } => {
                let ext_nibbles = NibbleSlice::from_nibbles(&ext_key);
                if key.starts_with(&ext_nibbles) {
                    let remaining = key.offset(ext_nibbles.len());
                    if let Some(new_child) = self.delete_at(&child, &remaining)? {
                        if new_child == Hash::ZERO {
                            Ok(Some(Hash::ZERO))
                        } else {
                            // Might need to collapse
                            let child_node = self.get_node(&new_child)?;
                            match child_node {
                                TrieNode::Extension { key: child_key, child: grandchild } => {
                                    // Merge extensions
                                    let child_nibbles = NibbleSlice::from_nibbles(&child_key);
                                    let mut merged = ext_nibbles.to_nibbles();
                                    merged.extend(child_nibbles.to_nibbles());
                                    let merged_ext = TrieNode::extension(
                                        NibbleSlice::from_nibbles(&merged),
                                        grandchild,
                                    );
                                    Ok(Some(self.store_node(merged_ext)?))
                                }
                                TrieNode::Leaf { key: child_key, value } => {
                                    // Merge extension with leaf
                                    let child_nibbles = NibbleSlice::from_nibbles(&child_key);
                                    let mut merged = ext_nibbles.to_nibbles();
                                    merged.extend(child_nibbles.to_nibbles());
                                    let merged_leaf = TrieNode::leaf(
                                        NibbleSlice::from_nibbles(&merged),
                                        value,
                                    );
                                    Ok(Some(self.store_node(merged_leaf)?))
                                }
                                _ => {
                                    let new_ext = TrieNode::extension(ext_nibbles, new_child);
                                    Ok(Some(self.store_node(new_ext)?))
                                }
                            }
                        }
                    } else {
                        Ok(None)
                    }
                } else {
                    Ok(None)
                }
            }

            TrieNode::Branch { mut children, value } => {
                if key.is_empty() {
                    if value.is_none() {
                        return Ok(None);
                    }
                    // Remove branch value
                    let new_branch = TrieNode::branch(children, None);
                    Ok(Some(self.maybe_collapse_branch(new_branch)?))
                } else {
                    let nibble = key.at(0) as usize;
                    if children[nibble].is_none() {
                        return Ok(None);
                    }
                    let remaining = key.offset(1);
                    if let Some(new_child) = self.delete_at(&children[nibble].unwrap(), &remaining)? {
                        children[nibble] = if new_child == Hash::ZERO {
                            None
                        } else {
                            Some(new_child)
                        };
                        let new_branch = TrieNode::branch(children, value);
                        Ok(Some(self.maybe_collapse_branch(new_branch)?))
                    } else {
                        Ok(None)
                    }
                }
            }
        }
    }

    fn maybe_collapse_branch(&self, branch: TrieNode) -> Result<Hash> {
        if let TrieNode::Branch { children, value } = &branch {
            let child_count = children.iter().filter(|c| c.is_some()).count();

            if child_count == 0 {
                if let Some(v) = value {
                    // Branch with only value becomes leaf with empty key
                    let leaf = TrieNode::leaf(NibbleSlice::from_nibbles(&[]), v.clone());
                    return self.store_node(leaf);
                } else {
                    return Ok(Hash::ZERO);
                }
            }

            if child_count == 1 && value.is_none() {
                // Collapse single-child branch
                let (nibble, child_hash) = children
                    .iter()
                    .enumerate()
                    .find(|(_, c)| c.is_some())
                    .map(|(i, c)| (i as u8, c.unwrap()))
                    .unwrap();

                let child_node = self.get_node(&child_hash)?;
                match child_node {
                    TrieNode::Extension { key, child } => {
                        let mut new_key = vec![nibble];
                        new_key.extend(&key);
                        let ext = TrieNode::extension(NibbleSlice::from_nibbles(&new_key), child);
                        return self.store_node(ext);
                    }
                    TrieNode::Leaf { key, value } => {
                        let mut new_key = vec![nibble];
                        new_key.extend(&key);
                        let leaf = TrieNode::leaf(NibbleSlice::from_nibbles(&new_key), value);
                        return self.store_node(leaf);
                    }
                    _ => {
                        // Can't collapse, create extension
                        let ext = TrieNode::extension(
                            NibbleSlice::from_nibbles(&[nibble]),
                            child_hash,
                        );
                        return self.store_node(ext);
                    }
                }
            }
        }

        self.store_node(branch)
    }

    /// Get a node from cache or database
    fn get_node(&self, hash: &Hash) -> Result<TrieNode> {
        if *hash == Hash::ZERO {
            return Ok(TrieNode::Empty);
        }

        // Check dirty nodes first
        if let Some(node) = self.dirty.read().get(hash) {
            return Ok(node.clone());
        }

        // Check cache
        if let Some(node) = self.cache.write().get(hash) {
            return Ok(node.clone());
        }

        // Load from database
        let bytes = self
            .db
            .get(cf::STATE, hash.as_bytes())?
            .ok_or_else(|| TrieError::NodeNotFound(hash.to_string()))?;

        let node =
            TrieNode::from_bytes(&bytes).map_err(|e| TrieError::Serialization(e.to_string()))?;

        // Add to cache
        self.cache.write().put(*hash, node.clone());

        Ok(node)
    }

    /// Store a node in the dirty set
    fn store_node(&self, node: TrieNode) -> Result<Hash> {
        let hash = node.hash();
        if hash != Hash::ZERO {
            self.dirty.write().insert(hash, node);
        }
        Ok(hash)
    }

    /// Commit all dirty nodes to the database
    pub fn commit(&mut self) -> Result<Hash> {
        let dirty_nodes = std::mem::take(&mut *self.dirty.write());

        let mut batch = qfc_storage::WriteBatch::new();
        for (hash, node) in dirty_nodes {
            let bytes = node.to_bytes();
            batch.put(cf::STATE, hash.as_bytes().to_vec(), bytes);

            // Also add to cache
            self.cache.write().put(hash, node);
        }

        self.db.write_batch(batch)?;

        Ok(self.root)
    }

    /// Check if a key exists
    pub fn contains(&self, key: &[u8]) -> Result<bool> {
        Ok(self.get(key)?.is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_trie() -> Trie {
        let db = Database::open_temp().unwrap();
        Trie::new(db)
    }

    #[test]
    fn test_empty_trie() {
        let trie = create_test_trie();
        assert_eq!(trie.root(), Hash::ZERO);
        assert!(trie.get(b"key").unwrap().is_none());
    }

    #[test]
    fn test_insert_and_get() {
        let mut trie = create_test_trie();

        trie.insert(b"key1", b"value1".to_vec()).unwrap();
        assert_eq!(trie.get(b"key1").unwrap(), Some(b"value1".to_vec()));
        assert!(trie.get(b"key2").unwrap().is_none());
    }

    #[test]
    fn test_insert_multiple() {
        let mut trie = create_test_trie();

        trie.insert(b"key1", b"value1".to_vec()).unwrap();
        trie.insert(b"key2", b"value2".to_vec()).unwrap();
        trie.insert(b"key3", b"value3".to_vec()).unwrap();

        assert_eq!(trie.get(b"key1").unwrap(), Some(b"value1".to_vec()));
        assert_eq!(trie.get(b"key2").unwrap(), Some(b"value2".to_vec()));
        assert_eq!(trie.get(b"key3").unwrap(), Some(b"value3".to_vec()));
    }

    #[test]
    fn test_update_value() {
        let mut trie = create_test_trie();

        trie.insert(b"key", b"value1".to_vec()).unwrap();
        trie.insert(b"key", b"value2".to_vec()).unwrap();

        assert_eq!(trie.get(b"key").unwrap(), Some(b"value2".to_vec()));
    }

    #[test]
    fn test_delete() {
        let mut trie = create_test_trie();

        trie.insert(b"key1", b"value1".to_vec()).unwrap();
        trie.insert(b"key2", b"value2".to_vec()).unwrap();

        assert!(trie.delete(b"key1").unwrap());
        assert!(trie.get(b"key1").unwrap().is_none());
        assert_eq!(trie.get(b"key2").unwrap(), Some(b"value2".to_vec()));
    }

    #[test]
    fn test_commit() {
        let db = Database::open_temp().unwrap();
        let root = {
            let mut trie = Trie::new(db.clone());
            trie.insert(b"key1", b"value1".to_vec()).unwrap();
            trie.insert(b"key2", b"value2".to_vec()).unwrap();
            trie.commit().unwrap()
        };

        // Reopen with same root
        let trie = Trie::with_root(db, root);
        assert_eq!(trie.get(b"key1").unwrap(), Some(b"value1".to_vec()));
        assert_eq!(trie.get(b"key2").unwrap(), Some(b"value2".to_vec()));
    }

    #[test]
    fn test_root_changes() {
        let mut trie = create_test_trie();

        let root1 = trie.root();
        trie.insert(b"key", b"value".to_vec()).unwrap();
        let root2 = trie.root();

        assert_ne!(root1, root2);
    }
}
