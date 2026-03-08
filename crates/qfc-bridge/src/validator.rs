//! Bridge validator set and multi-sig management

use crate::types::{BridgeDeposit, BridgeError, BridgeWithdrawal, DepositStatus, WithdrawalStatus};
use qfc_types::Address;
use std::collections::HashSet;
use tracing::debug;

/// A validator's signature on a bridge operation
#[derive(Clone, Debug)]
pub struct ValidatorSignature {
    pub validator: Address,
    pub signature: Vec<u8>,
}

/// Manages the set of bridge validators and threshold signing
pub struct BridgeValidatorSet {
    /// Set of authorized bridge validator addresses
    validators: HashSet<Address>,
    /// Number of signatures required (threshold)
    threshold: usize,
}

impl BridgeValidatorSet {
    pub fn new(validators: Vec<Address>, threshold: usize) -> Self {
        // When no validators are configured (bridge disabled), allow threshold=0
        let effective_threshold = if validators.is_empty() {
            0
        } else {
            assert!(
                threshold <= validators.len(),
                "Threshold ({}) cannot exceed validator count ({})",
                threshold,
                validators.len()
            );
            assert!(threshold > 0, "Threshold must be at least 1");
            threshold
        };

        Self {
            validators: validators.into_iter().collect(),
            threshold: effective_threshold,
        }
    }

    /// Check if an address is a bridge validator
    pub fn is_validator(&self, address: &Address) -> bool {
        self.validators.contains(address)
    }

    /// Get the signature threshold
    pub fn threshold(&self) -> usize {
        self.threshold
    }

    /// Get the number of validators
    pub fn validator_count(&self) -> usize {
        self.validators.len()
    }

    /// Add a validator signature to a deposit.
    /// Returns true if threshold is now met.
    pub fn sign_deposit(
        &self,
        deposit: &mut BridgeDeposit,
        validator: Address,
        signature: Vec<u8>,
    ) -> Result<bool, BridgeError> {
        if !self.is_validator(&validator) {
            return Err(BridgeError::InvalidValidator(validator));
        }

        if deposit.status == DepositStatus::Completed {
            return Err(BridgeError::AlreadyCompleted);
        }

        if deposit.signatures.iter().any(|(v, _)| *v == validator) {
            return Err(BridgeError::AlreadySigned(validator));
        }

        deposit.signatures.push((validator, signature));

        debug!(
            "Deposit {} signed by {} ({}/{})",
            deposit.deposit_id,
            validator,
            deposit.signature_count(),
            self.threshold
        );

        if deposit.signature_count() >= self.threshold {
            deposit.status = DepositStatus::Minting;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Add a validator signature to a withdrawal.
    /// Returns true if threshold is now met.
    pub fn sign_withdrawal(
        &self,
        withdrawal: &mut BridgeWithdrawal,
        validator: Address,
        signature: Vec<u8>,
    ) -> Result<bool, BridgeError> {
        if !self.is_validator(&validator) {
            return Err(BridgeError::InvalidValidator(validator));
        }

        if withdrawal.status == WithdrawalStatus::Completed {
            return Err(BridgeError::AlreadyCompleted);
        }

        if withdrawal.signatures.iter().any(|(v, _)| *v == validator) {
            return Err(BridgeError::AlreadySigned(validator));
        }

        withdrawal.signatures.push((validator, signature));

        debug!(
            "Withdrawal {} signed by {} ({}/{})",
            withdrawal.withdrawal_id,
            validator,
            withdrawal.signature_count(),
            self.threshold
        );

        if withdrawal.signature_count() >= self.threshold {
            withdrawal.status = WithdrawalStatus::Submitted;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Get all validator addresses
    pub fn validators(&self) -> Vec<Address> {
        self.validators.iter().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::DEFAULT_ETH_CONFIRMATIONS;
    use qfc_types::Hash;

    fn test_address(byte: u8) -> Address {
        Address::from_slice(&[byte; 20]).unwrap()
    }

    fn test_hash(byte: u8) -> Hash {
        Hash::from_slice(&[byte; 32]).unwrap()
    }

    fn make_deposit() -> BridgeDeposit {
        BridgeDeposit {
            deposit_id: test_hash(1),
            eth_tx_hash: test_hash(2),
            eth_block_number: 1000,
            eth_sender: test_address(0xAA),
            qfc_recipient: test_address(0xBB),
            token_address: Address::ZERO,
            amount: 1_000_000_000_000_000_000, // 1 ETH
            required_confirmations: DEFAULT_ETH_CONFIRMATIONS,
            status: DepositStatus::Confirmed,
            signatures: vec![],
            observed_at: 12345,
        }
    }

    fn make_validators() -> BridgeValidatorSet {
        let validators = (1..=7).map(test_address).collect();
        BridgeValidatorSet::new(validators, 5)
    }

    #[test]
    fn test_empty_validators() {
        // Bridge with no validators should not panic
        let vs = BridgeValidatorSet::new(vec![], 0);
        assert_eq!(vs.validator_count(), 0);
        assert_eq!(vs.threshold(), 0);
    }

    #[test]
    fn test_default_config_no_panic() {
        // RelayerConfig::default() should not panic when creating BridgeRelayer
        let config = crate::relayer::RelayerConfig::default();
        let _ = crate::relayer::BridgeRelayer::new(config);
    }

    #[test]
    fn test_sign_deposit_threshold() {
        let vs = make_validators();
        let mut deposit = make_deposit();

        // Sign with 4 validators — not enough
        for i in 1..=4u8 {
            let ready = vs
                .sign_deposit(&mut deposit, test_address(i), vec![i])
                .unwrap();
            assert!(!ready);
        }
        assert_eq!(deposit.status, DepositStatus::Confirmed);

        // 5th signature meets threshold
        let ready = vs
            .sign_deposit(&mut deposit, test_address(5), vec![5])
            .unwrap();
        assert!(ready);
        assert_eq!(deposit.status, DepositStatus::Minting);
    }

    #[test]
    fn test_duplicate_signature() {
        let vs = make_validators();
        let mut deposit = make_deposit();

        vs.sign_deposit(&mut deposit, test_address(1), vec![1])
            .unwrap();
        let err = vs
            .sign_deposit(&mut deposit, test_address(1), vec![1])
            .unwrap_err();
        assert!(matches!(err, BridgeError::AlreadySigned(_)));
    }

    #[test]
    fn test_invalid_validator() {
        let vs = make_validators();
        let mut deposit = make_deposit();

        let err = vs
            .sign_deposit(&mut deposit, test_address(99), vec![99])
            .unwrap_err();
        assert!(matches!(err, BridgeError::InvalidValidator(_)));
    }

    #[test]
    fn test_sign_withdrawal() {
        let vs = make_validators();
        let mut withdrawal = BridgeWithdrawal {
            withdrawal_id: test_hash(10),
            qfc_tx_hash: test_hash(11),
            qfc_block_number: 500,
            qfc_sender: test_address(0xCC),
            eth_recipient: test_address(0xDD),
            token_address: Address::ZERO,
            amount: 500_000_000_000_000_000,
            status: WithdrawalStatus::Signing,
            signatures: vec![],
            observed_at: 12345,
            eth_unlock_tx: None,
        };

        for i in 1..=5u8 {
            vs.sign_withdrawal(&mut withdrawal, test_address(i), vec![i])
                .unwrap();
        }
        assert_eq!(withdrawal.status, WithdrawalStatus::Submitted);
    }
}
