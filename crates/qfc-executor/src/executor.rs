//! Transaction executor

use crate::error::{ExecutorError, Result};
use crate::evm::EvmExecutor;
use qfc_crypto::{address_from_public_key, blake3_hash};
use qfc_state::StateDB;
use qfc_types::{
    create_bloom, max_validator_stake, Address, Log, Receipt, ReceiptStatus, SignedTransaction,
    Transaction, TransactionType, DEFAULT_CHAIN_ID, MAX_VALIDATOR_STAKE_PERCENT, MINIMUM_GAS,
    MIN_DELEGATION, MIN_VALIDATOR_STAKE, TRANSFER_GAS, U256, UNSTAKE_DELAY_SECS,
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
    /// Return data from EVM execution
    pub output: Vec<u8>,
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
            output: Vec::new(),
            error: None,
        }
    }

    pub fn success_with_contract(gas_used: u64, contract_address: Address) -> Self {
        Self {
            success: true,
            gas_used,
            logs: Vec::new(),
            contract_address: Some(contract_address),
            output: Vec::new(),
            error: None,
        }
    }

    pub fn failure(gas_used: u64, error: String) -> Self {
        Self {
            success: false,
            gas_used,
            logs: Vec::new(),
            contract_address: None,
            output: Vec::new(),
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
    /// Hash of the executing block's parent (set during execution).
    /// Drives PREVRANDAO and anchors the BLOCKHASH ancestor walk (ADR-0017).
    parent_hash: qfc_types::Hash,
    /// BLOCKHASH resolver walking the executing block's own ancestor chain
    /// (see [`crate::BlockHashLookup`]). Installed by the chain layer.
    block_hash_lookup: Option<crate::BlockHashLookup>,
    /// ADR-0017 hardfork gate. Production always uses
    /// [`qfc_types::EVM_OPCODE_ACTIVATION_HEIGHT`]; overridable ONLY in tests.
    evm_opcode_activation_height: u64,
}

impl Executor {
    /// Create a new executor
    pub fn new(chain_id: u64) -> Self {
        Self {
            chain_id,
            block_number: 0,
            block_timestamp: 0,
            block_gas_limit: qfc_types::DEFAULT_BLOCK_GAS_LIMIT,
            parent_hash: qfc_types::Hash::ZERO,
            block_hash_lookup: None,
            evm_opcode_activation_height: qfc_types::EVM_OPCODE_ACTIVATION_HEIGHT,
        }
    }

    /// Create an executor for the default testnet
    pub fn testnet() -> Self {
        Self::new(DEFAULT_CHAIN_ID)
    }

    fn ensure_validator_stake_cap(
        &self,
        state: &StateDB,
        validator: &Address,
        added_stake: U256,
    ) -> Result<()> {
        if added_stake.is_zero() {
            return Ok(());
        }

        let current_total_stake = state.get_total_stake()?;
        if current_total_stake.is_zero() {
            return Ok(());
        }

        // Genesis validators bypass transaction execution; this cap only governs
        // post-genesis stake/delegation changes.
        let validator_total_after = state
            .get_stake(validator)?
            .saturating_add(state.get_total_delegated_to(validator)?)
            .saturating_add(added_stake);
        let network_total_after = current_total_stake.saturating_add(added_stake);
        let max_allowed = max_validator_stake(network_total_after);

        if validator_total_after > max_allowed {
            return Err(ExecutorError::ValidatorStakeTooHigh {
                max_percent: MAX_VALIDATOR_STAKE_PERCENT,
                max_allowed: max_allowed.to_string(),
                attempted: validator_total_after.to_string(),
            });
        }

        Ok(())
    }

    /// Set block context for EVM execution.
    ///
    /// `parent_hash` is the hash of the EXECUTING block's parent — required
    /// post-activation (ADR-0017) for PREVRANDAO and as the BLOCKHASH
    /// ancestor-walk anchor.
    pub fn set_block_context(
        &mut self,
        block_number: u64,
        block_timestamp: u64,
        gas_limit: u64,
        parent_hash: qfc_types::Hash,
    ) {
        self.block_number = block_number;
        self.block_timestamp = block_timestamp;
        self.block_gas_limit = gas_limit;
        self.parent_hash = parent_hash;
    }

    /// Install the BLOCKHASH resolver (see [`crate::BlockHashLookup`]).
    /// Consensus paths MUST install one — without it, post-activation
    /// BLOCKHASH silently reads zero.
    pub fn set_block_hash_lookup(&mut self, lookup: crate::BlockHashLookup) {
        self.block_hash_lookup = Some(lookup);
    }

    /// Test-only override of the ADR-0017 activation height. Production
    /// code must NEVER call this — a per-node activation height is a silent
    /// consensus fork.
    #[doc(hidden)]
    pub fn set_evm_opcode_activation_height(&mut self, height: u64) {
        self.evm_opcode_activation_height = height;
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
                if current_stake.is_zero() && stake < U256::from_u128(MIN_VALIDATOR_STAKE) {
                    return Err(ExecutorError::StakeTooLow {
                        minimum: U256::from_u128(MIN_VALIDATOR_STAKE).to_string(),
                        provided: stake.to_string(),
                    });
                }
            }
            TransactionType::ValidatorRegister => {
                let stake = tx.value;
                if stake < U256::from_u128(MIN_VALIDATOR_STAKE) {
                    return Err(ExecutorError::StakeTooLow {
                        minimum: U256::from_u128(MIN_VALIDATOR_STAKE).to_string(),
                        provided: stake.to_string(),
                    });
                }
            }
            TransactionType::Delegate => {
                let validator = tx.to.ok_or(ExecutorError::MissingRecipient)?;
                let amount = tx.value;
                if amount < U256::from_u128(MIN_DELEGATION) {
                    return Err(ExecutorError::DelegationTooLow {
                        minimum: U256::from_u128(MIN_DELEGATION).to_string(),
                        provided: amount.to_string(),
                    });
                }

                let (existing_validator, _) = state.get_delegation(&sender)?;
                if let Some(existing) = existing_validator {
                    if existing != validator {
                        return Err(ExecutorError::AlreadyDelegated);
                    }
                }

                if state.get_stake(&validator)?.is_zero() {
                    return Err(ExecutorError::InvalidValidator);
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

        // 2. Nonce advance.
        //
        // Non-EVM tx types never run revm, so we advance the sender nonce here
        // (as they always have).
        //
        // EVM tx types (ContractCreate / ContractCall) must NOT be
        // pre-incremented: revm reads the caller's CURRENT (pre-increment)
        // nonce to derive the CREATE address — the Ethereum standard
        // `f(sender, tx.nonce)` — and bumps the caller nonce itself. Bumping
        // here first shifted every CREATE address to `f(sender, tx.nonce + 1)`
        // (off-by-one vs. every standard EVM tool) AND double-counted the
        // nonce (+2 per EVM tx). The nonce for EVM tx types is finalized to
        // exactly `tx.nonce + 1` after execution (see step 4), which is
        // correct whether revm persisted its own bump (success) or not
        // (revert/halt, or a ContractCall to an account with no code, neither
        // of which runs a state-applying revm pass).
        let evm_tx = matches!(
            tx.tx.tx_type,
            TransactionType::ContractCreate | TransactionType::ContractCall
        );
        if !evm_tx {
            state.increment_nonce(&sender)?;
        }

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
            TransactionType::InferenceTask => {
                // Inference tasks are recorded on-chain for auditability.
                // Fee escrow is handled by the RPC layer (submitPublicTask).
                // The tx.data contains the serialized task params.
                Ok(ExecutionResult {
                    success: true,
                    gas_used: 21_000, // base gas for inference task record
                    logs: Vec::new(),
                    contract_address: None,
                    output: Vec::new(),
                    error: None,
                })
            }
        };

        // 4. Handle result
        match result {
            Ok(exec_result) => {
                // Finalize the sender nonce for EVM tx types to exactly
                // tx.nonce + 1. On success revm already wrote this via
                // apply_state_changes (no-op here); on a reverted/halted EVM
                // tx, or a ContractCall to a code-less account (which does a
                // bare value transfer without invoking revm), revm never
                // persisted a bump, so this is the single authoritative +1.
                // Only the sender EOA is touched — contract/factory nonces
                // bumped by revm during execution are left intact.
                if evm_tx {
                    state.set_nonce(&sender, tx.tx.nonce + 1)?;
                }

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

        trace!("Transfer: {} -> {} value={}", sender, to, tx.value);

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
            self.parent_hash,
            self.block_hash_lookup.clone(),
        )
        .with_activation_height(self.evm_opcode_activation_height);

        let result = evm_executor.create(sender, tx.data.clone(), tx.value, tx.gas_limit)?;

        if result.success {
            let gas_used = result.gas_used.max(qfc_types::CONTRACT_CREATE_GAS);
            if let Some(contract_addr) = result.contract_address {
                debug!("Contract created at {} by {}", contract_addr, sender);
                let mut exec_result =
                    ExecutionResult::success_with_contract(gas_used, contract_addr);
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
            self.parent_hash,
            self.block_hash_lookup.clone(),
        )
        .with_activation_height(self.evm_opcode_activation_height);

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
            exec_result.output = result.output;
            Ok(exec_result)
        } else {
            let mut exec_result = ExecutionResult::failure(
                result.gas_used,
                result
                    .error
                    .unwrap_or_else(|| "Execution failed".to_string()),
            );
            exec_result.output = result.output;
            Ok(exec_result)
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
        if current_stake.is_zero() && new_stake < U256::from_u128(MIN_VALIDATOR_STAKE) {
            return Err(ExecutorError::StakeTooLow {
                minimum: U256::from_u128(MIN_VALIDATOR_STAKE).to_string(),
                provided: new_stake.to_string(),
            });
        }
        self.ensure_validator_stake_cap(state, sender, stake_amount)?;

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
        self.ensure_validator_stake_cap(state, sender, stake_amount)?;

        // Lock stake
        state.sub_balance(sender, stake_amount)?;
        state.set_stake(sender, stake_amount)?;
        state.set_contribution_score(sender, 0)?;

        debug!("Validator registered: {} stake={}", sender, stake_amount);

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
        self.ensure_validator_stake_cap(state, &validator, amount)?;

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

        debug!("Delegated: {} -> {} amount={}", sender, validator, amount);

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

        // Calculate unlock time from the BLOCK timestamp (set via
        // `set_block_context` by `Chain::execute_at`), never the local wall
        // clock: the producer and every importer must derive the exact same
        // `unlock_at` or their state roots diverge (consensus fork D12).
        let unlock_at = self.block_timestamp / 1000 + UNSTAKE_DELAY_SECS;

        // Store undelegation record — funds are locked until unlock_at
        let undelegation = qfc_types::Undelegation::new(*sender, validator, amount, unlock_at);
        state.store_undelegation(&undelegation)?;

        debug!(
            "Undelegated: {} <- {} amount={} unlock_at={}",
            sender, validator, amount, unlock_at
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

    /// Process mature undelegations: return locked funds to delegators.
    /// Called during block execution before processing transactions.
    ///
    /// `now_secs` is the maturity clock and MUST be derived from the block
    /// timestamp (`block.timestamp() / 1000`), never from `SystemTime::now()`:
    /// the producer and every importer must see the exact same set of mature
    /// undelegations or their state roots diverge (consensus fork D12).
    pub fn process_mature_undelegations(&self, state: &StateDB, now_secs: u64) -> u32 {
        let mature = match state.get_mature_undelegations(now_secs) {
            Ok(m) => m,
            Err(e) => {
                warn!("Failed to get mature undelegations: {}", e);
                return 0;
            }
        };

        let mut processed = 0u32;
        for u in &mature {
            if let Err(e) = state.add_balance(&u.delegator, u.amount) {
                warn!("Failed to return undelegation to {}: {}", u.delegator, e);
                continue;
            }
            if let Err(e) = state.delete_undelegation(&u.delegator, &u.validator, u.unlock_at) {
                warn!("Failed to delete undelegation record: {}", e);
                continue;
            }
            debug!(
                "Undelegation matured: {} received {} from validator {}",
                u.delegator, u.amount, u.validator
            );
            processed += 1;
        }

        if processed > 0 {
            debug!("Processed {} mature undelegations", processed);
        }
        processed
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

        state
            .set_balance(&sender, U256::from_u128(100_000_000_000_000_000))
            .unwrap(); // 0.1 ETH-equivalent

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

    // Minimal init code that deploys a valid, callable runtime:
    //   init: CODECOPY the 11-byte runtime to memory and RETURN it
    //   runtime: SLOAD slot0; MSTORE; RETURN 32 bytes (no INVALID opcodes)
    const TEST_INIT_CODE: &str = "600b80600b6000396000f360005460005260206000f3";

    fn fund(state: &StateDB, addr: &Address) {
        state
            .set_balance(addr, U256::from_u128(10_000_000_000_000_000_000))
            .unwrap();
    }

    /// Ethereum CREATE address `f(sender, nonce)` = keccak256(rlp([sender, nonce]))[12..].
    /// Computed via revm's own helper so the expectation matches the EVM
    /// authoritatively.
    fn eth_create_address(sender: &Address, nonce: u64) -> Address {
        use revm::primitives::Address as RevmAddress;
        let created = RevmAddress::from_slice(sender.as_bytes()).create(nonce);
        Address::from_slice(created.as_slice()).unwrap()
    }

    fn sign(tx: Transaction, sender: Address) -> SignedTransaction {
        let h = blake3_hash(&tx.to_bytes_without_signature());
        SignedTransaction::new(tx, h, sender)
    }

    #[test]
    fn test_create_address_uses_pre_increment_nonce() {
        let executor = Executor::testnet();
        let state = create_test_state();
        let sender = Address::new([0xAB; 20]);
        let producer = Address::new([0x33; 20]);
        fund(&state, &sender);
        state.set_nonce(&sender, 7).unwrap();

        let tx = Transaction::contract_create(
            hex::decode(TEST_INIT_CODE).unwrap(),
            U256::ZERO,
            7,
            1_000_000,
            U256::from_u64(1_000_000_000),
        );
        let result = executor
            .execute(&sign(tx, sender), &state, &producer)
            .unwrap();

        assert!(result.success, "create failed: {:?}", result.error);
        // Invariant 1: address == f(sender, tx.nonce), NOT tx.nonce + 1.
        assert_eq!(
            result.contract_address.unwrap(),
            eth_create_address(&sender, 7),
            "CREATE address must use the pre-increment nonce (Ethereum standard)"
        );
        assert_ne!(
            result.contract_address.unwrap(),
            eth_create_address(&sender, 8),
            "CREATE address must not use the post-increment nonce"
        );
        // Invariant 2: nonce advances by exactly 1.
        assert_eq!(state.get_nonce(&sender).unwrap(), 8);
    }

    #[test]
    fn test_sequential_creates_follow_ethereum_sequence() {
        let executor = Executor::testnet();
        let state = create_test_state();
        let sender = Address::new([0xCD; 20]);
        let producer = Address::new([0x33; 20]);
        fund(&state, &sender);
        state.set_nonce(&sender, 0).unwrap();

        for n in 0..3u64 {
            let tx = Transaction::contract_create(
                hex::decode(TEST_INIT_CODE).unwrap(),
                U256::ZERO,
                n,
                1_000_000,
                U256::from_u64(1_000_000_000),
            );
            let result = executor
                .execute(&sign(tx, sender), &state, &producer)
                .unwrap();
            assert!(result.success);
            // Invariant 3: each deploy lands at f(sender, n), no gaps/overlaps.
            assert_eq!(
                result.contract_address.unwrap(),
                eth_create_address(&sender, n),
                "deploy #{n} landed at the wrong address"
            );
            assert_eq!(state.get_nonce(&sender).unwrap(), n + 1);
        }
    }

    /// BUG B regression: a contract deploy must charge the sender EXACTLY
    /// `gas_used * gas_price` (1x), and the producer must receive EXACTLY
    /// `gas_used * gas_price`. Before the fix revm ALSO deducted gas from the
    /// caller (on top of the executor's prepay), draining ~2x. Now revm runs
    /// gas-neutral (tx gas_price = 0, basefee = 0) so the executor is the sole
    /// gas accountant.
    #[test]
    fn test_contract_create_charges_gas_once() {
        let executor = Executor::testnet();
        let state = create_test_state();
        let sender = Address::new([0x11; 20]);
        let producer = Address::new([0x33; 20]);
        fund(&state, &sender);
        state.set_nonce(&sender, 0).unwrap();

        let gas_price = 1_000_000_000u64; // 1 Gwei
        let sender_before = state.get_balance(&sender).unwrap();
        let producer_before = state.get_balance(&producer).unwrap();

        let tx = Transaction::contract_create(
            hex::decode(TEST_INIT_CODE).unwrap(),
            U256::ZERO, // no value transfer — isolate gas accounting
            0,
            1_000_000,
            U256::from_u64(gas_price),
        );
        let result = executor
            .execute(&sign(tx, sender), &state, &producer)
            .unwrap();
        assert!(result.success, "create failed: {:?}", result.error);

        let expected_charge = U256::from_u64(result.gas_used * gas_price);

        let sender_after = state.get_balance(&sender).unwrap();
        let producer_after = state.get_balance(&producer).unwrap();

        // Sender drops by EXACTLY 1x gas_used*gas_price (not 2x).
        assert_eq!(
            sender_before - sender_after,
            expected_charge,
            "sender must be charged exactly gas_used*gas_price (1x), not double"
        );
        // Producer receives EXACTLY gas_used*gas_price (gas payout preserved).
        assert_eq!(
            producer_after - producer_before,
            expected_charge,
            "producer must receive exactly gas_used*gas_price"
        );
    }

    /// BUG B second-order effect: an account funded to only the Ethereum
    /// requirement (gas + value) must NOT fail inside revm with
    /// insufficient-funds. With the executor already prepaying gas and revm
    /// running gas-neutral, revm's balance check requires only `value`, so a
    /// deploy succeeds without needing 2x the gas balance.
    #[test]
    fn test_contract_create_does_not_need_double_gas_balance() {
        let executor = Executor::testnet();
        let state = create_test_state();
        let sender = Address::new([0x44; 20]);
        let producer = Address::new([0x33; 20]);
        state.set_nonce(&sender, 0).unwrap();

        let gas_price = 1_000_000_000u64;
        let gas_limit = 1_000_000u64;
        // Fund the sender to EXACTLY the gas prepayment (Ethereum requirement),
        // no slack. Pre-fix this failed inside revm (double deduction).
        state
            .set_balance(&sender, U256::from_u64(gas_limit * gas_price))
            .unwrap();

        let tx = Transaction::contract_create(
            hex::decode(TEST_INIT_CODE).unwrap(),
            U256::ZERO,
            0,
            gas_limit,
            U256::from_u64(gas_price),
        );
        let result = executor
            .execute(&sign(tx, sender), &state, &producer)
            .unwrap();
        assert!(
            result.success,
            "deploy funded to exactly gas requirement must succeed: {:?}",
            result.error
        );
    }

    #[test]
    fn test_transfer_advances_nonce_by_one() {
        let executor = Executor::testnet();
        let state = create_test_state();
        let sender = Address::new([0x11; 20]);
        let producer = Address::new([0x33; 20]);
        fund(&state, &sender);
        state.set_nonce(&sender, 4).unwrap();

        let tx = Transaction::transfer(
            Address::new([0x22; 20]),
            U256::from_u64(1000),
            4,
            U256::from_u64(1_000_000_000),
        );
        let result = executor
            .execute(&sign(tx, sender), &state, &producer)
            .unwrap();
        assert!(result.success);
        assert_eq!(state.get_nonce(&sender).unwrap(), 5);
    }

    #[test]
    fn test_stake_advances_nonce_by_one() {
        let executor = Executor::testnet();
        let state = create_test_state();
        let sender = Address::new([0x44; 20]);
        let producer = Address::new([0x33; 20]);
        state
            .set_balance(&sender, U256::from_u128(MIN_VALIDATOR_STAKE * 2))
            .unwrap();
        state.set_nonce(&sender, 9).unwrap();

        let tx = Transaction::stake(
            U256::from_u128(MIN_VALIDATOR_STAKE),
            9,
            U256::from_u64(1_000_000_000),
        );
        let result = executor
            .execute(&sign(tx, sender), &state, &producer)
            .unwrap();
        assert!(result.success);
        assert_eq!(state.get_nonce(&sender).unwrap(), 10);
    }

    #[test]
    fn test_contract_call_to_deployed_contract_advances_nonce_by_one() {
        let executor = Executor::testnet();
        let state = create_test_state();
        let sender = Address::new([0xEE; 20]);
        let producer = Address::new([0x33; 20]);
        fund(&state, &sender);
        state.set_nonce(&sender, 0).unwrap();

        // Deploy first (nonce 0), then call it (nonce 1).
        let deploy = Transaction::contract_create(
            hex::decode(TEST_INIT_CODE).unwrap(),
            U256::ZERO,
            0,
            1_000_000,
            U256::from_u64(1_000_000_000),
        );
        let dr = executor
            .execute(&sign(deploy, sender), &state, &producer)
            .unwrap();
        assert!(dr.success);
        let contract = dr.contract_address.unwrap();
        assert_eq!(state.get_nonce(&sender).unwrap(), 1);

        let call = Transaction::contract_call(
            contract,
            Vec::new(),
            U256::ZERO,
            1,
            1_000_000,
            U256::from_u64(1_000_000_000),
        );
        let cr = executor
            .execute(&sign(call, sender), &state, &producer)
            .unwrap();
        assert!(cr.success, "call failed: {:?}", cr.error);
        // Invariant 4: call still works and nonce advances by exactly 1.
        assert_eq!(state.get_nonce(&sender).unwrap(), 2);
    }

    #[test]
    fn test_contract_call_to_eoa_advances_nonce_by_one() {
        // A ContractCall whose target has no code does a bare value transfer
        // and never invokes revm — the sender nonce must still advance by 1.
        let executor = Executor::testnet();
        let state = create_test_state();
        let sender = Address::new([0x55; 20]);
        let eoa = Address::new([0x66; 20]);
        let producer = Address::new([0x33; 20]);
        fund(&state, &sender);
        state.set_nonce(&sender, 2).unwrap();

        let tx = Transaction::contract_call(
            eoa,
            Vec::new(),
            U256::from_u64(500),
            2,
            100_000,
            U256::from_u64(1_000_000_000),
        );
        let result = executor
            .execute(&sign(tx, sender), &state, &producer)
            .unwrap();
        assert!(result.success);
        assert_eq!(state.get_nonce(&sender).unwrap(), 3);
        assert_eq!(state.get_balance(&eoa).unwrap(), U256::from_u64(500));
    }

    #[test]
    fn test_stake_cap_rejects_excessive_stake_increase() {
        let executor = Executor::testnet();
        let state = create_test_state();
        let validator = Address::new([0x11; 20]);
        let other_validator = Address::new([0x22; 20]);

        state.set_stake(&validator, U256::from_u64(2_000)).unwrap();
        state
            .set_stake(&other_validator, U256::from_u64(8_000))
            .unwrap();
        state.set_balance(&validator, U256::from_u64(100)).unwrap();

        let tx = Transaction::stake(U256::from_u64(100), 0, U256::ZERO);
        let err = executor.execute_stake(&tx, &validator, &state).unwrap_err();

        assert!(matches!(err, ExecutorError::ValidatorStakeTooHigh { .. }));
        assert_eq!(state.get_stake(&validator).unwrap(), U256::from_u64(2_000));
    }

    #[test]
    fn test_stake_cap_rejects_excessive_delegation() {
        let executor = Executor::testnet();
        let state = create_test_state();
        let validator = Address::new([0x11; 20]);
        let other_validator = Address::new([0x22; 20]);
        let delegator = Address::new([0x33; 20]);
        let qfc = qfc_types::ONE_QFC;

        state
            .set_stake(&validator, U256::from_u128(1_950 * qfc))
            .unwrap();
        state
            .set_stake(&other_validator, U256::from_u128(8_050 * qfc))
            .unwrap();
        state
            .set_balance(&delegator, U256::from_u128(qfc_types::MIN_DELEGATION))
            .unwrap();

        let tx = Transaction::delegate(
            validator,
            U256::from_u128(qfc_types::MIN_DELEGATION),
            0,
            U256::ZERO,
        );
        let err = executor
            .execute_delegate(&tx, &delegator, &state)
            .unwrap_err();

        assert!(matches!(err, ExecutorError::ValidatorStakeTooHigh { .. }));
        assert_eq!(
            state.get_total_delegated_to(&validator).unwrap(),
            U256::ZERO
        );
    }
}
