//! QVM Executor - bytecode interpreter
//!
//! Executes QVM bytecode instructions.

use primitive_types::{H160, H256, U256};
use std::collections::HashMap;
use thiserror::Error;

use qfc_qsc::{Instruction, Opcode};

use crate::gas::{GasError, GasMeter};
use crate::memory::{CallFrame, Heap, Memory, MemoryError, Stack, Storage};
use crate::value::Value;

/// Execution errors
#[derive(Debug, Error, Clone)]
pub enum ExecutionError {
    #[error("gas error: {0}")]
    Gas(#[from] GasError),

    #[error("memory error: {0}")]
    Memory(#[from] MemoryError),

    #[error("invalid opcode: {0}")]
    InvalidOpcode(u8),

    #[error("type error: expected {expected}, found {found}")]
    TypeError { expected: String, found: String },

    #[error("division by zero")]
    DivisionByZero,

    #[error("arithmetic overflow")]
    Overflow,

    #[error("invalid jump destination: {0}")]
    InvalidJump(usize),

    #[error("call depth exceeded: {0}")]
    CallDepthExceeded(usize),

    #[error("undefined function: {0}")]
    UndefinedFunction(String),

    #[error("resource error: {0}")]
    ResourceError(String),

    #[error("revert: {0}")]
    Revert(String),

    #[error("halt")]
    Halt,

    #[error("internal error: {0}")]
    Internal(String),
}

pub type ExecutionResult<T> = Result<T, ExecutionError>;

/// Maximum call depth
pub const MAX_CALL_DEPTH: usize = 1024;

/// Execution context
#[derive(Debug, Clone)]
pub struct ExecutionContext {
    /// Contract address
    pub address: H160,

    /// Caller address
    pub caller: H160,

    /// Call value (msg.value)
    pub value: U256,

    /// Calldata
    pub calldata: Vec<u8>,

    /// Origin (tx.origin)
    pub origin: H160,

    /// Gas price
    pub gas_price: U256,

    /// Block number
    pub block_number: u64,

    /// Block timestamp
    pub timestamp: u64,

    /// Block coinbase
    pub coinbase: H160,

    /// Block difficulty
    pub difficulty: U256,

    /// Block gas limit
    pub gas_limit: u64,

    /// Chain ID
    pub chain_id: u64,

    /// Is static call (read-only)
    pub is_static: bool,
}

impl Default for ExecutionContext {
    fn default() -> Self {
        Self {
            address: H160::zero(),
            caller: H160::zero(),
            value: U256::zero(),
            calldata: Vec::new(),
            origin: H160::zero(),
            gas_price: U256::zero(),
            block_number: 0,
            timestamp: 0,
            coinbase: H160::zero(),
            difficulty: U256::zero(),
            gas_limit: u64::MAX,
            chain_id: 1,
            is_static: false,
        }
    }
}

/// Execution result data
#[derive(Debug, Clone)]
pub struct ExecutionOutput {
    /// Return data
    pub data: Vec<u8>,

    /// Return value (if any)
    pub value: Option<Value>,

    /// Gas used
    pub gas_used: u64,

    /// Gas refund
    pub gas_refund: i64,

    /// Emitted logs
    pub logs: Vec<Log>,

    /// Storage changes
    pub storage_changes: Vec<(H256, H256)>,

    /// Whether execution succeeded
    pub success: bool,
}

/// Log entry
#[derive(Debug, Clone)]
pub struct Log {
    pub address: H160,
    pub topics: Vec<H256>,
    pub data: Vec<u8>,
}

/// Resource tracking for linear types
#[derive(Debug, Clone)]
pub struct ResourceTracker {
    /// Active resources
    resources: HashMap<u64, ResourceInfo>,

    /// Borrowed references
    borrows: HashMap<u64, BorrowInfo>,

    /// Next resource ID
    next_id: u64,
}

#[derive(Debug, Clone)]
pub struct ResourceInfo {
    pub type_name: String,
    pub owner: usize, // Frame index
    pub created_at: usize, // Instruction index
}

#[derive(Debug, Clone)]
pub struct BorrowInfo {
    pub resource_id: u64,
    pub is_mutable: bool,
    pub borrower: usize, // Frame index
}

impl ResourceTracker {
    pub fn new() -> Self {
        Self {
            resources: HashMap::new(),
            borrows: HashMap::new(),
            next_id: 0,
        }
    }

    pub fn create(&mut self, type_name: String, owner: usize, pc: usize) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.resources.insert(id, ResourceInfo {
            type_name,
            owner,
            created_at: pc,
        });
        id
    }

    pub fn destroy(&mut self, id: u64) -> ExecutionResult<()> {
        if self.borrows.values().any(|b| b.resource_id == id) {
            return Err(ExecutionError::ResourceError(
                "cannot destroy borrowed resource".to_string()
            ));
        }
        self.resources.remove(&id);
        Ok(())
    }

    pub fn borrow(&mut self, id: u64, borrower: usize, is_mutable: bool) -> ExecutionResult<()> {
        if !self.resources.contains_key(&id) {
            return Err(ExecutionError::ResourceError(
                "resource does not exist".to_string()
            ));
        }

        // Check for conflicting borrows
        for borrow in self.borrows.values() {
            if borrow.resource_id == id {
                if is_mutable || borrow.is_mutable {
                    return Err(ExecutionError::ResourceError(
                        "conflicting borrow".to_string()
                    ));
                }
            }
        }

        self.borrows.insert(id, BorrowInfo {
            resource_id: id,
            is_mutable,
            borrower,
        });
        Ok(())
    }

    pub fn release(&mut self, id: u64, borrower: usize) {
        self.borrows.retain(|_, b| !(b.resource_id == id && b.borrower == borrower));
    }

    pub fn transfer(&mut self, id: u64, new_owner: usize) -> ExecutionResult<()> {
        if self.borrows.values().any(|b| b.resource_id == id) {
            return Err(ExecutionError::ResourceError(
                "cannot transfer borrowed resource".to_string()
            ));
        }
        if let Some(resource) = self.resources.get_mut(&id) {
            resource.owner = new_owner;
            Ok(())
        } else {
            Err(ExecutionError::ResourceError("resource does not exist".to_string()))
        }
    }

    /// Check that all resources owned by a frame are properly handled
    pub fn check_frame_exit(&self, frame_index: usize) -> ExecutionResult<()> {
        for (id, info) in &self.resources {
            if info.owner == frame_index {
                return Err(ExecutionError::ResourceError(
                    format!("resource {} leaked on frame exit", id)
                ));
            }
        }
        Ok(())
    }
}

impl Default for ResourceTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// QVM Executor
pub struct Executor {
    /// Execution stack
    stack: Stack,

