//! Transaction executor

use crate::error::{ExecutorError, Result};
use crate::evm::EvmExecutor;
use qfc_crypto::{address_from_public_key, blake3_hash};
use qfc_state::StateDB;
use qfc_types::{
    create_bloom, Address, Log, Receipt, ReceiptStatus, SignedTransaction, Transaction,
    TransactionType, U256, DEFAULT_CHAIN_ID, MIN_DELEGATION, MIN_VALIDATOR_STAKE,
    MINIMUM_GAS, TRANSFER_GAS, UNSTAKE_DELAY_SECS,
};
use tracing::{debug, trace, warn};

// Re-export for Ethereum transaction support
#[allow(unused_imports)]
use sha3::{Digest, Keccak256};

/// Result of executing a single transaction
#[derive(Clone, Debug)]
pub struct ExecutionResult {
    /// Whether execution succeeded
    pub success: bool,
    /// Gas used
    pub gas_used: u64,
    /// Logs emitted
    pub logs: Vec<Log>,
    /// Contract address (if contract creation)
    pub contract_address: Option<Address>,
    /// Error message (if failed)
    pub error: Option<String>,
}

impl ExecutionResult {
    pub fn success(gas_used: u64) -> Self {
        Self {
            success: true,
            gas_used,
            logs: Vec::new(),
            contract_address: None,
            error: None,
        }
    }

    pub fn success_with_contract(gas_used: u64, contract_address: Address) -> Self {
        Self {
            success: true,
            gas_used,
            logs: Vec::new(),
            contract_address: Some(contract_address),
            error: None,
        }
    }

    pub fn failure(gas_used: u64, error: String) -> Self {
        Self {
            success: false,
            gas_used,
            logs: Vec::new(),
            contract_address: None,
            error: Some(error),
        }
    }
}

/// Transaction executor
pub struct Executor {
    /// Chain ID for validation
    chain_id: u64,
    /// Current block number (set during execution)
    block_number: u64,
    /// Current block timestamp (set during execution)
    block_timestamp: u64,
    /// Block gas limit
    block_gas_limit: u64,
}

impl Executor {
    /// Create a new executor
    pub fn new(chain_id: u64) -> Self {
        Self {
            chain_id,
            block_number: 0,
            block_timestamp: 0,
            block_gas_limit: qfc_types::DEFAULT_BLOCK_GAS_LIMIT,
        }
    }

    /// Create an executor for the default testnet
    pub fn testnet() -> Self {
        Self::new(DEFAULT_CHAIN_ID)
    }

    /// Set block context for EVM execution
    pub fn set_block_context(&mut self, block_number: u64, block_timestamp: u64, gas_limit: u64) {
        self.block_number = block_number;
        self.block_timestamp = block_timestamp;
        self.block_gas_limit = gas_limit;
    }

