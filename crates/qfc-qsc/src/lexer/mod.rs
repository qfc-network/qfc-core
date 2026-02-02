//! Lexer for QuantumScript
//!
//! Tokenizes QuantumScript source code into a stream of tokens.

pub mod token;

use std::str::Chars;
use std::iter::Peekable;
use thiserror::Error;

pub use token::{Token, TokenKind, Span};

/// Lexer errors
#[derive(Debug, Error, Clone, PartialEq)]
pub enum LexerError {
    #[error("unexpected character '{0}' at line {1}, column {2}")]
    UnexpectedChar(char, u32, u32),

    #[error("unterminated string literal at line {0}, column {1}")]
    UnterminatedString(u32, u32),

    #[error("unterminated block comment at line {0}, column {1}")]
    UnterminatedBlockComment(u32, u32),

    #[error("invalid escape sequence '\\{0}' at line {1}, column {2}")]
    InvalidEscapeSequence(char, u32, u32),

    #[error("invalid number literal at line {0}, column {1}")]
    InvalidNumber(u32, u32),

    #[error("invalid hex digit at line {0}, column {1}")]
    InvalidHexDigit(u32, u32),

    #[error("invalid address literal at line {0}, column {1}: expected 40 hex digits")]
    InvalidAddressLiteral(u32, u32),
}

/// Lexer result type
pub type LexerResult<T> = Result<T, LexerError>;

/// QuantumScript lexer
pub struct Lexer<'src> {
    source: &'src str,
    chars: Peekable<Chars<'src>>,
    /// Current byte offset
    pos: usize,
    /// Current line (1-indexed)
    line: u32,
    /// Current column (1-indexed)
    column: u32,
    /// Start position of current token
    token_start: usize,
    /// Start line of current token
    token_line: u32,
    /// Start column of current token
    token_column: u32,
}

impl<'src> Lexer<'src> {
    /// Create a new lexer for the given source code
    pub fn new(source: &'src str) -> Self {
        Self {
            source,
            chars: source.chars().peekable(),
            pos: 0,
            line: 1,
            column: 1,
            token_start: 0,
            token_line: 1,
            token_column: 1,
        }
    }

    /// Tokenize the entire source and return all tokens
    pub fn tokenize(mut self) -> LexerResult<Vec<Token>> {
        let mut tokens = Vec::new();
        loop {
            let token = self.next_token()?;
            let is_eof = token.kind == TokenKind::Eof;
            tokens.push(token);
            if is_eof {
                break;
            }
        }
        Ok(tokens)
    }

    /// Get the next token
    pub fn next_token(&mut self) -> LexerResult<Token> {
        self.skip_whitespace_and_comments()?;
        self.mark_token_start();

        match self.peek() {
            None => Ok(self.make_token(TokenKind::Eof)),
            Some(c) => match c {
                // Byte strings (must be before identifier check)
                'b' if self.peek_nth(1) == Some('"') => self.lex_byte_string(),

                // Identifiers and keywords
                'a'..='z' | 'A'..='Z' | '_' => self.lex_identifier(),

                // Numbers
                '0'..='9' => self.lex_number(),

                // Strings
                '"' => self.lex_string(),

                // Operators and punctuation
                '+' => self.lex_plus(),
                '-' => self.lex_minus(),
                '*' => self.lex_star(),
                '/' => self.lex_slash(),
                '%' => self.lex_percent(),
                '=' => self.lex_eq(),
                '!' => self.lex_bang(),
                '<' => self.lex_lt(),
                '>' => self.lex_gt(),
                '&' => self.lex_ampersand(),
                '|' => self.lex_pipe(),
                '^' => self.lex_caret(),
                '~' => self.lex_single(TokenKind::Tilde),
                '?' => self.lex_single(TokenKind::Question),
                ':' => self.lex_colon(),
                '.' => self.lex_dot(),
                '@' => self.lex_single(TokenKind::At),
                '#' => self.lex_single(TokenKind::Hash),
                '$' => self.lex_single(TokenKind::Dollar),

                // Delimiters
                '(' => self.lex_single(TokenKind::LParen),
                ')' => self.lex_single(TokenKind::RParen),
                '[' => self.lex_single(TokenKind::LBracket),
                ']' => self.lex_single(TokenKind::RBracket),
                '{' => self.lex_single(TokenKind::LBrace),
                '}' => self.lex_single(TokenKind::RBrace),

                // Punctuation
                ',' => self.lex_single(TokenKind::Comma),
                ';' => self.lex_single(TokenKind::Semi),

                _ => Err(LexerError::UnexpectedChar(c, self.line, self.column)),
            },
        }
    }

