//! Cross-VM Call Patterns
//!
//! Defines various call patterns for cross-VM communication.

use primitive_types::{H160, U256};

use crate::executor::{ExecutionError, ExecutionResult};
use super::{CallType, CrossVmCall, CrossVmResult, EvmBackend, InteropManager};

/// Callback interface for EVM contracts to call back into QVM
pub trait QvmCallback {
    /// Handle a callback from EVM
    fn on_callback(
        &mut self,
        caller: H160,
        calldata: &[u8],
        value: U256,
    ) -> ExecutionResult<Vec<u8>>;
}

/// Multi-call executor for batching cross-VM calls
pub struct MultiCall<'a, E: EvmBackend> {
    manager: &'a mut InteropManager<E>,
    calls: Vec<CrossVmCall>,
    results: Vec<CrossVmResult>,
}

impl<'a, E: EvmBackend> MultiCall<'a, E> {
    pub fn new(manager: &'a mut InteropManager<E>) -> Self {
        Self {
            manager,
            calls: Vec::new(),
            results: Vec::new(),
        }
    }

    /// Add a call to the batch
    pub fn add_call(
        &mut self,
        target: H160,
        calldata: Vec<u8>,
        value: U256,
        gas_limit: u64,
    ) -> &mut Self {
        self.calls.push(CrossVmCall {
            target,
            call_type: CallType::Call,
            calldata,
            value,
            gas_limit,
        });
        self
    }

    /// Add a static call to the batch
    pub fn add_static_call(
        &mut self,
        target: H160,
        calldata: Vec<u8>,
        gas_limit: u64,
    ) -> &mut Self {
        self.calls.push(CrossVmCall {
            target,
            call_type: CallType::StaticCall,
            calldata,
            value: U256::zero(),
            gas_limit,
        });
        self
    }

    /// Execute all calls in the batch
    pub fn execute(mut self) -> ExecutionResult<Vec<CrossVmResult>> {
        for call in self.calls.drain(..) {
            let result = self.manager.call_evm(call)?;
            self.results.push(result);
        }
        Ok(self.results)
    }

    /// Execute all calls, stopping on first failure
    pub fn execute_strict(mut self) -> ExecutionResult<Vec<CrossVmResult>> {
        for call in self.calls.drain(..) {
            let result = self.manager.call_evm(call)?;
            if !result.success {
                return Err(ExecutionError::Revert(
                    "Multi-call failed".to_string()
                ));
            }
            self.results.push(result);
        }
        Ok(self.results)
    }
}

/// Flash loan callback interface
pub trait FlashLoanReceiver {
    /// Called when flash loan is received
    fn on_flash_loan(
        &mut self,
        initiator: H160,
        token: H160,
        amount: U256,
        fee: U256,
        data: &[u8],
    ) -> ExecutionResult<bool>;
}

/// Reentrancy guard for cross-VM calls
#[derive(Debug, Default)]
pub struct ReentrancyGuard {
    locked: bool,
    entered_contracts: Vec<H160>,
}

impl ReentrancyGuard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Enter a contract (check for reentrancy)
    pub fn enter(&mut self, contract: H160) -> ExecutionResult<()> {
        if self.entered_contracts.contains(&contract) {
            return Err(ExecutionError::ResourceError(
                "Reentrancy detected".to_string()
            ));
        }
        self.entered_contracts.push(contract);
        Ok(())
    }

    /// Exit a contract
    pub fn exit(&mut self, contract: H160) {
        if let Some(pos) = self.entered_contracts.iter().position(|&c| c == contract) {
            self.entered_contracts.remove(pos);
        }
    }

    /// Lock the guard
    pub fn lock(&mut self) -> ExecutionResult<()> {
        if self.locked {
            return Err(ExecutionError::ResourceError(
                "Reentrancy guard already locked".to_string()
            ));
        }
        self.locked = true;
        Ok(())
    }

    /// Unlock the guard
    pub fn unlock(&mut self) {
        self.locked = false;
    }

    /// Check if locked
    pub fn is_locked(&self) -> bool {
        self.locked
    }
}

/// Cross-VM event listener
pub trait CrossVmEventListener {
    /// Called when a cross-VM log is emitted
    fn on_log(&mut self, log: &super::CrossVmLog);

    /// Called when a cross-VM call starts
    fn on_call_start(&mut self, call: &CrossVmCall);

    /// Called when a cross-VM call ends
    fn on_call_end(&mut self, call: &CrossVmCall, result: &CrossVmResult);
}

