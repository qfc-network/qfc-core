//! QVM Memory management
//!
//! Handles stack, heap, and memory operations for the VM.

use primitive_types::{H256, U256};
use std::collections::HashMap;
use thiserror::Error;

use crate::value::Value;

/// Memory errors
#[derive(Debug, Error, Clone)]
pub enum MemoryError {
    #[error("stack overflow: max depth {0} exceeded")]
    StackOverflow(usize),

    #[error("stack underflow: attempted to pop from empty stack")]
    StackUnderflow,

    #[error("out of memory: requested {requested} bytes, available {available}")]
    OutOfMemory { requested: usize, available: usize },

    #[error("invalid memory access at offset {0}")]
    InvalidAccess(usize),

    #[error("local variable index {0} out of bounds (max {1})")]
    LocalOutOfBounds(usize, usize),

    #[error("heap allocation failed")]
    AllocationFailed,
}

pub type MemoryResult<T> = Result<T, MemoryError>;

/// Maximum stack depth
pub const MAX_STACK_DEPTH: usize = 1024;

/// Maximum memory size (1 MB)
pub const MAX_MEMORY_SIZE: usize = 1024 * 1024;

/// Maximum number of locals per frame
pub const MAX_LOCALS: usize = 256;

/// QVM Stack
#[derive(Debug, Clone)]
pub struct Stack {
    values: Vec<Value>,
    max_depth: usize,
}

impl Stack {
    pub fn new() -> Self {
        Self::with_max_depth(MAX_STACK_DEPTH)
    }

    pub fn with_max_depth(max_depth: usize) -> Self {
        Self {
            values: Vec::with_capacity(64),
            max_depth,
        }
    }

    /// Push a value onto the stack
    pub fn push(&mut self, value: Value) -> MemoryResult<()> {
        if self.values.len() >= self.max_depth {
            return Err(MemoryError::StackOverflow(self.max_depth));
        }
        self.values.push(value);
        Ok(())
    }

    /// Pop a value from the stack
    pub fn pop(&mut self) -> MemoryResult<Value> {
        self.values.pop().ok_or(MemoryError::StackUnderflow)
    }

    /// Peek at the top value without removing it
    pub fn peek(&self) -> MemoryResult<&Value> {
        self.values.last().ok_or(MemoryError::StackUnderflow)
    }

    /// Peek at a value at depth n from the top (0 = top)
    pub fn peek_at(&self, depth: usize) -> MemoryResult<&Value> {
        let len = self.values.len();
        if depth >= len {
            return Err(MemoryError::StackUnderflow);
        }
        Ok(&self.values[len - 1 - depth])
    }

    /// Duplicate the top value
    pub fn dup(&mut self) -> MemoryResult<()> {
        let value = self.peek()?.clone();
        self.push(value)
    }

    /// Duplicate the value at depth n
    pub fn dup_at(&mut self, depth: usize) -> MemoryResult<()> {
        let value = self.peek_at(depth)?.clone();
        self.push(value)
    }

    /// Swap the top two values
    pub fn swap(&mut self) -> MemoryResult<()> {
        let len = self.values.len();
        if len < 2 {
            return Err(MemoryError::StackUnderflow);
        }
        self.values.swap(len - 1, len - 2);
        Ok(())
    }

    /// Swap the top value with the value at depth n
    pub fn swap_at(&mut self, depth: usize) -> MemoryResult<()> {
        let len = self.values.len();
        if depth >= len || len == 0 {
            return Err(MemoryError::StackUnderflow);
        }
        self.values.swap(len - 1, len - 1 - depth);
        Ok(())
    }

    /// Get current stack depth
    pub fn depth(&self) -> usize {
        self.values.len()
    }

    /// Check if stack is empty
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Clear the stack
    pub fn clear(&mut self) {
        self.values.clear();
    }

    /// Pop n values from the stack
    pub fn pop_n(&mut self, n: usize) -> MemoryResult<Vec<Value>> {
        if self.values.len() < n {
            return Err(MemoryError::StackUnderflow);
        }
        let start = self.values.len() - n;
        Ok(self.values.drain(start..).collect())
    }
}

impl Default for Stack {
    fn default() -> Self {
        Self::new()
    }
}

/// Call frame for function execution
#[derive(Debug, Clone)]
pub struct CallFrame {
    /// Function name/identifier
    pub function: String,

    /// Program counter (instruction pointer)
    pub pc: usize,

    /// Local variables
    pub locals: Vec<Value>,

    /// Stack base pointer (where this frame's stack starts)
    pub stack_base: usize,

    /// Return address (instruction to return to)
    pub return_pc: usize,

    /// Is this a constructor call?
    pub is_constructor: bool,

    /// Gas limit for this frame
    pub gas_limit: u64,

    /// Gas used in this frame
    pub gas_used: u64,
}

impl CallFrame {
    pub fn new(function: String, stack_base: usize) -> Self {
        Self {
            function,
            pc: 0,
            locals: Vec::with_capacity(16),
            stack_base,
            return_pc: 0,
            is_constructor: false,
            gas_limit: u64::MAX,
            gas_used: 0,
        }
    }