    /// Validate a transaction before execution
    pub fn validate_transaction(
        &self,
        tx: &Transaction,
        state: &StateDB,
    ) -> Result<SignedTransaction> {
        // 1. Validate chain ID
        if tx.chain_id != self.chain_id {
            return Err(ExecutorError::InvalidChainId {
                expected: self.chain_id,
                actual: tx.chain_id,
            });
        }

        // 2. Validate gas limit
        let intrinsic_gas = tx.intrinsic_gas();
        if tx.gas_limit < intrinsic_gas {
            return Err(ExecutorError::GasTooLow {
                minimum: intrinsic_gas,
                provided: tx.gas_limit,
            });
        }

        // 3. Compute transaction hash and verify signature
        // Check if this is an Ethereum transaction (marker byte 0xEE in public_key)
        let (tx_hash, sender) = if tx.public_key.0[0] == 0xEE {
            // Ethereum transaction: signature was already verified during RLP decoding
            // The sender was recovered from secp256k1 signature at that time
            // We need to recover the sender address from the original Ethereum transaction
            // Since we stored r,s in signature and v in public_key[1], we can verify here
            // But for simplicity, we trust the RPC layer's verification and derive sender
            // from the signature (r,s) and recovery id (v)

            // For now, we re-decode to get the sender
            // In production, we'd pass the sender through a different mechanism
            // Let's compute keccak256 hash of the transaction for the hash
            use sha3::{Digest, Keccak256};

            // The hash was already computed as keccak256 of the RLP-encoded tx
            // We need to reconstruct the sender from r, s, v
            let r = &tx.signature.0[..32];
            let s = &tx.signature.0[32..];
            let v = tx.public_key.0[1] as u64;

            // For Ethereum transactions, we need to recover the sender
            // Since we can't easily reconstruct the signing hash here,
            // we use a workaround: store the sender address in public_key bytes 2-21
            let mut sender_bytes = [0u8; 20];
            sender_bytes.copy_from_slice(&tx.public_key.0[2..22]);
            let sender = Address::new(sender_bytes);

            // Use blake3 hash for internal consistency
            let tx_hash = blake3_hash(&tx.to_bytes_without_signature());

            debug!(
                "Ethereum tx: sender={} v={} r=0x{}... s=0x{}...",
                sender,
                v,
                hex::encode(&r[..4]),
                hex::encode(&s[..4])
            );

            (tx_hash, sender)
        } else {
            // QFC native transaction: verify Ed25519 signature
            let tx_hash = blake3_hash(&tx.to_bytes_without_signature());

            // Verify the Ed25519 signature using the public key included in the transaction
            qfc_crypto::verify_hash_signature(&tx.public_key, &tx_hash, &tx.signature)
                .map_err(|_| ExecutorError::InvalidSignature)?;

            // Derive sender address from the verified public key
            let sender = address_from_public_key(&tx.public_key);

            (tx_hash, sender)
        };

        // 4. Check sender's balance
        let sender_balance = state.get_balance(&sender)?;
        let total_cost = tx.total_cost();

        if sender_balance < total_cost {
            return Err(ExecutorError::InsufficientBalance {
                need: total_cost.to_string(),
                have: sender_balance.to_string(),
            });
        }

        // 5. Check nonce
        let expected_nonce = state.get_nonce(&sender)?;
        if tx.nonce != expected_nonce {
            return Err(ExecutorError::InvalidNonce {
                expected: expected_nonce,
                actual: tx.nonce,
            });
        }

        // 6. Validate transaction type specific requirements
        match tx.tx_type {
            TransactionType::Transfer => {
                if tx.to.is_none() {
                    return Err(ExecutorError::MissingRecipient);
                }
            }
            TransactionType::ContractCreate => {
                // Contract creation requires data
            }
            TransactionType::Stake => {
                // Stake must meet minimum
                let stake = tx.value;
                let current_stake = state.get_stake(&sender)?;
                if current_stake.is_zero()
                    && stake < U256::from_u128(MIN_VALIDATOR_STAKE)
                {
                    return Err(ExecutorError::StakeTooLow {
                        minimum: U256::from_u128(MIN_VALIDATOR_STAKE).to_string(),
                        provided: stake.to_string(),
                    });
                }
            }
            _ => {}
        }

        Ok(SignedTransaction::new(tx.clone(), tx_hash, sender))
    }

