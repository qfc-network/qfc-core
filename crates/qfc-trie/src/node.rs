//! Trie node types

use crate::nibbles::NibbleSlice;
use borsh::{BorshDeserialize, BorshSerialize};
use qfc_types::Hash;

/// Trie node types
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum TrieNode {
    /// Empty node (null)
    Empty,

    /// Leaf node: contains remaining key path and value
    Leaf {
        /// Remaining nibbles of the key
        key: Vec<u8>,
        /// Value stored at this leaf
        value: Vec<u8>,
    },

    /// Extension node: contains shared prefix and child hash
    Extension {
        /// Shared nibbles prefix
        key: Vec<u8>,
        /// Hash of the child node
        child: Hash,
    },

    /// Branch node: 16 children (one per nibble) plus optional value
    Branch {
        /// Children (16 slots, one per nibble value 0-15)
        children: [Option<Hash>; 16],
        /// Value if this branch is also an end of a key
        value: Option<Vec<u8>>,
    },
}

impl Default for TrieNode {
    fn default() -> Self {
        Self::Empty
    }
}

impl TrieNode {
    /// Create an empty node
    pub fn empty() -> Self {
        Self::Empty
    }

    /// Create a leaf node
    pub fn leaf(key: NibbleSlice, value: Vec<u8>) -> Self {
        Self::Leaf {
            key: key.to_nibbles(),
            value,
        }
    }

    /// Create an extension node
    pub fn extension(key: NibbleSlice, child: Hash) -> Self {
        Self::Extension {
            key: key.to_nibbles(),
            child,
        }
    }

    /// Create a branch node
    pub fn branch(children: [Option<Hash>; 16], value: Option<Vec<u8>>) -> Self {
        Self::Branch { children, value }
    }

    /// Check if this is an empty node
    pub fn is_empty(&self) -> bool {
        matches!(self, Self::Empty)
    }

    /// Check if this is a leaf node
    pub fn is_leaf(&self) -> bool {
        matches!(self, Self::Leaf { .. })
    }

    /// Check if this is an extension node
    pub fn is_extension(&self) -> bool {
        matches!(self, Self::Extension { .. })
    }

    /// Check if this is a branch node
    pub fn is_branch(&self) -> bool {
        matches!(self, Self::Branch { .. })
    }

    /// Get the key for leaf or extension nodes
    pub fn get_key(&self) -> Option<NibbleSlice> {
        match self {
            Self::Leaf { key, .. } | Self::Extension { key, .. } => {
                Some(NibbleSlice::from_nibbles(key))
            }
            _ => None,
        }
    }

    /// Get the value for leaf or branch nodes
    pub fn get_value(&self) -> Option<&[u8]> {
        match self {
            Self::Leaf { value, .. } => Some(value),
            Self::Branch { value: Some(v), .. } => Some(v),
            _ => None,
        }
    }

    /// Get the child hash for extension nodes
    pub fn get_child(&self) -> Option<Hash> {
        match self {
            Self::Extension { child, .. } => Some(*child),
            _ => None,
        }
    }

    /// Get a child at index for branch nodes
    pub fn get_child_at(&self, index: u8) -> Option<Hash> {
        match self {
            Self::Branch { children, .. } if index < 16 => children[index as usize],
            _ => None,
        }
    }

    /// Count the number of children in a branch node
    pub fn child_count(&self) -> usize {
        match self {
            Self::Branch { children, .. } => children.iter().filter(|c| c.is_some()).count(),
            _ => 0,
        }
    }

    /// Serialize to bytes
    pub fn to_bytes(&self) -> Vec<u8> {
        borsh::to_vec(self).expect("serialization should not fail")
    }

    /// Deserialize from bytes
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, borsh::io::Error> {
        borsh::from_slice(bytes)
    }

    /// Compute the hash of this node
    pub fn hash(&self) -> Hash {
        if self.is_empty() {
            return Hash::ZERO;
        }
        qfc_crypto::blake3_hash(&self.to_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_node() {
        let node = TrieNode::empty();
        assert!(node.is_empty());
        assert_eq!(node.hash(), Hash::ZERO);
    }

    #[test]
    fn test_leaf_node() {
        let key = NibbleSlice::from_bytes(&[0x12, 0x34]);
        let node = TrieNode::leaf(key.clone(), b"value".to_vec());

        assert!(node.is_leaf());
        assert_eq!(node.get_key().unwrap().to_nibbles(), key.to_nibbles());
        assert_eq!(node.get_value(), Some(b"value".as_slice()));
    }

    #[test]
    fn test_extension_node() {
        let key = NibbleSlice::from_bytes(&[0xab]);
        let child_hash = Hash::new([0x11; 32]);
        let node = TrieNode::extension(key, child_hash);

        assert!(node.is_extension());
        assert_eq!(node.get_child(), Some(child_hash));
    }

    #[test]
    fn test_branch_node() {
        let mut children = [None; 16];
        children[0] = Some(Hash::new([0x01; 32]));
        children[5] = Some(Hash::new([0x05; 32]));

        let node = TrieNode::branch(children, Some(b"branch_value".to_vec()));

        assert!(node.is_branch());
        assert_eq!(node.child_count(), 2);
        assert!(node.get_child_at(0).is_some());
        assert!(node.get_child_at(1).is_none());
        assert!(node.get_child_at(5).is_some());
    }

    #[test]
    fn test_node_serialization() {
        let key = NibbleSlice::from_bytes(&[0x12, 0x34]);
        let node = TrieNode::leaf(key, b"test".to_vec());

        let bytes = node.to_bytes();
        let decoded = TrieNode::from_bytes(&bytes).unwrap();
        assert_eq!(node, decoded);
    }
}