    // ========== Helper methods ==========

    fn peek(&mut self) -> Option<char> {
        self.chars.peek().copied()
    }

    fn peek_nth(&self, n: usize) -> Option<char> {
        self.source[self.pos..].chars().nth(n)
    }

    fn advance(&mut self) -> Option<char> {
        let c = self.chars.next()?;
        self.pos += c.len_utf8();
        if c == '\n' {
            self.line += 1;
            self.column = 1;
        } else {
            self.column += 1;
        }
        Some(c)
    }

    fn advance_if(&mut self, expected: char) -> bool {
        if self.peek() == Some(expected) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn mark_token_start(&mut self) {
        self.token_start = self.pos;
        self.token_line = self.line;
        self.token_column = self.column;
    }

    fn make_token(&self, kind: TokenKind) -> Token {
        Token::new(
            kind,
            Span::new(self.token_start, self.pos, self.token_line, self.token_column),
        )
    }

    fn token_text(&self) -> &'src str {
        &self.source[self.token_start..self.pos]
    }

    // ========== Whitespace and comments ==========

    fn skip_whitespace_and_comments(&mut self) -> LexerResult<()> {
        loop {
            match self.peek() {
                Some(' ' | '\t' | '\r' | '\n') => {
                    self.advance();
                }
                Some('/') => {
                    match self.peek_nth(1) {
                        Some('/') => self.skip_line_comment(),
                        Some('*') => self.skip_block_comment()?,
                        _ => break,
                    }
                }
                _ => break,
            }
        }
        Ok(())
    }

    fn skip_line_comment(&mut self) {
        // Skip //
        self.advance();
        self.advance();
        while let Some(c) = self.peek() {
            if c == '\n' {
                break;
            }
            self.advance();
        }
    }

    fn skip_block_comment(&mut self) -> LexerResult<()> {
        let start_line = self.line;
        let start_column = self.column;

        // Skip /*
        self.advance();
        self.advance();

        let mut depth = 1;
        while depth > 0 {
            match (self.peek(), self.peek_nth(1)) {
                (None, _) => {
                    return Err(LexerError::UnterminatedBlockComment(start_line, start_column));
                }
                (Some('/'), Some('*')) => {
                    self.advance();
                    self.advance();
                    depth += 1;
                }
                (Some('*'), Some('/')) => {
                    self.advance();
                    self.advance();
                    depth -= 1;
                }
                _ => {
                    self.advance();
                }
            }
        }
        Ok(())
    }

    // ========== Token lexers ==========

    fn lex_single(&mut self, kind: TokenKind) -> LexerResult<Token> {
        self.advance();
        Ok(self.make_token(kind))
    }

    fn lex_identifier(&mut self) -> LexerResult<Token> {
        while let Some(c) = self.peek() {
            if c.is_alphanumeric() || c == '_' {
                self.advance();
            } else {
                break;
            }
        }

        let text = self.token_text();
        let kind = TokenKind::keyword_from_str(text)
            .unwrap_or_else(|| TokenKind::Identifier(text.to_string()));

        Ok(self.make_token(kind))
    }

