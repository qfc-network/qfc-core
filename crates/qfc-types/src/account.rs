//! Account types

use crate::{Hash, U256};
use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

/// Account type
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, std::hash::Hash, Serialize, Deserialize, BorshSerialize, BorshDeserialize,
)]
#[borsh(use_discriminant = true)]
#[repr(u8)]
pub enum AccountType {
    /// External Owned Account (user)
    EOA = 0,
    /// Contract account
    Contract = 1,
    /// Validator account
    Validator = 2,
}

impl Default for AccountType {
    fn default() -> Self {
        Self::EOA
    }
}

impl From<u8> for AccountType {
    fn from(v: u8) -> Self {
        match v {
            0 => Self::EOA,
            1 => Self::Contract,
            2 => Self::Validator,
            _ => Self::EOA,
        }
    }
}

/// Account state
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct Account {
    /// Account type
    pub account_type: AccountType,

    /// Balance in wei
    pub balance: U256,

    /// Nonce (transaction count)
    pub nonce: u64,

    /// Code hash (for contract accounts)
    pub code_hash: Option<Hash>,

    /// Storage root (for contract accounts)
    pub storage_root: Option<Hash>,

    /// Staked amount (for validators)
    pub stake: Option<U256>,

    /// Contribution score (for validators)
    pub contribution_score: Option<u64>,
}

impl Default for Account {
    fn default() -> Self {
        Self::new_eoa()
    }
}

impl Account {
    /// Create a new EOA (External Owned Account)
    pub fn new_eoa() -> Self {
        Self {
            account_type: AccountType::EOA,
            balance: U256::ZERO,
            nonce: 0,
            code_hash: None,
            storage_root: None,
            stake: None,
            contribution_score: None,
        }
    }

    /// Create a new EOA with initial balance
    pub fn new_eoa_with_balance(balance: U256) -> Self {
        Self {
            account_type: AccountType::EOA,
            balance,
            nonce: 0,
            code_hash: None,
            storage_root: None,
            stake: None,
            contribution_score: None,
        }
    }

    /// Create a new contract account
    pub fn new_contract(code_hash: Hash) -> Self {
        Self {
            account_type: AccountType::Contract,
            balance: U256::ZERO,
            nonce: 1, // Contract nonce starts at 1
            code_hash: Some(code_hash),
            storage_root: None,
            stake: None,
            contribution_score: None,
        }
    }

    /// Create a new validator account
    pub fn new_validator(stake: U256) -> Self {
        Self {
            account_type: AccountType::Validator,
            balance: U256::ZERO,
            nonce: 0,
            code_hash: None,
            storage_root: None,
            stake: Some(stake),
            contribution_score: Some(0),
        }
    }

    /// Check if this is an EOA
    pub fn is_eoa(&self) -> bool {
        self.account_type == AccountType::EOA
    }

    /// Check if this is a contract
    pub fn is_contract(&self) -> bool {
        self.account_type == AccountType::Contract
    }

    /// Check if this is a validator
    pub fn is_validator(&self) -> bool {
        self.account_type == AccountType::Validator
    }

    /// Check if account has code
    pub fn has_code(&self) -> bool {
        self.code_hash.is_some()
    }

    /// Check if account is empty (zero balance, zero nonce, no code)
    pub fn is_empty(&self) -> bool {
        self.balance.is_zero() && self.nonce == 0 && self.code_hash.is_none()
    }

    /// Get stake amount
    pub fn get_stake(&self) -> U256 {
        self.stake.unwrap_or(U256::ZERO)
    }

    /// Get contribution score
    pub fn get_contribution_score(&self) -> u64 {
        self.contribution_score.unwrap_or(0)
    }

    /// Add balance
    pub fn add_balance(&mut self, amount: U256) {
        self.balance = self.balance.saturating_add(amount);
    }

    /// Subtract balance (returns false if insufficient)
    pub fn sub_balance(&mut self, amount: U256) -> bool {
        if self.balance >= amount {
            self.balance = self.balance - amount;
            true
        } else {
            false
        }
    }

    /// Increment nonce
    pub fn increment_nonce(&mut self) {
        self.nonce = self.nonce.saturating_add(1);
    }

    /// Set storage root
    pub fn set_storage_root(&mut self, root: Hash) {
        self.storage_root = Some(root);
    }

    /// Set stake
    pub fn set_stake(&mut self, stake: U256) {
        self.stake = Some(stake);
        if self.account_type == AccountType::EOA {
            self.account_type = AccountType::Validator;
        }
    }

    /// Set contribution score
    pub fn set_contribution_score(&mut self, score: u64) {
        self.contribution_score = Some(score);
    }

    /// Serialize account
    pub fn to_bytes(&self) -> Vec<u8> {
        borsh::to_vec(self).expect("serialization should not fail")
    }

    /// Deserialize account
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, borsh::io::Error> {
        borsh::from_slice(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_account_eoa() {
        let account = Account::new_eoa();
        assert!(account.is_eoa());
        assert!(!account.is_contract());
        assert!(!account.is_validator());
        assert!(account.is_empty());
    }

    #[test]
    fn test_account_balance() {
        let mut account = Account::new_eoa_with_balance(U256::from_u64(1000));
        assert_eq!(account.balance, U256::from_u64(1000));

        account.add_balance(U256::from_u64(500));
        assert_eq!(account.balance, U256::from_u64(1500));

        assert!(account.sub_balance(U256::from_u64(200)));
        assert_eq!(account.balance, U256::from_u64(1300));

        assert!(!account.sub_balance(U256::from_u64(2000)));
        assert_eq!(account.balance, U256::from_u64(1300));
    }

    #[test]
    fn test_account_serialization() {
        let account = Account::new_eoa_with_balance(U256::from_u64(12345));
        let bytes = account.to_bytes();
        let decoded = Account::from_bytes(&bytes).unwrap();
        assert_eq!(account, decoded);
    }

    #[test]
    fn test_account_validator() {
        let stake = U256::from_u64(10000);
        let account = Account::new_validator(stake);
        assert!(account.is_validator());
        assert_eq!(account.get_stake(), stake);
        assert_eq!(account.get_contribution_score(), 0);
    }
}