/// Proxy pattern for cross-VM calls
pub struct ProxyCall<'a, E: EvmBackend> {
    manager: &'a mut InteropManager<E>,
    implementation: H160,
}

impl<'a, E: EvmBackend> ProxyCall<'a, E> {
    pub fn new(manager: &'a mut InteropManager<E>, implementation: H160) -> Self {
        Self { manager, implementation }
    }

    /// Forward a call to the implementation
    pub fn forward(
        &mut self,
        calldata: Vec<u8>,
        value: U256,
        gas_limit: u64,
    ) -> ExecutionResult<CrossVmResult> {
        let request = CrossVmCall {
            target: self.implementation,
            call_type: CallType::DelegateCall,
            calldata,
            value,
            gas_limit,
        };

        self.manager.call_evm(request)
    }

    /// Upgrade the implementation
    pub fn upgrade(&mut self, new_implementation: H160) {
        self.implementation = new_implementation;
    }
}

/// Safe call wrapper with automatic error handling
pub struct SafeCall;

impl SafeCall {
    /// Execute a call that returns bool, defaulting to false on failure
    pub fn try_call_bool<E: EvmBackend>(
        manager: &mut InteropManager<E>,
        request: CrossVmCall,
    ) -> bool {
        match manager.call_evm(request) {
            Ok(result) => {
                result.success
                    && result.return_data.len() >= 32
                    && result.return_data[31] != 0
            }
            Err(_) => false,
        }
    }

    /// Execute a call that returns uint256, defaulting to zero on failure
    pub fn try_call_uint256<E: EvmBackend>(
        manager: &mut InteropManager<E>,
        request: CrossVmCall,
    ) -> U256 {
        match manager.call_evm(request) {
            Ok(result) => {
                if result.success && result.return_data.len() >= 32 {
                    U256::from_big_endian(&result.return_data[0..32])
                } else {
                    U256::zero()
                }
            }
            Err(_) => U256::zero(),
        }
    }

    /// Execute a call that returns address, defaulting to zero on failure
    pub fn try_call_address<E: EvmBackend>(
        manager: &mut InteropManager<E>,
        request: CrossVmCall,
    ) -> H160 {
        match manager.call_evm(request) {
            Ok(result) => {
                if result.success && result.return_data.len() >= 32 {
                    H160::from_slice(&result.return_data[12..32])
                } else {
                    H160::zero()
                }
            }
            Err(_) => H160::zero(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interop::MockEvmBackend;

    #[test]
    fn test_multi_call() {
        let mut backend = MockEvmBackend::new();
        backend.deploy(H160::from_low_u64_be(0x1234), vec![0x60, 0x00]);
        backend.deploy(H160::from_low_u64_be(0x5678), vec![0x60, 0x00]);

        let mut manager = InteropManager::new(backend);

        let mut multi = MultiCall::new(&mut manager);
        multi.add_static_call(H160::from_low_u64_be(0x1234), vec![], 100000);
        multi.add_static_call(H160::from_low_u64_be(0x5678), vec![], 100000);
        let results = multi.execute().unwrap();

        assert_eq!(results.len(), 2);
        assert!(results[0].success);
        assert!(results[1].success);
    }

    #[test]
    fn test_reentrancy_guard() {
        let mut guard = ReentrancyGuard::new();
        let contract = H160::from_low_u64_be(0x1234);

        // First entry should succeed
        assert!(guard.enter(contract).is_ok());

        // Second entry to same contract should fail
        assert!(guard.enter(contract).is_err());

        // Exit and try again
        guard.exit(contract);
        assert!(guard.enter(contract).is_ok());
    }

    #[test]
    fn test_reentrancy_guard_lock() {
        let mut guard = ReentrancyGuard::new();

        assert!(!guard.is_locked());
        assert!(guard.lock().is_ok());
        assert!(guard.is_locked());
        assert!(guard.lock().is_err()); // Already locked
        guard.unlock();
        assert!(!guard.is_locked());
    }

    #[test]
    fn test_safe_call() {
        let mut backend = MockEvmBackend::new();
        backend.deploy(H160::from_low_u64_be(0x1234), vec![0x60, 0x00]);

        let mut manager = InteropManager::new(backend);

        let request = CrossVmCall {
            target: H160::from_low_u64_be(0x1234),
            call_type: CallType::StaticCall,
            calldata: vec![],
            value: U256::zero(),
            gas_limit: 100000,
        };

        // Should return zero (mock returns zeroed data)
        let result = SafeCall::try_call_uint256(&mut manager, request);
        assert_eq!(result, U256::zero());
    }
}