    /// Get a local variable
    pub fn get_local(&self, index: usize) -> MemoryResult<&Value> {
        self.locals
            .get(index)
            .ok_or(MemoryError::LocalOutOfBounds(index, self.locals.len()))
    }

    /// Set a local variable
    pub fn set_local(&mut self, index: usize, value: Value) -> MemoryResult<()> {
        if index >= MAX_LOCALS {
            return Err(MemoryError::LocalOutOfBounds(index, MAX_LOCALS));
        }
        // Extend locals if needed
        while self.locals.len() <= index {
            self.locals.push(Value::Unit);
        }
        self.locals[index] = value;
        Ok(())
    }

    /// Get remaining gas
    pub fn remaining_gas(&self) -> u64 {
        self.gas_limit.saturating_sub(self.gas_used)
    }

    /// Consume gas
    pub fn use_gas(&mut self, amount: u64) -> bool {
        if self.gas_used.saturating_add(amount) > self.gas_limit {
            false
        } else {
            self.gas_used += amount;
            true
        }
    }
}

/// Linear memory (byte-addressable)
#[derive(Debug, Clone)]
pub struct Memory {
    data: Vec<u8>,
    max_size: usize,
}

impl Memory {
    pub fn new() -> Self {
        Self::with_max_size(MAX_MEMORY_SIZE)
    }

    pub fn with_max_size(max_size: usize) -> Self {
        Self {
            data: Vec::new(),
            max_size,
        }
    }

    /// Get current memory size
    pub fn size(&self) -> usize {
        self.data.len()
    }

    /// Expand memory to accommodate offset + size
    pub fn expand(&mut self, offset: usize, size: usize) -> MemoryResult<()> {
        let required = offset.saturating_add(size);
        if required > self.max_size {
            return Err(MemoryError::OutOfMemory {
                requested: required,
                available: self.max_size,
            });
        }
        if required > self.data.len() {
            self.data.resize(required, 0);
        }
        Ok(())
    }

    /// Read bytes from memory
    pub fn read(&self, offset: usize, size: usize) -> MemoryResult<&[u8]> {
        let end = offset.saturating_add(size);
        if end > self.data.len() {
            return Err(MemoryError::InvalidAccess(offset));
        }
        Ok(&self.data[offset..end])
    }

    /// Write bytes to memory
    pub fn write(&mut self, offset: usize, data: &[u8]) -> MemoryResult<()> {
        self.expand(offset, data.len())?;
        self.data[offset..offset + data.len()].copy_from_slice(data);
        Ok(())
    }

    /// Read a U256 from memory (32 bytes, big-endian)
    pub fn read_u256(&self, offset: usize) -> MemoryResult<U256> {
        let bytes = self.read(offset, 32)?;
        Ok(U256::from_big_endian(bytes))
    }

    /// Write a U256 to memory (32 bytes, big-endian)
    pub fn write_u256(&mut self, offset: usize, value: U256) -> MemoryResult<()> {
        let mut bytes = [0u8; 32];
        value.to_big_endian(&mut bytes);
        self.write(offset, &bytes)
    }

    /// Read a single byte
    pub fn read_byte(&self, offset: usize) -> MemoryResult<u8> {
        if offset >= self.data.len() {
            return Err(MemoryError::InvalidAccess(offset));
        }
        Ok(self.data[offset])
    }

    /// Write a single byte
    pub fn write_byte(&mut self, offset: usize, value: u8) -> MemoryResult<()> {
        self.expand(offset, 1)?;
        self.data[offset] = value;
        Ok(())
    }

    /// Clear memory
    pub fn clear(&mut self) {
        self.data.clear();
    }

    /// Copy memory region
    pub fn copy_within(&mut self, src: usize, dst: usize, size: usize) -> MemoryResult<()> {
        self.expand(dst, size)?;
        if src + size > self.data.len() {
            return Err(MemoryError::InvalidAccess(src));
        }
        self.data.copy_within(src..src + size, dst);
        Ok(())
    }
}

impl Default for Memory {
    fn default() -> Self {
        Self::new()
    }
}

/// Storage interface (key-value store)
#[derive(Debug, Clone, Default)]
pub struct Storage {
    /// Current state
    slots: HashMap<H256, H256>,

    /// Original values (for gas refund calculation)
    original: HashMap<H256, H256>,

    /// Transient storage (cleared after transaction)
    transient: HashMap<H256, H256>,
}

impl Storage {
    pub fn new() -> Self {
        Self::default()
    }

    /// Load a value from storage
    pub fn load(&self, key: H256) -> H256 {
        self.slots.get(&key).copied().unwrap_or_default()
    }

    /// Store a value to storage
    pub fn store(&mut self, key: H256, value: H256) {
        // Track original value for gas refund
        if !self.original.contains_key(&key) {
            self.original
                .insert(key, self.slots.get(&key).copied().unwrap_or_default());
        }
        self.slots.insert(key, value);
    }