    fn lex_number(&mut self) -> LexerResult<Token> {
        // Check for hex, binary, or octal prefix
        if self.peek() == Some('0') {
            match self.peek_nth(1) {
                Some('x') | Some('X') => return self.lex_hex_number(),
                Some('b') | Some('B') => return self.lex_binary_number(),
                Some('o') | Some('O') => return self.lex_octal_number(),
                _ => {}
            }
        }

        // Decimal number
        self.lex_decimal_number()
    }

    fn lex_decimal_number(&mut self) -> LexerResult<Token> {
        // Integer part
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() || c == '_' {
                self.advance();
            } else {
                break;
            }
        }

        // Check for float
        if self.peek() == Some('.') && self.peek_nth(1).map_or(false, |c| c.is_ascii_digit()) {
            self.advance(); // consume '.'
            while let Some(c) = self.peek() {
                if c.is_ascii_digit() || c == '_' {
                    self.advance();
                } else {
                    break;
                }
            }

            // Exponent
            if let Some('e') | Some('E') = self.peek() {
                self.advance();
                if let Some('+') | Some('-') = self.peek() {
                    self.advance();
                }
                while let Some(c) = self.peek() {
                    if c.is_ascii_digit() {
                        self.advance();
                    } else {
                        break;
                    }
                }
            }

            let text = self.token_text().replace('_', "");
            return Ok(self.make_token(TokenKind::FloatLiteral(text)));
        }

        // Check for exponent (scientific notation without decimal point)
        if let Some('e') | Some('E') = self.peek() {
            self.advance();
            if let Some('+') | Some('-') = self.peek() {
                self.advance();
            }
            while let Some(c) = self.peek() {
                if c.is_ascii_digit() {
                    self.advance();
                } else {
                    break;
                }
            }
            let text = self.token_text().replace('_', "");
            return Ok(self.make_token(TokenKind::FloatLiteral(text)));
        }