    /// Linear memory
    memory: Memory,

    /// Contract storage
    storage: Storage,

    /// Heap allocator
    heap: Heap,

    /// Call frames
    frames: Vec<CallFrame>,

    /// Gas meter
    gas: GasMeter,

    /// Execution context
    context: ExecutionContext,

    /// Emitted logs
    logs: Vec<Log>,

    /// Return data from last call
    return_data: Vec<u8>,

    /// Resource tracker
    resources: ResourceTracker,

    /// Contract bytecode (function name -> instructions)
    contracts: HashMap<String, Vec<Instruction>>,
}

impl Executor {
    /// Create a new executor
    pub fn new(gas_limit: u64) -> Self {
        Self {
            stack: Stack::new(),
            memory: Memory::new(),
            storage: Storage::new(),
            heap: Heap::new(),
            frames: Vec::new(),
            gas: GasMeter::new(gas_limit),
            context: ExecutionContext::default(),
            logs: Vec::new(),
            return_data: Vec::new(),
            resources: ResourceTracker::new(),
            contracts: HashMap::new(),
        }
    }

    /// Set execution context
    pub fn with_context(mut self, context: ExecutionContext) -> Self {
        self.context = context;
        self
    }

    /// Load contract bytecode
    pub fn load_contract(&mut self, name: String, code: Vec<Instruction>) {
        self.contracts.insert(name, code);
    }

