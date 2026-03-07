//! EVM execution wrapper using revm

use crate::error::{ExecutorError, Result};
use qfc_state::StateDB;
use qfc_types::{Address, Hash, Log, U256};
use revm::{
    db::CacheDB,
    primitives::{
        AccountInfo, Address as RevmAddress, Bytecode, Bytes, CreateScheme,
        ExecutionResult as RevmResult, Output, TransactTo, B256, U256 as RevmU256,
    },
    Database as _, Evm,
};
use std::collections::HashMap;

/// Result of EVM execution
#[derive(Clone, Debug)]
pub struct EvmResult {
    /// Whether execution succeeded
    pub success: bool,
    /// Gas used
    pub gas_used: u64,
    /// Output data
    pub output: Vec<u8>,
    /// Created contract address (for CREATE)
    pub contract_address: Option<Address>,
    /// Logs emitted
    pub logs: Vec<Log>,
    /// Error message if failed
    pub error: Option<String>,
}

/// Wrapper around StateDB that implements revm's DatabaseRef trait.
/// This allows revm to read account info and storage on-demand from our state.
struct StateDBRef<'a> {
    state: &'a StateDB,
}

impl<'a> revm::DatabaseRef for StateDBRef<'a> {
    type Error = String;

    fn basic_ref(
        &self,
        address: RevmAddress,
    ) -> std::result::Result<Option<AccountInfo>, Self::Error> {
        let addr = revm_to_address(&address);
        let balance = self.state.get_balance(&addr).map_err(|e| e.to_string())?;
        let nonce = self.state.get_nonce(&addr).map_err(|e| e.to_string())?;
        let code = self.state.get_code(&addr).map_err(|e| e.to_string())?;

        if code.is_empty() {
            // EOA: use KECCAK_EMPTY as code_hash (required by EIP-3607)
            Ok(Some(AccountInfo {
                balance: u256_to_revm(balance),
                nonce,
                code_hash: revm::primitives::KECCAK_EMPTY,
                code: None,
            }))
        } else {
            let hash = qfc_crypto::blake3_hash(&code);
            Ok(Some(AccountInfo {
                balance: u256_to_revm(balance),
                nonce,
                code_hash: B256::from_slice(hash.as_bytes()),
                code: Some(Bytecode::new_raw(Bytes::from(code))),
            }))
        }
    }

    fn code_by_hash_ref(&self, _code_hash: B256) -> std::result::Result<Bytecode, Self::Error> {
        Ok(Bytecode::default())
    }

    fn storage_ref(
        &self,
        address: RevmAddress,
        index: RevmU256,
    ) -> std::result::Result<RevmU256, Self::Error> {
        let addr = revm_to_address(&address);
        let slot = revm_to_u256(index);
        let value = self
            .state
            .get_storage(&addr, &slot)
            .map_err(|e| e.to_string())?;
        Ok(u256_to_revm(value))
    }

    fn block_hash_ref(&self, _number: RevmU256) -> std::result::Result<B256, Self::Error> {
        Ok(B256::ZERO)
    }
}

/// EVM wrapper for executing smart contracts
pub struct EvmExecutor<'a> {
    state: &'a StateDB,
    chain_id: u64,
    block_number: u64,
    block_timestamp: u64,
    block_coinbase: Address,
    block_gas_limit: u64,
}

impl<'a> EvmExecutor<'a> {
    /// Create a new EVM executor
    pub fn new(
        state: &'a StateDB,
        chain_id: u64,
        block_number: u64,
        block_timestamp: u64,
        block_coinbase: Address,
        block_gas_limit: u64,
    ) -> Self {
        Self {
            state,
            chain_id,
            block_number,
            block_timestamp,
            block_coinbase,
            block_gas_limit,
        }
    }

