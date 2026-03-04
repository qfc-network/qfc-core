//! JIT compilation for QVM bytecode.
//!
//! This module provides Just-In-Time compilation of QVM bytecode to native
//! machine code using Cranelift. The JIT compiler is used for hot functions
//! to improve execution performance.
//!
//! # Architecture
//!
//! The JIT system uses a tiered compilation approach:
//! 1. Functions are initially interpreted
//! 2. After reaching a call threshold, they are JIT compiled
//! 3. Compiled code is cached for subsequent calls
//!
//! # Example
//!
//! ```ignore
//! use qfc_qvm::jit::{JitCompiler, JitConfig};
//!
//! let config = JitConfig::default();
//! let mut compiler = JitCompiler::new(config)?;
//!
//! // Compile a function
//! let compiled = compiler.compile(&function_bytecode)?;
//!
//! // Execute compiled code
//! let result = compiled.execute(&mut context)?;
//! ```

#[cfg(feature = "jit")]
mod codegen;
#[cfg(feature = "jit")]
mod compiler;
#[cfg(feature = "jit")]
mod runtime;

#[cfg(feature = "jit")]
pub use codegen::CodeGenerator;
#[cfg(feature = "jit")]
pub use compiler::{CompiledFunction, JitCompiler, JitConfig};
#[cfg(feature = "jit")]
pub use runtime::JitRuntime;

use thiserror::Error;

/// JIT compilation errors.
#[derive(Debug, Error)]
pub enum JitError {
    #[error("compilation failed: {0}")]
    CompilationFailed(String),

    #[error("unsupported opcode: {0:?}")]
    UnsupportedOpcode(u8),

    #[error("invalid bytecode: {0}")]
    InvalidBytecode(String),

    #[error("memory error: {0}")]
    MemoryError(String),

    #[error("execution error: {0}")]
    ExecutionError(String),

    #[error("cranelift error: {0}")]
    #[cfg(feature = "jit")]
    CraneliftError(String),
}

pub type JitResult<T> = Result<T, JitError>;

/// Statistics for JIT compilation.
#[derive(Debug, Default, Clone)]
pub struct JitStats {
    /// Number of functions compiled.
    pub functions_compiled: u64,
    /// Total compilation time in microseconds.
    pub compilation_time_us: u64,
    /// Total bytes of generated code.
    pub code_size_bytes: u64,
    /// Number of JIT cache hits.
    pub cache_hits: u64,
    /// Number of JIT cache misses.
    pub cache_misses: u64,
}

/// Execution mode for the VM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExecutionMode {
    /// Always interpret bytecode (no JIT).
    #[default]
    Interpret,
    /// Use JIT compilation for hot functions.
    #[cfg(feature = "jit")]
    Jit,
    /// Compile all functions ahead of time.
    #[cfg(feature = "jit")]
    AheadOfTime,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_jit_stats_default() {
        let stats = JitStats::default();
        assert_eq!(stats.functions_compiled, 0);
        assert_eq!(stats.cache_hits, 0);
    }

    #[test]
    fn test_execution_mode_default() {
        let mode = ExecutionMode::default();
        assert_eq!(mode, ExecutionMode::Interpret);
    }
}
