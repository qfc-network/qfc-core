//! JIT runtime support functions.
//!
//! These functions are called from JIT-compiled code to perform operations
//! that cannot be inlined, such as storage access and context queries.

use std::collections::HashMap;

use cranelift_jit::JITBuilder;
use primitive_types::U256;

/// Runtime functions that can be called from JIT code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuntimeFunctions {
    /// Load from storage.
    SLoad,
    /// Store to storage.
    SStore,
    /// Get caller address.
    Caller,
    /// Get call value.
    CallValue,
    /// Get contract address.
    Address,
    /// Get block number.
    BlockNumber,
    /// Get block timestamp.
    Timestamp,
    /// Get remaining gas.
    Gas,
    /// Consume gas.
    UseGas,
    /// Emit log.
    Log,
    /// Keccak256 hash.
    Keccak256,
}

/// JIT runtime context.
///
/// This struct is passed to JIT-compiled functions and provides access to
/// the execution context, storage, and other runtime services.
#[repr(C)]
pub struct JitRuntime {
    /// Contract storage.
    pub storage: HashMap<U256, U256>,
    /// Caller address (as U256 for simplicity).
    pub caller: U256,
    /// Call value.
    pub call_value: U256,
    /// Contract address.
    pub address: U256,
    /// Block number.
    pub block_number: u64,
    /// Block timestamp.
    pub timestamp: u64,
    /// Gas limit.
    pub gas_limit: u64,
    /// Gas used.
    pub gas_used: u64,
    /// Return data.
    pub return_data: Vec<u8>,
    /// Logs emitted.
    pub logs: Vec<LogEntry>,
    /// Execution reverted flag.
    pub reverted: bool,
}

/// Log entry.
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub topics: Vec<U256>,
    pub data: Vec<u8>,
}

impl JitRuntime {
    /// Create a new runtime context.
    pub fn new() -> Self {
        Self {
            storage: HashMap::new(),
            caller: U256::zero(),
            call_value: U256::zero(),
            address: U256::zero(),
            block_number: 0,
            timestamp: 0,
            gas_limit: 10_000_000,
            gas_used: 0,
            return_data: Vec::new(),
            logs: Vec::new(),
            reverted: false,
        }
    }

    /// Configure the runtime with execution context.
    pub fn with_context(
        mut self,
        caller: U256,
        address: U256,
        value: U256,
        block_number: u64,
        timestamp: u64,
        gas_limit: u64,
    ) -> Self {
        self.caller = caller;
        self.address = address;
        self.call_value = value;
        self.block_number = block_number;
        self.timestamp = timestamp;
        self.gas_limit = gas_limit;
        self
    }

    /// Get remaining gas.
    pub fn remaining_gas(&self) -> u64 {
        self.gas_limit.saturating_sub(self.gas_used)
    }

    /// Use gas, returning false if out of gas.
    pub fn use_gas(&mut self, amount: u64) -> bool {
        if self.gas_used + amount > self.gas_limit {
            false
        } else {
            self.gas_used += amount;
            true
        }
    }

    /// Register runtime functions with the JIT builder.
    pub fn register_symbols(builder: &mut JITBuilder) {
        // Register external functions that JIT code can call
        builder.symbol("qvm_sload", qvm_sload as *const u8);
        builder.symbol("qvm_sstore", qvm_sstore as *const u8);
        builder.symbol("qvm_caller", qvm_caller as *const u8);
        builder.symbol("qvm_call_value", qvm_call_value as *const u8);
        builder.symbol("qvm_address", qvm_address as *const u8);
        builder.symbol("qvm_block_number", qvm_block_number as *const u8);
        builder.symbol("qvm_timestamp", qvm_timestamp as *const u8);
        builder.symbol("qvm_gas", qvm_gas as *const u8);
        builder.symbol("qvm_use_gas", qvm_use_gas as *const u8);
    }
}

