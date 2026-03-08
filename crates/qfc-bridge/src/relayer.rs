//! Bridge relayer — manages deposit/withdrawal lifecycle

use crate::types::{
    BridgeDeposit, BridgeError, BridgeStatus, BridgeWithdrawal, DepositStatus, WithdrawalStatus,
    DEFAULT_ETH_CONFIRMATIONS,
};
use crate::validator::BridgeValidatorSet;
use qfc_types::{Address, Hash};
use std::collections::HashMap;
use tracing::{debug, info, warn};

/// Bridge relayer configuration
#[derive(Clone, Debug)]
pub struct RelayerConfig {
    /// Required Ethereum confirmations
    pub eth_confirmations: u64,
    /// Bridge validators
    pub validators: Vec<Address>,
    /// Signature threshold
    pub threshold: usize,
}

impl Default for RelayerConfig {
    fn default() -> Self {
        Self {
            eth_confirmations: DEFAULT_ETH_CONFIRMATIONS,
            validators: vec![],
            threshold: 0, // No validators configured = bridge disabled
        }
    }
}

/// Bridge relayer manages the deposit/withdrawal state machine
pub struct BridgeRelayer {
    /// Deposit records
    deposits: HashMap<Hash, BridgeDeposit>,
    /// Withdrawal records
    withdrawals: HashMap<Hash, BridgeWithdrawal>,
    /// Validator set for signing
    validator_set: BridgeValidatorSet,
    /// Configuration
    config: RelayerConfig,
    /// Whether bridge is paused
    paused: bool,
    /// Counters
    total_deposits: u64,
    total_withdrawals: u64,
    /// Total value locked (ETH in wei)
    total_value_locked: u128,
}

impl BridgeRelayer {
    pub fn new(config: RelayerConfig) -> Self {
        let validator_set = BridgeValidatorSet::new(config.validators.clone(), config.threshold);

        Self {
            deposits: HashMap::new(),
            withdrawals: HashMap::new(),
            validator_set,
            config,
            paused: false,
            total_deposits: 0,
            total_withdrawals: 0,
            total_value_locked: 0,
        }
    }

    /// Record a new deposit observed on Ethereum
    pub fn observe_deposit(
        &mut self,
        deposit_id: Hash,
        eth_tx_hash: Hash,
        eth_block_number: u64,
        eth_sender: Address,
        qfc_recipient: Address,
        token_address: Address,
        amount: u128,
        now: u64,
    ) -> Result<(), BridgeError> {
        if self.paused {
            return Err(BridgeError::Paused);
        }

        if amount == 0 {
            return Err(BridgeError::InvalidAmount);
        }

        if self.deposits.contains_key(&deposit_id) {
            return Err(BridgeError::DuplicateDeposit(deposit_id));
        }

        let deposit = BridgeDeposit {
            deposit_id,
            eth_tx_hash,
            eth_block_number,
            eth_sender,
            qfc_recipient,
            token_address,
            amount,
            required_confirmations: self.config.eth_confirmations,
            status: DepositStatus::Pending,
            signatures: vec![],
            observed_at: now,
        };

        info!(
            "Bridge deposit observed: {} from {} amount={} wei",
            deposit_id, eth_sender, amount
        );

        self.deposits.insert(deposit_id, deposit);
        Ok(())
    }

    /// Confirm a deposit after Ethereum confirmations are met
    pub fn confirm_deposit(&mut self, deposit_id: &Hash) -> Result<(), BridgeError> {
        let deposit = self
            .deposits
            .get_mut(deposit_id)
            .ok_or(BridgeError::DepositNotFound(*deposit_id))?;

        if deposit.status != DepositStatus::Pending {
            return Err(BridgeError::AlreadyCompleted);
        }

        deposit.status = DepositStatus::Confirmed;
        debug!("Deposit {} confirmed", deposit_id);
        Ok(())
    }

    /// Add a validator signature to a deposit
    pub fn sign_deposit(
        &mut self,
        deposit_id: &Hash,
        validator: Address,
        signature: Vec<u8>,
    ) -> Result<bool, BridgeError> {
        if self.paused {
            return Err(BridgeError::Paused);
        }

        let deposit = self
            .deposits
            .get_mut(deposit_id)
            .ok_or(BridgeError::DepositNotFound(*deposit_id))?;

        self.validator_set
            .sign_deposit(deposit, validator, signature)
    }

    /// Mark a deposit as completed (tokens minted on QFC)
    pub fn complete_deposit(&mut self, deposit_id: &Hash) -> Result<u128, BridgeError> {
        let deposit = self
            .deposits
            .get_mut(deposit_id)
            .ok_or(BridgeError::DepositNotFound(*deposit_id))?;

        if deposit.status != DepositStatus::Minting {
            return Err(BridgeError::AlreadyCompleted);
        }

        deposit.status = DepositStatus::Completed;
        self.total_deposits += 1;
        self.total_value_locked += deposit.amount;

        info!(
            "Bridge deposit completed: {} amount={} wei to {}",
            deposit_id, deposit.amount, deposit.qfc_recipient
        );

        Ok(deposit.amount)
    }