    /// Execute a contract creation
    pub fn create(
        &self,
        sender: &Address,
        init_code: Vec<u8>,
        value: U256,
        gas_limit: u64,
    ) -> Result<EvmResult> {
        let mut db = self.create_state_db()?;
        let mut evm = self.create_evm(&mut db);

        // Configure transaction
        let gas_price = RevmU256::from(1_000_000_000u64); // 1 Gwei (matches basefee)
        evm.tx_mut().caller = address_to_revm(sender);
        evm.tx_mut().transact_to = TransactTo::Create(CreateScheme::Create);
        evm.tx_mut().data = Bytes::from(init_code);
        evm.tx_mut().value = u256_to_revm(value);
        evm.tx_mut().gas_limit = gas_limit;
        evm.tx_mut().gas_price = gas_price;

        // Execute
        let result = evm
            .transact()
            .map_err(|e| ExecutorError::EvmError(e.to_string()))?;
        let execution_result = result.result;

        // Process result
        self.process_result(execution_result, &result.state)
    }

    /// Execute a contract call
    pub fn call(
        &self,
        sender: &Address,
        to: &Address,
        input: Vec<u8>,
        value: U256,
        gas_limit: u64,
    ) -> Result<EvmResult> {
        let mut db = self.create_state_db()?;
        let mut evm = self.create_evm(&mut db);

        // Configure transaction
        let gas_price = RevmU256::from(1_000_000_000u64); // 1 Gwei (matches basefee)
        evm.tx_mut().caller = address_to_revm(sender);
        evm.tx_mut().transact_to = TransactTo::Call(address_to_revm(to));
        evm.tx_mut().data = Bytes::from(input);
        evm.tx_mut().value = u256_to_revm(value);
        evm.tx_mut().gas_limit = gas_limit;
        evm.tx_mut().gas_price = gas_price;

        // Execute
        let result = evm
            .transact()
            .map_err(|e| ExecutorError::EvmError(e.to_string()))?;
        let execution_result = result.result;

        // Process result
        self.process_result(execution_result, &result.state)
    }

    /// Execute a static call (view function, no state changes)
    pub fn static_call(
        &self,
        sender: Option<&Address>,
        to: &Address,
        input: Vec<u8>,
        gas_limit: u64,
    ) -> Result<EvmResult> {
        let mut db = self.create_state_db()?;

        let caller = sender.unwrap_or(&Address::ZERO);

        // For static calls, give the caller enough balance to cover gas
        // so view functions work without requiring funded accounts
        let gas_balance = RevmU256::from(gas_limit) * RevmU256::from(1_000_000_000u64);
        let caller_revm = address_to_revm(caller);
        // Pre-load the caller account into cache so we can modify the balance
        let _ = db.basic(caller_revm);
        if let Some(account) = db.accounts.get_mut(&caller_revm) {
            if account.info.balance < gas_balance {
                account.info.balance = gas_balance;
            }
        }

        let mut evm = self.create_evm(&mut db);

        // Configure as static call
        let gas_price = RevmU256::from(1_000_000_000u64); // 1 Gwei (matches basefee)
        evm.tx_mut().caller = address_to_revm(caller);
        evm.tx_mut().transact_to = TransactTo::Call(address_to_revm(to));
        evm.tx_mut().data = Bytes::from(input);
        evm.tx_mut().value = RevmU256::ZERO;
        evm.tx_mut().gas_limit = gas_limit;
        evm.tx_mut().gas_price = gas_price;

        // Execute (static call doesn't modify state)
        let result = evm
            .transact()
            .map_err(|e| ExecutorError::EvmError(e.to_string()))?;

        // For static calls, we don't apply state changes
        self.process_result_no_state(result.result)
    }

