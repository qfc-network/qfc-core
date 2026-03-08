//! State rent and storage deposit mechanism
//!
//! Implements per-account storage deposits (refundable on deletion) and
//! periodic rent collection at epoch boundaries. Accounts inactive for
//! DORMANT_THRESHOLD_EPOCHS become dormant and can be reactivated with a fee.

use qfc_types::{
    Account, Address, DORMANT_THRESHOLD_EPOCHS, MIN_STORAGE_DEPOSIT, REACTIVATION_FEE,
    STORAGE_DEPOSIT_PER_ACCOUNT, STORAGE_DEPOSIT_PER_BYTE, STORAGE_RENT_PER_SLOT_PER_EPOCH, U256,
};
use std::collections::HashMap;
use thiserror::Error;
use tracing::{debug, info, warn};

/// State rent errors
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum RentError {
    #[error("Insufficient balance for storage deposit: need {need}, have {have}")]
    InsufficientDeposit { need: u128, have: u128 },

    #[error("Account is dormant: {0}")]
    AccountDormant(Address),

    #[error("Account not dormant")]
    NotDormant,

    #[error("Insufficient balance for reactivation fee")]
    InsufficientReactivationFee,

    #[error("Account not found: {0}")]
    AccountNotFound(Address),
}

/// Storage rent collector — runs at epoch boundaries
pub struct RentCollector {
    /// Current epoch number
    current_epoch: u64,
    /// Total rent collected this epoch (burned or sent to treasury)
    total_rent_collected: u128,
    /// Accounts marked dormant this epoch
    dormant_count: u64,
}

impl RentCollector {
    pub fn new(current_epoch: u64) -> Self {
        Self {
            current_epoch,
            total_rent_collected: 0,
            dormant_count: 0,
        }
    }

    /// Calculate the storage deposit required for deploying a contract
    pub fn contract_deposit(code_size: usize) -> u128 {
        STORAGE_DEPOSIT_PER_ACCOUNT + STORAGE_DEPOSIT_PER_BYTE * code_size as u128
    }

    /// Calculate the storage deposit required for a new EOA
    pub fn account_deposit() -> u128 {
        STORAGE_DEPOSIT_PER_ACCOUNT
    }

    /// Calculate rent owed for an account based on its storage usage
    pub fn calculate_rent(account: &Account, epochs_elapsed: u64) -> u128 {
        if account.storage_slot_count == 0 {
            return 0;
        }
        STORAGE_RENT_PER_SLOT_PER_EPOCH
            * account.storage_slot_count as u128
            * epochs_elapsed as u128
    }

    /// Collect rent from a single account. Returns the amount collected.
    /// If the storage deposit is depleted below MIN_STORAGE_DEPOSIT, the
    /// account becomes dormant.
    pub fn collect_rent(&mut self, address: &Address, account: &mut Account) -> u128 {
        if account.is_dormant || account.storage_slot_count == 0 {
            return 0;
        }

        let epochs_since_active = self.current_epoch.saturating_sub(account.last_active_epoch);

        if epochs_since_active == 0 {
            return 0;
        }

        let rent_owed = Self::calculate_rent(account, epochs_since_active);
        if rent_owed == 0 {
            return 0;
        }

        // Deduct from storage deposit
        let collected = rent_owed.min(account.storage_deposit);
        account.storage_deposit = account.storage_deposit.saturating_sub(collected);
        account.last_active_epoch = self.current_epoch;

        // Check if account should become dormant
        if account.storage_deposit < MIN_STORAGE_DEPOSIT {
            account.is_dormant = true;
            self.dormant_count += 1;
            warn!(
                "Account {} marked dormant (deposit depleted: {} wei)",
                address, account.storage_deposit
            );
        }

        self.total_rent_collected += collected;
        debug!(
            "Rent collected from {}: {} wei ({} slots, {} epochs)",
            address, collected, account.storage_slot_count, epochs_since_active
        );

        collected
    }