    /// Record a new withdrawal (burn on QFC, unlock on ETH)
    pub fn observe_withdrawal(
        &mut self,
        withdrawal_id: Hash,
        qfc_tx_hash: Hash,
        qfc_block_number: u64,
        qfc_sender: Address,
        eth_recipient: Address,
        token_address: Address,
        amount: u128,
        now: u64,
    ) -> Result<(), BridgeError> {
        if self.paused {
            return Err(BridgeError::Paused);
        }

        if amount == 0 {
            return Err(BridgeError::InvalidAmount);
        }

        let withdrawal = BridgeWithdrawal {
            withdrawal_id,
            qfc_tx_hash,
            qfc_block_number,
            qfc_sender,
            eth_recipient,
            token_address,
            amount,
            status: WithdrawalStatus::Pending,
            signatures: vec![],
            observed_at: now,
            eth_unlock_tx: None,
        };

        info!(
            "Bridge withdrawal observed: {} from {} amount={} wei",
            withdrawal_id, qfc_sender, amount
        );

        self.withdrawals.insert(withdrawal_id, withdrawal);
        Ok(())
    }

    /// Move a withdrawal to signing phase
    pub fn start_signing_withdrawal(&mut self, withdrawal_id: &Hash) -> Result<(), BridgeError> {
        let withdrawal = self
            .withdrawals
            .get_mut(withdrawal_id)
            .ok_or(BridgeError::WithdrawalNotFound(*withdrawal_id))?;

        withdrawal.status = WithdrawalStatus::Signing;
        Ok(())
    }

    /// Add a validator signature to a withdrawal
    pub fn sign_withdrawal(
        &mut self,
        withdrawal_id: &Hash,
        validator: Address,
        signature: Vec<u8>,
    ) -> Result<bool, BridgeError> {
        if self.paused {
            return Err(BridgeError::Paused);
        }

        let withdrawal = self
            .withdrawals
            .get_mut(withdrawal_id)
            .ok_or(BridgeError::WithdrawalNotFound(*withdrawal_id))?;

        self.validator_set
            .sign_withdrawal(withdrawal, validator, signature)
    }

    /// Mark a withdrawal as completed (unlocked on Ethereum)
    pub fn complete_withdrawal(
        &mut self,
        withdrawal_id: &Hash,
        eth_unlock_tx: Hash,
    ) -> Result<u128, BridgeError> {
        let withdrawal = self
            .withdrawals
            .get_mut(withdrawal_id)
            .ok_or(BridgeError::WithdrawalNotFound(*withdrawal_id))?;

        withdrawal.status = WithdrawalStatus::Completed;
        withdrawal.eth_unlock_tx = Some(eth_unlock_tx);
        self.total_withdrawals += 1;
        self.total_value_locked = self.total_value_locked.saturating_sub(withdrawal.amount);

        info!(
            "Bridge withdrawal completed: {} amount={} wei to {}",
            withdrawal_id, withdrawal.amount, withdrawal.eth_recipient
        );

        Ok(withdrawal.amount)
    }

    /// Get bridge status
    pub fn status(&self) -> BridgeStatus {
        let pending_deposits = self
            .deposits
            .values()
            .filter(|d| {
                d.status == DepositStatus::Pending
                    || d.status == DepositStatus::Confirmed
                    || d.status == DepositStatus::Minting
            })
            .count() as u64;

        let pending_withdrawals = self
            .withdrawals
            .values()
            .filter(|w| {
                w.status == WithdrawalStatus::Pending
                    || w.status == WithdrawalStatus::Signing
                    || w.status == WithdrawalStatus::Submitted
            })
            .count() as u64;

        BridgeStatus {
            active: !self.paused,
            validator_count: self.validator_set.validator_count(),
            threshold: self.validator_set.threshold(),
            total_deposits: self.total_deposits,
            total_withdrawals: self.total_withdrawals,
            pending_deposits,
            pending_withdrawals,
            total_value_locked: self.total_value_locked.to_string(),
        }
    }

    /// Get a deposit by ID
    pub fn get_deposit(&self, id: &Hash) -> Option<&BridgeDeposit> {
        self.deposits.get(id)
    }

    /// Get a withdrawal by ID
    pub fn get_withdrawal(&self, id: &Hash) -> Option<&BridgeWithdrawal> {
        self.withdrawals.get(id)
    }

    /// Get all pending deposits
    pub fn pending_deposits(&self) -> Vec<&BridgeDeposit> {
        self.deposits
            .values()
            .filter(|d| d.status != DepositStatus::Completed && d.status != DepositStatus::Failed)
            .collect()
    }

    /// Get all pending withdrawals
    pub fn pending_withdrawals(&self) -> Vec<&BridgeWithdrawal> {
        self.withdrawals
            .values()
            .filter(|w| {
                w.status != WithdrawalStatus::Completed && w.status != WithdrawalStatus::Failed
            })
            .collect()
    }

