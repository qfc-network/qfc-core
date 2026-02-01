//! Genesis block configuration

use qfc_crypto::blake3_hash;
use qfc_types::{
    Address, Block, BlockHeader, Hash, Signature, U256, VrfProof, BLOCK_VERSION,
    DEFAULT_BLOCK_GAS_LIMIT, DEFAULT_CHAIN_ID,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Genesis block configuration
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GenesisConfig {
    /// Chain ID
    pub chain_id: u64,

    /// Timestamp
    pub timestamp: u64,

    /// Extra data
    pub extra_data: Vec<u8>,

    /// Initial allocations (address -> balance)
    pub alloc: HashMap<String, GenesisAllocation>,

    /// Initial validators
    pub validators: Vec<GenesisValidator>,
}

/// Genesis allocation
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GenesisAllocation {
    /// Balance in wei
    pub balance: String,
}

/// Genesis validator
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GenesisValidator {
    /// Validator address
    pub address: String,
    /// Initial stake
    pub stake: String,
}

impl Default for GenesisConfig {
    fn default() -> Self {
        Self::testnet()
    }
}

impl GenesisConfig {
    /// Create testnet genesis config (same as dev for network compatibility)
    pub fn testnet() -> Self {
        // Use same genesis as dev() to ensure all nodes on the network
        // have the same genesis hash. The only difference is that dev mode
        // also enables automatic block production.
        Self::dev()
    }

    /// Create dev genesis config with rich accounts
    pub fn dev() -> Self {
        let mut alloc = HashMap::new();

        // Dev accounts with lots of tokens
        for i in 1..=10 {
            alloc.insert(
                format!("0x{:040x}", i),
                GenesisAllocation {
                    balance: "1000000000000000000000000000".to_string(), // 1B QFC each
                },
            );
        }

        // Dev validator address (derived from secret key [0x42; 32])
        // This is the deterministic dev validator that produces blocks in dev mode
        let validators = vec![GenesisValidator {
            address: "0x10d7812fbe50096ae82569fdad35f79628bc0084".to_string(),
            stake: "1000000".to_string(),
        }];

        Self {
            chain_id: DEFAULT_CHAIN_ID,
            timestamp: 0,
            extra_data: b"QFC Dev Genesis".to_vec(),
            alloc,
            validators,
        }
    }

    /// Build the genesis block
    pub fn build_genesis_block(&self) -> Block {
        let header = BlockHeader {
            version: BLOCK_VERSION,
            number: 0,
            parent_hash: Hash::ZERO,
            state_root: Hash::ZERO, // Will be computed after applying allocations
            transactions_root: Hash::ZERO,
            receipts_root: Hash::ZERO,
            producer: Address::ZERO,
            contribution_score: 0,
            vrf_proof: VrfProof::default(),
            timestamp: self.timestamp,
            gas_limit: DEFAULT_BLOCK_GAS_LIMIT,
            gas_used: 0,
            extra_data: self.extra_data.clone(),
        };

        Block {
            header,
            transactions: Vec::new(),
            votes: Vec::new(),
            signature: Signature::ZERO,
        }
    }

    /// Parse allocations into (Address, U256) pairs
    pub fn parse_allocations(&self) -> Vec<(Address, U256)> {
        self.alloc
            .iter()
            .filter_map(|(addr_str, alloc)| {
                let addr_str = addr_str.strip_prefix("0x").unwrap_or(addr_str);
                let addr_bytes = hex::decode(addr_str).ok()?;
                let address = Address::from_slice(&addr_bytes)?;

                let balance = alloc.balance.parse::<u128>().ok()?;
                Some((address, U256::from_u128(balance)))
            })
            .collect()
    }

    /// Parse validators
    pub fn parse_validators(&self) -> Vec<(Address, U256)> {
        self.validators
            .iter()
            .filter_map(|v| {
                let addr_str = v.address.strip_prefix("0x").unwrap_or(&v.address);
                let addr_bytes = hex::decode(addr_str).ok()?;
                let address = Address::from_slice(&addr_bytes)?;

                let stake = v.stake.parse::<u128>().ok()?;
                Some((address, U256::from_u128(stake)))
            })
            .collect()
    }
}

/// Compute genesis block hash
pub fn genesis_hash(genesis: &Block) -> Hash {
    blake3_hash(&genesis.header_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_genesis_config_default() {
        let config = GenesisConfig::default();
        assert_eq!(config.chain_id, DEFAULT_CHAIN_ID);
    }

    #[test]
    fn test_build_genesis_block() {
        let config = GenesisConfig::testnet();
        let genesis = config.build_genesis_block();

        assert_eq!(genesis.number(), 0);
        assert_eq!(genesis.parent_hash(), Hash::ZERO);
        assert!(genesis.is_genesis());
    }

    #[test]
    fn test_parse_allocations() {
        let config = GenesisConfig::testnet();
        let allocs = config.parse_allocations();

        assert!(!allocs.is_empty());
    }
}