impl Default for JitRuntime {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// External C functions called from JIT code
// ============================================================================

/// Load a value from storage.
#[no_mangle]
pub extern "C" fn qvm_sload(runtime: *mut JitRuntime, key: u64) -> u64 {
    let runtime = unsafe { &mut *runtime };

    // Use gas for storage read
    if !runtime.use_gas(200) {
        return 0;
    }

    let key = U256::from(key);
    runtime
        .storage
        .get(&key)
        .map(|v| v.low_u64())
        .unwrap_or(0)
}

/// Store a value to storage.
#[no_mangle]
pub extern "C" fn qvm_sstore(runtime: *mut JitRuntime, key: u64, value: u64) {
    let runtime = unsafe { &mut *runtime };

    // Use gas for storage write (simplified - real gas metering is more complex)
    let gas_cost = if value == 0 { 5000 } else { 20000 };
    if !runtime.use_gas(gas_cost) {
        runtime.reverted = true;
        return;
    }

    let key = U256::from(key);
    let value = U256::from(value);

    if value.is_zero() {
        runtime.storage.remove(&key);
    } else {
        runtime.storage.insert(key, value);
    }
}

/// Get the caller address.
#[no_mangle]
pub extern "C" fn qvm_caller(runtime: *mut JitRuntime) -> u64 {
    let runtime = unsafe { &*runtime };
    runtime.caller.low_u64()
}

/// Get the call value.
#[no_mangle]
pub extern "C" fn qvm_call_value(runtime: *mut JitRuntime) -> u64 {
    let runtime = unsafe { &*runtime };
    runtime.call_value.low_u64()
}

/// Get the contract address.
#[no_mangle]
pub extern "C" fn qvm_address(runtime: *mut JitRuntime) -> u64 {
    let runtime = unsafe { &*runtime };
    runtime.address.low_u64()
}

/// Get the current block number.
#[no_mangle]
pub extern "C" fn qvm_block_number(runtime: *mut JitRuntime) -> u64 {
    let runtime = unsafe { &*runtime };
    runtime.block_number
}

/// Get the current block timestamp.
#[no_mangle]
pub extern "C" fn qvm_timestamp(runtime: *mut JitRuntime) -> u64 {
    let runtime = unsafe { &*runtime };
    runtime.timestamp
}

/// Get the remaining gas.
#[no_mangle]
pub extern "C" fn qvm_gas(runtime: *mut JitRuntime) -> u64 {
    let runtime = unsafe { &*runtime };
    runtime.remaining_gas()
}

/// Use gas, returning 1 if successful, 0 if out of gas.
#[no_mangle]
pub extern "C" fn qvm_use_gas(runtime: *mut JitRuntime, amount: u64) -> u64 {
    let runtime = unsafe { &mut *runtime };
    if runtime.use_gas(amount) { 1 } else { 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_runtime_creation() {
        let runtime = JitRuntime::new();
        assert_eq!(runtime.gas_used, 0);
        assert_eq!(runtime.remaining_gas(), 10_000_000);
    }

    #[test]
    fn test_runtime_gas_usage() {
        let mut runtime = JitRuntime::new();
        assert!(runtime.use_gas(1000));
        assert_eq!(runtime.gas_used, 1000);
        assert_eq!(runtime.remaining_gas(), 10_000_000 - 1000);
    }

    #[test]
    fn test_runtime_out_of_gas() {
        let mut runtime = JitRuntime::new();
        runtime.gas_limit = 100;
        assert!(!runtime.use_gas(200));
        assert_eq!(runtime.gas_used, 0); // Should not have been charged
    }

    #[test]
    fn test_sload_sstore() {
        let mut runtime = JitRuntime::new();

        // Store a value
        qvm_sstore(&mut runtime as *mut _, 42, 100);
        assert!(!runtime.reverted);

        // Load the value back
        let value = qvm_sload(&mut runtime as *mut _, 42);
        assert_eq!(value, 100);
    }

    #[test]
    fn test_context_getters() {
        let mut runtime = JitRuntime::new()
            .with_context(
                U256::from(0x1234),  // caller
                U256::from(0x5678),  // address
                U256::from(1000),    // value
                100,                 // block number
                1700000000,          // timestamp
                1_000_000,           // gas limit
            );

        assert_eq!(qvm_caller(&mut runtime as *mut _), 0x1234);
        assert_eq!(qvm_address(&mut runtime as *mut _), 0x5678);
        assert_eq!(qvm_call_value(&mut runtime as *mut _), 1000);
        assert_eq!(qvm_block_number(&mut runtime as *mut _), 100);
        assert_eq!(qvm_timestamp(&mut runtime as *mut _), 1700000000);
    }
}