    /// Get original value (for gas calculation)
    pub fn original_value(&self, key: H256) -> H256 {
        self.original
            .get(&key)
            .copied()
            .unwrap_or_else(|| self.slots.get(&key).copied().unwrap_or_default())
    }

    /// Load from transient storage
    pub fn tload(&self, key: H256) -> H256 {
        self.transient.get(&key).copied().unwrap_or_default()
    }

    /// Store to transient storage
    pub fn tstore(&mut self, key: H256, value: H256) {
        self.transient.insert(key, value);
    }

    /// Clear transient storage (called at end of transaction)
    pub fn clear_transient(&mut self) {
        self.transient.clear();
    }

    /// Get all modified slots
    pub fn get_modified(&self) -> impl Iterator<Item = (&H256, &H256)> {
        self.slots.iter()
    }

    /// Check if a slot was modified
    pub fn is_modified(&self, key: &H256) -> bool {
        self.slots.contains_key(key)
    }

    /// Commit changes (clear original tracking)
    pub fn commit(&mut self) {
        self.original.clear();
    }

    /// Revert changes
    pub fn revert(&mut self) {
        for (key, original_value) in self.original.drain() {
            if original_value.is_zero() {
                self.slots.remove(&key);
            } else {
                self.slots.insert(key, original_value);
            }
        }
    }
}

/// Heap allocator for dynamic values
#[derive(Debug, Clone)]
pub struct Heap {
    objects: Vec<Option<Value>>,
    free_list: Vec<usize>,
    next_id: usize,
}

impl Heap {
    pub fn new() -> Self {
        Self {
            objects: Vec::new(),
            free_list: Vec::new(),
            next_id: 0,
        }
    }

    /// Allocate a value on the heap, returns handle
    pub fn alloc(&mut self, value: Value) -> usize {
        if let Some(idx) = self.free_list.pop() {
            self.objects[idx] = Some(value);
            idx
        } else {
            let idx = self.objects.len();
            self.objects.push(Some(value));
            idx
        }
    }

    /// Get a reference to a heap value
    pub fn get(&self, handle: usize) -> Option<&Value> {
        self.objects.get(handle).and_then(|v| v.as_ref())
    }

    /// Get a mutable reference to a heap value
    pub fn get_mut(&mut self, handle: usize) -> Option<&mut Value> {
        self.objects.get_mut(handle).and_then(|v| v.as_mut())
    }

    /// Free a heap value
    pub fn free(&mut self, handle: usize) -> Option<Value> {
        if handle < self.objects.len() {
            let value = self.objects[handle].take();
            if value.is_some() {
                self.free_list.push(handle);
            }
            value
        } else {
            None
        }
    }

    /// Generate a unique resource ID
    pub fn next_resource_id(&mut self) -> u64 {
        let id = self.next_id as u64;
        self.next_id += 1;
        id
    }
}

impl Default for Heap {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stack_push_pop() {
        let mut stack = Stack::new();
        stack.push(Value::from_u64(42)).unwrap();
        stack.push(Value::from_u64(100)).unwrap();

        assert_eq!(stack.depth(), 2);
        assert_eq!(stack.pop().unwrap(), Value::from_u64(100));
        assert_eq!(stack.pop().unwrap(), Value::from_u64(42));
        assert!(stack.is_empty());
    }

    #[test]
    fn test_stack_overflow() {
        let mut stack = Stack::with_max_depth(2);
        stack.push(Value::from_u64(1)).unwrap();
        stack.push(Value::from_u64(2)).unwrap();
        assert!(stack.push(Value::from_u64(3)).is_err());
    }

    #[test]
    fn test_memory_read_write() {
        let mut mem = Memory::new();
        mem.write(0, &[1, 2, 3, 4]).unwrap();
        assert_eq!(mem.read(0, 4).unwrap(), &[1, 2, 3, 4]);
        assert_eq!(mem.read_byte(2).unwrap(), 3);
    }

    #[test]
    fn test_memory_u256() {
        let mut mem = Memory::new();
        let value = U256::from(12345u64);
        mem.write_u256(0, value).unwrap();
        assert_eq!(mem.read_u256(0).unwrap(), value);
    }

    #[test]
    fn test_storage() {
        let mut storage = Storage::new();
        let key = H256::from_low_u64_be(1);
        let value = H256::from_low_u64_be(42);

        storage.store(key, value);
        assert_eq!(storage.load(key), value);
        assert_eq!(storage.original_value(key), H256::zero());
    }

    #[test]
    fn test_heap_alloc_free() {
        let mut heap = Heap::new();
        let h1 = heap.alloc(Value::from_u64(1));
        let h2 = heap.alloc(Value::from_u64(2));

        assert_eq!(heap.get(h1), Some(&Value::from_u64(1)));
        assert_eq!(heap.get(h2), Some(&Value::from_u64(2)));

        heap.free(h1);
        assert_eq!(heap.get(h1), None);

        // Reuse freed slot
        let h3 = heap.alloc(Value::from_u64(3));
        assert_eq!(h3, h1);
    }
}