    /// Execute bytecode
    pub fn execute(&mut self, code: &[Instruction]) -> ExecutionResult<ExecutionOutput> {
        // Create initial frame
        self.frames.push(CallFrame::new("main".to_string(), 0));

        let result = self.run(code);

        // Collect output
        let output = ExecutionOutput {
            data: self.return_data.clone(),
            value: self.stack.pop().ok(),
            gas_used: self.gas.used(),
            gas_refund: self.gas.refund(),
            logs: self.logs.clone(),
            storage_changes: self.storage.get_modified()
                .map(|(k, v)| (*k, *v))
                .collect(),
            success: result.is_ok(),
        };

        match result {
            Ok(()) => Ok(output),
            Err(ExecutionError::Halt) => Ok(output),
            Err(ExecutionError::Revert(_)) => Ok(ExecutionOutput { success: false, ..output }),
            Err(e) => Err(e),
        }
    }

    /// Main execution loop
    fn run(&mut self, code: &[Instruction]) -> ExecutionResult<()> {
        while let Some(frame) = self.frames.last_mut() {
            if frame.pc >= code.len() {
                // End of code, implicit return
                self.frames.pop();
                continue;
            }

            let instr = &code[frame.pc];
            frame.pc += 1;

            // Consume gas for opcode
            self.gas.consume_opcode(instr.opcode)?;

            // Execute instruction
            match self.execute_instruction(instr)? {
                ControlFlow::Continue => {}
                ControlFlow::Jump(target) => {
                    if let Some(frame) = self.frames.last_mut() {
                        frame.pc = target;
                    }
                }
                ControlFlow::Return => {
                    self.frames.pop();
                }
                ControlFlow::Halt => {
                    return Err(ExecutionError::Halt);
                }
                ControlFlow::Revert(msg) => {
                    return Err(ExecutionError::Revert(msg));
                }
            }
        }

        Ok(())
    }

