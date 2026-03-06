//! Genesis block configuration

use qfc_crypto::blake3_hash;
use qfc_types::{
    Address, Block, BlockHeader, Hash, PublicKey, Signature, VrfProof, BLOCK_VERSION,
    DEFAULT_BLOCK_GAS_LIMIT, DEFAULT_CHAIN_ID, U256,
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
    /// Ed25519 public key (hex)
    pub public_key: String,
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

        // Dev validator (public key [0x01;32], for --dev mode local development)
        alloc.insert(
            "0x1bff2fcb945733cc3012879cb9fab07928667062".to_string(),
            GenesisAllocation {
                balance: "1000000000000000000000000000".to_string(), // 1B QFC
            },
        );

        // Testnet validator accounts (Ed25519 derived addresses)
        alloc.insert(
            "0x8d1dd4291ea7fe924cd4b2e577f6c81f3e4025c8".to_string(),
            GenesisAllocation {
                balance: "1000000000000000000000000000".to_string(), // 1B QFC
            },
        );
        alloc.insert(
            "0x4a737feb30701eb86b708896bf6eff0fb7b2e0b2".to_string(),
            GenesisAllocation {
                balance: "1000000000000000000000000000".to_string(), // 1B QFC
            },
        );
        alloc.insert(
            "0xb560ab667adac60e0c9fd5c48b7d309cdf2be685".to_string(),
            GenesisAllocation {
                balance: "1000000000000000000000000000".to_string(), // 1B QFC
            },
        );

        // Faucet account (Ed25519 address from key [0x42;32])
        // This key is used by the built-in qfc_requestFaucet RPC
        alloc.insert(
            "0x10d7812fbe50096ae82569fdad35f79628bc0084".to_string(),
            GenesisAllocation {
                balance: "1000000000000000000000000000".to_string(), // 1B QFC (faucet)
            },
        );

        // Validators: 1 dev (public) + 3 testnet (keys in private repo)
        // Public keys are Ed25519 public keys (safe to include in public code)
        let validators = vec![
            GenesisValidator {
                address: "0x1bff2fcb945733cc3012879cb9fab07928667062".to_string(), // dev [0x01;32]
                public_key: "8a88e3dd7409f195fd52db2d3cba5d72ca6709bf1d94121bf3748801b40f6f5c"
                    .to_string(),
                stake: "1000000".to_string(),
            },
            GenesisValidator {
                address: "0x8d1dd4291ea7fe924cd4b2e577f6c81f3e4025c8".to_string(),
                public_key: "7f225ba786bafc9374245d06ac28ec427543d05dff4b55bcbc08cf02ab94ea1d"
                    .to_string(),
                stake: "1000000".to_string(),
            },
            GenesisValidator {
                address: "0x4a737feb30701eb86b708896bf6eff0fb7b2e0b2".to_string(),
                public_key: "ede3fe8baf9c26d9cff9af12bbc0b2fd74d390f5e5f2911a02870bccde37d506"
                    .to_string(),
                stake: "1000000".to_string(),
            },
            GenesisValidator {
                address: "0xb560ab667adac60e0c9fd5c48b7d309cdf2be685".to_string(),
                public_key: "f4aba22b9a83084f549f63a34ddaf417fb46e759611ddb22e7b3cc6b207aabee"
                    .to_string(),
                stake: "1000000".to_string(),
            },
        ];

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
            proofs_root: Hash::ZERO,
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
            inference_proofs: Vec::new(),
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

    /// Parse validators (returns address, public_key, stake)
    pub fn parse_validators(&self) -> Vec<(Address, PublicKey, U256)> {
        self.validators
            .iter()
            .filter_map(|v| {
                let addr_str = v.address.strip_prefix("0x").unwrap_or(&v.address);
                let addr_bytes = hex::decode(addr_str).ok()?;
                let address = Address::from_slice(&addr_bytes)?;

                let pk_bytes = hex::decode(&v.public_key).ok()?;
                let public_key = if pk_bytes.len() == 32 {
                    let mut arr = [0u8; 32];
                    arr.copy_from_slice(&pk_bytes);
                    PublicKey::new(arr)
                } else {
                    PublicKey::ZERO
                };

                let stake = v.stake.parse::<u128>().ok()?;
                Some((address, public_key, U256::from_u128(stake)))
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
