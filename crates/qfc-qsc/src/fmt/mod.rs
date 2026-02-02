//! Code formatter for QuantumScript
//!
//! Pretty-prints QuantumScript AST with consistent formatting.

mod printer;
mod config;

pub use config::FormatConfig;
pub use printer::Formatter;

use crate::ast::SourceFile;
use crate::lexer::Lexer;
use crate::parser::Parser;

/// Format QuantumScript source code.
pub fn format(source: &str) -> Result<String, FormatError> {
    format_with_config(source, &FormatConfig::default())
}

/// Format QuantumScript source code with custom configuration.
pub fn format_with_config(source: &str, config: &FormatConfig) -> Result<String, FormatError> {
    // Parse the source
    let lexer = Lexer::new(source);
    let tokens = lexer.tokenize().map_err(|e| FormatError::LexError(e.to_string()))?;

    let mut parser = Parser::new(tokens);
    let ast = parser.parse_file().map_err(|e| FormatError::ParseError(e.to_string()))?;

    // Format the AST
    let mut formatter = Formatter::new(config.clone());
    Ok(formatter.format_file(&ast))
}

/// Format a single file (AST already parsed).
pub fn format_ast(ast: &SourceFile, config: &FormatConfig) -> String {
    let mut formatter = Formatter::new(config.clone());
    formatter.format_file(ast)
}

/// Formatting errors
#[derive(Debug, Clone)]
pub enum FormatError {
    LexError(String),
    ParseError(String),
}

impl std::fmt::Display for FormatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FormatError::LexError(e) => write!(f, "Lexer error: {}", e),
            FormatError::ParseError(e) => write!(f, "Parser error: {}", e),
        }
    }
}

impl std::error::Error for FormatError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_simple_contract() {
        let source = r#"contract  Token{storage{balance:u256,}pub view fn get()->u256{return balance;}}"#;
        let formatted = format(source).unwrap();

        assert!(formatted.contains("contract Token {"));
        assert!(formatted.contains("storage {"));
        assert!(formatted.contains("balance: u256,"));
        assert!(formatted.contains("pub fn get() -> u256 view {"));
    }

    #[test]
    fn test_format_preserves_semantics() {
        let source = r#"
contract Counter {
    storage { count: u256, }

    pub fn increment() {
        count = count + 1;
    }
}
"#;
        let formatted = format(source).unwrap();

        // Should be able to re-parse the formatted output
        let lexer = Lexer::new(&formatted);
        let tokens = lexer.tokenize().expect("should tokenize");
        let mut parser = Parser::new(tokens);
        parser.parse_file().expect("should parse");
    }

    #[test]
    fn test_format_with_custom_config() {
        let source = "contract Foo { storage { x: u256, } }";
        let config = FormatConfig {
            indent_size: 2,
            ..Default::default()
        };
        let formatted = format_with_config(source, &config).unwrap();

        // With 2-space indent
        assert!(formatted.contains("  storage"));
    }
}