    /// Execute a single instruction
    fn execute_instruction(&mut self, instr: &Instruction) -> ExecutionResult<ControlFlow> {
        match instr.opcode {
            // Stack operations
            Opcode::Push => {
                let value = self.decode_push_value(instr)?;
                self.stack.push(value)?;
            }
            Opcode::Pop => {
                self.stack.pop()?;
            }
            Opcode::Dup => {
                self.stack.dup()?;
            }
            Opcode::Swap => {
                self.stack.swap()?;
            }

            // Arithmetic
            Opcode::Add => {
                let b = self.pop_u256()?;
                let a = self.pop_u256()?;
                self.stack.push(Value::U256(a.overflowing_add(b).0))?;
            }
            Opcode::Sub => {
                let b = self.pop_u256()?;
                let a = self.pop_u256()?;
                self.stack.push(Value::U256(a.overflowing_sub(b).0))?;
            }
            Opcode::Mul => {
                let b = self.pop_u256()?;
                let a = self.pop_u256()?;
                self.stack.push(Value::U256(a.overflowing_mul(b).0))?;
            }
            Opcode::Div => {
                let b = self.pop_u256()?;
                let a = self.pop_u256()?;
                if b.is_zero() {
                    self.stack.push(Value::zero())?;
                } else {
                    self.stack.push(Value::U256(a / b))?;
                }
            }
            Opcode::Mod => {
                let b = self.pop_u256()?;
                let a = self.pop_u256()?;
                if b.is_zero() {
                    self.stack.push(Value::zero())?;
                } else {
                    self.stack.push(Value::U256(a % b))?;
                }
            }
            Opcode::Pow => {
                let exp = self.pop_u256()?;
                let base = self.pop_u256()?;
                // Simple exponentiation (could overflow)
                let result = self.pow_u256(base, exp);
                self.stack.push(Value::U256(result))?;
            }
            Opcode::Neg => {
                let a = self.pop_u256()?;
                self.stack.push(Value::U256(U256::zero().overflowing_sub(a).0))?;
            }

            // Comparison
            Opcode::Eq => {
                let b = self.stack.pop()?;
                let a = self.stack.pop()?;
                self.stack.push(Value::Bool(a == b))?;
            }
            Opcode::Ne => {
                let b = self.stack.pop()?;
                let a = self.stack.pop()?;
                self.stack.push(Value::Bool(a != b))?;
            }
            Opcode::Lt => {
                let b = self.pop_u256()?;
                let a = self.pop_u256()?;
                self.stack.push(Value::Bool(a < b))?;
            }
            Opcode::Le => {
                let b = self.pop_u256()?;
                let a = self.pop_u256()?;
                self.stack.push(Value::Bool(a <= b))?;
            }
            Opcode::Gt => {
                let b = self.pop_u256()?;
                let a = self.pop_u256()?;
                self.stack.push(Value::Bool(a > b))?;
            }
            Opcode::Ge => {
                let b = self.pop_u256()?;
                let a = self.pop_u256()?;
                self.stack.push(Value::Bool(a >= b))?;
            }

            // Logical
            Opcode::And => {
                let b = self.pop_bool()?;
                let a = self.pop_bool()?;
                self.stack.push(Value::Bool(a && b))?;
            }
            Opcode::Or => {
                let b = self.pop_bool()?;
                let a = self.pop_bool()?;
                self.stack.push(Value::Bool(a || b))?;
            }
            Opcode::Not => {
                let a = self.pop_bool()?;
                self.stack.push(Value::Bool(!a))?;
            }

            // Bitwise
            Opcode::BitAnd => {
                let b = self.pop_u256()?;
                let a = self.pop_u256()?;
                self.stack.push(Value::U256(a & b))?;
            }
            Opcode::BitOr => {
                let b = self.pop_u256()?;
                let a = self.pop_u256()?;
                self.stack.push(Value::U256(a | b))?;
            }
            Opcode::BitXor => {
                let b = self.pop_u256()?;
                let a = self.pop_u256()?;
                self.stack.push(Value::U256(a ^ b))?;
            }
            Opcode::BitNot => {
                let a = self.pop_u256()?;
                self.stack.push(Value::U256(!a))?;
            }
            Opcode::Shl => {
                let shift = self.pop_u256()?;
                let a = self.pop_u256()?;
                if shift >= U256::from(256) {
                    self.stack.push(Value::zero())?;
                } else {
                    self.stack.push(Value::U256(a << shift.as_u32()))?;
                }
            }
            Opcode::Shr => {
                let shift = self.pop_u256()?;
                let a = self.pop_u256()?;
                if shift >= U256::from(256) {
                    self.stack.push(Value::zero())?;
                } else {
                    self.stack.push(Value::U256(a >> shift.as_u32()))?;
                }
            }

            // Memory
            Opcode::Load => {
                let offset = self.pop_u256()?.as_usize();
                let value = self.memory.read_u256(offset)?;
                self.stack.push(Value::U256(value))?;
            }
            Opcode::Store => {
                let value = self.pop_u256()?;
                let offset = self.pop_u256()?.as_usize();
                self.memory.write_u256(offset, value)?;
            }
            Opcode::LoadLocal => {
                let index = self.decode_u16(instr)? as usize;
                let frame = self.current_frame()?;
                let value = frame.get_local(index)?.clone();
                self.stack.push(value)?;
            }
            Opcode::StoreLocal => {
                let index = self.decode_u16(instr)? as usize;
                let value = self.stack.pop()?;
                let frame = self.current_frame_mut()?;
                frame.set_local(index, value)?;
            }

            // Storage
            Opcode::SLoad => {
                let key = self.pop_h256()?;
                self.gas.consume_sload(key)?;
                let value = self.storage.load(key);
                self.stack.push(Value::U256(U256::from_big_endian(value.as_bytes())))?;
            }
            Opcode::SStore => {
                if self.context.is_static {
                    return Err(ExecutionError::ResourceError(
                        "cannot write storage in static call".to_string()
                    ));
                }
                let value = self.pop_u256()?;
                let key = self.pop_h256()?;

                let original = self.storage.original_value(key);
                let current = self.storage.load(key);

                self.gas.consume_sstore(
                    key,
                    original.is_zero(),
                    current.is_zero(),
                    value.is_zero(),
                )?;

                let mut value_bytes = [0u8; 32];
                value.to_big_endian(&mut value_bytes);
                self.storage.store(key, H256::from(value_bytes));
            }

            // Control flow
            Opcode::Jump => {
                let target = self.decode_u16(instr)? as usize;
                return Ok(ControlFlow::Jump(target));
            }
            Opcode::JumpIf => {
                let target = self.decode_u16(instr)? as usize;
                let cond = self.stack.pop()?;
                if cond.is_truthy() {
                    return Ok(ControlFlow::Jump(target));
                }
            }
            Opcode::JumpIfNot => {
                let target = self.decode_u16(instr)? as usize;
                let cond = self.stack.pop()?;
                if !cond.is_truthy() {
                    return Ok(ControlFlow::Jump(target));
                }
            }
            Opcode::Call => {
                // Internal function call
                let func_name = self.decode_string(instr)?;
                self.call_function(&func_name)?;
            }
            Opcode::Return => {
                // Check resources before returning
                let frame_index = self.frames.len() - 1;
                self.resources.check_frame_exit(frame_index)?;
                return Ok(ControlFlow::Return);
            }
            Opcode::Revert => {
                return Ok(ControlFlow::Revert("execution reverted".to_string()));
            }
            Opcode::Halt => {
                return Ok(ControlFlow::Halt);
            }

            // Contract info
            Opcode::Address => {
                self.stack.push(Value::Address(self.context.address))?;
            }
            Opcode::Balance => {
                // Simplified: return 0
                self.stack.push(Value::zero())?;
            }
            Opcode::Caller => {
                self.stack.push(Value::Address(self.context.caller))?;
            }
            Opcode::CallValue => {
                self.stack.push(Value::U256(self.context.value))?;
            }
            Opcode::Origin => {
                self.stack.push(Value::Address(self.context.origin))?;
            }
            Opcode::GasPrice => {
                self.stack.push(Value::U256(self.context.gas_price))?;
            }
            Opcode::BlockHash => {
                // Simplified: return zero
                self.stack.push(Value::Bytes32(H256::zero()))?;
            }
            Opcode::Coinbase => {
                self.stack.push(Value::Address(self.context.coinbase))?;
            }
            Opcode::Timestamp => {
                self.stack.push(Value::from_u64(self.context.timestamp))?;
            }
            Opcode::BlockNumber => {
                self.stack.push(Value::from_u64(self.context.block_number))?;
            }
            Opcode::Difficulty => {
                self.stack.push(Value::U256(self.context.difficulty))?;
            }
            Opcode::GasLimit => {
                self.stack.push(Value::from_u64(self.context.gas_limit))?;
            }
            Opcode::ChainId => {
                self.stack.push(Value::from_u64(self.context.chain_id))?;
            }
            Opcode::SelfBalance => {
                // Simplified: return 0
                self.stack.push(Value::zero())?;
            }
            Opcode::Gas => {
                self.stack.push(Value::from_u64(self.gas.remaining()))?;
            }

            // Logs
            Opcode::Log0 | Opcode::Log1 | Opcode::Log2 | Opcode::Log3 | Opcode::Log4 => {
                if self.context.is_static {
                    return Err(ExecutionError::ResourceError(
                        "cannot emit log in static call".to_string()
                    ));
                }
                let topic_count = match instr.opcode {
                    Opcode::Log0 => 0,
                    Opcode::Log1 => 1,
                    Opcode::Log2 => 2,
                    Opcode::Log3 => 3,
                    Opcode::Log4 => 4,
                    _ => unreachable!(),
                };
                self.emit_log(topic_count)?;
            }

            // Crypto
            Opcode::Keccak256 => {
                let size = self.pop_u256()?.as_usize();
                let offset = self.pop_u256()?.as_usize();
                let data = self.memory.read(offset, size)?;

                // Use blake3 for hashing
                let hash = blake3::hash(data);
                self.stack.push(Value::Bytes32(H256::from_slice(hash.as_bytes())))?;
            }

            // Resource operations
            Opcode::ResourceCreate => {
                let type_name = self.decode_string(instr)?;
                let frame_index = self.frames.len() - 1;
                let pc = self.current_frame()?.pc;
                let id = self.resources.create(type_name.clone(), frame_index, pc);

                // Create resource value with fields from stack
                let value = Value::Resource {
                    type_name,
                    fields: Vec::new(),
                    id,
                };
                self.stack.push(value)?;
            }
            Opcode::ResourceDestroy => {
                let resource = self.stack.pop()?;
                if let Value::Resource { id, .. } = resource {
                    self.resources.destroy(id)?;
                } else {
                    return Err(ExecutionError::TypeError {
                        expected: "resource".to_string(),
                        found: resource.type_name().to_string(),
                    });
                }
            }
            Opcode::ResourceMove => {
                // Move resource to new owner (new frame)
                let resource = self.stack.pop()?;
                if let Value::Resource { id, .. } = &resource {
                    let new_owner = self.frames.len() - 1;
                    self.resources.transfer(*id, new_owner)?;
                    self.stack.push(resource)?;
                } else {
                    return Err(ExecutionError::TypeError {
                        expected: "resource".to_string(),
                        found: resource.type_name().to_string(),
                    });
                }
            }
            Opcode::ResourceBorrow => {
                let resource = self.stack.peek()?;
                if let Value::Resource { id, .. } = resource {
                    let borrower = self.frames.len() - 1;
                    self.resources.borrow(*id, borrower, false)?;
                }
            }
            Opcode::ResourceBorrowMut => {
                let resource = self.stack.peek()?;
                if let Value::Resource { id, .. } = resource {
                    let borrower = self.frames.len() - 1;
                    self.resources.borrow(*id, borrower, true)?;
                }
            }
            Opcode::ResourceCopy => {
                // Only allowed for resources with Copy ability
                return Err(ExecutionError::ResourceError(
                    "resource copy requires Copy ability".to_string()
                ));
            }

            // Parallel hints (no-op in interpreter)
            Opcode::ParallelStart | Opcode::ParallelEnd
            | Opcode::StateRead | Opcode::StateWrite => {
                // These are hints for parallel execution
                // In the interpreter, they are no-ops
            }

            // External calls (simplified)
            Opcode::ExternalCall | Opcode::StaticCall | Opcode::DelegateCall => {
                // Simplified: just push success
                self.stack.push(Value::Bool(true))?;
            }
            Opcode::Create | Opcode::Create2 => {
                // Simplified: push zero address
                self.stack.push(Value::Address(H160::zero()))?;
            }

            Opcode::Sha256 | Opcode::Ripemd160 | Opcode::Ecrecover => {
                // Simplified: push zero
                self.stack.push(Value::Bytes32(H256::zero()))?;
            }

            Opcode::Nop => {}
        }

        Ok(ControlFlow::Continue)
    }