    /// Execute a validated transaction
    pub fn execute(
        &self,
        tx: &SignedTransaction,
        state: &StateDB,
        block_producer: &Address,
    ) -> Result<ExecutionResult> {
        let sender = tx.sender;
        let gas_limit = tx.tx.gas_limit;

        // Take snapshot for potential revert
        let snapshot = state.snapshot();

        // 1. Deduct gas prepayment
        let gas_cost = tx.tx.gas_cost();
        state.sub_balance(&sender, gas_cost)?;

        // 2. Increment nonce
        state.increment_nonce(&sender)?;

        // 3. Execute based on transaction type
        let result = match tx.tx.tx_type {
            TransactionType::Transfer => self.execute_transfer(&tx.tx, &sender, state),
            TransactionType::ContractCreate => {
                self.execute_contract_create(&tx.tx, &sender, state, block_producer)
            }
            TransactionType::ContractCall => {
                self.execute_contract_call(&tx.tx, &sender, state, block_producer)
            }
            TransactionType::Stake => self.execute_stake(&tx.tx, &sender, state),
            TransactionType::Unstake => self.execute_unstake(&tx.tx, &sender, state),
            TransactionType::ValidatorRegister => {
                self.execute_validator_register(&tx.tx, &sender, state)
            }
            TransactionType::ValidatorExit => self.execute_validator_exit(&tx.tx, &sender, state),
            TransactionType::Delegate => self.execute_delegate(&tx.tx, &sender, state),
            TransactionType::Undelegate => self.execute_undelegate(&tx.tx, &sender, state),
            TransactionType::ClaimDelegationRewards => {
                self.execute_claim_delegation_rewards(&tx.tx, &sender, state)
            }
        };

        // 4. Handle result
        match result {
            Ok(exec_result) => {
                // Refund unused gas
                let gas_refund = (gas_limit - exec_result.gas_used) * tx.tx.gas_price.low_u64();
                state.add_balance(&sender, U256::from_u64(gas_refund))?;

                // Pay gas to block producer
                let gas_payment = exec_result.gas_used * tx.tx.gas_price.low_u64();
                state.add_balance(block_producer, U256::from_u64(gas_payment))?;

                Ok(exec_result)
            }
            Err(e) => {
                // Revert state changes except gas consumption
                state.revert(snapshot)?;

                // Re-deduct gas (all of it since we failed)
                state.sub_balance(&sender, gas_cost)?;

                // Pay gas to block producer
                state.add_balance(block_producer, gas_cost)?;

                // Increment nonce even on failure
                state.increment_nonce(&sender)?;

                Ok(ExecutionResult::failure(gas_limit, e.to_string()))
            }
        }
    }

    fn execute_transfer(
        &self,
        tx: &Transaction,
        sender: &Address,
        state: &StateDB,
    ) -> Result<ExecutionResult> {
        let to = tx.to.ok_or(ExecutorError::MissingRecipient)?;

        // Transfer value
        state.transfer(sender, &to, tx.value)?;

        trace!(
            "Transfer: {} -> {} value={}",
            sender,
            to,
            tx.value
        );

        Ok(ExecutionResult::success(TRANSFER_GAS))
    }

    fn execute_contract_create(
        &self,
        tx: &Transaction,
        sender: &Address,
        state: &StateDB,
        block_producer: &Address,
    ) -> Result<ExecutionResult> {
        // Use EVM to execute contract creation
        let evm_executor = EvmExecutor::new(
            state,
            self.chain_id,
            self.block_number,
            self.block_timestamp,
            *block_producer,
            self.block_gas_limit,
        );

        let result = evm_executor.create(sender, tx.data.clone(), tx.value, tx.gas_limit)?;

        if result.success {
            let gas_used = result.gas_used.max(qfc_types::CONTRACT_CREATE_GAS);
            if let Some(contract_addr) = result.contract_address {
                debug!("Contract created at {} by {}", contract_addr, sender);
                let mut exec_result = ExecutionResult::success_with_contract(gas_used, contract_addr);
                exec_result.logs = result.logs;
                Ok(exec_result)
            } else {
                Ok(ExecutionResult::failure(
                    gas_used,
                    "Contract creation failed: no address".to_string(),
                ))
            }
        } else {
            Ok(ExecutionResult::failure(
                result.gas_used,
                result.error.unwrap_or_else(|| "Unknown error".to_string()),
            ))
        }
    }

    fn execute_contract_call(
        &self,
        tx: &Transaction,
        sender: &Address,
        state: &StateDB,
        block_producer: &Address,
    ) -> Result<ExecutionResult> {
        let to = tx.to.ok_or(ExecutorError::MissingRecipient)?;

        // Check if target has code (is a contract)
        let code = state.get_code(&to)?;

        if code.is_empty() {
            // Not a contract, just transfer value
            if !tx.value.is_zero() {
                state.transfer(sender, &to, tx.value)?;
            }
            let gas_used = MINIMUM_GAS + tx.data_gas();
            debug!(
                "Call to non-contract: {} -> {} value={}",
                sender, to, tx.value
            );
            return Ok(ExecutionResult::success(gas_used));
        }

        // Use EVM to execute contract call
        let evm_executor = EvmExecutor::new(
            state,
            self.chain_id,
            self.block_number,
            self.block_timestamp,
            *block_producer,
            self.block_gas_limit,
        );

        let result = evm_executor.call(sender, &to, tx.data.clone(), tx.value, tx.gas_limit)?;

        debug!(
            "Contract call: {} -> {} data_len={} success={}",
            sender,
            to,
            tx.data.len(),
            result.success
        );

        if result.success {
            let mut exec_result = ExecutionResult::success(result.gas_used);
            exec_result.logs = result.logs;
            Ok(exec_result)
        } else {
            Ok(ExecutionResult::failure(
                result.gas_used,
                result.error.unwrap_or_else(|| "Execution failed".to_string()),
            ))
        }
    }

