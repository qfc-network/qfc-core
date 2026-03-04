//! Gas metering for QVM
//!
//! Implements gas costs and metering for all VM operations.

use thiserror::Error;

use qfc_qsc::Opcode;

/// Gas errors
#[derive(Debug, Error, Clone)]
pub enum GasError {
    #[error("out of gas: required {required}, available {available}")]
    OutOfGas { required: u64, available: u64 },

    #[error("gas limit exceeded: {0}")]
    LimitExceeded(u64),

    #[error("invalid gas calculation")]
    InvalidCalculation,
}

pub type GasResult<T> = Result<T, GasError>;

/// Gas costs for operations (based on EVM with QVM extensions)
#[derive(Debug, Clone, Copy)]
pub struct GasCosts {
    // Base costs
    pub zero: u64,
    pub base: u64,
    pub very_low: u64,
    pub low: u64,
    pub mid: u64,
    pub high: u64,

    // Memory costs
    pub memory_word: u64,
    pub memory_copy: u64,

    // Storage costs
    pub sload_cold: u64,
    pub sload_warm: u64,
    pub sstore_set: u64,
    pub sstore_reset: u64,
    pub sstore_clear_refund: u64,

    // Call costs
    pub call_base: u64,
    pub call_value: u64,
    pub call_new_account: u64,
    pub call_stipend: u64,

    // Create costs
    pub create: u64,
    pub create2: u64,
    pub init_code_word: u64,

    // Crypto costs
    pub keccak256_word: u64,
    pub sha256_word: u64,
    pub ecrecover: u64,

    // Log costs
    pub log: u64,
    pub log_topic: u64,
    pub log_data: u64,

    // QVM-specific costs
    pub resource_create: u64,
    pub resource_destroy: u64,
    pub resource_move: u64,
    pub resource_borrow: u64,
    pub parallel_hint: u64,
}

impl Default for GasCosts {
    fn default() -> Self {
        Self {
            // Base costs (similar to EVM)
            zero: 0,
            base: 2,
            very_low: 3,
            low: 5,
            mid: 8,
            high: 10,

            // Memory
            memory_word: 3,
            memory_copy: 3,

            // Storage (EIP-2929 style)
            sload_cold: 2100,
            sload_warm: 100,
            sstore_set: 20000,
            sstore_reset: 2900,
            sstore_clear_refund: 4800,

            // Calls
            call_base: 100,
            call_value: 9000,
            call_new_account: 25000,
            call_stipend: 2300,

            // Create
            create: 32000,
            create2: 32000,
            init_code_word: 2,

            // Crypto
            keccak256_word: 6,
            sha256_word: 12,
            ecrecover: 3000,

            // Logs
            log: 375,
            log_topic: 375,
            log_data: 8,

            // QVM-specific (lower cost to encourage use)
            resource_create: 100,
            resource_destroy: 50,
            resource_move: 20,
            resource_borrow: 10,
            parallel_hint: 5,
        }
    }
}

impl GasCosts {
    /// Get gas cost for an opcode
    pub fn opcode_cost(&self, opcode: Opcode) -> u64 {
        match opcode {
            // Zero cost
            Opcode::Nop => self.zero,

            // Base cost (stack operations)
            Opcode::Pop | Opcode::Dup | Opcode::Swap => self.base,

            // Very low cost (simple arithmetic)
            Opcode::Add
            | Opcode::Sub
            | Opcode::Not
            | Opcode::Lt
            | Opcode::Gt
            | Opcode::Eq
            | Opcode::And
            | Opcode::Or
            | Opcode::BitAnd
            | Opcode::BitOr
            | Opcode::BitXor
            | Opcode::BitNot => self.very_low,

            // Low cost
            Opcode::Mul
            | Opcode::Div
            | Opcode::Mod
            | Opcode::Shl
            | Opcode::Shr
            | Opcode::Ne
            | Opcode::Le
            | Opcode::Ge
            | Opcode::Neg => self.low,

            // Mid cost
            Opcode::Push | Opcode::LoadLocal | Opcode::StoreLocal => self.mid,

            // High cost (pow)
            Opcode::Pow => self.high,

            // Memory operations
            Opcode::Load | Opcode::Store => self.memory_word,

            // Storage (warm access - cold handled separately)
            Opcode::SLoad => self.sload_warm,
            Opcode::SStore => self.sstore_reset, // Base cost

            // Control flow
            Opcode::Jump | Opcode::JumpIf | Opcode::JumpIfNot => self.mid,
            Opcode::Call => self.call_base,
            Opcode::Return => self.zero,
            Opcode::Revert => self.zero,
            Opcode::Halt => self.zero,

            // Contract info
            Opcode::Address
            | Opcode::Caller
            | Opcode::CallValue
            | Opcode::Origin
            | Opcode::GasPrice
            | Opcode::Coinbase
            | Opcode::Timestamp
            | Opcode::BlockNumber
            | Opcode::Difficulty
            | Opcode::GasLimit
            | Opcode::ChainId
            | Opcode::SelfBalance
            | Opcode::Gas => self.base,
            Opcode::Balance | Opcode::BlockHash => self.sload_warm, // Account/block access

            // External calls
            Opcode::ExternalCall | Opcode::StaticCall | Opcode::DelegateCall => self.call_base,
            Opcode::Create | Opcode::Create2 => self.create,

            // Logs
            Opcode::Log0 => self.log,
            Opcode::Log1 => self.log + self.log_topic,
            Opcode::Log2 => self.log + self.log_topic * 2,
            Opcode::Log3 => self.log + self.log_topic * 3,
            Opcode::Log4 => self.log + self.log_topic * 4,

            // Crypto
            Opcode::Keccak256 | Opcode::Sha256 | Opcode::Ripemd160 => self.mid, // Base, word cost added separately
            Opcode::Ecrecover => self.ecrecover,

            // QVM Resource operations
            Opcode::ResourceCreate => self.resource_create,
            Opcode::ResourceDestroy => self.resource_destroy,
            Opcode::ResourceMove => self.resource_move,
            Opcode::ResourceCopy => self.resource_move * 2, // Copy is more expensive
            Opcode::ResourceBorrow | Opcode::ResourceBorrowMut => self.resource_borrow,

            // Parallel hints
            Opcode::ParallelStart
            | Opcode::ParallelEnd
            | Opcode::StateRead
            | Opcode::StateWrite => self.parallel_hint,
        }
    }

