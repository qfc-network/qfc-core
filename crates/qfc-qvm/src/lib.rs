//! QFC Virtual Machine (qfc-qvm)
//!
//! A stack-based virtual machine for executing QuantumScript bytecode.
//!
//! # Architecture
//!
//! The QVM is designed with the following key components:
//!
//! - **Stack**: Operand stack for computation
//! - **Memory**: Linear byte-addressable memory
//! - **Storage**: Persistent key-value storage
//! - **Heap**: Dynamic allocation for complex values
//! - **Gas Metering**: EVM-compatible gas accounting
//! - **Resource Tracking**: Linear type enforcement for resources
//!
//! # Example
//!
//! ```ignore
//! use qfc_qvm::{Executor, ExecutionContext};
//! use qfc_qsc::{compile, CompilerOptions};
//!
//! // Compile QuantumScript to bytecode
//! let source = r#"
//!     contract Counter {
//!         storage {
//!             count: u256,
//!         }
//!
//!         pub fn increment() {
//!             count = count + 1;
//!         }
//!     }
//! "#;
//!
//! let contracts = compile(source, &CompilerOptions::default())?;
//! let contract = &contracts[0];
//!
//! // Execute bytecode
//! let mut executor = Executor::new(1_000_000);
//! let result = executor.execute(&contract.functions[0].code)?;
//! ```
//!
//! # Features
//!
//! - Stack-based execution model
//! - EVM-compatible gas metering
//! - Resource types with linear ownership tracking
//! - Parallel execution hints (for future optimization)
//! - Full QuantumScript opcode support

pub mod executor;
pub mod gas;
pub mod interop;
pub mod jit;
pub mod memory;
pub mod stdlib;
pub mod value;

pub use executor::{ExecutionContext, ExecutionError, ExecutionOutput, ExecutionResult, Executor, Log};
pub use gas::{GasCosts, GasError, GasMeter, GasResult};
pub use interop::{
    CallType, ContractType, CrossVmCall, CrossVmResult, EvmBackend, InteropManager,
};
pub use memory::{CallFrame, Heap, Memory, MemoryError, MemoryResult, Stack, Storage};
pub use stdlib::StdlibRegistry;
pub use value::{ResourceAbility, Value, ValueRef, ValueType};

// JIT compilation (optional feature)
pub use jit::{ExecutionMode, JitError, JitResult, JitStats};
#[cfg(feature = "jit")]
pub use jit::{CodeGenerator, CompiledFunction, JitCompiler, JitConfig, JitRuntime};

/// QVM version
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Maximum stack depth
pub const MAX_STACK_DEPTH: usize = memory::MAX_STACK_DEPTH;

/// Maximum memory size
pub const MAX_MEMORY_SIZE: usize = memory::MAX_MEMORY_SIZE;

/// Maximum call depth
pub const MAX_CALL_DEPTH: usize = executor::MAX_CALL_DEPTH;

#[cfg(test)]
mod tests {
    use super::*;
    use qfc_qsc::{Instruction, Opcode};

    fn make_push(value: u64) -> Instruction {
        use primitive_types::U256;
        let mut bytes = [0u8; 32];
        U256::from(value).to_big_endian(&mut bytes);
        Instruction::with_operand(Opcode::Push, bytes.to_vec())
    }

    #[test]
    fn test_simple_execution() {
        let code = vec![
            make_push(10),
            make_push(20),
            Instruction::new(Opcode::Add),
            Instruction::new(Opcode::Return),
        ];

        let mut executor = Executor::new(100000);
        let result = executor.execute(&code).unwrap();

        assert!(result.success);
        assert_eq!(result.value, Some(Value::from_u64(30)));
    }

    #[test]
    fn test_multiplication() {
        let code = vec![
            make_push(7),
            make_push(6),
            Instruction::new(Opcode::Mul),
            Instruction::new(Opcode::Return),
        ];

        let mut executor = Executor::new(100000);
        let result = executor.execute(&code).unwrap();

        assert!(result.success);
        assert_eq!(result.value, Some(Value::from_u64(42)));
    }

    #[test]
    fn test_comparison_chain() {
        // Test: (5 < 10) && (10 < 20)
        let code = vec![
            make_push(5),
            make_push(10),
            Instruction::new(Opcode::Lt),  // 5 < 10 = true
            make_push(10),
            make_push(20),
            Instruction::new(Opcode::Lt),  // 10 < 20 = true
            Instruction::new(Opcode::And), // true && true = true
            Instruction::new(Opcode::Return),
        ];

        let mut executor = Executor::new(100000);
        let result = executor.execute(&code).unwrap();

        assert!(result.success);
        assert_eq!(result.value, Some(Value::Bool(true)));
    }

    #[test]
    fn test_local_variables() {
        let code = vec![
            // Store 100 in local 0
            make_push(100),
            Instruction::with_operand(Opcode::StoreLocal, vec![0, 0]),
            // Store 50 in local 1
            make_push(50),
            Instruction::with_operand(Opcode::StoreLocal, vec![0, 1]),
            // Load local 0
            Instruction::with_operand(Opcode::LoadLocal, vec![0, 0]),
            // Load local 1
            Instruction::with_operand(Opcode::LoadLocal, vec![0, 1]),
            // Add them
            Instruction::new(Opcode::Add),
            Instruction::new(Opcode::Return),
        ];

        let mut executor = Executor::new(100000);
        let result = executor.execute(&code).unwrap();

        assert!(result.success);
        assert_eq!(result.value, Some(Value::from_u64(150)));
    }

    #[test]
    fn test_bitwise_operations() {
        // Test: 0xFF & 0x0F = 0x0F
        let code = vec![
            make_push(0xFF),
            make_push(0x0F),
            Instruction::new(Opcode::BitAnd),
            Instruction::new(Opcode::Return),
        ];

        let mut executor = Executor::new(100000);
        let result = executor.execute(&code).unwrap();

        assert!(result.success);
        assert_eq!(result.value, Some(Value::from_u64(0x0F)));
    }

    #[test]
    fn test_out_of_gas() {
        let code = vec![
            make_push(1),
            make_push(2),
            Instruction::new(Opcode::Add),
            Instruction::new(Opcode::Return),
        ];

        // Very low gas limit
        let mut executor = Executor::new(1);
        let result = executor.execute(&code);

        assert!(result.is_err());
    }

    #[test]
    fn test_storage_operations() {
        let code = vec![
            // Store 999 at slot 5
            make_push(5),    // slot (key, pushed first)
            make_push(999),  // value (pushed second, on top)
            Instruction::new(Opcode::SStore),
            // Load from slot 5
            make_push(5),    // slot
            Instruction::new(Opcode::SLoad),
            Instruction::new(Opcode::Return),
        ];

        let mut executor = Executor::new(1_000_000);
        let result = executor.execute(&code).unwrap();

        assert!(result.success);
        assert_eq!(result.value, Some(Value::from_u64(999)));
        assert!(!result.storage_changes.is_empty());
    }

    #[test]
    fn test_context_access() {
        use primitive_types::H160;

        let code = vec![
            Instruction::new(Opcode::Caller),
            Instruction::new(Opcode::Return),
        ];

        let caller = H160::from_low_u64_be(0x1234);
        let context = ExecutionContext {
            caller,
            ..Default::default()
        };

        let mut executor = Executor::new(100000).with_context(context);
        let result = executor.execute(&code).unwrap();

        assert!(result.success);
        assert_eq!(result.value, Some(Value::Address(caller)));
    }
}
