//! EIP-4337 style account abstraction
//!
//! Implements UserOperation mempool, EntryPoint logic, and Paymaster support.
//! Smart contract wallets can validate their own signatures and sponsor gas.

use qfc_types::{Address, Hash};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use thiserror::Error;
use tracing::{debug, info};

/// Account abstraction errors
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum AAError {
    #[error("UserOp validation failed: {0}")]
    ValidationFailed(String),

    #[error("Insufficient pre-fund: need {need}, have {have}")]
    InsufficientPreFund { need: String, have: String },

    #[error("Invalid signature")]
    InvalidSignature,

    #[error("UserOp expired: deadline {deadline}")]
    Expired { deadline: u64 },

    #[error("Nonce mismatch: expected {expected}, got {got}")]
    NonceMismatch { expected: u64, got: u64 },

    #[error("Paymaster not approved: {0}")]
    PaymasterNotApproved(Address),

    #[error("Paymaster deposit insufficient")]
    PaymasterDepositInsufficient,

    #[error("UserOp pool full")]
    PoolFull,

    #[error("Duplicate UserOp: {0}")]
    DuplicateUserOp(Hash),

    #[error("Sender not a contract wallet: {0}")]
    NotContractWallet(Address),
}

/// A user operation (EIP-4337)
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UserOperation {
    /// Smart contract wallet address (sender)
    pub sender: Address,
    /// Anti-replay nonce (managed by the wallet contract)
    pub nonce: u64,
    /// Wallet creation code (if deploying a new wallet)
    pub init_code: Vec<u8>,
    /// Data to pass to the sender for execution
    pub call_data: Vec<u8>,
    /// Gas limit for the main execution call
    pub call_gas_limit: u64,
    /// Gas limit for verification (validateUserOp)
    pub verification_gas_limit: u64,
    /// Gas to compensate the bundler for pre-verification overhead
    pub pre_verification_gas: u64,
    /// Maximum fee per gas (EIP-1559 style)
    pub max_fee_per_gas: u128,
    /// Maximum priority fee per gas
    pub max_priority_fee_per_gas: u128,
    /// Paymaster address + extra data (empty = self-funded)
    pub paymaster_and_data: Vec<u8>,
    /// Signature over the UserOp hash (validated by the wallet contract)
    pub signature: Vec<u8>,
}

impl UserOperation {
    /// Compute the hash of this UserOperation for signing
    pub fn hash(&self, entry_point: &Address, chain_id: u64) -> Hash {
        let mut data = Vec::new();
        data.extend_from_slice(self.sender.as_bytes());
        data.extend_from_slice(&self.nonce.to_le_bytes());
        data.extend_from_slice(qfc_crypto::blake3_hash(&self.init_code).as_bytes());
        data.extend_from_slice(qfc_crypto::blake3_hash(&self.call_data).as_bytes());
        data.extend_from_slice(&self.call_gas_limit.to_le_bytes());
        data.extend_from_slice(&self.verification_gas_limit.to_le_bytes());
        data.extend_from_slice(&self.pre_verification_gas.to_le_bytes());
        data.extend_from_slice(&self.max_fee_per_gas.to_le_bytes());
        data.extend_from_slice(&self.max_priority_fee_per_gas.to_le_bytes());
        data.extend_from_slice(qfc_crypto::blake3_hash(&self.paymaster_and_data).as_bytes());
        // Pack with entry_point and chain_id for replay protection
        data.extend_from_slice(entry_point.as_bytes());
        data.extend_from_slice(&chain_id.to_le_bytes());
        qfc_crypto::blake3_hash(&data)
    }

    /// Total gas this UserOp requires
    pub fn total_gas(&self) -> u64 {
        self.call_gas_limit
            .saturating_add(self.verification_gas_limit)
            .saturating_add(self.pre_verification_gas)
    }

    /// Maximum cost in wei this UserOp can incur
    pub fn max_cost(&self) -> u128 {
        self.total_gas() as u128 * self.max_fee_per_gas
    }

    /// Whether this UserOp deploys a new wallet
    pub fn is_create(&self) -> bool {
        !self.init_code.is_empty()
    }

    /// Extract paymaster address (first 20 bytes of paymaster_and_data)
    pub fn paymaster(&self) -> Option<Address> {
        if self.paymaster_and_data.len() >= 20 {
            Address::from_slice(&self.paymaster_and_data[..20])
        } else {
            None
        }
    }
}