    /// Create a revm database backed by our state
    fn create_state_db(&self) -> Result<CacheDB<StateDBRef<'a>>> {
        Ok(CacheDB::new(StateDBRef { state: self.state }))
    }

    /// Create a configured EVM instance
    fn create_evm<'b>(
        &self,
        db: &'b mut CacheDB<StateDBRef<'a>>,
    ) -> Evm<'b, (), &'b mut CacheDB<StateDBRef<'a>>> {
        let mut evm = Evm::builder().with_db(db).build();

        // Configure block environment
        evm.block_mut().number = RevmU256::from(self.block_number);
        evm.block_mut().timestamp = RevmU256::from(self.block_timestamp);
        evm.block_mut().coinbase = address_to_revm(&self.block_coinbase);
        evm.block_mut().gas_limit = RevmU256::from(self.block_gas_limit);
        evm.block_mut().basefee = RevmU256::from(1_000_000_000u64); // 1 Gwei

        // Configure chain
        evm.cfg_mut().chain_id = self.chain_id;

        evm
    }

    /// Process EVM execution result
    fn process_result(
        &self,
        result: RevmResult,
        state_changes: &HashMap<RevmAddress, revm::primitives::Account>,
    ) -> Result<EvmResult> {
        match result {
            RevmResult::Success {
                reason: _,
                gas_used,
                gas_refunded: _,
                logs,
                output,
            } => {
                // Apply state changes
                self.apply_state_changes(state_changes)?;

                let (output_data, contract_address) = match output {
                    Output::Create(bytes, addr) => {
                        let contract_addr = addr.map(|a| revm_to_address(&a));
                        (bytes.to_vec(), contract_addr)
                    }
                    Output::Call(bytes) => (bytes.to_vec(), None),
                };

                Ok(EvmResult {
                    success: true,
                    gas_used,
                    output: output_data,
                    contract_address,
                    logs: logs.iter().map(revm_log_to_log).collect(),
                    error: None,
                })
            }
            RevmResult::Revert { gas_used, output } => Ok(EvmResult {
                success: false,
                gas_used,
                output: output.to_vec(),
                contract_address: None,
                logs: Vec::new(),
                error: Some("Execution reverted".to_string()),
            }),
            RevmResult::Halt { reason, gas_used } => Ok(EvmResult {
                success: false,
                gas_used,
                output: Vec::new(),
                contract_address: None,
                logs: Vec::new(),
                error: Some(format!("Execution halted: {:?}", reason)),
            }),
        }
    }

    /// Process result without applying state changes (for static calls)
    fn process_result_no_state(&self, result: RevmResult) -> Result<EvmResult> {
        match result {
            RevmResult::Success {
                reason: _,
                gas_used,
                gas_refunded: _,
                logs,
                output,
            } => {
                let output_data = match output {
                    Output::Create(bytes, _) => bytes.to_vec(),
                    Output::Call(bytes) => bytes.to_vec(),
                };

                Ok(EvmResult {
                    success: true,
                    gas_used,
                    output: output_data,
                    contract_address: None,
                    logs: logs.iter().map(revm_log_to_log).collect(),
                    error: None,
                })
            }
            RevmResult::Revert { gas_used, output } => Ok(EvmResult {
                success: false,
                gas_used,
                output: output.to_vec(),
                contract_address: None,
                logs: Vec::new(),
                error: Some("Execution reverted".to_string()),
            }),
            RevmResult::Halt { reason, gas_used } => Ok(EvmResult {
                success: false,
                gas_used,
                output: Vec::new(),
                contract_address: None,
                logs: Vec::new(),
                error: Some(format!("Execution halted: {:?}", reason)),
            }),
        }
    }

    /// Apply state changes from EVM execution to our state
    fn apply_state_changes(
        &self,
        state_changes: &HashMap<RevmAddress, revm::primitives::Account>,
    ) -> Result<()> {
        for (revm_addr, account) in state_changes {
            let address = revm_to_address(revm_addr);

            // Skip if account wasn't touched
            if !account.is_touched() {
                continue;
            }

            // Update balance
            let new_balance = revm_to_u256(account.info.balance);
            self.state.set_balance(&address, new_balance)?;

            // Update nonce
            self.state.set_nonce(&address, account.info.nonce)?;

            // Update code if it changed
            if let Some(ref code) = account.info.code {
                if !code.is_empty() {
                    self.state.set_code(&address, code.bytes().to_vec())?;
                }
            }

            // Update storage
            for (slot, value) in &account.storage {
                let slot_u256 = revm_to_u256(*slot);
                let value_u256 = revm_to_u256(value.present_value);
                self.state.set_storage(&address, slot_u256, value_u256)?;
            }
        }

        Ok(())
    }
}

