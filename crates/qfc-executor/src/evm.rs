//! EVM execution wrapper using revm

use crate::error::{ExecutorError, Result};
use qfc_state::StateDB;
use qfc_types::{Address, Hash, Log, EVM_OPCODE_ACTIVATION_HEIGHT, U256};
use revm::{
    db::CacheDB,
    primitives::{
        keccak256, AccountInfo, Address as RevmAddress, Bytecode, Bytes, CreateScheme,
        ExecutionResult as RevmResult, Output, SpecId, TransactTo, B256, U256 as RevmU256,
    },
    Database as _, Evm,
};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

/// Resolver for the BLOCKHASH opcode: `(ancestor_walk_start, wanted_height)
/// -> block hash`.
///
/// `ancestor_walk_start` is the hash of a block ON THE EXECUTING BLOCK'S OWN
/// ANCESTOR CHAIN (in practice its parent). The implementation must resolve
/// `wanted_height` by walking parent hashes from that block down through the
/// HASH-KEYED block store — NEVER through the canonical number index.
/// Rationale (reorg safety): `reorg_to` re-executes branch blocks while the
/// number-keyed canonical store still holds the OLD branch (the atomic batch
/// swaps it only after the whole branch re-executed), so a number-index read
/// would return the wrong ancestors and fail the state-root check.
///
/// The returned hash is the chain's native block hash — blake3(header_bytes),
/// the same hash the eth RPC reports for blocks.
pub type BlockHashLookup = Arc<dyn Fn(&Hash, u64) -> Option<Hash> + Send + Sync>;

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
    /// Whether the EVM opcode hardfork (ADR-0017) is active for the
    /// executing block. Gates the EXTCODEHASH and BLOCKHASH behavior below.
    fixes_active: bool,
    /// Number of the EXECUTING block (BLOCKHASH range checks).
    block_number: u64,
    /// Hash of the executing block's parent — the start of the BLOCKHASH
    /// ancestor walk.
    parent_hash: Hash,
    /// Resolver walking the executing block's own ancestor chain.
    block_hash_lookup: Option<BlockHashLookup>,
    /// Per-execution cache of resolved (height -> hash) pairs, so repeated
    /// BLOCKHASH reads of the same height walk the store only once.
    block_hash_cache: RefCell<HashMap<u64, B256>>,
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
            // The revm-facing code_hash is what the EXTCODEHASH opcode
            // returns. Post-activation it must be keccak256(code) — contracts
            // doing proxy/clone detection compare against known keccak
            // hashes. Pre-activation it stays blake3(code), the historical
            // (wrong but consensus-frozen) behavior fresh-sync re-execution
            // must reproduce. NOTE: this is revm-facing ONLY — the storage
            // layer's CODE cf remains keyed by blake3 (StateDB::set_code);
            // revm never loads code through code_by_hash_ref because the code
            // is supplied inline below.
            let code_hash = if self.fixes_active {
                keccak256(&code)
            } else {
                B256::from_slice(qfc_crypto::blake3_hash(&code).as_bytes())
            };
            Ok(Some(AccountInfo {
                balance: u256_to_revm(balance),
                nonce,
                code_hash,
                code: Some(Bytecode::new_raw(Bytes::from(code))),
            }))
        }
    }

    fn code_by_hash_ref(&self, _code_hash: B256) -> std::result::Result<Bytecode, Self::Error> {
        // Unreachable in practice: `basic_ref` always supplies the bytecode
        // inline (`code: Some(..)`), so revm never needs to load code by
        // hash. Kept as a harmless default rather than a panic.
        debug_assert!(
            false,
            "code_by_hash_ref should be unreachable: basic_ref supplies code inline"
        );
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

    fn block_hash_ref(&self, number: RevmU256) -> std::result::Result<B256, Self::Error> {
        // Pre-activation: the historical behavior (always zero) is
        // consensus-frozen — fresh nodes re-execute old blocks with this
        // binary and must reproduce the old state roots.
        if !self.fixes_active {
            return Ok(B256::ZERO);
        }

        // Spec semantics: BLOCKHASH(n) is valid only for
        // `block_number - 256 <= n < block_number`; everything else is zero.
        let wanted = match u64::try_from(number) {
            Ok(n) => n,
            Err(_) => return Ok(B256::ZERO),
        };
        if wanted >= self.block_number || self.block_number - wanted > 256 {
            return Ok(B256::ZERO);
        }

        if let Some(cached) = self.block_hash_cache.borrow().get(&wanted) {
            return Ok(*cached);
        }

        // Resolve along the EXECUTING block's own ancestor chain, starting
        // from its parent (see [`BlockHashLookup`] for why the canonical
        // number index must not be used). A missing ancestor (e.g. walking
        // past a pruned range) yields zero rather than an execution error —
        // deterministic as long as all nodes retain the 256-block window,
        // which block validation already requires.
        let resolved = self
            .block_hash_lookup
            .as_ref()
            .and_then(|lookup| lookup(&self.parent_hash, wanted))
            .map(|h| B256::from_slice(h.as_bytes()))
            .unwrap_or(B256::ZERO);

        self.block_hash_cache.borrow_mut().insert(wanted, resolved);
        Ok(resolved)
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
    /// Hash of the executing block's parent. Drives PREVRANDAO and anchors
    /// the BLOCKHASH ancestor walk (ADR-0017).
    parent_hash: Hash,
    /// BLOCKHASH resolver walking the executing block's own ancestor chain.
    /// `None` (unit tests, detached execution) makes post-activation
    /// BLOCKHASH return zero.
    block_hash_lookup: Option<BlockHashLookup>,
    /// Hardfork gate for the ADR-0017 opcode fixes. Production always uses
    /// [`EVM_OPCODE_ACTIVATION_HEIGHT`]; overridable ONLY for tests via
    /// [`Self::with_activation_height`].
    activation_height: u64,
}

impl<'a> EvmExecutor<'a> {
    /// Create a new EVM executor.
    ///
    /// `parent_hash` is the hash of the EXECUTING block's parent;
    /// `block_hash_lookup` resolves BLOCKHASH along that block's own
    /// ancestor chain (see [`BlockHashLookup`]). Consensus paths MUST pass
    /// both — a `None` lookup silently degrades post-activation BLOCKHASH
    /// to zero.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        state: &'a StateDB,
        chain_id: u64,
        block_number: u64,
        block_timestamp: u64,
        block_coinbase: Address,
        block_gas_limit: u64,
        parent_hash: Hash,
        block_hash_lookup: Option<BlockHashLookup>,
    ) -> Self {
        Self {
            state,
            chain_id,
            block_number,
            block_timestamp,
            block_coinbase,
            block_gas_limit,
            parent_hash,
            block_hash_lookup,
            activation_height: EVM_OPCODE_ACTIVATION_HEIGHT,
        }
    }

    /// Test-only override of the ADR-0017 activation height. Production
    /// code must NEVER call this — a per-node activation height is a silent
    /// consensus fork.
    #[doc(hidden)]
    pub fn with_activation_height(mut self, height: u64) -> Self {
        self.activation_height = height;
        self
    }

    /// Whether the ADR-0017 opcode fixes are active for the executing block.
    fn fixes_active(&self) -> bool {
        self.block_number >= self.activation_height
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

        // Configure transaction.
        //
        // gas_price = 0 makes revm gas-neutral (BUG B fix): revm's
        // deduct_caller charges `gas_limit * gas_price` = 0, its
        // balance check requires only `value` (not value + gas), and its
        // beneficiary reward is 0. All gas accounting (prepay → refund-unused
        // → pay-producer) is therefore the Executor's single responsibility,
        // avoiding the previous ~2x double-charge. Paired with basefee = 0 in
        // create_evm so the EIP-1559 `gas_price >= basefee` check still passes
        // without needing revm's cfg-gated `disable_base_fee`. `value`
        // transfers are handled by revm's inner CREATE/CALL logic and are
        // unaffected. Side-effect: the GASPRICE opcode reads 0 (see ADR-0015).
        let gas_price = RevmU256::ZERO;
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

        // Configure transaction. gas_price = 0 makes revm gas-neutral so the
        // Executor is the single source of gas accounting (BUG B fix — see the
        // detailed note in `create`). `value` transfer still handled by revm.
        let gas_price = RevmU256::ZERO;
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

        // Configure as static call. gas_price = 0 keeps revm gas-neutral
        // (consistent with create/call); the caller pre-funding above is then
        // belt-and-suspenders since revm's balance check requires only `value`.
        let gas_price = RevmU256::ZERO;
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
        Ok(CacheDB::new(StateDBRef {
            state: self.state,
            fixes_active: self.fixes_active(),
            block_number: self.block_number,
            parent_hash: self.parent_hash,
            block_hash_lookup: self.block_hash_lookup.clone(),
            block_hash_cache: RefCell::new(HashMap::new()),
        }))
    }

    /// Create a configured EVM instance
    fn create_evm<'b>(
        &self,
        db: &'b mut CacheDB<StateDBRef<'a>>,
    ) -> Evm<'b, (), &'b mut CacheDB<StateDBRef<'a>>> {
        let mut evm = Evm::builder().with_db(db).build();

        // Configure block environment
        evm.block_mut().number = RevmU256::from(self.block_number);
        // BUG A fix: `block_timestamp` is the block header timestamp in
        // milliseconds (consensus slot clock). Solidity `block.timestamp`
        // and every DeFi deadline (Uniswap `ensure`, permit, timelocks,
        // vesting, TWAP) expect Unix SECONDS, so convert here — the single
        // point where the header timestamp enters revm, covering both the
        // execution path (execute_at) and the eth_call/estimateGas simulate
        // path. The maturity clocks (unstake/undelegate) divide by 1000
        // themselves and are untouched.
        evm.block_mut().timestamp = RevmU256::from(self.block_timestamp / 1000);
        evm.block_mut().coinbase = address_to_revm(&self.block_coinbase);
        evm.block_mut().gas_limit = RevmU256::from(self.block_gas_limit);
        // basefee = 0 so the EIP-1559 `gas_price >= basefee` check passes with
        // our gas_price = 0 (BUG B). Side-effect: the BASEFEE opcode reads 0
        // (see ADR-0015).
        evm.block_mut().basefee = RevmU256::ZERO;
        // PREVRANDAO (post-merge DIFFICULTY, opcode 0x44): post-activation,
        // expose a deterministic per-block value derived from the parent
        // hash — identical on every node and on every path (produce, import,
        // reorg re-execution, eth_call). Pre-activation, revm's BlockEnv
        // default (Some(B256::ZERO)) is kept, i.e. the opcode reads 0 —
        // the consensus-frozen historical behavior.
        //
        // SECURITY: keccak256(parent_hash) is NOT secure randomness — the
        // block producer knows it before including transactions and can
        // grind inclusion. Acceptable for the testnet; revisit (e.g. VRF
        // output mixing) before mainnet. See ADR-0017.
        if self.fixes_active() {
            evm.block_mut().prevrandao = Some(keccak256(self.parent_hash.as_bytes()));
        }

        // Configure chain
        evm.cfg_mut().chain_id = self.chain_id;

        // Explicitly lock to Cancun spec for deterministic behavior.
        // Without this, revm defaults to LATEST which changes across revm upgrades.
        evm.modify_spec_id(SpecId::CANCUN);

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
        let executor = EvmExecutor::new(
            &state,
            9000,
            1,
            1234567890,
            Address::ZERO,
            30_000_000,
            Hash::ZERO,
            None,
        );

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

        let executor = EvmExecutor::new(
            &state,
            9000,
            1,
            1234567890,
            Address::ZERO,
            30_000_000,
            Hash::ZERO,
            None,
        );

        // Simple call to recipient address (no code, just value check)
        let result = executor.static_call(Some(&sender), &recipient, Vec::new(), 100_000);

        // Should succeed (static call to empty account)
        assert!(result.is_ok(), "static_call failed: {:?}", result.err());
        let evm_result = result.unwrap();
        // Static call to non-contract address succeeds
        assert!(evm_result.success);
    }

    #[test]
    fn test_precompile_sha256() {
        let state = create_test_state();
        let sender = Address::new([0x11; 20]);
        state
            .set_balance(&sender, U256::from_u128(1_000_000_000_000_000_000))
            .unwrap();

        let executor = EvmExecutor::new(
            &state,
            9000,
            1,
            1234567890,
            Address::ZERO,
            30_000_000,
            Hash::ZERO,
            None,
        );

        // Call SHA256 precompile (address 0x02) with "hello"
        let input = b"hello".to_vec();
        let sha256_addr =
            Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]);
        let result = executor.static_call(Some(&sender), &sha256_addr, input, 100_000);
        assert!(
            result.is_ok(),
            "sha256 precompile call failed: {:?}",
            result.err()
        );
        let evm_result = result.unwrap();
        assert!(
            evm_result.success,
            "sha256 precompile failed: {:?}",
            evm_result.error
        );
        assert_eq!(evm_result.output.len(), 32, "sha256 should return 32 bytes");

        // Verify against known SHA256("hello")
        let expected =
            hex::decode("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824")
                .unwrap();
        assert_eq!(evm_result.output, expected);
    }

    #[test]
    fn test_precompile_ecrecover() {
        let state = create_test_state();
        let sender = Address::new([0x11; 20]);
        state
            .set_balance(&sender, U256::from_u128(1_000_000_000_000_000_000))
            .unwrap();

        let executor = EvmExecutor::new(
            &state,
            9000,
            1,
            1234567890,
            Address::ZERO,
            30_000_000,
            Hash::ZERO,
            None,
        );

        // ecrecover precompile at address 0x01
        // Input: hash (32 bytes) + v (32 bytes) + r (32 bytes) + s (32 bytes) = 128 bytes
        // Use a known test vector
        let input = hex::decode(
            "456e9aea5e197a1f1af7a3e85a3212fa4049a3ba34c2289b4c860fc0b0c64ef3\
             000000000000000000000000000000000000000000000000000000000000001c\
             9242685bf161793cc25603c231bc2f568eb630ea16aa137d2664ac8038825608\
             4f8ae3bd7535248d0bd448298cc2e2071e56992d0774dc340c368ae950852ada",
        )
        .unwrap();
        let ecrecover_addr =
            Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        let result = executor.static_call(Some(&sender), &ecrecover_addr, input, 100_000);
        assert!(result.is_ok(), "ecrecover call failed: {:?}", result.err());
        let evm_result = result.unwrap();
        assert!(
            evm_result.success,
            "ecrecover failed: {:?}",
            evm_result.error
        );
        // Should return 32 bytes (left-padded address)
        assert_eq!(evm_result.output.len(), 32);
        // The recovered address should be non-zero
        assert_ne!(evm_result.output, vec![0u8; 32]);
    }

    #[test]
    fn test_precompile_identity() {
        let state = create_test_state();
        let sender = Address::new([0x11; 20]);
        state
            .set_balance(&sender, U256::from_u128(1_000_000_000_000_000_000))
            .unwrap();

        let executor = EvmExecutor::new(
            &state,
            9000,
            1,
            1234567890,
            Address::ZERO,
            30_000_000,
            Hash::ZERO,
            None,
        );

        // Call identity precompile (address 0x04) — returns input unchanged
        let input = vec![1, 2, 3, 4, 5];
        let identity_addr =
            Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 4]);
        let result = executor.static_call(Some(&sender), &identity_addr, input.clone(), 100_000);
        assert!(result.is_ok());
        let evm_result = result.unwrap();
        assert!(
            evm_result.success,
            "identity precompile failed: {:?}",
            evm_result.error
        );
        assert_eq!(evm_result.output, input);
    }

    /// BUG A regression: revm's `block.timestamp` (the TIMESTAMP opcode) must
    /// be Unix SECONDS, not the header's milliseconds. Deploys a contract
    /// whose constructor stores TIMESTAMP to slot 0, then asserts the stored
    /// value equals `input_ms / 1000`.
    #[test]
    fn test_block_timestamp_is_seconds_not_millis() {
        let state = create_test_state();
        let sender = Address::new([0x11; 20]);
        state
            .set_balance(&sender, U256::from_u128(10_000_000_000_000_000_000))
            .unwrap();
        state.set_nonce(&sender, 0).unwrap();

        // Realistic live-testnet header timestamp in ms (from the bug report).
        let ts_ms = 1_783_237_135_001u64;
        let executor = EvmExecutor::new(
            &state,
            9000,
            1,
            ts_ms,
            Address::ZERO,
            30_000_000,
            Hash::ZERO,
            None,
        );

        // init code: constructor runs `TIMESTAMP PUSH1 0 SSTORE` then returns
        // empty runtime. Bytecode: 42 6000 55 6000 6000 f3
        let init_code = hex::decode("4260005560006000f3").unwrap();
        let result = executor
            .create(&sender, init_code, U256::ZERO, 1_000_000)
            .unwrap();
        assert!(result.success, "create failed: {:?}", result.error);
        let contract = result.contract_address.unwrap();

        // Slot 0 now holds the TIMESTAMP the EVM saw.
        let stored = state.get_storage(&contract, &U256::ZERO).unwrap();
        assert_eq!(
            stored,
            U256::from_u64(ts_ms / 1000),
            "EVM block.timestamp must be Unix seconds (input_ms/1000)"
        );
        // And it must NOT be the raw milliseconds.
        assert_ne!(stored, U256::from_u64(ts_ms));
    }

    /// BUG A regression: a deadline-style guard `require(block.timestamp <=
    /// deadline)` with a realistic Unix-SECONDS deadline must SUCCEED. Pre-fix
    /// the EVM saw milliseconds (1_783_237_135_001) which exceeds the deadline
    /// (2_000_000_000) and every such call reverted (Uniswap `ensure`, permit,
    /// timelocks). Post-fix the EVM sees seconds (1_783_237_135) which is below
    /// the deadline, so the call succeeds.
    #[test]
    fn test_deadline_guard_succeeds_with_seconds_timestamp() {
        let state = create_test_state();
        let sender = Address::new([0x11; 20]);
        state
            .set_balance(&sender, U256::from_u128(10_000_000_000_000_000_000))
            .unwrap();
        state.set_nonce(&sender, 0).unwrap();

        let ts_ms = 1_783_237_135_001u64; // > 2e9 in ms, < 2e9 in seconds
        let executor = EvmExecutor::new(
            &state,
            9000,
            1,
            ts_ms,
            Address::ZERO,
            30_000_000,
            Hash::ZERO,
            None,
        );

        // Runtime reverts iff `block.timestamp > 2_000_000_000`:
        //   PUSH4 0x77359400  (deadline = 2_000_000_000)
        //   TIMESTAMP; GT      -> (timestamp > deadline)
        //   PUSH1 0x0b; JUMPI  -> jump to revert if expired
        //   STOP               -> success path
        //   JUMPDEST; PUSH1 0 PUSH1 0 REVERT
        // init code (deployer prefix + runtime):
        let init_code =
            hex::decode("601180600b6000396000f363773594004211600b57005b60006000fd").unwrap();
        let create = executor
            .create(&sender, init_code, U256::ZERO, 1_000_000)
            .unwrap();
        assert!(create.success, "deploy failed: {:?}", create.error);
        let contract = create.contract_address.unwrap();

        // Call the deadline guard.
        let call = executor
            .call(&sender, &contract, Vec::new(), U256::ZERO, 1_000_000)
            .unwrap();
        assert!(
            call.success,
            "deadline guard must pass with seconds timestamp (pre-fix this reverted): {:?}",
            call.error
        );
    }

    // ================= ADR-0017 opcode hardfork tests =================

    /// Deterministic fake block hash for a mocked ancestor chain.
    fn mock_hash_for_height(height: u64) -> Hash {
        qfc_crypto::blake3_hash(&height.to_le_bytes())
    }

    /// A mocked [`BlockHashLookup`] that resolves every height and asserts
    /// the walk starts at the expected parent hash.
    fn mock_lookup(expected_start: Hash) -> BlockHashLookup {
        Arc::new(move |start: &Hash, wanted: u64| {
            assert_eq!(
                *start, expected_start,
                "BLOCKHASH walk must start at the executing block's parent"
            );
            Some(mock_hash_for_height(wanted))
        })
    }

    fn funded_sender(state: &StateDB) -> Address {
        let sender = Address::new([0x11; 20]);
        state
            .set_balance(&sender, U256::from_u128(10_000_000_000_000_000_000))
            .unwrap();
        state.set_nonce(&sender, 0).unwrap();
        sender
    }

    /// Deploy `init_code` and return the created contract address.
    fn deploy(executor: &EvmExecutor, sender: &Address, init_hex: &str) -> Address {
        let init_code = hex::decode(init_hex).unwrap();
        let result = executor
            .create(sender, init_code, U256::ZERO, 1_000_000)
            .unwrap();
        assert!(result.success, "deploy failed: {:?}", result.error);
        result.contract_address.unwrap()
    }

    fn hash_as_u256(hash: &Hash) -> U256 {
        U256::from_be_bytes(hash.as_bytes())
    }

    /// EXTCODEHASH gating at the DB-adapter level: pre-activation the
    /// revm-facing code_hash is blake3(code) (historical behavior),
    /// post-activation it is keccak256(code). EOAs stay KECCAK_EMPTY on
    /// both sides of the fork.
    #[test]
    fn test_extcodehash_code_hash_gating() {
        use revm::DatabaseRef as _;

        let state = create_test_state();
        let contract = Address::new([0x33; 20]);
        let code = hex::decode("6001600101").unwrap();
        state.set_code(&contract, code.clone()).unwrap();
        let eoa = Address::new([0x44; 20]);
        state.set_balance(&eoa, U256::from_u64(1)).unwrap();

        let make_ref = |fixes_active: bool| StateDBRef {
            state: &state,
            fixes_active,
            block_number: if fixes_active {
                EVM_OPCODE_ACTIVATION_HEIGHT
            } else {
                EVM_OPCODE_ACTIVATION_HEIGHT - 1
            },
            parent_hash: Hash::ZERO,
            block_hash_lookup: None,
            block_hash_cache: RefCell::new(HashMap::new()),
        };

        let blake3_hash = B256::from_slice(qfc_crypto::blake3_hash(&code).as_bytes());
        let keccak_hash = keccak256(&code);
        assert_ne!(blake3_hash, keccak_hash);

        // Pre-activation: blake3 (consensus-frozen historical behavior).
        let pre = make_ref(false);
        let info = pre.basic_ref(address_to_revm(&contract)).unwrap().unwrap();
        assert_eq!(info.code_hash, blake3_hash);

        // Post-activation: keccak256(code) — Ethereum semantics.
        let post = make_ref(true);
        let info = post.basic_ref(address_to_revm(&contract)).unwrap().unwrap();
        assert_eq!(info.code_hash, keccak_hash);

        // EOA: KECCAK_EMPTY on both sides (EIP-3607, unchanged).
        for db_ref in [&pre, &post] {
            let info = db_ref.basic_ref(address_to_revm(&eoa)).unwrap().unwrap();
            assert_eq!(info.code_hash, revm::primitives::KECCAK_EMPTY);
        }
    }

    /// BLOCKHASH below the activation height stays hard-zero even with a
    /// working lookup installed (fresh-sync re-execution must reproduce the
    /// historical state roots).
    #[test]
    fn test_blockhash_zero_below_activation() {
        let state = create_test_state();
        let sender = funded_sender(&state);
        let block_number = EVM_OPCODE_ACTIVATION_HEIGHT - 1;
        let parent_hash = mock_hash_for_height(block_number - 1);
        let executor = EvmExecutor::new(
            &state,
            9000,
            block_number,
            1234567890,
            Address::ZERO,
            30_000_000,
            parent_hash,
            Some(mock_lookup(parent_hash)),
        );

        // Constructor stores blockhash(number - 1) at slot 0.
        let contract = deploy(&executor, &sender, "43600190034060005560006000f3");
        let stored = state.get_storage(&contract, &U256::ZERO).unwrap();
        assert_eq!(stored, U256::ZERO, "pre-activation BLOCKHASH must be 0");
    }

    /// BLOCKHASH at/after activation resolves via the ancestor-walk lookup:
    /// blockhash(number - 1) is the parent hash reported by the lookup.
    #[test]
    fn test_blockhash_resolves_at_activation() {
        let state = create_test_state();
        let sender = funded_sender(&state);
        let block_number = EVM_OPCODE_ACTIVATION_HEIGHT;
        let parent_hash = mock_hash_for_height(block_number - 1);
        let executor = EvmExecutor::new(
            &state,
            9000,
            block_number,
            1234567890,
            Address::ZERO,
            30_000_000,
            parent_hash,
            Some(mock_lookup(parent_hash)),
        );

        let contract = deploy(&executor, &sender, "43600190034060005560006000f3");
        let stored = state.get_storage(&contract, &U256::ZERO).unwrap();
        assert_eq!(
            stored,
            hash_as_u256(&mock_hash_for_height(block_number - 1)),
            "BLOCKHASH(number-1) must resolve to the parent hash"
        );
    }

    /// Spec range: blockhash(number) (the current block) is invalid and must
    /// be zero — and the lookup must not even be consulted.
    #[test]
    fn test_blockhash_current_block_is_zero_and_lookup_not_called() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let state = create_test_state();
        let sender = funded_sender(&state);
        let called = Arc::new(AtomicBool::new(false));
        let called_clone = called.clone();
        let lookup: BlockHashLookup = Arc::new(move |_start, wanted| {
            called_clone.store(true, Ordering::SeqCst);
            Some(mock_hash_for_height(wanted))
        });

        let block_number = EVM_OPCODE_ACTIVATION_HEIGHT;
        let executor = EvmExecutor::new(
            &state,
            9000,
            block_number,
            1234567890,
            Address::ZERO,
            30_000_000,
            mock_hash_for_height(block_number - 1),
            Some(lookup),
        );

        // Constructor stores blockhash(number) at slot 0.
        let contract = deploy(&executor, &sender, "434060005560006000f3");
        let stored = state.get_storage(&contract, &U256::ZERO).unwrap();
        assert_eq!(stored, U256::ZERO, "BLOCKHASH(current) must be 0");
        assert!(
            !called.load(Ordering::SeqCst),
            "out-of-range BLOCKHASH must not hit the lookup"
        );
    }

    /// Spec range boundary: blockhash(number - 256) is the oldest valid
    /// height; blockhash(number - 257) is out of range and zero.
    #[test]
    fn test_blockhash_256_window_boundary() {
        let state = create_test_state();
        let sender = funded_sender(&state);
        let block_number = EVM_OPCODE_ACTIVATION_HEIGHT;
        let parent_hash = mock_hash_for_height(block_number - 1);
        let executor = EvmExecutor::new(
            &state,
            9000,
            block_number,
            1234567890,
            Address::ZERO,
            30_000_000,
            parent_hash,
            Some(mock_lookup(parent_hash)),
        );

        // Constructor stores blockhash(number-256) at slot 0 and
        // blockhash(number-257) at slot 1.
        let contract = deploy(
            &executor,
            &sender,
            "436101009003406000554361010190034060015560006000f3",
        );
        assert_eq!(
            state.get_storage(&contract, &U256::ZERO).unwrap(),
            hash_as_u256(&mock_hash_for_height(block_number - 256)),
            "BLOCKHASH(number-256) is the oldest valid height"
        );
        assert_eq!(
            state.get_storage(&contract, &U256::from_u64(1)).unwrap(),
            U256::ZERO,
            "BLOCKHASH(number-257) is out of range"
        );
    }

    /// PREVRANDAO gating: pre-activation the opcode reads 0 (revm BlockEnv
    /// default), at/after activation it reads keccak256(parent_hash) — and
    /// is deterministic across independent executions.
    #[test]
    fn test_prevrandao_gating_and_determinism() {
        // Constructor stores PREVRANDAO (0x44) at slot 0.
        let init = "4460005560006000f3";
        let parent_hash = mock_hash_for_height(41);

        let run = |block_number: u64| -> U256 {
            let state = create_test_state();
            let sender = funded_sender(&state);
            let executor = EvmExecutor::new(
                &state,
                9000,
                block_number,
                1234567890,
                Address::ZERO,
                30_000_000,
                parent_hash,
                None,
            );
            let contract = deploy(&executor, &sender, init);
            state.get_storage(&contract, &U256::ZERO).unwrap()
        };

        // Pre-activation: 0.
        assert_eq!(run(EVM_OPCODE_ACTIVATION_HEIGHT - 1), U256::ZERO);

        // Post-activation: keccak256(parent_hash), deterministic.
        let expected = U256::from_be_bytes(&keccak256(parent_hash.as_bytes()).0);
        let first = run(EVM_OPCODE_ACTIVATION_HEIGHT);
        let second = run(EVM_OPCODE_ACTIVATION_HEIGHT);
        assert_eq!(first, expected);
        assert_eq!(first, second, "PREVRANDAO must be deterministic");
        assert_ne!(first, U256::ZERO);
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

        let executor = EvmExecutor::new(
            &state,
            9000,
            1,
            1234567890,
            Address::ZERO,
            30_000_000,
            Hash::ZERO,
            None,
        );

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