    // Helper methods

    fn current_frame(&self) -> ExecutionResult<&CallFrame> {
        self.frames.last().ok_or(ExecutionError::Internal("no frame".to_string()))
    }

    fn current_frame_mut(&mut self) -> ExecutionResult<&mut CallFrame> {
        self.frames.last_mut().ok_or(ExecutionError::Internal("no frame".to_string()))
    }

    fn pop_u256(&mut self) -> ExecutionResult<U256> {
        let value = self.stack.pop()?;
        value.as_u256().ok_or(ExecutionError::TypeError {
            expected: "u256".to_string(),
            found: value.type_name().to_string(),
        })
    }

    fn pop_bool(&mut self) -> ExecutionResult<bool> {
        let value = self.stack.pop()?;
        value.as_bool().ok_or(ExecutionError::TypeError {
            expected: "bool".to_string(),
            found: value.type_name().to_string(),
        })
    }

    fn pop_h256(&mut self) -> ExecutionResult<H256> {
        let value = self.stack.pop()?;
        match value {
            Value::Bytes32(h) => Ok(h),
            Value::U256(n) => {
                let mut bytes = [0u8; 32];
                n.to_big_endian(&mut bytes);
                Ok(H256::from(bytes))
            }
            _ => Err(ExecutionError::TypeError {
                expected: "bytes32".to_string(),
                found: value.type_name().to_string(),
            }),
        }
    }