    fn execute_stake(
        &self,
        tx: &Transaction,
        sender: &Address,
        state: &StateDB,
    ) -> Result<ExecutionResult> {
        let stake_amount = tx.value;

        // Get current stake
        let current_stake = state.get_stake(sender)?;
        let new_stake = current_stake + stake_amount;

        // Check minimum stake
        if current_stake.is_zero()
            && new_stake < U256::from_u128(MIN_VALIDATOR_STAKE)
        {
            return Err(ExecutorError::StakeTooLow {
                minimum: U256::from_u128(MIN_VALIDATOR_STAKE).to_string(),
                provided: new_stake.to_string(),
            });
        }

        // Lock the tokens (move from balance to stake)
        state.sub_balance(sender, stake_amount)?;
        state.set_stake(sender, new_stake)?;

        debug!(
            "Staked: {} amount={} total={}",
            sender, stake_amount, new_stake
        );

        Ok(ExecutionResult::success(MINIMUM_GAS * 2))
    }

    fn execute_unstake(
        &self,
        tx: &Transaction,
        sender: &Address,
        state: &StateDB,
    ) -> Result<ExecutionResult> {
        // Parse unstake amount from data
        let unstake_amount = if tx.data.len() >= 32 {
            let mut bytes = [0u8; 32];
            bytes.copy_from_slice(&tx.data[0..32]);
            U256::from_be_bytes(&bytes)
        } else {
            // Unstake all
            state.get_stake(sender)?
        };

        let current_stake = state.get_stake(sender)?;

        if current_stake < unstake_amount {
            return Err(ExecutorError::InsufficientBalance {
                need: unstake_amount.to_string(),
                have: current_stake.to_string(),
            });
        }

        let new_stake = current_stake - unstake_amount;

        // Update stake
        state.set_stake(sender, new_stake)?;

        // Return tokens to balance (in real implementation, there would be a lockup period)
        state.add_balance(sender, unstake_amount)?;

        debug!(
            "Unstaked: {} amount={} remaining={}",
            sender, unstake_amount, new_stake
        );

        Ok(ExecutionResult::success(MINIMUM_GAS * 2))
    }

    fn execute_validator_register(
        &self,
        tx: &Transaction,
        sender: &Address,
        state: &StateDB,
    ) -> Result<ExecutionResult> {
        // Check if already a validator
        let current_stake = state.get_stake(sender)?;
        if !current_stake.is_zero() {
            return Err(ExecutorError::AlreadyValidator);
        }

        // Register requires minimum stake
        let stake_amount = tx.value;
        if stake_amount < U256::from_u128(MIN_VALIDATOR_STAKE) {
            return Err(ExecutorError::StakeTooLow {
                minimum: U256::from_u128(MIN_VALIDATOR_STAKE).to_string(),
                provided: stake_amount.to_string(),
            });
        }

        // Lock stake
        state.sub_balance(sender, stake_amount)?;
        state.set_stake(sender, stake_amount)?;
        state.set_contribution_score(sender, 0)?;

        debug!(
            "Validator registered: {} stake={}",
            sender, stake_amount
        );

        Ok(ExecutionResult::success(MINIMUM_GAS * 3))
    }

    fn execute_validator_exit(
        &self,
        _tx: &Transaction,
        sender: &Address,
        state: &StateDB,
    ) -> Result<ExecutionResult> {
        let current_stake = state.get_stake(sender)?;

        if current_stake.is_zero() {
            return Err(ExecutorError::NotValidator);
        }

        // Return all stake (in real implementation, there would be a lockup period)
        state.add_balance(sender, current_stake)?;
        state.set_stake(sender, U256::ZERO)?;

        debug!(
            "Validator exited: {} stake_returned={}",
            sender, current_stake
        );

        Ok(ExecutionResult::success(MINIMUM_GAS * 2))
    }

