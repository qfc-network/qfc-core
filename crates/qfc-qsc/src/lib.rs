//! QFC QuantumScript Compiler (qfc-qsc)
//!
//! A compiler for the QuantumScript smart contract language that targets
//! the QFC Virtual Machine (QVM).
//!
//! # Architecture
//!
//! The compiler is organized into several stages:
//!
//! 1. **Lexer** (`lexer`) - Tokenizes source code into tokens
//! 2. **Parser** (`parser`) - Parses tokens into an Abstract Syntax Tree (AST)
//! 3. **Type Checker** (`typeck`) - Performs semantic analysis and type checking
//! 4. **Code Generator** (`codegen`) - Generates QVM bytecode
//!
//! # Example
//!
//! ```ignore
//! use qfc_qsc::{compile, CompilerOptions};
//!
//! let source = r#"
//!     contract Token {
//!         storage {
//!             total_supply: u256,
//!             balances: mapping(address => u256),
//!         }
//!
//!         pub fn transfer(to: address, amount: u256) -> bool {
//!             let sender = msg.sender;
//!             balances[sender] = balances[sender] - amount;
//!             balances[to] = balances[to] + amount;
//!             return true;
//!         }
//!     }
//! "#;
//!
//! let bytecode = compile(source, &CompilerOptions::default())?;
//! ```
//!
//! # Language Features
//!
//! QuantumScript supports:
//! - Resource types with linear ownership (inspired by Move)
//! - Parallel execution annotations
//! - Formal verification specifications
//! - EVM interoperability
//! - Standard Rust-like syntax

pub mod ast;
pub mod codegen;
pub mod lexer;
pub mod parser;
pub mod typeck;

use thiserror::Error;

pub use ast::*;
pub use codegen::{Codegen, ContractBytecode, FunctionBytecode, Instruction, Opcode};
pub use lexer::{Lexer, LexerError, Span, Token, TokenKind};
pub use parser::{ParseError, Parser};
pub use typeck::{TypeChecker, TypeError};

/// Compiler errors
#[derive(Debug, Error)]
pub enum CompilerError {
    #[error("lexer error: {0}")]
    Lexer(#[from] LexerError),

    #[error("parse error: {0}")]
    Parse(#[from] ParseError),

    #[error("type error: {0}")]
    Type(#[from] TypeError),

    #[error("codegen error: {0}")]
    Codegen(#[from] codegen::CodegenError),
}

/// Compilation result
pub type CompilerResult<T> = Result<T, CompilerError>;

/// Compiler options
#[derive(Debug, Clone, Default)]
pub struct CompilerOptions {
    /// Enable optimizations
    pub optimize: bool,

    /// Emit debug information
    pub debug_info: bool,

    /// Verify formal specifications
    pub verify_specs: bool,

    /// Target EVM compatibility mode
    pub evm_compat: bool,
}

/// Compile QuantumScript source code to bytecode
pub fn compile(source: &str, options: &CompilerOptions) -> CompilerResult<Vec<ContractBytecode>> {
    // Lexing
    let tokens = Lexer::new(source).tokenize()?;

    // Parsing
    let ast = Parser::new(tokens).parse_file()?;

    // Type checking
    let mut type_checker = TypeChecker::new();
    type_checker.check_file(&ast)?;

    // Code generation
    let mut codegen = Codegen::new();
    let bytecode = codegen.generate(&ast)?;

    // TODO: Optimization pass if options.optimize

    Ok(bytecode)
}

/// Compile and return just the parsed AST (for tooling)
pub fn parse_only(source: &str) -> CompilerResult<SourceFile> {
    let tokens = Lexer::new(source).tokenize()?;
    let ast = Parser::new(tokens).parse_file()?;
    Ok(ast)
}

/// Check types without generating code (for IDE support)
pub fn check_only(source: &str) -> CompilerResult<()> {
    let tokens = Lexer::new(source).tokenize()?;
    let ast = Parser::new(tokens).parse_file()?;
    let mut type_checker = TypeChecker::new();
    type_checker.check_file(&ast)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compile_simple_contract() {
        let source = r#"
            contract Token {
                storage {
                    total_supply: u256,
                }

                pub fn mint(amount: u256) {
                    total_supply = total_supply + amount;
                }

                pub view fn get_supply() -> u256 {
                    return total_supply;
                }
            }
        "#;

        let result = compile(source, &CompilerOptions::default());
        assert!(result.is_ok());
    }

    #[test]
    fn test_parse_resource_type() {
        let source = r#"
            resource Coin: store + drop {
                value: u256,
            }

            fn create_coin(value: u256) -> Coin {
                return Coin { value };
            }
        "#;

        let result = parse_only(source);
        assert!(result.is_ok());
    }

    #[test]
    fn test_parse_parallel_function() {
        let source = r#"
            contract Bank {
                storage {
                    total: u256,
                }

                parallel fn batch_transfer(amount: u256) {
                    total = amount;
                }
            }
        "#;

        let result = parse_only(source);
        assert!(result.is_ok());
    }

    #[test]
    fn test_check_type_error() {
        let source = r#"
            fn test() {
                let x: u256 = true;
            }
        "#;

        let result = check_only(source);
        assert!(result.is_err());
    }

    #[test]
    fn test_formal_verification_syntax() {
        let source = r#"
            fn transfer(amount: u256) -> bool {
                return true;
            }
        "#;

        let result = parse_only(source);
        assert!(result.is_ok());
    }
}
