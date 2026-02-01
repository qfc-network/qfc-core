//! Transaction types

use crate::{Address, Hash, PublicKey, Signature, U256};
use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

/// Transaction type
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, std::hash::Hash, Serialize, Deserialize, BorshSerialize, BorshDeserialize,
)]
#[borsh(use_discriminant = true)]
#[repr(u8)]
pub enum TransactionType {
    /// Normal transfer
    Transfer = 0,
    /// Contract creation
    ContractCreate = 1,
    /// Contract call
    ContractCall = 2,
    /// Stake tokens
    Stake = 3,
    /// Unstake tokens
    Unstake = 4,
    /// Register as validator
    ValidatorRegister = 5,
    /// Exit as validator
    ValidatorExit = 6,
}

impl Default for TransactionType {
    fn default() -> Self {
        Self::Transfer
    }
}

impl From<u8> for TransactionType {
    fn from(v: u8) -> Self {
        match v {
            0 => Self::Transfer,
            1 => Self::ContractCreate,
            2 => Self::ContractCall,
            3 => Self::Stake,
            4 => Self::Unstake,
            5 => Self::ValidatorRegister,
            6 => Self::ValidatorExit,
            _ => Self::Transfer,
        }
    }
}

/// Transaction
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct Transaction {
    /// Transaction type
    pub tx_type: TransactionType,

    /// Chain ID (prevents replay attacks)
    pub chain_id: u64,

    /// Sender's nonce
    pub nonce: u64,

    /// Recipient address (None for contract creation)
    pub to: Option<Address>,

    /// Transfer value in wei
    pub value: U256,

    /// Call data
    pub data: Vec<u8>,

    /// Gas limit
    pub gas_limit: u64,

    /// Gas price in wei
    pub gas_price: U256,

    /// Sender's public key (required for Ed25519 signature verification)
    pub public_key: PublicKey,

    /// Signature
    pub signature: Signature,
}

impl Default for Transaction {
    fn default() -> Self {
        Self {
            tx_type: TransactionType::Transfer,
            chain_id: crate::DEFAULT_CHAIN_ID,
            nonce: 0,
            to: None,
            value: U256::ZERO,
            data: Vec::new(),
            gas_limit: crate::MINIMUM_GAS,
            gas_price: U256::from_u64(crate::MIN_GAS_PRICE),
            public_key: PublicKey::ZERO,
            signature: Signature::ZERO,
        }
    }
}

impl Transaction {
    /// Create a new transfer transaction
    pub fn transfer(to: Address, value: U256, nonce: u64, gas_price: U256) -> Self {
        Self {
            tx_type: TransactionType::Transfer,
            chain_id: crate::DEFAULT_CHAIN_ID,
            nonce,
            to: Some(to),
            value,
            data: Vec::new(),
            gas_limit: crate::TRANSFER_GAS,
            gas_price,
            public_key: PublicKey::ZERO,
            signature: Signature::ZERO,
        }
    }

    /// Create a new contract creation transaction
    pub fn contract_create(code: Vec<u8>, value: U256, nonce: u64, gas_limit: u64, gas_price: U256) -> Self {
        Self {
            tx_type: TransactionType::ContractCreate,
            chain_id: crate::DEFAULT_CHAIN_ID,
            nonce,
            to: None,
            value,
            data: code,
            gas_limit,
            gas_price,
            public_key: PublicKey::ZERO,
            signature: Signature::ZERO,
        }
    }

    /// Create a new contract call transaction
    pub fn contract_call(
        to: Address,
        data: Vec<u8>,
        value: U256,
        nonce: u64,
        gas_limit: u64,
        gas_price: U256,
    ) -> Self {
        Self {
            tx_type: TransactionType::ContractCall,
            chain_id: crate::DEFAULT_CHAIN_ID,
            nonce,
            to: Some(to),
            value,
            data,
            gas_limit,
            gas_price,
            public_key: PublicKey::ZERO,
            signature: Signature::ZERO,
        }
    }

    /// Create a stake transaction
    pub fn stake(amount: U256, nonce: u64, gas_price: U256) -> Self {
        Self {
            tx_type: TransactionType::Stake,
            chain_id: crate::DEFAULT_CHAIN_ID,
            nonce,
            to: None,
            value: amount,
            data: Vec::new(),
            gas_limit: crate::MINIMUM_GAS * 2,
            gas_price,
            public_key: PublicKey::ZERO,
            signature: Signature::ZERO,
        }
    }

    /// Create an unstake transaction
    pub fn unstake(amount: U256, nonce: u64, gas_price: U256) -> Self {
        let mut data = Vec::with_capacity(32);
        data.extend_from_slice(&amount.to_be_bytes());

        Self {
            tx_type: TransactionType::Unstake,
            chain_id: crate::DEFAULT_CHAIN_ID,
            nonce,
            to: None,
            value: U256::ZERO,
            data,
            gas_limit: crate::MINIMUM_GAS * 2,
            gas_price,
            public_key: PublicKey::ZERO,
            signature: Signature::ZERO,
        }
    }