// Conversion helpers

fn address_to_revm(addr: &Address) -> RevmAddress {
    RevmAddress::from_slice(addr.as_bytes())
}

fn revm_to_address(addr: &RevmAddress) -> Address {
    Address::from_slice(addr.as_slice()).unwrap()
}

fn u256_to_revm(val: U256) -> RevmU256 {
    RevmU256::from_be_bytes(val.to_be_bytes())
}

fn revm_to_u256(val: RevmU256) -> U256 {
    U256::from_be_bytes(&val.to_be_bytes())
}

fn revm_log_to_log(log: &revm::primitives::Log) -> Log {
    Log {
        address: revm_to_address(&log.address),
        topics: log
            .data
            .topics()
            .iter()
            .map(|t| Hash::from_slice(t.as_slice()).unwrap())
            .collect(),
        data: log.data.data.to_vec(),
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
    fn test_evm_executor_creation() {
        let state = create_test_state();
        let executor = EvmExecutor::new(&state, 9000, 1, 1234567890, Address::ZERO, 30_000_000);

        assert_eq!(executor.chain_id, 9000);
    }

    #[test]
    fn test_simple_contract_call() {
        let state = create_test_state();

        // Setup sender and recipient
        let sender = Address::new([0x11; 20]);
        let recipient = Address::new([0x22; 20]);
        state
            .set_balance(&sender, U256::from_u128(1_000_000_000_000_000_000))
            .unwrap();
        state.set_balance(&recipient, U256::from_u64(0)).unwrap();

        let executor = EvmExecutor::new(&state, 9000, 1, 1234567890, Address::ZERO, 30_000_000);

        // Simple call to recipient address (no code, just value check)
        let result = executor.static_call(Some(&sender), &recipient, Vec::new(), 100_000);

        // Should succeed (static call to empty account)
        assert!(result.is_ok(), "static_call failed: {:?}", result.err());
        let evm_result = result.unwrap();
        // Static call to non-contract address succeeds
        assert!(evm_result.success);
    }

    #[test]
    fn test_contract_deployment() {
        let state = create_test_state();

        // Setup sender with funds
        let sender = Address::new([0x11; 20]);
        state
            .set_balance(&sender, U256::from_u128(10_000_000_000_000_000_000)) // 10 ETH
            .unwrap();
        state.set_nonce(&sender, 0).unwrap();

        let executor = EvmExecutor::new(&state, 9000, 1, 1234567890, Address::ZERO, 30_000_000);

        // Simple contract that just stores 42 and returns it
        // PUSH1 42, PUSH1 0, SSTORE (store 42 at slot 0)
        // PUSH1 32, PUSH1 0, RETURN (return empty)
        // Runtime code: PUSH1 0, SLOAD, PUSH1 0, MSTORE, PUSH1 32, PUSH1 0, RETURN
        // This is minimal bytecode for a contract that stores 42
        let init_code =
            hex::decode("602a60005560208060106000396000f3fe60005460005260206000f3").unwrap();

        let result = executor.create(&sender, init_code, U256::ZERO, 1_000_000);
        assert!(result.is_ok(), "create failed: {:?}", result.err());
        let evm_result = result.unwrap();
        assert!(
            evm_result.success,
            "Contract creation failed: {:?}",
            evm_result.error
        );
        assert!(evm_result.contract_address.is_some());

        // Gas should be consumed
        assert!(evm_result.gas_used > 0);
    }
}
