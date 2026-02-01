//! Address derivation functions

use crate::signature::Keypair;
use qfc_types::{Address, Hash, PublicKey};

/// Derive address from public key
///
/// The address is the last 20 bytes of the Blake3 hash of the public key
pub fn address_from_public_key(public_key: &PublicKey) -> Address {
    let hash = blake3::hash(public_key.as_bytes());
    let bytes = hash.as_bytes();

    let mut address_bytes = [0u8; 20];
    address_bytes.copy_from_slice(&bytes[12..32]);
    Address::new(address_bytes)
}

/// Derive address from keypair
pub fn address_from_keypair(keypair: &Keypair) -> Address {
    address_from_public_key(&keypair.public_key())
}

/// Compute contract address from deployer and nonce
///
/// The contract address is derived from the deployer address and their nonce
/// at the time of deployment
pub fn contract_address(deployer: &Address, nonce: u64) -> Address {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&[0xff]); // Contract creation prefix
    hasher.update(deployer.as_bytes());
    hasher.update(&nonce.to_le_bytes());

    let hash = hasher.finalize();
    let bytes = hash.as_bytes();

    let mut address_bytes = [0u8; 20];
    address_bytes.copy_from_slice(&bytes[12..32]);
    Address::new(address_bytes)
}

/// Compute CREATE2 address
///
/// The address is derived from deployer, salt, and init code hash
/// Similar to Ethereum's CREATE2
pub fn create2_address(deployer: &Address, salt: &[u8; 32], init_code_hash: &Hash) -> Address {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&[0xff]); // CREATE2 prefix
    hasher.update(deployer.as_bytes());
    hasher.update(salt);
    hasher.update(init_code_hash.as_bytes());

    let hash = hasher.finalize();
    let bytes = hash.as_bytes();

    let mut address_bytes = [0u8; 20];
    address_bytes.copy_from_slice(&bytes[12..32]);
    Address::new(address_bytes)
}

/// Check if an address is valid (non-zero, proper format)
pub fn is_valid_address(address: &Address) -> bool {
    // For now, just check that it's not the zero address
    *address != Address::ZERO
}

/// Check if an address looks like a contract address (heuristic)
///
/// This is just a heuristic - we can't determine if an address is a contract
/// without checking the blockchain state
pub fn looks_like_contract(address: &Address) -> bool {
    // All addresses look the same in QFC, so this is just a placeholder
    // In practice, you'd check the state to see if there's code at this address
    !address.0.iter().all(|&b| b == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_address_from_public_key() {
        let kp = Keypair::generate();
        let addr = address_from_public_key(&kp.public_key());

        // Address should be non-zero
        assert_ne!(addr, Address::ZERO);

        // Same public key should give same address
        let addr2 = address_from_public_key(&kp.public_key());
        assert_eq!(addr, addr2);
    }

    #[test]
    fn test_address_from_keypair() {
        let kp = Keypair::generate();
        let addr1 = address_from_keypair(&kp);
        let addr2 = address_from_public_key(&kp.public_key());
        assert_eq!(addr1, addr2);
    }

    #[test]
    fn test_different_keypairs_different_addresses() {
        let kp1 = Keypair::generate();
        let kp2 = Keypair::generate();

        let addr1 = address_from_keypair(&kp1);
        let addr2 = address_from_keypair(&kp2);

        assert_ne!(addr1, addr2);
    }

    #[test]
    fn test_contract_address() {
        let deployer = Address::new([0x11; 20]);

        let addr1 = contract_address(&deployer, 0);
        let addr2 = contract_address(&deployer, 1);
        let addr3 = contract_address(&deployer, 0);

        // Different nonces should give different addresses
        assert_ne!(addr1, addr2);

        // Same nonce should give same address
        assert_eq!(addr1, addr3);
    }

    #[test]
    fn test_create2_address() {
        let deployer = Address::new([0x11; 20]);
        let salt = [0xab; 32];
        let init_code_hash = Hash::new([0xcd; 32]);

        let addr = create2_address(&deployer, &salt, &init_code_hash);
        assert_ne!(addr, Address::ZERO);

        // Deterministic
        let addr2 = create2_address(&deployer, &salt, &init_code_hash);
        assert_eq!(addr, addr2);

        // Different salt gives different address
        let addr3 = create2_address(&deployer, &[0xef; 32], &init_code_hash);
        assert_ne!(addr, addr3);
    }

    #[test]
    fn test_is_valid_address() {
        assert!(!is_valid_address(&Address::ZERO));
        assert!(is_valid_address(&Address::new([0x11; 20])));
    }
}
