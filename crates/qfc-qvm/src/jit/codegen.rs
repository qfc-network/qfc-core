//! Code generation from QVM bytecode to Cranelift IR.

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types::I64;
use cranelift_codegen::ir::{Block, InstBuilder, Type, Value};
use cranelift_frontend::FunctionBuilder;

use qfc_qsc::{Instruction, Opcode};

use super::runtime::RuntimeFunctions;
use super::{JitError, JitResult};

/// Maximum stack depth for JIT compilation.
const MAX_STACK_DEPTH: usize = 1024;

/// Code generator that converts QVM bytecode to Cranelift IR.
pub struct CodeGenerator {
    pointer_type: Type,
    param_count: u8,
    local_count: u8,
    bounds_check: bool,
    /// Virtual stack for tracking values.
    stack: Vec<Value>,
    /// Local variable storage.
    locals: Vec<Value>,
    /// Runtime pointer parameter.
    runtime_ptr: Value,
    /// Track if current block is terminated.
    block_terminated: bool,
}

impl CodeGenerator {
    /// Create a new code generator.
    pub fn new(pointer_type: Type, param_count: u8, local_count: u8, bounds_check: bool) -> Self {
        Self {
            pointer_type,
            param_count,
            local_count,
            bounds_check,
            stack: Vec::with_capacity(MAX_STACK_DEPTH),
            locals: Vec::new(),
            runtime_ptr: Value::from_u32(0), // Will be set during generation
            block_terminated: false,
        }
    }