    fn decode_push_value(&self, instr: &Instruction) -> ExecutionResult<Value> {
        if let Some(ref operand) = instr.operand {
            if operand.len() == 32 {
                Ok(Value::U256(U256::from_big_endian(operand)))
            } else {
                let mut bytes = [0u8; 32];
                let start = 32 - operand.len();
                bytes[start..].copy_from_slice(operand);
                Ok(Value::U256(U256::from_big_endian(&bytes)))
            }
        } else {
            Ok(Value::zero())
        }
    }

    fn decode_u16(&self, instr: &Instruction) -> ExecutionResult<u16> {
        if let Some(ref operand) = instr.operand {
            if operand.len() >= 2 {
                Ok(u16::from_be_bytes([operand[0], operand[1]]))
            } else {
                Ok(0)
            }
        } else {
            Ok(0)
        }
    }

    fn decode_string(&self, instr: &Instruction) -> ExecutionResult<String> {
        if let Some(ref operand) = instr.operand {
            String::from_utf8(operand.clone())
                .map_err(|_| ExecutionError::Internal("invalid string".to_string()))
        } else {
            Ok(String::new())
        }
    }

    fn pow_u256(&self, base: U256, exp: U256) -> U256 {
        if exp.is_zero() {
            return U256::one();
        }
        if base.is_zero() {
            return U256::zero();
        }

        let mut result = U256::one();
        let mut base = base;
        let mut exp = exp;

        while !exp.is_zero() {
            if exp & U256::one() == U256::one() {
                result = result.overflowing_mul(base).0;
            }
            exp >>= 1;
            base = base.overflowing_mul(base).0;
        }

        result
    }