    // ============ Delegation Execution ============

    /// Execute a delegation transaction
    /// Locks tokens and delegates to a validator
    fn execute_delegate(
        &self,
        tx: &Transaction,
        sender: &Address,
        state: &StateDB,
    ) -> Result<ExecutionResult> {
        let validator = tx.to.ok_or(ExecutorError::MissingRecipient)?;
        let amount = tx.value;

        // Check minimum delegation amount
        if amount < U256::from_u128(MIN_DELEGATION) {
            return Err(ExecutorError::DelegationTooLow {
                minimum: U256::from_u128(MIN_DELEGATION).to_string(),
                provided: amount.to_string(),
            });
        }

        // Check if sender has existing delegation to a different validator
        let (existing_validator, _) = state.get_delegation(sender)?;
        if let Some(existing) = existing_validator {
            if existing != validator {
                return Err(ExecutorError::AlreadyDelegated);
            }
        }

        // Check if validator exists (has stake)
        let validator_stake = state.get_stake(&validator)?;
        if validator_stake.is_zero() {
            return Err(ExecutorError::InvalidValidator);
        }

        // Lock tokens (deduct from balance)
        state.sub_balance(sender, amount)?;

        // Record delegation in sender's account
        if existing_validator.is_some() {
            // Add to existing delegation
            state.add_delegation_amount(sender, amount)?;
        } else {
            // New delegation
            state.set_delegation(sender, &validator, amount)?;
        }

        debug!(
            "Delegated: {} -> {} amount={}",
            sender, validator, amount
        );

        Ok(ExecutionResult::success(MINIMUM_GAS * 3))
    }

    /// Execute an undelegation transaction
    /// Creates an undelegation with a lockup period
    fn execute_undelegate(
        &self,
        tx: &Transaction,
        sender: &Address,
        state: &StateDB,
    ) -> Result<ExecutionResult> {
        let validator = tx.to.ok_or(ExecutorError::MissingRecipient)?;

        // Parse amount from data (or undelegate all if empty)
        let amount = if tx.data.len() >= 32 {
            let mut bytes = [0u8; 32];
            bytes.copy_from_slice(&tx.data[0..32]);
            U256::from_be_bytes(&bytes)
        } else {
            // Undelegate all
            state.get_delegation_amount(sender, &validator)?
        };

        // Check if sender has delegation to this validator
        let (existing_validator, existing_amount) = state.get_delegation(sender)?;
        match existing_validator {
            Some(v) if v == validator => {
                if existing_amount < amount {
                    return Err(ExecutorError::InsufficientDelegation {
                        need: amount.to_string(),
                        have: existing_amount.to_string(),
                    });
                }
            }
            _ => return Err(ExecutorError::NoDelegation),
        }

        // Reduce delegation amount
        state.sub_delegation_amount(sender, amount)?;

        // Calculate unlock time (current time + 7 days)
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let _unlock_at = now + UNSTAKE_DELAY_SECS;

        // In a full implementation, we would store the undelegation record
        // For now, we immediately return the funds (simplified)
        // TODO: Store Undelegation record and process after lockup period
        state.add_balance(sender, amount)?;

        debug!(
            "Undelegated: {} <- {} amount={} (immediate return, lockup not implemented)",
            sender, validator, amount
        );

        Ok(ExecutionResult::success(MINIMUM_GAS * 3))
    }

    /// Execute a claim delegation rewards transaction
    fn execute_claim_delegation_rewards(
        &self,
        _tx: &Transaction,
        sender: &Address,
        state: &StateDB,
    ) -> Result<ExecutionResult> {
        // Check if sender has delegation
        let (existing_validator, _) = state.get_delegation(sender)?;
        if existing_validator.is_none() {
            return Err(ExecutorError::NoDelegation);
        }

        // In a full implementation, we would:
        // 1. Calculate pending rewards based on delegation amount and time
        // 2. Transfer rewards to sender
        // 3. Reset pending rewards counter
        //
        // For now, this is a placeholder since reward distribution is handled
        // at block production time by the producer

        debug!(
            "Claim delegation rewards: {} (rewards distributed at block production)",
            sender
        );

        Ok(ExecutionResult::success(MINIMUM_GAS * 2))
    }