    /// Process rent collection for a batch of accounts.
    /// Returns a map of address → rent collected.
    pub fn collect_rent_batch(
        &mut self,
        accounts: &mut [(Address, &mut Account)],
    ) -> HashMap<Address, u128> {
        let mut results = HashMap::new();

        for (address, account) in accounts.iter_mut() {
            let rent = self.collect_rent(address, account);
            if rent > 0 {
                results.insert(*address, rent);
            }
        }

        if !results.is_empty() {
            info!(
                "Epoch {} rent collection: {} accounts, {} total wei, {} newly dormant",
                self.current_epoch,
                results.len(),
                self.total_rent_collected,
                self.dormant_count
            );
        }

        results
    }

    /// Check if an account is dormant based on inactivity
    pub fn check_dormancy(account: &Account, current_epoch: u64) -> bool {
        if account.is_dormant {
            return true;
        }
        let inactive_epochs = current_epoch.saturating_sub(account.last_active_epoch);
        inactive_epochs >= DORMANT_THRESHOLD_EPOCHS
    }

    /// Reactivate a dormant account. Caller must ensure the reactivation fee
    /// is deducted from the sender's balance.
    pub fn reactivate_account(account: &mut Account, current_epoch: u64) -> Result<(), RentError> {
        if !account.is_dormant {
            return Err(RentError::NotDormant);
        }

        account.is_dormant = false;
        account.last_active_epoch = current_epoch;

        info!("Account reactivated at epoch {}", current_epoch);
        Ok(())
    }

    /// Calculate refund amount when an account or contract is deleted
    pub fn deletion_refund(account: &Account) -> u128 {
        account.storage_deposit
    }

    /// Touch an account (mark as active for the current epoch)
    pub fn touch_account(account: &mut Account, current_epoch: u64) {
        account.last_active_epoch = current_epoch;
    }

    /// Get total rent collected this epoch
    pub fn total_rent_collected(&self) -> u128 {
        self.total_rent_collected
    }

    /// Get count of newly dormant accounts this epoch
    pub fn dormant_count(&self) -> u64 {
        self.dormant_count
    }

    /// Get the reactivation fee constant
    pub fn reactivation_fee() -> u128 {
        REACTIVATION_FEE
    }
}

/// Summary of a self-destruct refund
#[derive(Clone, Debug)]
pub struct SelfDestructRefund {
    /// Address being destroyed
    pub contract: Address,
    /// Beneficiary receiving the refund
    pub beneficiary: Address,
    /// Storage deposit refunded
    pub deposit_refund: u128,
    /// Remaining balance refunded
    pub balance_refund: U256,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_address(byte: u8) -> Address {
        Address::from_slice(&[byte; 20]).unwrap()
    }

    fn make_contract_account(slots: u64, deposit: u128, last_epoch: u64) -> Account {
        let mut acct = Account::new_contract(qfc_types::Hash::ZERO);
        acct.storage_slot_count = slots;
        acct.storage_deposit = deposit;
        acct.last_active_epoch = last_epoch;
        acct
    }

    #[test]
    fn test_account_deposit_calculation() {
        assert_eq!(
            RentCollector::account_deposit(),
            STORAGE_DEPOSIT_PER_ACCOUNT
        );

        let code_deposit = RentCollector::contract_deposit(1000);
        assert_eq!(
            code_deposit,
            STORAGE_DEPOSIT_PER_ACCOUNT + STORAGE_DEPOSIT_PER_BYTE * 1000
        );
    }

    #[test]
    fn test_rent_calculation() {
        let acct = make_contract_account(10, 1_000_000, 0);
        let rent = RentCollector::calculate_rent(&acct, 100);
        assert_eq!(rent, STORAGE_RENT_PER_SLOT_PER_EPOCH * 10 * 100);
    }