/// Paymaster deposit record
#[derive(Clone, Debug)]
pub struct PaymasterDeposit {
    /// Paymaster contract address
    pub address: Address,
    /// Deposited balance for gas sponsorship
    pub deposit: u128,
    /// Staked amount (for reputation)
    pub stake: u128,
    /// Timestamp after which unstake is allowed
    pub unstake_delay_end: u64,
}

/// A pooled UserOperation with metadata
#[derive(Clone, Debug)]
pub struct PooledUserOp {
    pub user_op: UserOperation,
    pub hash: Hash,
    pub added_at: u64,
}

/// EntryPoint — central coordinator for UserOp validation and execution
pub struct EntryPoint {
    /// Entry point contract address (well-known)
    address: Address,
    /// Chain ID for replay protection
    chain_id: u64,
    /// Smart contract wallet nonces
    nonces: HashMap<Address, u64>,
    /// Paymaster deposits
    paymaster_deposits: HashMap<Address, PaymasterDeposit>,
    /// Approved paymasters (addresses that have staked)
    approved_paymasters: std::collections::HashSet<Address>,
}

impl EntryPoint {
    /// Well-known EntryPoint address
    pub const ADDRESS_BYTES: [u8; 20] = [
        0x43, 0x37, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x01,
    ];

    pub fn new(chain_id: u64) -> Self {
        let address = Address::from_slice(&Self::ADDRESS_BYTES).unwrap();
        Self {
            address,
            chain_id,
            nonces: HashMap::new(),
            paymaster_deposits: HashMap::new(),
            approved_paymasters: std::collections::HashSet::new(),
        }
    }

    /// Get the entry point address
    pub fn address(&self) -> &Address {
        &self.address
    }

    /// Get the current nonce for a wallet
    pub fn get_nonce(&self, sender: &Address) -> u64 {
        self.nonces.get(sender).copied().unwrap_or(0)
    }

    /// Validate a UserOperation before inclusion
    pub fn validate_user_op(&self, user_op: &UserOperation, now: u64) -> Result<(), AAError> {
        // 1. Nonce check
        let expected_nonce = self.get_nonce(&user_op.sender);
        if user_op.nonce != expected_nonce {
            return Err(AAError::NonceMismatch {
                expected: expected_nonce,
                got: user_op.nonce,
            });
        }

        // 2. Gas limits must be non-zero
        if user_op.call_gas_limit == 0 {
            return Err(AAError::ValidationFailed(
                "call_gas_limit must be > 0".into(),
            ));
        }
        if user_op.verification_gas_limit == 0 {
            return Err(AAError::ValidationFailed(
                "verification_gas_limit must be > 0".into(),
            ));
        }
        if user_op.max_fee_per_gas == 0 {
            return Err(AAError::ValidationFailed(
                "max_fee_per_gas must be > 0".into(),
            ));
        }

        // 3. Signature must not be empty
        if user_op.signature.is_empty() {
            return Err(AAError::InvalidSignature);
        }

        // 4. If paymaster is specified, check it's approved and has sufficient deposit
        if let Some(paymaster) = user_op.paymaster() {
            if !self.approved_paymasters.contains(&paymaster) {
                return Err(AAError::PaymasterNotApproved(paymaster));
            }
            if let Some(deposit) = self.paymaster_deposits.get(&paymaster) {
                if deposit.deposit < user_op.max_cost() {
                    return Err(AAError::PaymasterDepositInsufficient);
                }
            } else {
                return Err(AAError::PaymasterDepositInsufficient);
            }
        }

        let _ = now; // Reserved for deadline checks
        Ok(())
    }

    /// Simulate UserOp execution (for gas estimation, no state changes)
    pub fn simulate_validation(
        &self,
        user_op: &UserOperation,
    ) -> Result<SimulationResult, AAError> {
        // Basic validation
        self.validate_user_op(user_op, 0)?;

        let op_hash = user_op.hash(&self.address, self.chain_id);
        let pre_fund = user_op.max_cost();

        Ok(SimulationResult {
            op_hash,
            pre_fund,
            valid_after: 0,
            valid_until: u64::MAX,
            paymaster: user_op.paymaster(),
        })
    }