    /// Set the public key and signature
    pub fn sign(&mut self, public_key: PublicKey, signature: Signature) {
        self.public_key = public_key;
        self.signature = signature;
    }

    /// Serialize transaction without signature for hashing
    pub fn to_bytes_without_signature(&self) -> Vec<u8> {
        // We create a copy without signature for hashing
        // Note: public_key IS included because it's part of the signed message
        let unsigned = UnsignedTransaction {
            tx_type: self.tx_type,
            chain_id: self.chain_id,
            nonce: self.nonce,
            to: self.to,
            value: self.value,
            data: self.data.clone(),
            gas_limit: self.gas_limit,
            gas_price: self.gas_price,
            public_key: self.public_key,
        };
        borsh::to_vec(&unsigned).expect("serialization should not fail")
    }

    /// Serialize complete transaction
    pub fn to_bytes(&self) -> Vec<u8> {
        borsh::to_vec(self).expect("serialization should not fail")
    }

    /// Deserialize transaction from bytes
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, borsh::io::Error> {
        borsh::from_slice(bytes)
    }

    /// Set the signature
    pub fn set_signature(&mut self, signature: Signature) {
        self.signature = signature;
    }

    /// Calculate gas cost (gas_limit * gas_price)
    pub fn gas_cost(&self) -> U256 {
        self.gas_price * U256::from_u64(self.gas_limit)
    }

    /// Calculate total cost (value + gas_cost)
    pub fn total_cost(&self) -> U256 {
        self.value + self.gas_cost()
    }

    /// Check if this is a contract creation
    pub fn is_contract_create(&self) -> bool {
        self.tx_type == TransactionType::ContractCreate
    }

    /// Get data gas cost
    pub fn data_gas(&self) -> u64 {
        let mut gas = 0u64;
        for &byte in &self.data {
            if byte == 0 {
                gas += crate::GAS_PER_ZERO_BYTE;
            } else {
                gas += crate::GAS_PER_BYTE;
            }
        }
        gas
    }

    /// Get intrinsic gas (base gas + data gas)
    pub fn intrinsic_gas(&self) -> u64 {
        let base = if self.is_contract_create() {
            crate::CONTRACT_CREATE_GAS
        } else {
            crate::TRANSFER_GAS
        };
        base + self.data_gas()
    }
}

/// Unsigned transaction for hashing
#[derive(BorshSerialize, BorshDeserialize)]
struct UnsignedTransaction {
    pub tx_type: TransactionType,
    pub chain_id: u64,
    pub nonce: u64,
    pub to: Option<Address>,
    pub value: U256,
    pub data: Vec<u8>,
    pub gas_limit: u64,
    pub gas_price: U256,
    pub public_key: PublicKey,
}

/// Signed transaction with recovered sender
#[derive(Clone, Debug)]
pub struct SignedTransaction {
    /// The transaction
    pub tx: Transaction,
    /// Transaction hash
    pub hash: Hash,
    /// Recovered sender address
    pub sender: Address,
}

impl SignedTransaction {
    pub fn new(tx: Transaction, hash: Hash, sender: Address) -> Self {
        Self { tx, hash, sender }
    }

    pub fn nonce(&self) -> u64 {
        self.tx.nonce
    }

    pub fn gas_price(&self) -> U256 {
        self.tx.gas_price
    }

    pub fn value(&self) -> U256 {
        self.tx.value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_transaction_default() {
        let tx = Transaction::default();
        assert_eq!(tx.tx_type, TransactionType::Transfer);
        assert_eq!(tx.chain_id, crate::DEFAULT_CHAIN_ID);
    }

    #[test]
    fn test_transaction_serialization() {
        let tx = Transaction::transfer(
            Address::new([0x11; 20]),
            U256::from_u64(1000),
            1,
            U256::from_u64(crate::ONE_GWEI),
        );

        let bytes = tx.to_bytes();
        let decoded = Transaction::from_bytes(&bytes).unwrap();
        assert_eq!(tx, decoded);
    }

    #[test]
    fn test_transaction_costs() {
        let tx = Transaction::transfer(
            Address::new([0x11; 20]),
            U256::from_u64(1000),
            1,
            U256::from_u64(crate::ONE_GWEI),
        );

        let gas_cost = tx.gas_cost();
        let expected_gas_cost = U256::from_u64(crate::TRANSFER_GAS * crate::ONE_GWEI);
        assert_eq!(gas_cost, expected_gas_cost);

        let total_cost = tx.total_cost();
        assert_eq!(total_cost, U256::from_u64(1000) + expected_gas_cost);
    }

    #[test]
    fn test_data_gas() {
        let mut tx = Transaction::default();
        tx.data = vec![0, 0, 1, 2, 0, 3];

        // 3 zero bytes + 3 non-zero bytes
        let expected = 3 * crate::GAS_PER_ZERO_BYTE + 3 * crate::GAS_PER_BYTE;
        assert_eq!(tx.data_gas(), expected);
    }
}
