//! Write batch for atomic operations

/// Operation in a write batch
#[derive(Clone, Debug)]
pub enum BatchOp {
    /// Put a key-value pair
    Put {
        cf: String,
        key: Vec<u8>,
        value: Vec<u8>,
    },
    /// Delete a key
    Delete { cf: String, key: Vec<u8> },
}

/// Write batch for atomic database operations
#[derive(Default)]
pub struct WriteBatch {
    ops: Vec<BatchOp>,
}

impl WriteBatch {
    /// Create a new empty batch
    pub fn new() -> Self {
        Self { ops: Vec::new() }
    }

    /// Create a new batch with capacity
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            ops: Vec::with_capacity(capacity),
        }
    }

    /// Add a put operation
    pub fn put(&mut self, cf: &str, key: Vec<u8>, value: Vec<u8>) {
        self.ops.push(BatchOp::Put {
            cf: cf.to_string(),
            key,
            value,
        });
    }

    /// Add a delete operation
    pub fn delete(&mut self, cf: &str, key: Vec<u8>) {
        self.ops.push(BatchOp::Delete {
            cf: cf.to_string(),
            key,
        });
    }

    /// Get the number of operations
    pub fn len(&self) -> usize {
        self.ops.len()
    }

    /// Check if the batch is empty
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// Clear all operations
    pub fn clear(&mut self) {
        self.ops.clear();
    }

    /// Get operations
    pub fn ops(&self) -> &[BatchOp] {
        &self.ops
    }

    /// Take operations
    pub fn take_ops(&mut self) -> Vec<BatchOp> {
        std::mem::take(&mut self.ops)
    }
}

impl std::fmt::Debug for WriteBatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriteBatch")
            .field("ops_count", &self.ops.len())
            .finish()
    }
}