    /// Execute a validated UserOp: increment nonce and debit paymaster/sender
    pub fn execute_user_op(&mut self, user_op: &UserOperation) -> Result<UserOpReceipt, AAError> {
        // Validate first
        self.validate_user_op(user_op, 0)?;

        let op_hash = user_op.hash(&self.address, self.chain_id);
        let gas_cost = user_op.max_cost();

        // Debit paymaster if present
        let actual_gas_cost = if let Some(paymaster) = user_op.paymaster() {
            let deposit = self
                .paymaster_deposits
                .get_mut(&paymaster)
                .ok_or(AAError::PaymasterDepositInsufficient)?;
            if deposit.deposit < gas_cost {
                return Err(AAError::PaymasterDepositInsufficient);
            }
            deposit.deposit -= gas_cost;
            debug!(
                "Paymaster {} charged {} wei for UserOp {}",
                paymaster,
                gas_cost,
                hex::encode(&op_hash.as_bytes()[..8])
            );
            gas_cost
        } else {
            gas_cost
        };

        // Increment nonce
        let nonce = self.nonces.entry(user_op.sender).or_insert(0);
        *nonce += 1;

        info!(
            "UserOp executed: {} sender={} nonce={}",
            hex::encode(&op_hash.as_bytes()[..8]),
            user_op.sender,
            user_op.nonce,
        );

        Ok(UserOpReceipt {
            op_hash,
            sender: user_op.sender,
            nonce: user_op.nonce,
            paymaster: user_op.paymaster(),
            actual_gas_cost,
            success: true,
        })
    }

    /// Add paymaster deposit
    pub fn add_paymaster_deposit(&mut self, paymaster: Address, amount: u128) {
        let entry = self
            .paymaster_deposits
            .entry(paymaster)
            .or_insert_with(|| PaymasterDeposit {
                address: paymaster,
                deposit: 0,
                stake: 0,
                unstake_delay_end: 0,
            });
        entry.deposit += amount;
        self.approved_paymasters.insert(paymaster);
        info!(
            "Paymaster {} deposit: +{} (total: {})",
            paymaster, amount, entry.deposit
        );
    }

    /// Get paymaster deposit info
    pub fn get_paymaster_deposit(&self, paymaster: &Address) -> Option<&PaymasterDeposit> {
        self.paymaster_deposits.get(paymaster)
    }

    /// Stake as paymaster (required for approval)
    pub fn stake_paymaster(
        &mut self,
        paymaster: Address,
        stake: u128,
        unstake_delay: u64,
        now: u64,
    ) {
        let entry = self
            .paymaster_deposits
            .entry(paymaster)
            .or_insert_with(|| PaymasterDeposit {
                address: paymaster,
                deposit: 0,
                stake: 0,
                unstake_delay_end: 0,
            });
        entry.stake += stake;
        entry.unstake_delay_end = now + unstake_delay;
        self.approved_paymasters.insert(paymaster);
    }
}

/// Result of simulating a UserOp
#[derive(Clone, Debug)]
pub struct SimulationResult {
    pub op_hash: Hash,
    pub pre_fund: u128,
    pub valid_after: u64,
    pub valid_until: u64,
    pub paymaster: Option<Address>,
}

/// Receipt after executing a UserOp
#[derive(Clone, Debug)]
pub struct UserOpReceipt {
    pub op_hash: Hash,
    pub sender: Address,
    pub nonce: u64,
    pub paymaster: Option<Address>,
    pub actual_gas_cost: u128,
    pub success: bool,
}

/// UserOp mempool — separate from the regular transaction pool
pub struct UserOpPool {
    /// Pending UserOps by hash
    ops: HashMap<Hash, PooledUserOp>,
    /// Per-sender pending ops
    sender_ops: HashMap<Address, Vec<Hash>>,
    /// Maximum pool size
    max_size: usize,
    /// Maximum ops per sender
    max_per_sender: usize,
}

impl UserOpPool {
    pub fn new(max_size: usize, max_per_sender: usize) -> Self {
        Self {
            ops: HashMap::new(),
            sender_ops: HashMap::new(),
            max_size,
            max_per_sender,
        }
    }

