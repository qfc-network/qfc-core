//! EVM Interoperability Module
//!
//! Enables QVM contracts to interact with EVM contracts and vice versa.
//!
//! # Architecture
//!
//! The interop layer provides:
//! - Call bridging between QVM and EVM
//! - ABI encoding/decoding for cross-VM calls
//! - State access coordination
//! - Gas metering translation

pub mod bridge;
pub mod calls;
pub mod state;

use primitive_types::{H160, H256, U256};
use std::collections::HashMap;

use crate::executor::{ExecutionError, ExecutionResult};
use crate::value::Value;

/// Cross-VM call type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallType {
    /// Regular call (can modify state)
    Call,
    /// Static call (read-only)
    StaticCall,
    /// Delegate call (use caller's context)
    DelegateCall,
}

/// Cross-VM call request
#[derive(Debug, Clone)]
pub struct CrossVmCall {
    /// Target contract address
    pub target: H160,
    /// Call type
    pub call_type: CallType,
    /// Calldata (ABI encoded)
    pub calldata: Vec<u8>,
    /// Value to send (for Call type only)
    pub value: U256,
    /// Gas limit for the call
    pub gas_limit: u64,
}

/// Cross-VM call result
#[derive(Debug, Clone)]
pub struct CrossVmResult {
    /// Whether the call succeeded
    pub success: bool,
    /// Return data
    pub return_data: Vec<u8>,
    /// Gas used
    pub gas_used: u64,
    /// Logs emitted
    pub logs: Vec<CrossVmLog>,
}

/// Log from cross-VM call
#[derive(Debug, Clone)]
pub struct CrossVmLog {
    pub address: H160,
    pub topics: Vec<H256>,
    pub data: Vec<u8>,
}

/// Trait for EVM execution backend
pub trait EvmBackend {
    /// Execute a call to an EVM contract
    fn call(&mut self, request: CrossVmCall) -> ExecutionResult<CrossVmResult>;

    /// Get EVM contract code
    fn get_code(&self, address: H160) -> Option<Vec<u8>>;

    /// Check if address is an EVM contract
    fn is_evm_contract(&self, address: H160) -> bool;

    /// Get storage value from EVM contract
    fn get_storage(&self, address: H160, slot: H256) -> H256;
}

/// Interop manager for handling cross-VM communication
pub struct InteropManager<E: EvmBackend> {
    /// EVM execution backend
    evm_backend: E,

    /// Pending cross-VM calls (for batching)
    #[allow(dead_code)]
    pending_calls: Vec<CrossVmCall>,

    /// Call depth tracking
    call_depth: usize,

    /// Maximum call depth
    max_call_depth: usize,

    /// Address registry (QVM contract -> type)
    registry: HashMap<H160, ContractType>,
}

/// Contract type in the registry
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContractType {
    /// QVM contract (QuantumScript)
    Qvm,
    /// EVM contract (Solidity/Vyper)
    Evm,
    /// Precompiled contract
    Precompile,
}

impl<E: EvmBackend> InteropManager<E> {
    /// Create a new interop manager
    pub fn new(evm_backend: E) -> Self {
        Self {
            evm_backend,
            pending_calls: Vec::new(),
            call_depth: 0,
            max_call_depth: 1024,
            registry: HashMap::new(),
        }
    }

    /// Register a contract address
    pub fn register(&mut self, address: H160, contract_type: ContractType) {
        self.registry.insert(address, contract_type);
    }

    /// Get contract type for an address
    pub fn get_contract_type(&self, address: H160) -> ContractType {
        if let Some(&contract_type) = self.registry.get(&address) {
            return contract_type;
        }

        // Check if it's an EVM contract
        if self.evm_backend.is_evm_contract(address) {
            ContractType::Evm
        } else {
            ContractType::Qvm
        }
    }

    /// Execute a cross-VM call from QVM to EVM
    pub fn call_evm(&mut self, request: CrossVmCall) -> ExecutionResult<CrossVmResult> {
        // Check call depth
        if self.call_depth >= self.max_call_depth {
            return Err(ExecutionError::CallDepthExceeded(self.max_call_depth));
        }

        // Increment call depth
        self.call_depth += 1;

        // Execute the call
        let result = self.evm_backend.call(request);

        // Decrement call depth
        self.call_depth -= 1;

        result
    }

    /// Encode a QVM value for EVM consumption
    pub fn encode_for_evm(&self, value: &Value) -> Vec<u8> {
        let mut result = vec![0u8; 32];

        match value {
            Value::U256(n) => {
                n.to_big_endian(&mut result);
            }
            Value::Bool(b) => {
                result[31] = if *b { 1 } else { 0 };
            }
            Value::Address(a) => {
                result[12..32].copy_from_slice(a.as_bytes());
            }
            Value::Bytes32(h) => {
                result.copy_from_slice(h.as_bytes());
            }
            _ => {
                // Default to zero for complex types
            }
        }

        result
    }

    /// Decode EVM return data to QVM value
    pub fn decode_from_evm(&self, data: &[u8], expected_type: &str) -> Value {
        if data.len() < 32 {
            return Value::Unit;
        }

        match expected_type {
            "uint256" | "int256" => {
                Value::U256(U256::from_big_endian(&data[0..32]))
            }
            "bool" => {
                Value::Bool(data[31] != 0)
            }
            "address" => {
                Value::Address(H160::from_slice(&data[12..32]))
            }
            "bytes32" => {
                Value::Bytes32(H256::from_slice(&data[0..32]))
            }
            _ => {
                Value::Bytes(data.to_vec())
            }
        }
    }