    /// Calculate memory expansion cost
    pub fn memory_expansion_cost(&self, current_words: u64, new_words: u64) -> u64 {
        if new_words <= current_words {
            return 0;
        }

        let new_cost = self.memory_cost(new_words);
        let current_cost = self.memory_cost(current_words);
        new_cost.saturating_sub(current_cost)
    }

    /// Memory cost formula: memory_cost = (words^2 / 512) + (words * 3)
    pub fn memory_cost(&self, words: u64) -> u64 {
        let linear = words.saturating_mul(self.memory_word);
        let quadratic = words.saturating_mul(words) / 512;
        linear.saturating_add(quadratic)
    }

    /// Calculate storage write cost with refund
    pub fn sstore_cost(
        &self,
        original: bool,
        current_is_zero: bool,
        new_is_zero: bool,
    ) -> (u64, i64) {
        // EIP-2200 style gas calculation
        if original {
            // Original value is zero
            if new_is_zero {
                // 0 -> 0: warm access
                (self.sload_warm, 0)
            } else {
                // 0 -> non-zero: set
                (self.sstore_set, 0)
            }
        } else {
            // Original value is non-zero
            if current_is_zero {
                // Already zero in current state
                if new_is_zero {
                    (self.sload_warm, 0)
                } else {
                    (self.sstore_set, 0)
                }
            } else {
                // Current is non-zero
                if new_is_zero {
                    // Clear: refund
                    (self.sstore_reset, self.sstore_clear_refund as i64)
                } else {
                    // Reset
                    (self.sstore_reset, 0)
                }
            }
        }
    }

    /// Calculate log cost
    pub fn log_cost(&self, topics: usize, data_size: usize) -> u64 {
        self.log
            .saturating_add(self.log_topic.saturating_mul(topics as u64))
            .saturating_add(self.log_data.saturating_mul(data_size as u64))
    }

    /// Calculate hash cost
    pub fn hash_cost(&self, data_size: usize, cost_per_word: u64) -> u64 {
        let words = data_size.div_ceil(32);
        self.mid
            .saturating_add(cost_per_word.saturating_mul(words as u64))
    }
}

/// Gas meter for tracking consumption
#[derive(Debug, Clone)]
pub struct GasMeter {
    /// Gas limit
    limit: u64,

    /// Gas used
    used: u64,

    /// Gas refund
    refund: i64,

    /// Gas costs configuration
    costs: GasCosts,

    /// Access tracking for warm/cold storage
    warm_slots: std::collections::HashSet<primitive_types::H256>,

    /// Access tracking for warm/cold accounts
    warm_accounts: std::collections::HashSet<primitive_types::H160>,
}

impl GasMeter {
    pub fn new(limit: u64) -> Self {
        Self {
            limit,
            used: 0,
            refund: 0,
            costs: GasCosts::default(),
            warm_slots: std::collections::HashSet::new(),
            warm_accounts: std::collections::HashSet::new(),
        }
    }

    pub fn with_costs(limit: u64, costs: GasCosts) -> Self {
        Self {
            limit,
            used: 0,
            refund: 0,
            costs,
            warm_slots: std::collections::HashSet::new(),
            warm_accounts: std::collections::HashSet::new(),
        }
    }