    /// Add a validated UserOp to the pool
    pub fn add(
        &mut self,
        user_op: UserOperation,
        entry_point: &Address,
        chain_id: u64,
        now: u64,
    ) -> Result<Hash, AAError> {
        if self.ops.len() >= self.max_size {
            return Err(AAError::PoolFull);
        }

        let op_hash = user_op.hash(entry_point, chain_id);

        if self.ops.contains_key(&op_hash) {
            return Err(AAError::DuplicateUserOp(op_hash));
        }

        let sender_ops = self.sender_ops.entry(user_op.sender).or_default();
        if sender_ops.len() >= self.max_per_sender {
            return Err(AAError::PoolFull);
        }

        sender_ops.push(op_hash);
        self.ops.insert(
            op_hash,
            PooledUserOp {
                user_op,
                hash: op_hash,
                added_at: now,
            },
        );

        debug!(
            "UserOp added to pool: {}",
            hex::encode(&op_hash.as_bytes()[..8])
        );
        Ok(op_hash)
    }

    /// Remove a UserOp from the pool (after execution or expiry)
    pub fn remove(&mut self, hash: &Hash) -> Option<PooledUserOp> {
        if let Some(op) = self.ops.remove(hash) {
            if let Some(sender_ops) = self.sender_ops.get_mut(&op.user_op.sender) {
                sender_ops.retain(|h| h != hash);
                if sender_ops.is_empty() {
                    self.sender_ops.remove(&op.user_op.sender);
                }
            }
            Some(op)
        } else {
            None
        }
    }

    /// Get pending UserOps for bundling (up to max_count)
    pub fn get_pending(&self, max_count: usize) -> Vec<&PooledUserOp> {
        self.ops.values().take(max_count).collect()
    }

    /// Get a specific UserOp by hash
    pub fn get(&self, hash: &Hash) -> Option<&PooledUserOp> {
        self.ops.get(hash)
    }

    /// Get pending ops for a specific sender
    pub fn get_sender_ops(&self, sender: &Address) -> Vec<&PooledUserOp> {
        self.sender_ops
            .get(sender)
            .map(|hashes| hashes.iter().filter_map(|h| self.ops.get(h)).collect())
            .unwrap_or_default()
    }

    /// Pool size
    pub fn size(&self) -> usize {
        self.ops.len()
    }

