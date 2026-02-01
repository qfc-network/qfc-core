//! Block and BlockHeader types

use crate::{Address, Hash, Signature, Transaction, Vote};
use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// VRF proof for block producer selection
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct VrfProof {
    /// VRF output (32 bytes)
    pub output: [u8; 32],
    /// VRF proof (64 bytes)
    pub proof: [u8; 64],
}

impl Serialize for VrfProof {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("VrfProof", 2)?;
        state.serialize_field("output", &hex::encode(&self.output))?;
        state.serialize_field("proof", &hex::encode(&self.proof))?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for VrfProof {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct VrfProofHelper {
            output: String,
            proof: String,
        }
        let helper = VrfProofHelper::deserialize(deserializer)?;
        let output_bytes = hex::decode(helper.output.strip_prefix("0x").unwrap_or(&helper.output))
            .map_err(serde::de::Error::custom)?;
        let proof_bytes = hex::decode(helper.proof.strip_prefix("0x").unwrap_or(&helper.proof))
            .map_err(serde::de::Error::custom)?;

        let mut output = [0u8; 32];
        let mut proof = [0u8; 64];

        if output_bytes.len() != 32 {
            return Err(serde::de::Error::custom("invalid output length"));
        }
        if proof_bytes.len() != 64 {
            return Err(serde::de::Error::custom("invalid proof length"));
        }

        output.copy_from_slice(&output_bytes);
        proof.copy_from_slice(&proof_bytes);

        Ok(VrfProof { output, proof })
    }
}

impl Default for VrfProof {
    fn default() -> Self {
        Self {
            output: [0u8; 32],
            proof: [0u8; 64],
        }
    }
}

impl VrfProof {
    pub fn new(output: [u8; 32], proof: [u8; 64]) -> Self {
        Self { output, proof }
    }
}

/// Block header containing metadata
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct BlockHeader {
    /// Block version number
    pub version: u32,

    /// Block height
    pub number: u64,

    /// Parent block hash
    pub parent_hash: Hash,

    /// State root (Merkle Patricia Trie)
    pub state_root: Hash,

    /// Transactions root (Merkle Tree)
    pub transactions_root: Hash,

    /// Receipts root (Merkle Tree)
    pub receipts_root: Hash,

    /// Block producer address
    pub producer: Address,

    /// Producer's contribution score
    pub contribution_score: u64,

    /// VRF proof for random selection
    pub vrf_proof: VrfProof,

    /// Timestamp in milliseconds
    pub timestamp: u64,

    /// Gas limit for this block
    pub gas_limit: u64,

    /// Total gas used
    pub gas_used: u64,

    /// Extra data (max 32 bytes)
    pub extra_data: Vec<u8>,
}

impl Default for BlockHeader {
    fn default() -> Self {
        Self {
            version: 1,
            number: 0,
            parent_hash: Hash::ZERO,
            state_root: Hash::ZERO,
            transactions_root: Hash::ZERO,
            receipts_root: Hash::ZERO,
            producer: Address::ZERO,
            contribution_score: 0,
            vrf_proof: VrfProof::default(),
            timestamp: 0,
            gas_limit: crate::DEFAULT_BLOCK_GAS_LIMIT,
            gas_used: 0,
            extra_data: Vec::new(),
        }
    }
}

impl BlockHeader {
    /// Serialize header for hashing
    pub fn to_bytes(&self) -> Vec<u8> {
        borsh::to_vec(self).expect("serialization should not fail")
    }
}

/// Complete block with transactions and votes
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct Block {
    /// Block header
    pub header: BlockHeader,

    /// Transactions included in this block
    pub transactions: Vec<Transaction>,

    /// Votes from validators for finality
    pub votes: Vec<Vote>,

    /// Block producer's signature
    pub signature: Signature,
}

impl Default for Block {
    fn default() -> Self {
        Self {
            header: BlockHeader::default(),
            transactions: Vec::new(),
            votes: Vec::new(),
            signature: Signature::ZERO,
        }
    }
}

impl Block {
    /// Create a new block
    pub fn new(header: BlockHeader, transactions: Vec<Transaction>) -> Self {
        Self {
            header,
            transactions,
            votes: Vec::new(),
            signature: Signature::ZERO,
        }
    }

    /// Get block number (height)
    pub fn number(&self) -> u64 {
        self.header.number
    }

    /// Get parent hash
    pub fn parent_hash(&self) -> Hash {
        self.header.parent_hash
    }

    /// Get state root
    pub fn state_root(&self) -> Hash {
        self.header.state_root
    }

    /// Get block producer
    pub fn producer(&self) -> Address {
        self.header.producer
    }

    /// Get timestamp
    pub fn timestamp(&self) -> u64 {
        self.header.timestamp
    }

    /// Get gas used
    pub fn gas_used(&self) -> u64 {
        self.header.gas_used
    }

    /// Get gas limit
    pub fn gas_limit(&self) -> u64 {
        self.header.gas_limit
    }

    /// Get transaction count
    pub fn tx_count(&self) -> usize {
        self.transactions.len()
    }

    /// Check if this is the genesis block
    pub fn is_genesis(&self) -> bool {
        self.header.number == 0
    }

    /// Set the block signature
    pub fn set_signature(&mut self, signature: Signature) {
        self.signature = signature;
    }

    /// Add a vote
    pub fn add_vote(&mut self, vote: Vote) {
        self.votes.push(vote);
    }

    /// Serialize header for hashing (hash is computed from header only)
    pub fn header_bytes(&self) -> Vec<u8> {
        self.header.to_bytes()
    }
}

/// Block body (transactions only, for separate storage)
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct BlockBody {
    pub transactions: Vec<Transaction>,
    pub votes: Vec<Vote>,
}

impl BlockBody {
    pub fn new(transactions: Vec<Transaction>, votes: Vec<Vote>) -> Self {
        Self { transactions, votes }
    }

    pub fn from_block(block: &Block) -> Self {
        Self {
            transactions: block.transactions.clone(),
            votes: block.votes.clone(),
        }
    }
}

/// Sealed block with computed hash (after mining/producing)
#[derive(Clone, Debug)]
pub struct SealedBlock {
    /// Block hash
    pub hash: Hash,
    /// The block
    pub block: Block,
}

impl SealedBlock {
    pub fn new(hash: Hash, block: Block) -> Self {
        Self { hash, block }
    }

    pub fn number(&self) -> u64 {
        self.block.number()
    }

    pub fn parent_hash(&self) -> Hash {
        self.block.parent_hash()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_block_header_default() {
        let header = BlockHeader::default();
        assert_eq!(header.version, 1);
        assert_eq!(header.number, 0);
        assert_eq!(header.parent_hash, Hash::ZERO);
    }

    #[test]
    fn test_block_serialization() {
        let block = Block::default();
        let bytes = borsh::to_vec(&block).unwrap();
        let decoded: Block = borsh::from_slice(&bytes).unwrap();
        assert_eq!(block, decoded);
    }

    #[test]
    fn test_block_is_genesis() {
        let mut block = Block::default();
        assert!(block.is_genesis());

        block.header.number = 1;
        assert!(!block.is_genesis());
    }
}
