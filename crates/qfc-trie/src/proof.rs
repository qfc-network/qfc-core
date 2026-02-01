//! Merkle proof generation and verification

use crate::error::{Result, TrieError};
use crate::nibbles::NibbleSlice;
use crate::node::TrieNode;
use borsh::{BorshDeserialize, BorshSerialize};
use qfc_types::Hash;

/// Merkle proof for a key-value pair
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct MerkleProof {
    /// The key being proved
    pub key: Vec<u8>,
    /// The value (None if proving non-existence)
    pub value: Option<Vec<u8>>,
    /// The proof nodes (serialized)
    pub nodes: Vec<Vec<u8>>,
}

impl MerkleProof {
    /// Verify this proof against a root hash
    pub fn verify(&self, root: &Hash) -> Result<bool> {
        if *root == Hash::ZERO {
            // Empty trie - value must be None
            return Ok(self.value.is_none());
        }

        if self.nodes.is_empty() {
            return Ok(false);
        }

        // Reconstruct the path and verify
        let nibbles = NibbleSlice::from_bytes(&self.key);
        self.verify_path(root, &nibbles, 0)
    }

    fn verify_path(&self, expected_hash: &Hash, key: &NibbleSlice, node_index: usize) -> Result<bool> {
        if node_index >= self.nodes.len() {
            return Ok(false);
        }

        let node = TrieNode::from_bytes(&self.nodes[node_index])
            .map_err(|e| TrieError::Serialization(e.to_string()))?;

        // Verify the node hash matches
        if node.hash() != *expected_hash {
            return Ok(false);
        }

        match node {
            TrieNode::Empty => Ok(self.value.is_none()),

            TrieNode::Leaf { key: leaf_key, value } => {
                let leaf_nibbles = NibbleSlice::from_nibbles(&leaf_key);
                if key.to_nibbles() == leaf_nibbles.to_nibbles() {
                    // Found the key
                    Ok(self.value.as_ref() == Some(&value))
                } else {
                    // Key not found
                    Ok(self.value.is_none())
                }
            }

            TrieNode::Extension { key: ext_key, child } => {
                let ext_nibbles = NibbleSlice::from_nibbles(&ext_key);
                if key.starts_with(&ext_nibbles) {
                    let remaining = key.offset(ext_nibbles.len());
                    self.verify_path(&child, &remaining, node_index + 1)
                } else {
                    // Key doesn't match extension
                    Ok(self.value.is_none())
                }
            }

            TrieNode::Branch { children, value: branch_value } => {
                if key.is_empty() {
                    // At end of key
                    Ok(self.value == branch_value)
                } else {
                    let nibble = key.at(0) as usize;
                    match children[nibble] {
                        Some(child_hash) => {
                            let remaining = key.offset(1);
                            self.verify_path(&child_hash, &remaining, node_index + 1)
                        }
                        None => {
                            // No child at this nibble
                            Ok(self.value.is_none())
                        }
                    }
                }
            }
        }
    }

    /// Serialize the proof
    pub fn to_bytes(&self) -> Vec<u8> {
        borsh::to_vec(self).expect("serialization should not fail")
    }

    /// Deserialize the proof
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        borsh::from_slice(bytes).map_err(|e| TrieError::Serialization(e.to_string()))
    }
}

/// Builder for generating Merkle proofs
pub struct ProofBuilder {
    nodes: Vec<Vec<u8>>,
}

impl ProofBuilder {
    pub fn new() -> Self {
        Self { nodes: Vec::new() }
    }

    /// Add a node to the proof
    pub fn add_node(&mut self, node: &TrieNode) {
        self.nodes.push(node.to_bytes());
    }

    /// Build the proof
    pub fn build(self, key: Vec<u8>, value: Option<Vec<u8>>) -> MerkleProof {
        MerkleProof {
            key,
            value,
            nodes: self.nodes,
        }
    }
}

impl Default for ProofBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_proof_serialization() {
        let proof = MerkleProof {
            key: b"test_key".to_vec(),
            value: Some(b"test_value".to_vec()),
            nodes: vec![
                TrieNode::leaf(
                    NibbleSlice::from_bytes(b"test_key"),
                    b"test_value".to_vec(),
                )
                .to_bytes(),
            ],
        };

        let bytes = proof.to_bytes();
        let decoded = MerkleProof::from_bytes(&bytes).unwrap();
        assert_eq!(proof, decoded);
    }

    #[test]
    fn test_proof_builder() {
        let mut builder = ProofBuilder::new();
        let node = TrieNode::leaf(NibbleSlice::from_bytes(b"key"), b"value".to_vec());
        builder.add_node(&node);

        let proof = builder.build(b"key".to_vec(), Some(b"value".to_vec()));
        assert_eq!(proof.nodes.len(), 1);
    }
}