    /// Remove expired UserOps
    pub fn prune_expired(&mut self, deadline: u64) -> usize {
        let expired: Vec<Hash> = self
            .ops
            .iter()
            .filter(|(_, op)| op.added_at < deadline)
            .map(|(h, _)| *h)
            .collect();

        let count = expired.len();
        for hash in expired {
            self.remove(&hash);
        }

        if count > 0 {
            info!("Pruned {} expired UserOps from pool", count);
        }
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_address(byte: u8) -> Address {
        Address::from_slice(&[byte; 20]).unwrap()
    }

    fn make_user_op(sender: Address, nonce: u64) -> UserOperation {
        UserOperation {
            sender,
            nonce,
            init_code: vec![],
            call_data: vec![1, 2, 3, 4],
            call_gas_limit: 100_000,
            verification_gas_limit: 50_000,
            pre_verification_gas: 21_000,
            max_fee_per_gas: 1_000_000_000, // 1 Gwei
            max_priority_fee_per_gas: 100_000_000,
            paymaster_and_data: vec![],
            signature: vec![0xAA; 64],
        }
    }

    fn make_paymaster_op(sender: Address, nonce: u64, paymaster: Address) -> UserOperation {
        let mut op = make_user_op(sender, nonce);
        let mut pm_data = paymaster.as_bytes().to_vec();
        pm_data.extend_from_slice(&[0xBB; 32]); // extra paymaster data
        op.paymaster_and_data = pm_data;
        op
    }

    #[test]
    fn test_user_op_hash() {
        let sender = test_address(1);
        let op = make_user_op(sender, 0);
        let ep = test_address(0x43);
        let hash1 = op.hash(&ep, 9000);
        let hash2 = op.hash(&ep, 9000);
        assert_eq!(hash1, hash2); // Deterministic

        let hash3 = op.hash(&ep, 9001); // Different chain
        assert_ne!(hash1, hash3);
    }

    #[test]
    fn test_user_op_total_gas() {
        let op = make_user_op(test_address(1), 0);
        assert_eq!(op.total_gas(), 100_000 + 50_000 + 21_000);
    }

    #[test]
    fn test_user_op_max_cost() {
        let op = make_user_op(test_address(1), 0);
        assert_eq!(op.max_cost(), 171_000 * 1_000_000_000);
    }

    #[test]
    fn test_validate_user_op() {
        let ep = EntryPoint::new(9000);
        let sender = test_address(1);
        let op = make_user_op(sender, 0);
        assert!(ep.validate_user_op(&op, 0).is_ok());
    }

    #[test]
    fn test_validate_nonce_mismatch() {
        let ep = EntryPoint::new(9000);
        let sender = test_address(1);
        let op = make_user_op(sender, 5); // Expected 0
        let err = ep.validate_user_op(&op, 0).unwrap_err();
        assert!(matches!(
            err,
            AAError::NonceMismatch {
                expected: 0,
                got: 5
            }
        ));
    }

    #[test]
    fn test_validate_empty_signature() {
        let ep = EntryPoint::new(9000);
        let sender = test_address(1);
        let mut op = make_user_op(sender, 0);
        op.signature = vec![];
        let err = ep.validate_user_op(&op, 0).unwrap_err();
        assert!(matches!(err, AAError::InvalidSignature));
    }

    #[test]
    fn test_validate_zero_gas() {
        let ep = EntryPoint::new(9000);
        let sender = test_address(1);
        let mut op = make_user_op(sender, 0);
        op.call_gas_limit = 0;
        let err = ep.validate_user_op(&op, 0).unwrap_err();
        assert!(matches!(err, AAError::ValidationFailed(_)));
    }

    #[test]
    fn test_execute_user_op() {
        let mut ep = EntryPoint::new(9000);
        let sender = test_address(1);
        let op = make_user_op(sender, 0);
        let receipt = ep.execute_user_op(&op).unwrap();
        assert!(receipt.success);
        assert_eq!(receipt.sender, sender);
        assert_eq!(receipt.nonce, 0);
        assert_eq!(ep.get_nonce(&sender), 1); // Nonce incremented
    }

    #[test]
    fn test_execute_increments_nonce() {
        let mut ep = EntryPoint::new(9000);
        let sender = test_address(1);

        ep.execute_user_op(&make_user_op(sender, 0)).unwrap();
        ep.execute_user_op(&make_user_op(sender, 1)).unwrap();
        ep.execute_user_op(&make_user_op(sender, 2)).unwrap();

        assert_eq!(ep.get_nonce(&sender), 3);

        // Wrong nonce should fail
        let err = ep.execute_user_op(&make_user_op(sender, 5)).unwrap_err();
        assert!(matches!(err, AAError::NonceMismatch { .. }));
    }

    #[test]
    fn test_paymaster_flow() {
        let mut ep = EntryPoint::new(9000);
        let sender = test_address(1);
        let paymaster = test_address(0xBE);

        // Add paymaster deposit
        ep.add_paymaster_deposit(paymaster, 1_000_000_000_000_000_000); // 1 ETH
        ep.stake_paymaster(paymaster, 100_000, 86400, 0);

        let op = make_paymaster_op(sender, 0, paymaster);
        let receipt = ep.execute_user_op(&op).unwrap();
        assert!(receipt.success);
        assert_eq!(receipt.paymaster, Some(paymaster));

        // Paymaster deposit decreased
        let deposit = ep.get_paymaster_deposit(&paymaster).unwrap();
        assert!(deposit.deposit < 1_000_000_000_000_000_000);
    }

    #[test]
    fn test_paymaster_insufficient_deposit() {
        let mut ep = EntryPoint::new(9000);
        let sender = test_address(1);
        let paymaster = test_address(0xAB);

        ep.add_paymaster_deposit(paymaster, 100); // Tiny deposit
        let op = make_paymaster_op(sender, 0, paymaster);
        let err = ep.execute_user_op(&op).unwrap_err();
        assert!(matches!(err, AAError::PaymasterDepositInsufficient));
    }

    #[test]
    fn test_unapproved_paymaster() {
        let ep = EntryPoint::new(9000);
        let sender = test_address(1);
        let paymaster = test_address(0xCD);

        let op = make_paymaster_op(sender, 0, paymaster);
        let err = ep.validate_user_op(&op, 0).unwrap_err();
        assert!(matches!(err, AAError::PaymasterNotApproved(_)));
    }

    #[test]
    fn test_simulate_validation() {
        let ep = EntryPoint::new(9000);
        let sender = test_address(1);
        let op = make_user_op(sender, 0);
        let result = ep.simulate_validation(&op).unwrap();
        assert_eq!(result.pre_fund, op.max_cost());
        assert!(result.paymaster.is_none());
    }

    #[test]
    fn test_user_op_pool_add_remove() {
        let mut pool = UserOpPool::new(100, 10);
        let sender = test_address(1);
        let op = make_user_op(sender, 0);
        let ep_addr = test_address(0x43);

        let hash = pool.add(op, &ep_addr, 9000, 1000).unwrap();
        assert_eq!(pool.size(), 1);

        let removed = pool.remove(&hash).unwrap();
        assert_eq!(removed.user_op.sender, sender);
        assert_eq!(pool.size(), 0);
    }

    #[test]
    fn test_user_op_pool_full() {
        let mut pool = UserOpPool::new(2, 10);
        let ep_addr = test_address(0x43);

        pool.add(make_user_op(test_address(1), 0), &ep_addr, 9000, 0)
            .unwrap();
        pool.add(make_user_op(test_address(2), 0), &ep_addr, 9000, 0)
            .unwrap();

        let err = pool
            .add(make_user_op(test_address(3), 0), &ep_addr, 9000, 0)
            .unwrap_err();
        assert!(matches!(err, AAError::PoolFull));
    }

    #[test]
    fn test_user_op_pool_duplicate() {
        let mut pool = UserOpPool::new(100, 10);
        let sender = test_address(1);
        let op = make_user_op(sender, 0);
        let ep_addr = test_address(0x43);

        pool.add(op.clone(), &ep_addr, 9000, 0).unwrap();
        let err = pool.add(op, &ep_addr, 9000, 0).unwrap_err();
        assert!(matches!(err, AAError::DuplicateUserOp(_)));
    }

    #[test]
    fn test_user_op_pool_per_sender_limit() {
        let mut pool = UserOpPool::new(100, 2);
        let sender = test_address(1);
        let ep_addr = test_address(0x43);

        pool.add(make_user_op(sender, 0), &ep_addr, 9000, 0)
            .unwrap();
        pool.add(make_user_op(sender, 1), &ep_addr, 9000, 0)
            .unwrap();

        let err = pool
            .add(make_user_op(sender, 2), &ep_addr, 9000, 0)
            .unwrap_err();
        assert!(matches!(err, AAError::PoolFull));
    }

    #[test]
    fn test_user_op_pool_get_sender_ops() {
        let mut pool = UserOpPool::new(100, 10);
        let sender1 = test_address(1);
        let sender2 = test_address(2);
        let ep_addr = test_address(0x43);

        pool.add(make_user_op(sender1, 0), &ep_addr, 9000, 0)
            .unwrap();
        pool.add(make_user_op(sender1, 1), &ep_addr, 9000, 0)
            .unwrap();
        pool.add(make_user_op(sender2, 0), &ep_addr, 9000, 0)
            .unwrap();

        assert_eq!(pool.get_sender_ops(&sender1).len(), 2);
        assert_eq!(pool.get_sender_ops(&sender2).len(), 1);
    }

    #[test]
    fn test_user_op_pool_prune_expired() {
        let mut pool = UserOpPool::new(100, 10);
        let ep_addr = test_address(0x43);

        pool.add(make_user_op(test_address(1), 0), &ep_addr, 9000, 100)
            .unwrap();
        pool.add(make_user_op(test_address(2), 0), &ep_addr, 9000, 200)
            .unwrap();
        pool.add(make_user_op(test_address(3), 0), &ep_addr, 9000, 500)
            .unwrap();

        let pruned = pool.prune_expired(300); // Remove ops added before 300
        assert_eq!(pruned, 2);
        assert_eq!(pool.size(), 1);
    }

    #[test]
    fn test_user_op_is_create() {
        let mut op = make_user_op(test_address(1), 0);
        assert!(!op.is_create());
        op.init_code = vec![0x60, 0x60]; // Some init code
        assert!(op.is_create());
    }

    #[test]
    fn test_user_op_paymaster_extraction() {
        let sender = test_address(1);
        let paymaster = test_address(0xAB);
        let op = make_paymaster_op(sender, 0, paymaster);
        assert_eq!(op.paymaster(), Some(paymaster));

        let op2 = make_user_op(sender, 0);
        assert_eq!(op2.paymaster(), None);
    }
}