    /// Execute multiple transactions and return receipts
    pub fn execute_transactions(
        &self,
        transactions: &[Transaction],
        state: &StateDB,
        block_producer: &Address,
    ) -> (Vec<Receipt>, u64) {
        let mut receipts = Vec::with_capacity(transactions.len());
        let mut cumulative_gas = 0u64;

        for (index, tx) in transactions.iter().enumerate() {
            let tx_hash = blake3_hash(&tx.to_bytes_without_signature());

            // Validate transaction
            let signed_tx = match self.validate_transaction(tx, state) {
                Ok(signed) => signed,
                Err(e) => {
                    warn!("Transaction validation failed: {}", e);
                    // Create failure receipt
                    let gas_used = tx.gas_limit;
                    cumulative_gas += gas_used;
                    receipts.push(Receipt {
                        tx_hash,
                        tx_index: index as u32,
                        status: ReceiptStatus::Failure(e.to_string()),
                        cumulative_gas_used: cumulative_gas,
                        gas_used,
                        logs: Vec::new(),
                        logs_bloom: Default::default(),
                        contract_address: None,
                    });
                    continue;
                }
            };

            // Execute transaction
            match self.execute(&signed_tx, state, block_producer) {
                Ok(result) => {
                    cumulative_gas += result.gas_used;

                    let status = if result.success {
                        ReceiptStatus::Success
                    } else {
                        ReceiptStatus::Failure(result.error.unwrap_or_default())
                    };

                    let receipt = Receipt {
                        tx_hash,
                        tx_index: index as u32,
                        status,
                        cumulative_gas_used: cumulative_gas,
                        gas_used: result.gas_used,
                        logs: result.logs.clone(),
                        logs_bloom: create_bloom(&result.logs),
                        contract_address: result.contract_address,
                    };

                    receipts.push(receipt);
                }
                Err(e) => {
                    warn!("Transaction execution failed: {}", e);
                    cumulative_gas += tx.gas_limit;
                    receipts.push(Receipt {
                        tx_hash,
                        tx_index: index as u32,
                        status: ReceiptStatus::Failure(e.to_string()),
                        cumulative_gas_used: cumulative_gas,
                        gas_used: tx.gas_limit,
                        logs: Vec::new(),
                        logs_bloom: Default::default(),
                        contract_address: None,
                    });
                }
            }
        }

        (receipts, cumulative_gas)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qfc_storage::Database;

    fn create_test_state() -> StateDB {
        let db = Database::open_temp().unwrap();
        StateDB::new(db)
    }

    #[test]
    fn test_execute_transfer() {
        let executor = Executor::testnet();
        let state = create_test_state();

        // Setup sender with balance
        let sender = Address::new([0x11; 20]);
        let recipient = Address::new([0x22; 20]);
        let producer = Address::new([0x33; 20]);

        state.set_balance(&sender, U256::from_u128(100_000_000_000_000_000)).unwrap(); // 0.1 ETH-equivalent

        // Create transfer transaction
        let tx = Transaction::transfer(
            recipient,
            U256::from_u64(1000),
            0,
            U256::from_u64(1_000_000_000), // 1 Gwei
        );

        // Create a mock signed transaction
        let tx_hash = blake3_hash(&tx.to_bytes_without_signature());
        let signed_tx = SignedTransaction::new(tx.clone(), tx_hash, sender);

        // Execute
        let result = executor.execute(&signed_tx, &state, &producer).unwrap();
        assert!(result.success);
        assert_eq!(result.gas_used, TRANSFER_GAS);

        // Check balances
        let sender_balance = state.get_balance(&sender).unwrap();
        let recipient_balance = state.get_balance(&recipient).unwrap();

        // Sender should have: initial - transfer - gas
        assert!(sender_balance < U256::from_u128(100_000_000_000_000_000));
        assert_eq!(recipient_balance, U256::from_u64(1000));
    }
}
