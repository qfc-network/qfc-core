//! QuantumScript Language Server Protocol implementation.
//!
//! Provides IDE features for QuantumScript:
//! - Diagnostics (errors, warnings)
//! - Code completion
//! - Hover information
//! - Go-to-definition
//! - Document symbols

pub mod backend;
pub mod diagnostics;
pub mod document;

pub use backend::Backend;