    /// Generate code for a sequence of instructions.
    /// Takes ownership of the builder and returns it after generation.
    pub fn generate<'a>(
        mut self,
        mut builder: FunctionBuilder<'a>,
        instructions: &[Instruction],
    ) -> JitResult<FunctionBuilder<'a>> {
        // Create entry block
        let entry_block = builder.create_block();
        builder.append_block_params_for_function_params(entry_block);
        builder.switch_to_block(entry_block);
        builder.seal_block(entry_block);

        // Get runtime pointer from first parameter
        self.runtime_ptr = builder.block_params(entry_block)[0];

        // Allocate locals
        self.allocate_locals(&mut builder);

        // Create basic blocks for jump targets
        let mut blocks: Vec<Block> = Vec::new();
        blocks.push(entry_block);

        // Simple approach: one block per instruction (can be optimized later)
        for _ in 1..instructions.len() {
            blocks.push(builder.create_block());
        }

        // Track which blocks we've sealed (entry block is already sealed)
        let mut sealed: Vec<bool> = vec![false; blocks.len()];
        sealed[0] = true;

        // Generate code for each instruction
        for (i, instruction) in instructions.iter().enumerate() {
            if i > 0 {
                // Jump to this block if not already terminated
                if !self.block_terminated {
                    builder.ins().jump(blocks[i], &[]);
                }
                builder.switch_to_block(blocks[i]);
                self.block_terminated = false;
            }

            self.generate_instruction(&mut builder, instruction, &blocks, &mut sealed)?;
        }

        // Ensure function is properly terminated
        if !self.block_terminated {
            let zero = builder.ins().iconst(I64, 0);
            builder.ins().return_(&[zero]);
        }

        // Seal all remaining blocks
        for (i, block) in blocks.iter().enumerate() {
            if !sealed[i] {
                builder.seal_block(*block);
            }
        }

        Ok(builder)
    }

    fn allocate_locals(&mut self, builder: &mut FunctionBuilder) {
        // Allocate space for local variables
        let total_locals = self.param_count as usize + self.local_count as usize;
        for _ in 0..total_locals {
            let zero = builder.ins().iconst(I64, 0);
            self.locals.push(zero);
        }
    }

    fn generate_instruction(
        &mut self,
        builder: &mut FunctionBuilder,
        instruction: &Instruction,
        blocks: &[Block],
        sealed: &mut [bool],
    ) -> JitResult<()> {
        match instruction.opcode {
            // Stack operations
            Opcode::Push => {
                let value = self.read_operand_u64(instruction)?;
                let val = builder.ins().iconst(I64, value as i64);
                self.push(val)?;
            }
            Opcode::Pop => {
                self.pop()?;
            }
            Opcode::Dup => {
                let val = self.peek()?;
                self.push(val)?;
            }
            Opcode::Swap => {
                let a = self.pop()?;
                let b = self.pop()?;
                self.push(a)?;
                self.push(b)?;
            }

            // Arithmetic
            Opcode::Add => {
                let b = self.pop()?;
                let a = self.pop()?;
                let result = builder.ins().iadd(a, b);
                self.push(result)?;
            }
            Opcode::Sub => {
                let b = self.pop()?;
                let a = self.pop()?;
                let result = builder.ins().isub(a, b);
                self.push(result)?;
            }
            Opcode::Mul => {
                let b = self.pop()?;
                let a = self.pop()?;
                let result = builder.ins().imul(a, b);
                self.push(result)?;
            }
            Opcode::Div => {
                let b = self.pop()?;
                let a = self.pop()?;
                let result = builder.ins().udiv(a, b);
                self.push(result)?;
            }
            Opcode::Mod => {
                let b = self.pop()?;
                let a = self.pop()?;
                let result = builder.ins().urem(a, b);
                self.push(result)?;
            }
            Opcode::Neg => {
                let a = self.pop()?;
                let result = builder.ins().ineg(a);
                self.push(result)?;
            }

            // Comparison
            Opcode::Eq => {
                let b = self.pop()?;
                let a = self.pop()?;
                let cmp = builder.ins().icmp(IntCC::Equal, a, b);
                let result = builder.ins().uextend(I64, cmp);
                self.push(result)?;
            }
            Opcode::Ne => {
                let b = self.pop()?;
                let a = self.pop()?;
                let cmp = builder.ins().icmp(IntCC::NotEqual, a, b);
                let result = builder.ins().uextend(I64, cmp);
                self.push(result)?;
            }
            Opcode::Lt => {
                let b = self.pop()?;
                let a = self.pop()?;
                let cmp = builder.ins().icmp(IntCC::UnsignedLessThan, a, b);
                let result = builder.ins().uextend(I64, cmp);
                self.push(result)?;
            }
            Opcode::Le => {
                let b = self.pop()?;
                let a = self.pop()?;
                let cmp = builder.ins().icmp(IntCC::UnsignedLessThanOrEqual, a, b);
                let result = builder.ins().uextend(I64, cmp);
                self.push(result)?;
            }
            Opcode::Gt => {
                let b = self.pop()?;
                let a = self.pop()?;
                let cmp = builder.ins().icmp(IntCC::UnsignedGreaterThan, a, b);
                let result = builder.ins().uextend(I64, cmp);
                self.push(result)?;
            }
            Opcode::Ge => {
                let b = self.pop()?;
                let a = self.pop()?;
                let cmp = builder.ins().icmp(IntCC::UnsignedGreaterThanOrEqual, a, b);
                let result = builder.ins().uextend(I64, cmp);
                self.push(result)?;
            }

            // Logical
            Opcode::And => {
                let b = self.pop()?;
                let a = self.pop()?;
                let result = builder.ins().band(a, b);
                self.push(result)?;
            }
            Opcode::Or => {
                let b = self.pop()?;
                let a = self.pop()?;
                let result = builder.ins().bor(a, b);
                self.push(result)?;
            }
            Opcode::Not => {
                let a = self.pop()?;
                let zero = builder.ins().iconst(I64, 0);
                let cmp = builder.ins().icmp(IntCC::Equal, a, zero);
                let result = builder.ins().uextend(I64, cmp);
                self.push(result)?;
            }

            // Bitwise
            Opcode::BitAnd => {
                let b = self.pop()?;
                let a = self.pop()?;
                let result = builder.ins().band(a, b);
                self.push(result)?;
            }
            Opcode::BitOr => {
                let b = self.pop()?;
                let a = self.pop()?;
                let result = builder.ins().bor(a, b);
                self.push(result)?;
            }
            Opcode::BitXor => {
                let b = self.pop()?;
                let a = self.pop()?;
                let result = builder.ins().bxor(a, b);
                self.push(result)?;
            }
            Opcode::BitNot => {
                let a = self.pop()?;
                let result = builder.ins().bnot(a);
                self.push(result)?;
            }
            Opcode::Shl => {
                let b = self.pop()?;
                let a = self.pop()?;
                let result = builder.ins().ishl(a, b);
                self.push(result)?;
            }
            Opcode::Shr => {
                let b = self.pop()?;
                let a = self.pop()?;
                let result = builder.ins().ushr(a, b);
                self.push(result)?;
            }

            // Local variables
            Opcode::LoadLocal => {
                let idx = self.read_operand_u16(instruction)? as usize;
                if idx < self.locals.len() {
                    let val = self.locals[idx];
                    self.push(val)?;
                } else {
                    return Err(JitError::InvalidBytecode(format!(
                        "local index {} out of bounds",
                        idx
                    )));
                }
            }
            Opcode::StoreLocal => {
                let idx = self.read_operand_u16(instruction)? as usize;
                let val = self.pop()?;
                if idx < self.locals.len() {
                    self.locals[idx] = val;
                } else {
                    return Err(JitError::InvalidBytecode(format!(
                        "local index {} out of bounds",
                        idx
                    )));
                }
            }

            // Control flow
            Opcode::Jump => {
                let target = self.read_operand_u16(instruction)? as usize;
                if target < blocks.len() {
                    // Seal the target block if it hasn't been sealed yet
                    if !sealed[target] {
                        builder.seal_block(blocks[target]);
                        sealed[target] = true;
                    }
                    builder.ins().jump(blocks[target], &[]);
                    self.block_terminated = true;
                }
            }
            Opcode::JumpIf => {
                let target = self.read_operand_u16(instruction)? as usize;
                let cond = self.pop()?;
                if target < blocks.len() {
                    let zero = builder.ins().iconst(I64, 0);
                    let cmp = builder.ins().icmp(IntCC::NotEqual, cond, zero);

                    // Create a fallthrough block
                    let next_block = builder.create_block();
                    builder
                        .ins()
                        .brif(cmp, blocks[target], &[], next_block, &[]);
                    builder.switch_to_block(next_block);
                    builder.seal_block(next_block);
                    // Note: conditional branch doesn't terminate the block in the fallthrough case
                }
            }
            Opcode::JumpIfNot => {
                let target = self.read_operand_u16(instruction)? as usize;
                let cond = self.pop()?;
                if target < blocks.len() {
                    let zero = builder.ins().iconst(I64, 0);
                    let cmp = builder.ins().icmp(IntCC::Equal, cond, zero);

                    let next_block = builder.create_block();
                    builder
                        .ins()
                        .brif(cmp, blocks[target], &[], next_block, &[]);
                    builder.switch_to_block(next_block);
                    builder.seal_block(next_block);
                    // Note: conditional branch doesn't terminate the block in the fallthrough case
                }
            }

            // Return
            Opcode::Return => {
                let result = if !self.stack.is_empty() {
                    self.pop()?
                } else {
                    builder.ins().iconst(I64, 0)
                };
                builder.ins().return_(&[result]);
                self.block_terminated = true;
            }

            // Storage operations - delegate to runtime
            Opcode::SLoad => {
                let key = self.pop()?;
                let result = self.call_runtime(builder, RuntimeFunctions::SLoad, &[key])?;
                self.push(result)?;
            }
            Opcode::SStore => {
                let value = self.pop()?;
                let key = self.pop()?;
                self.call_runtime(builder, RuntimeFunctions::SStore, &[key, value])?;
            }

            // Context - delegate to runtime
            Opcode::Caller => {
                let result = self.call_runtime(builder, RuntimeFunctions::Caller, &[])?;
                self.push(result)?;
            }
            Opcode::CallValue => {
                let result = self.call_runtime(builder, RuntimeFunctions::CallValue, &[])?;
                self.push(result)?;
            }
            Opcode::Address => {
                let result = self.call_runtime(builder, RuntimeFunctions::Address, &[])?;
                self.push(result)?;
            }
            Opcode::BlockNumber => {
                let result = self.call_runtime(builder, RuntimeFunctions::BlockNumber, &[])?;
                self.push(result)?;
            }
            Opcode::Timestamp => {
                let result = self.call_runtime(builder, RuntimeFunctions::Timestamp, &[])?;
                self.push(result)?;
            }
            Opcode::Gas => {
                let result = self.call_runtime(builder, RuntimeFunctions::Gas, &[])?;
                self.push(result)?;
            }

            // Halt
            Opcode::Halt | Opcode::Nop => {
                // No-op
            }

            // Unsupported opcodes - fall back to interpreter
            _ => {
                return Err(JitError::UnsupportedOpcode(instruction.opcode as u8));
            }
        }

        Ok(())
    }

    fn call_runtime(
        &mut self,
        builder: &mut FunctionBuilder,
        _func: RuntimeFunctions,
        _args: &[Value],
    ) -> JitResult<Value> {
        // For now, return a dummy value
        // In a full implementation, we would call the actual runtime function
        let result = builder.ins().iconst(I64, 0);
        Ok(result)
    }

    fn push(&mut self, value: Value) -> JitResult<()> {
        if self.bounds_check && self.stack.len() >= MAX_STACK_DEPTH {
            return Err(JitError::ExecutionError("stack overflow".to_string()));
        }
        self.stack.push(value);
        Ok(())
    }

    fn pop(&mut self) -> JitResult<Value> {
        self.stack
            .pop()
            .ok_or_else(|| JitError::ExecutionError("stack underflow".to_string()))
    }

    fn peek(&self) -> JitResult<Value> {
        self.stack
            .last()
            .copied()
            .ok_or_else(|| JitError::ExecutionError("stack underflow".to_string()))
    }

    fn read_operand_u64(&self, instruction: &Instruction) -> JitResult<u64> {
        let operand = instruction
            .operand
            .as_ref()
            .ok_or_else(|| JitError::InvalidBytecode("missing operand".to_string()))?;

        if operand.len() < 8 {
            return Err(JitError::InvalidBytecode("operand too short".to_string()));
        }

        Ok(u64::from_le_bytes([
            operand[0], operand[1], operand[2], operand[3], operand[4], operand[5], operand[6],
            operand[7],
        ]))
    }

    fn read_operand_u16(&self, instruction: &Instruction) -> JitResult<u16> {
        let operand = instruction
            .operand
            .as_ref()
            .ok_or_else(|| JitError::InvalidBytecode("missing operand".to_string()))?;

        if operand.len() < 2 {
            return Err(JitError::InvalidBytecode("operand too short".to_string()));
        }

        Ok(u16::from_le_bytes([operand[0], operand[1]]))
    }
}
