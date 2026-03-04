//! Diagnostic conversion from compiler errors to LSP diagnostics.

use qfc_qsc::{lexer::LexerError, parser::ParseError, typeck::TypeError};
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

use crate::document::Document;

/// Convert a lexer error to an LSP diagnostic.
pub fn lexer_error_to_diagnostic(error: &LexerError, doc: &Document) -> Diagnostic {
    let (line, column) = match error {
        LexerError::UnexpectedChar(_, line, col) => (*line, *col),
        LexerError::UnterminatedString(line, col) => (*line, *col),
        LexerError::UnterminatedBlockComment(line, col) => (*line, *col),
        LexerError::InvalidEscapeSequence(_, line, col) => (*line, *col),
        LexerError::InvalidNumber(line, col) => (*line, *col),
        LexerError::InvalidHexDigit(line, col) => (*line, *col),
        LexerError::InvalidAddressLiteral(line, col) => (*line, *col),
    };

    let position = Position::new(line.saturating_sub(1), column.saturating_sub(1));
    let range = word_range_at_position(doc, position);

    Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::ERROR),
        code: None,
        code_description: None,
        source: Some("qsc".to_string()),
        message: error.to_string(),
        related_information: None,
        tags: None,
        data: None,
    }
}

/// Convert a parse error to an LSP diagnostic.
pub fn parse_error_to_diagnostic(error: &ParseError, doc: &Document) -> Diagnostic {
    let (line, column) = match error {
        ParseError::UnexpectedToken { line, column, .. } => (*line, *column),
        ParseError::UnexpectedEof => {
            // End of file - use last position
            let last_line = doc.content.len_lines().saturating_sub(1) as u32;
            (last_line + 1, 1)
        }
        ParseError::InvalidExpression(line, col) => (*line, *col),
        ParseError::InvalidPattern(line, col) => (*line, *col),
        ParseError::InvalidType(line, col) => (*line, *col),
        ParseError::ExpectedIdentifier(line, col) => (*line, *col),
        ParseError::DuplicateModifier(_, line, col) => (*line, *col),
    };

    let position = Position::new(line.saturating_sub(1), column.saturating_sub(1));
    let range = word_range_at_position(doc, position);

    Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::ERROR),
        code: None,
        code_description: None,
        source: Some("qsc".to_string()),
        message: error.to_string(),
        related_information: None,
        tags: None,
        data: None,
    }
}

/// Convert a type error to an LSP diagnostic.
pub fn type_error_to_diagnostic(error: &TypeError, doc: &Document) -> Diagnostic {
    let (line, column, severity): (u32, u32, DiagnosticSeverity) = match error {
        TypeError::UndefinedVariable(_, line, col) => (*line, *col, DiagnosticSeverity::ERROR),
        TypeError::UndefinedType(_, line, col) => (*line, *col, DiagnosticSeverity::ERROR),
        TypeError::UndefinedFunction(_, line, col) => (*line, *col, DiagnosticSeverity::ERROR),
        TypeError::TypeMismatch { line, column, .. } => (*line, *column, DiagnosticSeverity::ERROR),
        TypeError::ImmutableAssignment(_, line, col) => (*line, *col, DiagnosticSeverity::ERROR),
        TypeError::MoveOutOfBorrow(line, col) => (*line, *col, DiagnosticSeverity::ERROR),
        TypeError::MissingAbility(_, _, line, col) => (*line, *col, DiagnosticSeverity::ERROR),
        TypeError::DuplicateDefinition(_, line, col) => (*line, *col, DiagnosticSeverity::ERROR),
        TypeError::ArgumentCountMismatch { line, column, .. } => {
            (*line, *column, DiagnosticSeverity::ERROR)
        }
        TypeError::NotCallable(line, col) => (*line, *col, DiagnosticSeverity::ERROR),
        TypeError::NotIndexable(line, col) => (*line, *col, DiagnosticSeverity::ERROR),
        TypeError::FieldNotFound(_, _, line, col) => (*line, *col, DiagnosticSeverity::ERROR),
        TypeError::PureFunctionModifiesState(line, col) => (*line, *col, DiagnosticSeverity::ERROR),
        TypeError::ViewFunctionModifiesState(line, col) => (*line, *col, DiagnosticSeverity::ERROR),
        TypeError::InvalidReturnType(line, col) => (*line, *col, DiagnosticSeverity::ERROR),
        TypeError::MissingReturnValue(line, col) => (*line, *col, DiagnosticSeverity::ERROR),
        TypeError::SelfOutsideContract(line, col) => (*line, *col, DiagnosticSeverity::ERROR),
        TypeError::ParallelStateConflict(line, col) => (*line, *col, DiagnosticSeverity::ERROR),
    };

    let position = Position::new(line.saturating_sub(1), column.saturating_sub(1));
    let range = word_range_at_position(doc, position);

    Diagnostic {
        range,
        severity: Some(severity),
        code: None,
        code_description: None,
        source: Some("qsc".to_string()),
        message: error.to_string(),
        related_information: None,
        tags: None,
        data: None,
    }
}

/// Get a word range at the given position, or a single-character range if no word found.
fn word_range_at_position(doc: &Document, position: Position) -> Range {
    if let Some((_, range)) = doc.word_at_position(position) {
        range
    } else {
        Range::new(
            position,
            Position::new(position.line, position.character + 1),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lexer_error_diagnostic() {
        let doc = Document::new("let x = @invalid;", 1);
        let error = LexerError::UnexpectedChar('@', 1, 9);
        let diag = lexer_error_to_diagnostic(&error, &doc);

        assert_eq!(diag.severity, Some(DiagnosticSeverity::ERROR));
        assert!(diag.message.contains("unexpected character"));
    }

    #[test]
    fn test_parse_error_diagnostic() {
        let doc = Document::new("let = 42;", 1);
        let error = ParseError::ExpectedIdentifier(1, 5);
        let diag = parse_error_to_diagnostic(&error, &doc);

        assert_eq!(diag.severity, Some(DiagnosticSeverity::ERROR));
        assert!(diag.message.contains("identifier"));
    }
}