    /// Get remaining gas
    pub fn remaining(&self) -> u64 {
        self.limit.saturating_sub(self.used)
    }

    /// Get used gas
    pub fn used(&self) -> u64 {
        self.used
    }

    /// Get gas limit
    pub fn limit(&self) -> u64 {
        self.limit
    }

    /// Get refund
    pub fn refund(&self) -> i64 {
        self.refund
    }

    /// Get effective gas used (after refund)
    pub fn effective_used(&self) -> u64 {
        let max_refund = (self.used / 5) as i64; // Max 20% refund
        let actual_refund = self.refund.min(max_refund).max(0) as u64;
        self.used.saturating_sub(actual_refund)
    }

    /// Consume gas
    pub fn consume(&mut self, amount: u64) -> GasResult<()> {
        let new_used = self.used.saturating_add(amount);
        if new_used > self.limit {
            return Err(GasError::OutOfGas {
                required: amount,
                available: self.remaining(),
            });
        }
        self.used = new_used;
        Ok(())
    }

    /// Add refund
    pub fn add_refund(&mut self, amount: i64) {
        self.refund = self.refund.saturating_add(amount);
    }

    /// Consume gas for an opcode
    pub fn consume_opcode(&mut self, opcode: Opcode) -> GasResult<()> {
        self.consume(self.costs.opcode_cost(opcode))
    }

    /// Check if a storage slot is warm
    pub fn is_slot_warm(&self, slot: primitive_types::H256) -> bool {
        self.warm_slots.contains(&slot)
    }

    /// Mark a storage slot as warm
    pub fn warm_slot(&mut self, slot: primitive_types::H256) -> bool {
        !self.warm_slots.insert(slot)
    }

    /// Consume gas for storage load
    pub fn consume_sload(&mut self, slot: primitive_types::H256) -> GasResult<()> {
        let cost = if self.warm_slot(slot) {
            self.costs.sload_warm
        } else {
            self.costs.sload_cold
        };
        self.consume(cost)
    }

    /// Consume gas for storage store
    pub fn consume_sstore(
        &mut self,
        slot: primitive_types::H256,
        original_is_zero: bool,
        current_is_zero: bool,
        new_is_zero: bool,
    ) -> GasResult<()> {
        self.warm_slot(slot);
        let (cost, refund) = self
            .costs
            .sstore_cost(original_is_zero, current_is_zero, new_is_zero);
        self.consume(cost)?;
        self.add_refund(refund);
        Ok(())
    }

    /// Check if an account is warm
    pub fn is_account_warm(&self, address: primitive_types::H160) -> bool {
        self.warm_accounts.contains(&address)
    }

    /// Mark an account as warm
    pub fn warm_account(&mut self, address: primitive_types::H160) -> bool {
        !self.warm_accounts.insert(address)
    }

    /// Consume gas for memory expansion
    pub fn consume_memory_expansion(
        &mut self,
        current_words: u64,
        new_words: u64,
    ) -> GasResult<()> {
        let cost = self.costs.memory_expansion_cost(current_words, new_words);
        self.consume(cost)
    }

    /// Reset for new transaction
    pub fn reset(&mut self, new_limit: u64) {
        self.limit = new_limit;
        self.used = 0;
        self.refund = 0;
        self.warm_slots.clear();
        self.warm_accounts.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gas_consumption() {
        let mut meter = GasMeter::new(1000);
        assert!(meter.consume(100).is_ok());
        assert_eq!(meter.used(), 100);
        assert_eq!(meter.remaining(), 900);
    }

    #[test]
    fn test_out_of_gas() {
        let mut meter = GasMeter::new(100);
        assert!(meter.consume(50).is_ok());
        assert!(meter.consume(60).is_err());
    }

    #[test]
    fn test_memory_cost() {
        let costs = GasCosts::default();
        assert_eq!(costs.memory_cost(1), 3);
        assert_eq!(costs.memory_cost(10), 30);
        // Quadratic kicks in at larger sizes
        assert!(costs.memory_cost(1000) > 3000);
    }

    #[test]
    fn test_warm_cold_storage() {
        let mut meter = GasMeter::new(100000);
        let slot = primitive_types::H256::zero();

        // First access is cold
        assert!(!meter.is_slot_warm(slot));
        meter.consume_sload(slot).unwrap();
        assert_eq!(meter.used(), 2100); // Cold cost

        // Second access is warm
        assert!(meter.is_slot_warm(slot));
        meter.consume_sload(slot).unwrap();
        assert_eq!(meter.used(), 2200); // Cold + warm
    }

    #[test]
    fn test_refund() {
        let mut meter = GasMeter::new(100000);
        meter.consume(50000).unwrap();
        meter.add_refund(10000);

        // Max refund is 20% of used
        assert_eq!(meter.effective_used(), 40000); // 50000 - 10000
    }
}