    /// Build calldata for an EVM function call
    pub fn build_calldata(&self, selector: [u8; 4], args: &[Value]) -> Vec<u8> {
        let mut calldata = selector.to_vec();

        for arg in args {
            calldata.extend(self.encode_for_evm(arg));
        }

        calldata
    }

    /// Get function selector from signature
    pub fn get_selector(&self, signature: &str) -> [u8; 4] {
        use tiny_keccak::{Hasher, Keccak};

        let mut hasher = Keccak::v256();
        hasher.update(signature.as_bytes());
        let mut hash = [0u8; 32];
        hasher.finalize(&mut hash);

        [hash[0], hash[1], hash[2], hash[3]]
    }
}

/// Mock EVM backend for testing
#[cfg(test)]
pub struct MockEvmBackend {
    contracts: HashMap<H160, Vec<u8>>,
    storage: HashMap<(H160, H256), H256>,
}

#[cfg(test)]
impl MockEvmBackend {
    pub fn new() -> Self {
        Self {
            contracts: HashMap::new(),
            storage: HashMap::new(),
        }
    }

    pub fn deploy(&mut self, address: H160, code: Vec<u8>) {
        self.contracts.insert(address, code);
    }

    pub fn set_storage(&mut self, address: H160, slot: H256, value: H256) {
        self.storage.insert((address, slot), value);
    }
}

#[cfg(test)]
impl EvmBackend for MockEvmBackend {
    fn call(&mut self, _request: CrossVmCall) -> ExecutionResult<CrossVmResult> {
        // Simple mock: return success with empty data
        Ok(CrossVmResult {
            success: true,
            return_data: vec![0u8; 32],
            gas_used: 21000,
            logs: Vec::new(),
        })
    }

    fn get_code(&self, address: H160) -> Option<Vec<u8>> {
        self.contracts.get(&address).cloned()
    }

    fn is_evm_contract(&self, address: H160) -> bool {
        self.contracts.contains_key(&address)
    }

    fn get_storage(&self, address: H160, slot: H256) -> H256 {
        self.storage.get(&(address, slot)).copied().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_interop_manager_creation() {
        let backend = MockEvmBackend::new();
        let manager = InteropManager::new(backend);
        assert_eq!(manager.call_depth, 0);
    }

    #[test]
    fn test_encode_decode() {
        let backend = MockEvmBackend::new();
        let manager = InteropManager::new(backend);

        // Test U256
        let value = Value::from_u64(42);
        let encoded = manager.encode_for_evm(&value);
        assert_eq!(encoded.len(), 32);
        assert_eq!(encoded[31], 42);

        // Decode back
        let decoded = manager.decode_from_evm(&encoded, "uint256");
        assert_eq!(decoded, value);
    }

    #[test]
    fn test_get_selector() {
        let backend = MockEvmBackend::new();
        let manager = InteropManager::new(backend);

        // transfer(address,uint256) = 0xa9059cbb
        let selector = manager.get_selector("transfer(address,uint256)");
        assert_eq!(selector, [0xa9, 0x05, 0x9c, 0xbb]);
    }

    #[test]
    fn test_build_calldata() {
        let backend = MockEvmBackend::new();
        let manager = InteropManager::new(backend);

        let selector = manager.get_selector("transfer(address,uint256)");
        let args = vec![
            Value::Address(H160::from_low_u64_be(0x1234)),
            Value::from_u64(100),
        ];

        let calldata = manager.build_calldata(selector, &args);
        assert_eq!(calldata.len(), 4 + 32 + 32); // selector + 2 args
        assert_eq!(&calldata[0..4], &selector);
    }

    #[test]
    fn test_call_evm() {
        let mut backend = MockEvmBackend::new();
        backend.deploy(H160::from_low_u64_be(0x1234), vec![0x60, 0x00]);

        let mut manager = InteropManager::new(backend);

        let request = CrossVmCall {
            target: H160::from_low_u64_be(0x1234),
            call_type: CallType::Call,
            calldata: vec![],
            value: U256::zero(),
            gas_limit: 100000,
        };

        let result = manager.call_evm(request).unwrap();
        assert!(result.success);
    }

    #[test]
    fn test_contract_type_detection() {
        let mut backend = MockEvmBackend::new();
        backend.deploy(H160::from_low_u64_be(0x1234), vec![0x60, 0x00]);

        let mut manager = InteropManager::new(backend);

        // Registered QVM contract
        manager.register(H160::from_low_u64_be(0x5678), ContractType::Qvm);
        assert_eq!(
            manager.get_contract_type(H160::from_low_u64_be(0x5678)),
            ContractType::Qvm
        );

        // Detected EVM contract
        assert_eq!(
            manager.get_contract_type(H160::from_low_u64_be(0x1234)),
            ContractType::Evm
        );

        // Unknown address defaults to QVM
        assert_eq!(
            manager.get_contract_type(H160::from_low_u64_be(0x9999)),
            ContractType::Qvm
        );
    }
}
