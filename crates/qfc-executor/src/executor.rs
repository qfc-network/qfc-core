//! Transaction executor

use crate::error::{ExecutorError, Result};
use qfc_crypto::{address_from_public_key, blake3_hash, contract_address, verify_hash_signature};
use qfc_state::StateDB;
use qfc_types::{
    create_bloom, Address, Hash, Log, Receipt, ReceiptStatus, SignedTransaction, Transaction,
    TransactionType, U256, DEFAULT_CHAIN_ID, MIN_VALIDATOR_STAKE, MINIMUM_GAS, TRANSFER_GAS,
};
use tracing::{debug, trace, warn};

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
}

impl Executor {
    /// Create a new executor
    pub fn new(chain_id: u64) -> Self {
        Self { chain_id }
    }

    /// Create an executor for the default testnet
    pub fn testnet() -> Self {
        Self::new(DEFAULT_CHAIN_ID)
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

        // 3. Compute transaction hash and recover sender
        let tx_hash = blake3_hash(&tx.to_bytes_without_signature());

        // For Ed25519, we need the public key to verify, so we encode sender address
        // In a real implementation, we'd recover the public key from the signature
        // For now, we'll use a simplified approach where the sender is derived
        // from the transaction's signature verification

        // TODO: Implement proper sender recovery from Ed25519 signature
        // For now, we'll trust the signature and compute sender from a public key
        // This is a placeholder - in production, we'd need to include the public key
        // in the transaction or use a different signature scheme

        // Placeholder: derive sender from first 20 bytes of signature hash
        let sender_hash = blake3_hash(tx.signature.as_bytes());
        let sender = Address::from_slice(&sender_hash.as_bytes()[12..32]).unwrap();

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
                self.execute_contract_create(&tx.tx, &sender, state)
            }
            TransactionType::ContractCall => self.execute_contract_call(&tx.tx, &sender, state),
            TransactionType::Stake => self.execute_stake(&tx.tx, &sender, state),
            TransactionType::Unstake => self.execute_unstake(&tx.tx, &sender, state),
            TransactionType::ValidatorRegister => {
                self.execute_validator_register(&tx.tx, &sender, state)
            }
            TransactionType::ValidatorExit => self.execute_validator_exit(&tx.tx, &sender, state),
        };

        // 4. Handle result
        match result {
            Ok(mut exec_result) => {
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
    ) -> Result<ExecutionResult> {
        // Calculate contract address
        let nonce = state.get_nonce(sender)?.saturating_sub(1); // Nonce was already incremented
        let contract_addr = contract_address(sender, nonce);

        // Transfer value to contract
        if !tx.value.is_zero() {
            state.transfer(sender, &contract_addr, tx.value)?;
        }

        // Store contract code
        // For now, we just store the init code as the runtime code
        // In a real implementation, we'd execute the init code
        let code = tx.data.clone();
        let gas_used = qfc_types::CONTRACT_CREATE_GAS + tx.data_gas();

        if !code.is_empty() {
            state.set_code(&contract_addr, code)?;
        }

        debug!(
            "Contract created at {} by {}",
            contract_addr, sender
        );

        Ok(ExecutionResult::success_with_contract(gas_used, contract_addr))
    }

    fn execute_contract_call(
        &self,
        tx: &Transaction,
        sender: &Address,
        state: &StateDB,
    ) -> Result<ExecutionResult> {
        let to = tx.to.ok_or(ExecutorError::MissingRecipient)?;

        // Transfer value
        if !tx.value.is_zero() {
            state.transfer(sender, &to, tx.value)?;
        }

        // TODO: Execute contract code (needs VM implementation)
        // For now, just consume gas for the call
        let gas_used = MINIMUM_GAS + tx.data_gas();

        debug!(
            "Contract call: {} -> {} data_len={}",
            sender,
            to,
            tx.data.len()
        );

        Ok(ExecutionResult::success(gas_used))
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
        tx: &Transaction,
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

                    let mut receipt = Receipt {
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
        let mut tx = Transaction::transfer(
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