    #[test]
    fn test_rent_zero_slots() {
        let acct = make_contract_account(0, 1_000_000, 0);
        let rent = RentCollector::calculate_rent(&acct, 100);
        assert_eq!(rent, 0);
    }

    #[test]
    fn test_collect_rent() {
        let mut collector = RentCollector::new(100);
        let addr = test_address(1);
        let deposit = STORAGE_RENT_PER_SLOT_PER_EPOCH * 5 * 100 + MIN_STORAGE_DEPOSIT;
        let mut acct = make_contract_account(5, deposit, 0);

        let collected = collector.collect_rent(&addr, &mut acct);
        assert_eq!(collected, STORAGE_RENT_PER_SLOT_PER_EPOCH * 5 * 100);
        assert_eq!(acct.storage_deposit, MIN_STORAGE_DEPOSIT);
        assert!(!acct.is_dormant);
        assert_eq!(acct.last_active_epoch, 100);
    }

    #[test]
    fn test_collect_rent_triggers_dormancy() {
        let mut collector = RentCollector::new(100);
        let addr = test_address(2);
        // Small deposit that will be depleted
        let mut acct = make_contract_account(10, 100, 0);

        let collected = collector.collect_rent(&addr, &mut acct);
        assert_eq!(collected, 100); // All deposit consumed
        assert!(acct.is_dormant);
        assert_eq!(collector.dormant_count(), 1);
    }

    #[test]
    fn test_dormant_account_no_rent() {
        let mut collector = RentCollector::new(200);
        let addr = test_address(3);
        let mut acct = make_contract_account(5, 1000, 0);
        acct.is_dormant = true;

        let collected = collector.collect_rent(&addr, &mut acct);
        assert_eq!(collected, 0);
    }

    #[test]
    fn test_reactivate_account() {
        let mut acct = make_contract_account(5, 0, 0);
        acct.is_dormant = true;

        RentCollector::reactivate_account(&mut acct, 500).unwrap();
        assert!(!acct.is_dormant);
        assert_eq!(acct.last_active_epoch, 500);
    }

    #[test]
    fn test_reactivate_non_dormant_fails() {
        let mut acct = make_contract_account(5, 1000, 0);
        let err = RentCollector::reactivate_account(&mut acct, 500).unwrap_err();
        assert!(matches!(err, RentError::NotDormant));
    }

    #[test]
    fn test_check_dormancy() {
        let acct = make_contract_account(5, 1000, 0);
        assert!(!RentCollector::check_dormancy(&acct, 100));
        assert!(RentCollector::check_dormancy(
            &acct,
            DORMANT_THRESHOLD_EPOCHS
        ));
    }

    #[test]
    fn test_deletion_refund() {
        let acct = make_contract_account(5, 500_000, 0);
        assert_eq!(RentCollector::deletion_refund(&acct), 500_000);
    }

    #[test]
    fn test_touch_account() {
        let mut acct = make_contract_account(5, 1000, 0);
        RentCollector::touch_account(&mut acct, 42);
        assert_eq!(acct.last_active_epoch, 42);
    }

    #[test]
    fn test_batch_collection() {
        let mut collector = RentCollector::new(50);

        let addr1 = test_address(1);
        let addr2 = test_address(2);
        let addr3 = test_address(3);

        let mut acct1 = make_contract_account(10, 10_000_000_000_000, 0);
        let mut acct2 = make_contract_account(0, 0, 0); // no slots, no rent
        let mut acct3 = make_contract_account(5, 10_000_000_000_000, 0);

        let mut batch: Vec<(Address, &mut Account)> = vec![
            (addr1, &mut acct1),
            (addr2, &mut acct2),
            (addr3, &mut acct3),
        ];

        let results = collector.collect_rent_batch(&mut batch);
        assert_eq!(results.len(), 2); // acct2 has 0 slots
        assert!(results.contains_key(&addr1));
        assert!(results.contains_key(&addr3));
        assert!(!results.contains_key(&addr2));
    }
}
