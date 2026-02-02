//! QVM Standard Library
//!
//! Built-in functions available to all QuantumScript contracts.

pub mod math;
pub mod crypto;
pub mod collections;
pub mod abi;

use primitive_types::{H160, H256, U256};
use std::collections::HashMap;

use crate::executor::{ExecutionError, ExecutionResult};
use crate::value::Value;

/// Standard library function signature
pub type StdlibFn = fn(&mut StdlibContext, Vec<Value>) -> ExecutionResult<Value>;

/// Context for stdlib function execution
pub struct StdlibContext<'a> {
    /// Current contract address
    pub address: H160,
    /// Caller address
    pub caller: H160,
    /// Call value
    pub value: U256,
    /// Block number
    pub block_number: u64,
    /// Block timestamp
    pub timestamp: u64,
    /// Memory access (for ABI encoding)
    pub memory: &'a mut Vec<u8>,
}

/// Standard library registry
pub struct StdlibRegistry {
    functions: HashMap<String, StdlibFn>,
}

impl StdlibRegistry {
    pub fn new() -> Self {
        let mut registry = Self {
            functions: HashMap::new(),
        };
        registry.register_all();
        registry
    }

    fn register_all(&mut self) {
        // Math functions
        self.register("math::min", math::min);
        self.register("math::max", math::max);
        self.register("math::abs", math::abs);
        self.register("math::sqrt", math::sqrt);
        self.register("math::pow", math::pow);
        self.register("math::log2", math::log2);
        self.register("math::clamp", math::clamp);
        self.register("math::mulDiv", math::mul_div);
        self.register("math::mulDivUp", math::mul_div_up);

        // Crypto functions
        self.register("crypto::keccak256", crypto::keccak256);
        self.register("crypto::sha256", crypto::sha256);
        self.register("crypto::blake3", crypto::blake3);
        self.register("crypto::ecrecover", crypto::ecrecover);
        self.register("crypto::verify", crypto::verify);

        // ABI functions
        self.register("abi::encode", abi::encode);
        self.register("abi::encodePacked", abi::encode_packed);
        self.register("abi::decode", abi::decode);
        self.register("abi::encodeCall", abi::encode_call);

        // Collection functions
        self.register("array::length", collections::array_length);
        self.register("array::push", collections::array_push);
        self.register("array::pop", collections::array_pop);
        self.register("array::get", collections::array_get);
        self.register("array::set", collections::array_set);
        self.register("array::slice", collections::array_slice);
        self.register("array::concat", collections::array_concat);

        self.register("bytes::length", collections::bytes_length);
        self.register("bytes::concat", collections::bytes_concat);
        self.register("bytes::slice", collections::bytes_slice);

        self.register("string::length", collections::string_length);
        self.register("string::concat", collections::string_concat);
        self.register("string::slice", collections::string_slice);
    }

    fn register(&mut self, name: &str, func: StdlibFn) {
        self.functions.insert(name.to_string(), func);
    }

    /// Call a stdlib function by name
    pub fn call(
        &self,
        name: &str,
        ctx: &mut StdlibContext,
        args: Vec<Value>,
    ) -> ExecutionResult<Value> {
        let func = self.functions.get(name).ok_or_else(|| {
            ExecutionError::UndefinedFunction(format!("stdlib function not found: {}", name))
        })?;
        func(ctx, args)
    }

    /// Check if a function exists
    pub fn has_function(&self, name: &str) -> bool {
        self.functions.contains_key(name)
    }

    /// Get all function names
    pub fn function_names(&self) -> Vec<&str> {
        self.functions.keys().map(|s| s.as_str()).collect()
    }
}

impl Default for StdlibRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_registry_has_functions() {
        let registry = StdlibRegistry::new();
        assert!(registry.has_function("math::min"));
        assert!(registry.has_function("crypto::keccak256"));
        assert!(registry.has_function("abi::encode"));
        assert!(!registry.has_function("nonexistent"));
    }
}