        let text = self.token_text().replace('_', "");
        Ok(self.make_token(TokenKind::IntLiteral(text)))
    }

    fn lex_hex_number(&mut self) -> LexerResult<Token> {
        // Skip 0x
        self.advance();
        self.advance();

        let hex_start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_ascii_hexdigit() || c == '_' {
                self.advance();
            } else {
                break;
            }
        }

        let hex_len = self.source[hex_start..self.pos].replace('_', "").len();

        // Check if this is an address literal (40 hex digits = 20 bytes)
        if hex_len == 40 {
            let text = self.token_text().to_string();
            return Ok(self.make_token(TokenKind::AddressLiteral(text)));
        }

        let text = self.token_text().to_string();
        Ok(self.make_token(TokenKind::IntLiteral(text)))
    }

    fn lex_binary_number(&mut self) -> LexerResult<Token> {
        // Skip 0b
        self.advance();
        self.advance();

        while let Some(c) = self.peek() {
            if c == '0' || c == '1' || c == '_' {
                self.advance();
            } else {
                break;
            }
        }

        let text = self.token_text().to_string();
        Ok(self.make_token(TokenKind::IntLiteral(text)))
    }

    fn lex_octal_number(&mut self) -> LexerResult<Token> {
        // Skip 0o
        self.advance();
        self.advance();

        while let Some(c) = self.peek() {
            if ('0'..='7').contains(&c) || c == '_' {
                self.advance();
            } else {
                break;
            }
        }

        let text = self.token_text().to_string();
        Ok(self.make_token(TokenKind::IntLiteral(text)))
    }

    fn lex_string(&mut self) -> LexerResult<Token> {
        let start_line = self.line;
        let start_column = self.column;

        // Skip opening quote
        self.advance();

        let mut value = String::new();

        loop {
            match self.peek() {
                None | Some('\n') => {
                    return Err(LexerError::UnterminatedString(start_line, start_column));
                }
                Some('"') => {
                    self.advance();
                    break;
                }
                Some('\\') => {
                    self.advance();
                    match self.peek() {
                        Some('n') => {
                            self.advance();
                            value.push('\n');
                        }
                        Some('r') => {
                            self.advance();
                            value.push('\r');
                        }
                        Some('t') => {
                            self.advance();
                            value.push('\t');
                        }
                        Some('\\') => {
                            self.advance();
                            value.push('\\');
                        }
                        Some('"') => {
                            self.advance();
                            value.push('"');
                        }
                        Some('0') => {
                            self.advance();
                            value.push('\0');
                        }
                        Some('x') => {
                            self.advance();
                            let hex = self.read_hex_escape(2)?;
                            value.push(char::from_u32(hex).unwrap_or('\u{FFFD}'));
                        }
                        Some('u') => {
                            self.advance();
                            if self.advance_if('{') {
                                let hex = self.read_unicode_escape()?;
                                value.push(char::from_u32(hex).unwrap_or('\u{FFFD}'));
                            } else {
                                return Err(LexerError::InvalidEscapeSequence(
                                    'u',
                                    self.line,
                                    self.column,
                                ));
                            }
                        }
                        Some(c) => {
                            return Err(LexerError::InvalidEscapeSequence(c, self.line, self.column));
                        }
                        None => {
                            return Err(LexerError::UnterminatedString(start_line, start_column));
                        }
                    }
                }
                Some(c) => {
                    self.advance();
                    value.push(c);
                }
            }
        }

        Ok(self.make_token(TokenKind::StringLiteral(value)))
    }

    fn lex_byte_string(&mut self) -> LexerResult<Token> {
        let start_line = self.line;
        let start_column = self.column;

        // Skip b"
        self.advance();
        self.advance();

        let mut value = Vec::new();

        loop {
            match self.peek() {
                None | Some('\n') => {
                    return Err(LexerError::UnterminatedString(start_line, start_column));
                }
                Some('"') => {
                    self.advance();
                    break;
                }
                Some('\\') => {
                    self.advance();
                    match self.peek() {
                        Some('n') => {
                            self.advance();
                            value.push(b'\n');
                        }
                        Some('r') => {
                            self.advance();
                            value.push(b'\r');
                        }
                        Some('t') => {
                            self.advance();
                            value.push(b'\t');
                        }
                        Some('\\') => {
                            self.advance();
                            value.push(b'\\');
                        }
                        Some('"') => {
                            self.advance();
                            value.push(b'"');
                        }
                        Some('0') => {
                            self.advance();
                            value.push(0);
                        }
                        Some('x') => {
                            self.advance();
                            let hex = self.read_hex_escape(2)? as u8;
                            value.push(hex);
                        }
                        Some(c) => {
                            return Err(LexerError::InvalidEscapeSequence(c, self.line, self.column));
                        }
                        None => {
                            return Err(LexerError::UnterminatedString(start_line, start_column));
                        }
                    }
                }
                Some(c) if c.is_ascii() => {
                    self.advance();
                    value.push(c as u8);
                }
                Some(c) => {
                    return Err(LexerError::UnexpectedChar(c, self.line, self.column));
                }
            }
        }

        Ok(self.make_token(TokenKind::ByteStringLiteral(value)))
    }

    fn read_hex_escape(&mut self, count: usize) -> LexerResult<u32> {
        let mut value = 0u32;
        for _ in 0..count {
            match self.peek() {
                Some(c) if c.is_ascii_hexdigit() => {
                    self.advance();
                    value = value * 16 + c.to_digit(16).unwrap();
                }
                _ => {
                    return Err(LexerError::InvalidHexDigit(self.line, self.column));
                }
            }
        }
        Ok(value)
    }

    fn read_unicode_escape(&mut self) -> LexerResult<u32> {
        let mut value = 0u32;
        let mut count = 0;
        while let Some(c) = self.peek() {
            if c == '}' {
                self.advance();
                break;
            }
            if !c.is_ascii_hexdigit() {
                return Err(LexerError::InvalidHexDigit(self.line, self.column));
            }
            self.advance();
            value = value * 16 + c.to_digit(16).unwrap();
            count += 1;
            if count > 6 {
                return Err(LexerError::InvalidHexDigit(self.line, self.column));
            }
        }
        Ok(value)
    }

    // ========== Operator lexers ==========

    fn lex_plus(&mut self) -> LexerResult<Token> {
        self.advance();
        if self.advance_if('=') {
            Ok(self.make_token(TokenKind::PlusEq))
        } else {
            Ok(self.make_token(TokenKind::Plus))
        }
    }

    fn lex_minus(&mut self) -> LexerResult<Token> {
        self.advance();
        if self.advance_if('>') {
            Ok(self.make_token(TokenKind::Arrow))
        } else if self.advance_if('=') {
            Ok(self.make_token(TokenKind::MinusEq))
        } else {
            Ok(self.make_token(TokenKind::Minus))
        }
    }

    fn lex_star(&mut self) -> LexerResult<Token> {
        self.advance();
        if self.advance_if('*') {
            Ok(self.make_token(TokenKind::StarStar))
        } else if self.advance_if('=') {
            Ok(self.make_token(TokenKind::StarEq))
        } else {
            Ok(self.make_token(TokenKind::Star))
        }
    }

    fn lex_slash(&mut self) -> LexerResult<Token> {
        self.advance();
        if self.advance_if('=') {
            Ok(self.make_token(TokenKind::SlashEq))
        } else {
            Ok(self.make_token(TokenKind::Slash))
        }
    }

    fn lex_percent(&mut self) -> LexerResult<Token> {
        self.advance();
        if self.advance_if('=') {
            Ok(self.make_token(TokenKind::PercentEq))
        } else {
            Ok(self.make_token(TokenKind::Percent))
        }
    }

    fn lex_eq(&mut self) -> LexerResult<Token> {
        self.advance();
        if self.advance_if('=') {
            Ok(self.make_token(TokenKind::EqEq))
        } else if self.advance_if('>') {
            Ok(self.make_token(TokenKind::FatArrow))
        } else {
            Ok(self.make_token(TokenKind::Eq))
        }
    }

    fn lex_bang(&mut self) -> LexerResult<Token> {
        self.advance();
        if self.advance_if('=') {
            Ok(self.make_token(TokenKind::NotEq))
        } else {
            Ok(self.make_token(TokenKind::Not))
        }
    }

    fn lex_lt(&mut self) -> LexerResult<Token> {
        self.advance();
        if self.advance_if('<') {
            if self.advance_if('=') {
                Ok(self.make_token(TokenKind::ShlEq))
            } else {
                Ok(self.make_token(TokenKind::Shl))
            }
        } else if self.advance_if('=') {
            Ok(self.make_token(TokenKind::LtEq))
        } else {
            Ok(self.make_token(TokenKind::Lt))
        }
    }

    fn lex_gt(&mut self) -> LexerResult<Token> {
        self.advance();
        if self.advance_if('>') {
            if self.advance_if('=') {
                Ok(self.make_token(TokenKind::ShrEq))
            } else {
                Ok(self.make_token(TokenKind::Shr))
            }
        } else if self.advance_if('=') {
            Ok(self.make_token(TokenKind::GtEq))
        } else {
            Ok(self.make_token(TokenKind::Gt))
        }
    }

    fn lex_ampersand(&mut self) -> LexerResult<Token> {
        self.advance();
        if self.advance_if('&') {
            Ok(self.make_token(TokenKind::And))
        } else if self.advance_if('=') {
            Ok(self.make_token(TokenKind::AmpersandEq))
        } else {
            Ok(self.make_token(TokenKind::Ampersand))
        }
    }

    fn lex_pipe(&mut self) -> LexerResult<Token> {
        self.advance();
        if self.advance_if('|') {
            Ok(self.make_token(TokenKind::Or))
        } else if self.advance_if('=') {
            Ok(self.make_token(TokenKind::PipeEq))
        } else {
            Ok(self.make_token(TokenKind::Pipe))
        }
    }

    fn lex_caret(&mut self) -> LexerResult<Token> {
        self.advance();
        if self.advance_if('=') {
            Ok(self.make_token(TokenKind::CaretEq))
        } else {
            Ok(self.make_token(TokenKind::Caret))
        }
    }

    fn lex_colon(&mut self) -> LexerResult<Token> {
        self.advance();
        if self.advance_if(':') {
            Ok(self.make_token(TokenKind::ColonColon))
        } else {
            Ok(self.make_token(TokenKind::Colon))
        }
    }

    fn lex_dot(&mut self) -> LexerResult<Token> {
        self.advance();
        if self.advance_if('.') {
            if self.advance_if('=') {
                Ok(self.make_token(TokenKind::DotDotEq))
            } else {
                Ok(self.make_token(TokenKind::DotDot))
            }
        } else {
            Ok(self.make_token(TokenKind::Dot))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lex(source: &str) -> Vec<TokenKind> {
        Lexer::new(source)
            .tokenize()
            .unwrap()
            .into_iter()
            .map(|t| t.kind)
            .filter(|k| *k != TokenKind::Eof)
            .collect()
    }

    #[test]
    fn test_keywords() {
        assert_eq!(lex("contract"), vec![TokenKind::Contract]);
        assert_eq!(lex("fn"), vec![TokenKind::Fn]);
        assert_eq!(lex("let"), vec![TokenKind::Let]);
        assert_eq!(lex("mut"), vec![TokenKind::Mut]);
        assert_eq!(lex("if"), vec![TokenKind::If]);
        assert_eq!(lex("else"), vec![TokenKind::Else]);
        assert_eq!(lex("return"), vec![TokenKind::Return]);
        assert_eq!(lex("resource"), vec![TokenKind::Resource]);
        assert_eq!(lex("parallel"), vec![TokenKind::Parallel]);
    }

    #[test]
    fn test_identifiers() {
        assert_eq!(lex("foo"), vec![TokenKind::Identifier("foo".to_string())]);
        assert_eq!(lex("_bar"), vec![TokenKind::Identifier("_bar".to_string())]);
        assert_eq!(lex("baz123"), vec![TokenKind::Identifier("baz123".to_string())]);
    }

    #[test]
    fn test_numbers() {
        assert_eq!(lex("123"), vec![TokenKind::IntLiteral("123".to_string())]);
        assert_eq!(lex("0xff"), vec![TokenKind::IntLiteral("0xff".to_string())]);
        assert_eq!(lex("0b1010"), vec![TokenKind::IntLiteral("0b1010".to_string())]);
        assert_eq!(lex("0o777"), vec![TokenKind::IntLiteral("0o777".to_string())]);
        assert_eq!(lex("1_000_000"), vec![TokenKind::IntLiteral("1000000".to_string())]);
        assert_eq!(lex("3.14"), vec![TokenKind::FloatLiteral("3.14".to_string())]);
        assert_eq!(lex("1e10"), vec![TokenKind::FloatLiteral("1e10".to_string())]);
    }

    #[test]
    fn test_address_literal() {
        let addr = "0x1234567890abcdef1234567890abcdef12345678";
        assert_eq!(lex(addr), vec![TokenKind::AddressLiteral(addr.to_string())]);
    }

    #[test]
    fn test_strings() {
        assert_eq!(
            lex("\"hello\""),
            vec![TokenKind::StringLiteral("hello".to_string())]
        );
        assert_eq!(
            lex("\"hello\\nworld\""),
            vec![TokenKind::StringLiteral("hello\nworld".to_string())]
        );
        assert_eq!(
            lex("\"tab\\there\""),
            vec![TokenKind::StringLiteral("tab\there".to_string())]
        );
    }

    #[test]
    fn test_byte_strings() {
        assert_eq!(
            lex("b\"hello\""),
            vec![TokenKind::ByteStringLiteral(b"hello".to_vec())]
        );
    }

    #[test]
    fn test_operators() {
        assert_eq!(lex("+"), vec![TokenKind::Plus]);
        assert_eq!(lex("-"), vec![TokenKind::Minus]);
        assert_eq!(lex("*"), vec![TokenKind::Star]);
        assert_eq!(lex("/"), vec![TokenKind::Slash]);
        assert_eq!(lex("%"), vec![TokenKind::Percent]);
        assert_eq!(lex("**"), vec![TokenKind::StarStar]);
        assert_eq!(lex("=="), vec![TokenKind::EqEq]);
        assert_eq!(lex("!="), vec![TokenKind::NotEq]);
        assert_eq!(lex("<"), vec![TokenKind::Lt]);
        assert_eq!(lex(">"), vec![TokenKind::Gt]);
        assert_eq!(lex("<="), vec![TokenKind::LtEq]);
        assert_eq!(lex(">="), vec![TokenKind::GtEq]);
        assert_eq!(lex("&&"), vec![TokenKind::And]);
        assert_eq!(lex("||"), vec![TokenKind::Or]);
        assert_eq!(lex("!"), vec![TokenKind::Not]);
        assert_eq!(lex("<<"), vec![TokenKind::Shl]);
        assert_eq!(lex(">>"), vec![TokenKind::Shr]);
        assert_eq!(lex("->"), vec![TokenKind::Arrow]);
        assert_eq!(lex("=>"), vec![TokenKind::FatArrow]);
        assert_eq!(lex("::"), vec![TokenKind::ColonColon]);
        assert_eq!(lex(".."), vec![TokenKind::DotDot]);
        assert_eq!(lex("..="), vec![TokenKind::DotDotEq]);
    }

    #[test]
    fn test_compound_assignment() {
        assert_eq!(lex("+="), vec![TokenKind::PlusEq]);
        assert_eq!(lex("-="), vec![TokenKind::MinusEq]);
        assert_eq!(lex("*="), vec![TokenKind::StarEq]);
        assert_eq!(lex("/="), vec![TokenKind::SlashEq]);
        assert_eq!(lex("%="), vec![TokenKind::PercentEq]);
        assert_eq!(lex("&="), vec![TokenKind::AmpersandEq]);
        assert_eq!(lex("|="), vec![TokenKind::PipeEq]);
        assert_eq!(lex("^="), vec![TokenKind::CaretEq]);
        assert_eq!(lex("<<="), vec![TokenKind::ShlEq]);
        assert_eq!(lex(">>="), vec![TokenKind::ShrEq]);
    }

    #[test]
    fn test_delimiters() {
        assert_eq!(lex("()"), vec![TokenKind::LParen, TokenKind::RParen]);
        assert_eq!(lex("[]"), vec![TokenKind::LBracket, TokenKind::RBracket]);
        assert_eq!(lex("{}"), vec![TokenKind::LBrace, TokenKind::RBrace]);
    }

    #[test]
    fn test_comments() {
        assert_eq!(lex("// comment\nfoo"), vec![TokenKind::Identifier("foo".to_string())]);
        assert_eq!(lex("/* block */ foo"), vec![TokenKind::Identifier("foo".to_string())]);
        assert_eq!(lex("/* nested /* comment */ */ foo"), vec![TokenKind::Identifier("foo".to_string())]);
    }

    #[test]
    fn test_booleans() {
        assert_eq!(lex("true"), vec![TokenKind::BoolLiteral(true)]);
        assert_eq!(lex("false"), vec![TokenKind::BoolLiteral(false)]);
    }

    #[test]
    fn test_contract_snippet() {
        let source = r#"
            contract Token {
                storage {
                    total_supply: u256,
                }

                pub fn transfer(to: address, amount: u256) -> bool {
                    return true;
                }
            }
        "#;
        let tokens = lex(source);
        assert!(tokens.contains(&TokenKind::Contract));
        assert!(tokens.contains(&TokenKind::Storage));
        assert!(tokens.contains(&TokenKind::Pub));
        assert!(tokens.contains(&TokenKind::Fn));
        assert!(tokens.contains(&TokenKind::Return));
        assert!(tokens.contains(&TokenKind::BoolLiteral(true)));
    }
}