    fn call_function(&mut self, _name: &str) -> ExecutionResult<()> {
        // Simplified: just create a new frame
        if self.frames.len() >= MAX_CALL_DEPTH {
            return Err(ExecutionError::CallDepthExceeded(MAX_CALL_DEPTH));
        }
        // In a full implementation, we would look up the function and jump to it
        Ok(())
    }

    fn emit_log(&mut self, topic_count: usize) -> ExecutionResult<()> {
        let size = self.pop_u256()?.as_usize();
        let offset = self.pop_u256()?.as_usize();

        let mut topics = Vec::with_capacity(topic_count);
        for _ in 0..topic_count {
            topics.push(self.pop_h256()?);
        }
        topics.reverse();

        let data = self.memory.read(offset, size)?.to_vec();

        self.logs.push(Log {
            address: self.context.address,
            topics,
            data,
        });

        Ok(())
    }
}

/// Control flow result
enum ControlFlow {
    Continue,
    Jump(usize),
    Return,
    Halt,
    Revert(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_push(value: u64) -> Instruction {
        let mut bytes = [0u8; 32];
        U256::from(value).to_big_endian(&mut bytes);
        Instruction::with_operand(Opcode::Push, bytes.to_vec())
    }

    #[test]
    fn test_simple_add() {
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
    fn test_comparison() {
        let code = vec![
            make_push(10),
            make_push(20),
            Instruction::new(Opcode::Lt),
            Instruction::new(Opcode::Return),
        ];

        let mut executor = Executor::new(100000);
        let result = executor.execute(&code).unwrap();

        assert!(result.success);
        assert_eq!(result.value, Some(Value::Bool(true)));
    }

    #[test]
    fn test_storage() {
        let code = vec![
            make_push(0),   // key (pushed first, goes to bottom)
            make_push(42),  // value (pushed second, goes to top)
            Instruction::new(Opcode::SStore),
            make_push(0),   // key
            Instruction::new(Opcode::SLoad),
            Instruction::new(Opcode::Return),
        ];

        let mut executor = Executor::new(100000);
        let result = executor.execute(&code).unwrap();

        assert!(result.success);
        assert_eq!(result.value, Some(Value::from_u64(42)));
    }

    #[test]
    fn test_conditional_jump() {
        let code = vec![
            // 0: push true
            make_push(1),
            // 1: jump if true to 4
            Instruction::with_operand(Opcode::JumpIf, vec![0, 4]),
            // 2: push 0 (not executed)
            make_push(0),
            // 3: return
            Instruction::new(Opcode::Return),
            // 4: push 100
            make_push(100),
            // 5: return
            Instruction::new(Opcode::Return),
        ];

        let mut executor = Executor::new(100000);
        let result = executor.execute(&code).unwrap();

        assert!(result.success);
        assert_eq!(result.value, Some(Value::from_u64(100)));
    }

    #[test]
    fn test_gas_metering() {
        let code = vec![
            make_push(1),
            make_push(2),
            Instruction::new(Opcode::Add),
            Instruction::new(Opcode::Return),
        ];

        let mut executor = Executor::new(100000);
        let result = executor.execute(&code).unwrap();

        assert!(result.success);
        assert!(result.gas_used > 0);
    }
}