    /// Pause the bridge (emergency)
    pub fn pause(&mut self) {
        self.paused = true;
        warn!("Bridge paused");
    }

    /// Unpause the bridge
    pub fn unpause(&mut self) {
        self.paused = false;
        info!("Bridge unpaused");
    }

    /// Check if bridge is paused
    pub fn is_paused(&self) -> bool {
        self.paused
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_address(byte: u8) -> Address {
        Address::from_slice(&[byte; 20]).unwrap()
    }

    fn test_hash(byte: u8) -> Hash {
        Hash::from_slice(&[byte; 32]).unwrap()
    }

    fn make_config() -> RelayerConfig {
        RelayerConfig {
            eth_confirmations: 12,
            validators: (1..=7).map(test_address).collect(),
            threshold: 5,
        }
    }

    #[test]
    fn test_deposit_lifecycle() {
        let mut relayer = BridgeRelayer::new(make_config());

        let deposit_id = test_hash(1);

        // Observe deposit
        relayer
            .observe_deposit(
                deposit_id,
                test_hash(2),
                1000,
                test_address(0xAA),
                test_address(0xBB),
                Address::ZERO,
                1_000_000_000_000_000_000,
                12345,
            )
            .unwrap();

        // Confirm after Ethereum finality
        relayer.confirm_deposit(&deposit_id).unwrap();
        assert_eq!(
            relayer.get_deposit(&deposit_id).unwrap().status,
            DepositStatus::Confirmed
        );

        // Collect signatures (need 5 of 7)
        for i in 1..=5u8 {
            relayer
                .sign_deposit(&deposit_id, test_address(i), vec![i])
                .unwrap();
        }
        assert_eq!(
            relayer.get_deposit(&deposit_id).unwrap().status,
            DepositStatus::Minting
        );

        // Complete
        let amount = relayer.complete_deposit(&deposit_id).unwrap();
        assert_eq!(amount, 1_000_000_000_000_000_000);
        assert_eq!(relayer.status().total_deposits, 1);
        assert_eq!(relayer.status().total_value_locked, "1000000000000000000");
    }

    #[test]
    fn test_withdrawal_lifecycle() {
        let mut relayer = BridgeRelayer::new(make_config());

        let withdrawal_id = test_hash(10);

        // Observe withdrawal (burn on QFC)
        relayer
            .observe_withdrawal(
                withdrawal_id,
                test_hash(11),
                500,
                test_address(0xCC),
                test_address(0xDD),
                Address::ZERO,
                500_000_000_000_000_000,
                12345,
            )
            .unwrap();

        // Start signing
        relayer.start_signing_withdrawal(&withdrawal_id).unwrap();

        // Collect signatures
        for i in 1..=5u8 {
            relayer
                .sign_withdrawal(&withdrawal_id, test_address(i), vec![i])
                .unwrap();
        }
        assert_eq!(
            relayer.get_withdrawal(&withdrawal_id).unwrap().status,
            WithdrawalStatus::Submitted
        );

        // Complete
        let amount = relayer
            .complete_withdrawal(&withdrawal_id, test_hash(99))
            .unwrap();
        assert_eq!(amount, 500_000_000_000_000_000);
        assert_eq!(relayer.status().total_withdrawals, 1);
    }

    #[test]
    fn test_duplicate_deposit() {
        let mut relayer = BridgeRelayer::new(make_config());
        let deposit_id = test_hash(1);

        relayer
            .observe_deposit(
                deposit_id,
                test_hash(2),
                1000,
                test_address(0xAA),
                test_address(0xBB),
                Address::ZERO,
                1000,
                12345,
            )
            .unwrap();

        let err = relayer
            .observe_deposit(
                deposit_id,
                test_hash(2),
                1000,
                test_address(0xAA),
                test_address(0xBB),
                Address::ZERO,
                1000,
                12345,
            )
            .unwrap_err();

        assert!(matches!(err, BridgeError::DuplicateDeposit(_)));
    }

    #[test]
    fn test_bridge_pause() {
        let mut relayer = BridgeRelayer::new(make_config());
        relayer.pause();

        let err = relayer
            .observe_deposit(
                test_hash(1),
                test_hash(2),
                1000,
                test_address(0xAA),
                test_address(0xBB),
                Address::ZERO,
                1000,
                12345,
            )
            .unwrap_err();

        assert!(matches!(err, BridgeError::Paused));
        assert!(!relayer.status().active);

        relayer.unpause();
        assert!(relayer.status().active);
    }

    #[test]
    fn test_bridge_status() {
        let relayer = BridgeRelayer::new(make_config());
        let status = relayer.status();

        assert!(status.active);
        assert_eq!(status.validator_count, 7);
        assert_eq!(status.threshold, 5);
        assert_eq!(status.total_deposits, 0);
        assert_eq!(status.pending_deposits, 0);
    }
}
