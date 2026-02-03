//! Database schema definitions

/// Column family names
pub mod cf {
    /// Block headers: height (u64 BE) -> BlockHeader
    pub const BLOCK_HEADERS: &str = "block_headers";

    /// Block bodies: height (u64 BE) -> BlockBody
    pub const BLOCK_BODIES: &str = "block_bodies";

    /// Block hash index: hash -> height (u64 BE)
    pub const BLOCK_HASH_INDEX: &str = "block_hash_index";

    /// Transactions: hash -> Transaction
    pub const TRANSACTIONS: &str = "transactions";

    /// Transaction index: hash -> (block_height, tx_index)
    pub const TX_INDEX: &str = "tx_index";

    /// Receipts: hash -> Receipt
    pub const RECEIPTS: &str = "receipts";

    /// State trie nodes: hash -> TrieNode
    pub const STATE: &str = "state";

    /// Contract code: hash -> bytes
    pub const CODE: &str = "code";

    /// Metadata: key -> value
    pub const METADATA: &str = "metadata";

    /// Validators: address -> ValidatorNode
    pub const VALIDATORS: &str = "validators";

    /// Rewards: block_height (u64 BE) -> RewardDistribution
    pub const REWARDS: &str = "rewards";

    /// Delegations: delegator_address + validator_address -> Delegation
    pub const DELEGATIONS: &str = "delegations";

    /// Undelegations: delegator_address + validator_address + unlock_at -> Undelegation
    pub const UNDELEGATIONS: &str = "undelegations";

    /// Validator checkpoints: epoch (u64 BE) -> ValidatorCheckpoint
    pub const CHECKPOINTS: &str = "checkpoints";

    /// All column families
    pub const ALL: &[&str] = &[
        BLOCK_HEADERS,
        BLOCK_BODIES,
        BLOCK_HASH_INDEX,
        TRANSACTIONS,
        TX_INDEX,
        RECEIPTS,
        STATE,
        CODE,
        METADATA,
        VALIDATORS,
        REWARDS,
        DELEGATIONS,
        UNDELEGATIONS,
        CHECKPOINTS,
    ];
}

/// Metadata keys
pub mod meta {
    /// Latest block number
    pub const LATEST_BLOCK_NUMBER: &[u8] = b"latest_block_number";

    /// Latest state root
    pub const LATEST_STATE_ROOT: &[u8] = b"latest_state_root";

    /// Genesis hash
    pub const GENESIS_HASH: &[u8] = b"genesis_hash";

    /// Chain ID
    pub const CHAIN_ID: &[u8] = b"chain_id";

    /// Database version
    pub const DB_VERSION: &[u8] = b"db_version";
}

/// Current database version
pub const DB_VERSION: u32 = 1;

/// Encode a u64 as big-endian bytes for key ordering
pub fn encode_block_number(number: u64) -> [u8; 8] {
    number.to_be_bytes()
}

/// Decode a u64 from big-endian bytes
pub fn decode_block_number(bytes: &[u8]) -> Option<u64> {
    if bytes.len() != 8 {
        return None;
    }
    Some(u64::from_be_bytes(bytes.try_into().ok()?))
}

/// Encode transaction location (block height + tx index)
pub fn encode_tx_location(block_height: u64, tx_index: u32) -> [u8; 12] {
    let mut bytes = [0u8; 12];
    bytes[0..8].copy_from_slice(&block_height.to_be_bytes());
    bytes[8..12].copy_from_slice(&tx_index.to_be_bytes());
    bytes
}

/// Decode transaction location
pub fn decode_tx_location(bytes: &[u8]) -> Option<(u64, u32)> {
    if bytes.len() != 12 {
        return None;
    }
    let height = u64::from_be_bytes(bytes[0..8].try_into().ok()?);
    let index = u32::from_be_bytes(bytes[8..12].try_into().ok()?);
    Some((height, index))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_block_number() {
        let num = 12345u64;
        let encoded = encode_block_number(num);
        let decoded = decode_block_number(&encoded).unwrap();
        assert_eq!(num, decoded);
    }

    #[test]
    fn test_block_number_ordering() {
        // Verify that encoding preserves ordering
        let nums = [0u64, 1, 100, 1000, u64::MAX];
        let encoded: Vec<_> = nums.iter().map(|&n| encode_block_number(n)).collect();

        for i in 0..encoded.len() - 1 {
            assert!(encoded[i] < encoded[i + 1]);
        }
    }

    #[test]
    fn test_encode_decode_tx_location() {
        let height = 12345u64;
        let index = 42u32;
        let encoded = encode_tx_location(height, index);
        let (dec_height, dec_index) = decode_tx_location(&encoded).unwrap();
        assert_eq!(height, dec_height);
        assert_eq!(index, dec_index);
    }
}
