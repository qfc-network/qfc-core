//! Parser for QuantumScript
//!
//! Parses a token stream into an Abstract Syntax Tree (AST).

use thiserror::Error;

use crate::ast::*;
use crate::lexer::{Span, Token, TokenKind};

/// Parser errors
#[derive(Debug, Error, Clone)]
pub enum ParseError {
    #[error(
        "unexpected token: expected {expected}, found {found} at line {line}, column {column}"
    )]
    UnexpectedToken {
        expected: String,
        found: String,
        line: u32,
        column: u32,
    },

    #[error("unexpected end of file")]
    UnexpectedEof,

    #[error("invalid expression at line {0}, column {1}")]
    InvalidExpression(u32, u32),

    #[error("invalid pattern at line {0}, column {1}")]
    InvalidPattern(u32, u32),

    #[error("invalid type at line {0}, column {1}")]
    InvalidType(u32, u32),

    #[error("expected identifier at line {0}, column {1}")]
    ExpectedIdentifier(u32, u32),

    #[error("duplicate modifier '{0}' at line {1}, column {2}")]
    DuplicateModifier(String, u32, u32),
}

pub type ParseResult<T> = Result<T, ParseError>;

/// Parser state
pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    next_node_id: NodeId,
}

impl Parser {
    /// Create a new parser from tokens
    pub fn new(tokens: Vec<Token>) -> Self {
        Self {
            tokens,
            pos: 0,
            next_node_id: 0,
        }
    }

    /// Parse a complete source file
    pub fn parse_file(&mut self) -> ParseResult<SourceFile> {
        let start_span = self.current_span();
        let mut items = Vec::new();

        while !self.is_eof() {
            items.push(self.parse_item()?);
        }

        let end_span = self.current_span();
        Ok(SourceFile {
            id: self.next_id(),
            span: start_span.merge(end_span),
            items,
        })
    }

    // ========================================================================
    // Helper methods
    // ========================================================================

    fn next_id(&mut self) -> NodeId {
        let id = self.next_node_id;
        self.next_node_id += 1;
        id
    }

    fn current(&self) -> &Token {
        self.tokens
            .get(self.pos)
            .unwrap_or_else(|| self.tokens.last().expect("tokens should not be empty"))
    }

    fn current_kind(&self) -> &TokenKind {
        &self.current().kind
    }

    fn current_span(&self) -> Span {
        self.current().span
    }

    fn is_eof(&self) -> bool {
        matches!(self.current_kind(), TokenKind::Eof)
    }

    fn advance(&mut self) -> Token {
        let token = self.current().clone();
        if self.pos < self.tokens.len() - 1 {
            self.pos += 1;
        }
        token
    }

    #[allow(dead_code)]
    fn peek(&self) -> &TokenKind {
        self.tokens
            .get(self.pos + 1)
            .map(|t| &t.kind)
            .unwrap_or(&TokenKind::Eof)
    }

    fn check(&self, kind: &TokenKind) -> bool {
        std::mem::discriminant(self.current_kind()) == std::mem::discriminant(kind)
    }

    fn check_keyword(&self, keyword: &TokenKind) -> bool {
        self.current_kind() == keyword
    }

    fn eat(&mut self, kind: &TokenKind) -> bool {
        if self.check(kind) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, kind: &TokenKind) -> ParseResult<Token> {
        if self.check(kind) {
            Ok(self.advance())
        } else {
            let span = self.current_span();
            Err(ParseError::UnexpectedToken {
                expected: format!("{}", kind),
                found: format!("{}", self.current_kind()),
                line: span.line,
                column: span.column,
            })
        }
    }

    fn expect_identifier(&mut self) -> ParseResult<Ident> {
        match self.current_kind().clone() {
            TokenKind::Identifier(name) => {
                let span = self.current_span();
                self.advance();
                Ok(Ident::new(name, span))
            }
            _ => {
                let span = self.current_span();
                Err(ParseError::ExpectedIdentifier(span.line, span.column))
            }
        }
    }

    // ========================================================================
    // Item parsing
    // ========================================================================

    fn parse_item(&mut self) -> ParseResult<Item> {
        // Parse optional visibility
        let visibility = self.parse_visibility();

        match self.current_kind() {
            TokenKind::Import => self.parse_import().map(Item::Import),
            TokenKind::Contract => self.parse_contract(visibility).map(Item::Contract),
            TokenKind::Interface => self.parse_interface(visibility).map(Item::Interface),
            TokenKind::Library => self.parse_library(visibility).map(Item::Library),
            TokenKind::Struct | TokenKind::Resource => {
                self.parse_struct(visibility).map(Item::Struct)
            }
            TokenKind::Enum => self.parse_enum(visibility).map(Item::Enum),
            TokenKind::Type => self.parse_type_alias(visibility).map(Item::TypeAlias),
            TokenKind::Const => self.parse_const(visibility).map(Item::Const),
            TokenKind::Fn
            | TokenKind::Pure
            | TokenKind::View
            | TokenKind::Payable
            | TokenKind::Parallel => self.parse_function(visibility).map(Item::Function),
            _ => {
                let span = self.current_span();
                Err(ParseError::UnexpectedToken {
                    expected: "item".to_string(),
                    found: format!("{}", self.current_kind()),
                    line: span.line,
                    column: span.column,
                })
            }
        }
    }

    fn parse_visibility(&mut self) -> Visibility {
        if self.eat(&TokenKind::Pub) {
            Visibility::Public
        } else {
            Visibility::Private
        }
    }

    fn parse_import(&mut self) -> ParseResult<ImportItem> {
        let start_span = self.current_span();
        self.expect(&TokenKind::Import)?;

        let path = self.parse_import_path()?;

        let alias = if self.eat(&TokenKind::As) {
            Some(self.expect_identifier()?)
        } else {
            None
        };

        self.expect(&TokenKind::Semi)?;

        Ok(ImportItem {
            id: self.next_id(),
            span: start_span.merge(self.current_span()),
            path,
            alias,
        })
    }

    fn parse_import_path(&mut self) -> ParseResult<ImportPath> {
        let start_span = self.current_span();
        let mut segments = vec![self.expect_identifier()?];

        while self.eat(&TokenKind::ColonColon) {
            segments.push(self.expect_identifier()?);
        }

        Ok(ImportPath {
            segments,
            span: start_span.merge(self.current_span()),
        })
    }

    fn parse_contract(&mut self, _visibility: Visibility) -> ParseResult<ContractDef> {
        let start_span = self.current_span();
        self.expect(&TokenKind::Contract)?;

        let name = self.expect_identifier()?;
        let generics = self.parse_optional_generics()?;

        // Parse inheritance
        let mut inherits = Vec::new();
        if self.eat(&TokenKind::Colon) {
            inherits.push(self.parse_type_path()?);
            while self.eat(&TokenKind::Comma) {
                inherits.push(self.parse_type_path()?);
            }
        }

        self.expect(&TokenKind::LBrace)?;

        let mut items = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.is_eof() {
            items.push(self.parse_contract_item()?);
        }

        self.expect(&TokenKind::RBrace)?;

        Ok(ContractDef {
            id: self.next_id(),
            span: start_span.merge(self.current_span()),
            name,
            generics,
            inherits,
            items,
        })
    }

    fn parse_contract_item(&mut self) -> ParseResult<ContractItem> {
        let visibility = self.parse_visibility();

        match self.current_kind() {
            TokenKind::Storage => self.parse_storage_block().map(ContractItem::Storage),
            TokenKind::Event => self.parse_event().map(ContractItem::Event),
            TokenKind::Error => self.parse_error_def().map(ContractItem::Error),
            TokenKind::Modifier => self.parse_modifier().map(ContractItem::Modifier),
            TokenKind::Constructor => self
                .parse_constructor(visibility)
                .map(ContractItem::Constructor),
            TokenKind::Fallback => self.parse_fallback().map(ContractItem::Fallback),
            TokenKind::Receive => self.parse_receive().map(ContractItem::Receive),
            TokenKind::Const => self.parse_const(visibility).map(ContractItem::Const),
            TokenKind::Struct | TokenKind::Resource => {
                self.parse_struct(visibility).map(ContractItem::Struct)
            }
            TokenKind::Enum => self.parse_enum(visibility).map(ContractItem::Enum),
            TokenKind::Fn
            | TokenKind::Pure
            | TokenKind::View
            | TokenKind::Payable
            | TokenKind::Parallel => self.parse_function(visibility).map(ContractItem::Function),
            _ => {
                let span = self.current_span();
                Err(ParseError::UnexpectedToken {
                    expected: "contract item".to_string(),
                    found: format!("{}", self.current_kind()),
                    line: span.line,
                    column: span.column,
                })
            }
        }
    }

    fn parse_interface(&mut self, _visibility: Visibility) -> ParseResult<InterfaceDef> {
        let start_span = self.current_span();
        self.expect(&TokenKind::Interface)?;

        let name = self.expect_identifier()?;
        let generics = self.parse_optional_generics()?;

        let mut extends = Vec::new();
        if self.eat(&TokenKind::Colon) {
            extends.push(self.parse_type_path()?);
            while self.eat(&TokenKind::Comma) {
                extends.push(self.parse_type_path()?);
            }
        }

        self.expect(&TokenKind::LBrace)?;

        let mut items = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.is_eof() {
            items.push(self.parse_interface_item()?);
        }

        self.expect(&TokenKind::RBrace)?;

        Ok(InterfaceDef {
            id: self.next_id(),
            span: start_span.merge(self.current_span()),
            name,
            generics,
            extends,
            items,
        })
    }

    fn parse_interface_item(&mut self) -> ParseResult<InterfaceItem> {
        match self.current_kind() {
            TokenKind::Event => self.parse_event().map(InterfaceItem::Event),
            TokenKind::Error => self.parse_error_def().map(InterfaceItem::Error),
            _ => self.parse_function_sig().map(InterfaceItem::Function),
        }
    }

    fn parse_library(&mut self, _visibility: Visibility) -> ParseResult<LibraryDef> {
        let start_span = self.current_span();
        self.expect(&TokenKind::Library)?;

        let name = self.expect_identifier()?;

        self.expect(&TokenKind::LBrace)?;

        let mut items = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.is_eof() {
            items.push(self.parse_library_item()?);
        }

        self.expect(&TokenKind::RBrace)?;

        Ok(LibraryDef {
            id: self.next_id(),
            span: start_span.merge(self.current_span()),
            name,
            items,
        })
    }

    fn parse_library_item(&mut self) -> ParseResult<LibraryItem> {
        let visibility = self.parse_visibility();

        match self.current_kind() {
            TokenKind::Struct | TokenKind::Resource => {
                self.parse_struct(visibility).map(LibraryItem::Struct)
            }
            TokenKind::Const => self.parse_const(visibility).map(LibraryItem::Const),
            _ => self.parse_function(visibility).map(LibraryItem::Function),
        }
    }

    // ========================================================================
    // Storage
    // ========================================================================

    fn parse_storage_block(&mut self) -> ParseResult<StorageBlock> {
        let start_span = self.current_span();
        self.expect(&TokenKind::Storage)?;
        self.expect(&TokenKind::LBrace)?;

        let mut fields = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.is_eof() {
            fields.push(self.parse_storage_field()?);
        }

        self.expect(&TokenKind::RBrace)?;

        Ok(StorageBlock {
            id: self.next_id(),
            span: start_span.merge(self.current_span()),
            fields,
        })
    }

    fn parse_storage_field(&mut self) -> ParseResult<StorageField> {
        let start_span = self.current_span();
        let visibility = self.parse_visibility();
        let name = self.expect_identifier()?;

        self.expect(&TokenKind::Colon)?;
        let ty = self.parse_type()?;

        let default = if self.eat(&TokenKind::Eq) {
            Some(self.parse_expr()?)
        } else {
            None
        };

        self.expect(&TokenKind::Comma)?;

        Ok(StorageField {
            id: self.next_id(),
            span: start_span.merge(self.current_span()),
            visibility,
            name,
            ty,
            default,
        })
    }

    // ========================================================================
    // Events and Errors
    // ========================================================================

    fn parse_event(&mut self) -> ParseResult<EventDef> {
        let start_span = self.current_span();
        self.expect(&TokenKind::Event)?;

        let name = self.expect_identifier()?;

        self.expect(&TokenKind::LBrace)?;

        let mut fields = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.is_eof() {
            fields.push(self.parse_event_field()?);
        }

        self.expect(&TokenKind::RBrace)?;

        Ok(EventDef {
            id: self.next_id(),
            span: start_span.merge(self.current_span()),
            name,
            fields,
        })
    }

    fn parse_event_field(&mut self) -> ParseResult<EventField> {
        let start_span = self.current_span();

        // Check for #[indexed]
        let indexed = if self.eat(&TokenKind::Hash) {
            self.expect(&TokenKind::LBracket)?;
            let attr = self.expect_identifier()?;
            self.expect(&TokenKind::RBracket)?;
            attr.name == "indexed"
        } else {
            false
        };

        let name = self.expect_identifier()?;
        self.expect(&TokenKind::Colon)?;
        let ty = self.parse_type()?;
        self.expect(&TokenKind::Comma)?;

        Ok(EventField {
            id: self.next_id(),
            span: start_span.merge(self.current_span()),
            indexed,
            name,
            ty,
        })
    }

    fn parse_error_def(&mut self) -> ParseResult<ErrorDef> {
        let start_span = self.current_span();
        self.expect(&TokenKind::Error)?;

        let name = self.expect_identifier()?;

        self.expect(&TokenKind::LBrace)?;

        let mut fields = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.is_eof() {
            fields.push(self.parse_error_field()?);
        }

        self.expect(&TokenKind::RBrace)?;

        Ok(ErrorDef {
            id: self.next_id(),
            span: start_span.merge(self.current_span()),
            name,
            fields,
        })
    }

    fn parse_error_field(&mut self) -> ParseResult<ErrorField> {
        let start_span = self.current_span();
        let name = self.expect_identifier()?;
        self.expect(&TokenKind::Colon)?;
        let ty = self.parse_type()?;
        self.expect(&TokenKind::Comma)?;

        Ok(ErrorField {
            id: self.next_id(),
            span: start_span.merge(self.current_span()),
            name,
            ty,
        })
    }

    // ========================================================================
    // Functions
    // ========================================================================

    fn parse_function(&mut self, visibility: Visibility) -> ParseResult<FunctionDef> {
        let start_span = self.current_span();
        let sig = self.parse_function_sig_with_visibility(visibility)?;
        let body = self.parse_block()?;

        // Parse optional spec blocks
        let mut specs = Vec::new();
        while self.check_keyword(&TokenKind::Spec) {
            specs.push(self.parse_spec_block()?);
        }

        Ok(FunctionDef {
            id: self.next_id(),
            span: start_span.merge(self.current_span()),
            sig,
            body,
            specs,
        })
    }

    fn parse_function_sig(&mut self) -> ParseResult<FunctionSig> {
        let visibility = self.parse_visibility();
        self.parse_function_sig_with_visibility(visibility)
    }

    fn parse_function_sig_with_visibility(
        &mut self,
        visibility: Visibility,
    ) -> ParseResult<FunctionSig> {
        let start_span = self.current_span();

        // Parse modifiers (pure, view, payable, parallel)
        let modifiers = self.parse_function_modifiers()?;

        self.expect(&TokenKind::Fn)?;
        let name = self.expect_identifier()?;
        let generics = self.parse_optional_generics()?;

        // Parse parameters
        self.expect(&TokenKind::LParen)?;
        let params = self.parse_params()?;
        self.expect(&TokenKind::RParen)?;

        // Parse return type
        let return_type = if self.eat(&TokenKind::Arrow) {
            Some(self.parse_type()?)
        } else {
            None
        };

        Ok(FunctionSig {
            id: self.next_id(),
            span: start_span.merge(self.current_span()),
            visibility,
            modifiers,
            name,
            generics,
            params,
            return_type,
        })
    }

    fn parse_function_modifiers(&mut self) -> ParseResult<FunctionModifiers> {
        let mut modifiers = FunctionModifiers::default();

        loop {
            match self.current_kind() {
                TokenKind::Pure => {
                    self.advance();
                    modifiers.is_pure = true;
                }
                TokenKind::View => {
                    self.advance();
                    modifiers.is_view = true;
                }
                TokenKind::Payable => {
                    self.advance();
                    modifiers.is_payable = true;
                }
                TokenKind::Parallel => {
                    self.advance();
                    modifiers.is_parallel = true;
                }
                _ => break,
            }
        }

        Ok(modifiers)
    }

    fn parse_params(&mut self) -> ParseResult<Vec<Param>> {
        let mut params = Vec::new();

        if !self.check(&TokenKind::RParen) {
            params.push(self.parse_param()?);
            while self.eat(&TokenKind::Comma) {
                if self.check(&TokenKind::RParen) {
                    break; // trailing comma
                }
                params.push(self.parse_param()?);
            }
        }

        Ok(params)
    }

    fn parse_param(&mut self) -> ParseResult<Param> {
        let start_span = self.current_span();
        let pattern = self.parse_pattern()?;
        self.expect(&TokenKind::Colon)?;
        let ty = self.parse_type()?;

        Ok(Param {
            id: self.next_id(),
            span: start_span.merge(self.current_span()),
            pattern,
            ty,
        })
    }

    fn parse_constructor(&mut self, visibility: Visibility) -> ParseResult<ConstructorDef> {
        let start_span = self.current_span();
        self.expect(&TokenKind::Constructor)?;

        let modifiers = self.parse_function_modifiers()?;

        self.expect(&TokenKind::LParen)?;
        let params = self.parse_params()?;
        self.expect(&TokenKind::RParen)?;

        let body = self.parse_block()?;

        Ok(ConstructorDef {
            id: self.next_id(),
            span: start_span.merge(self.current_span()),
            visibility,
            modifiers,
            params,
            body,
        })
    }

    fn parse_fallback(&mut self) -> ParseResult<FallbackDef> {
        let start_span = self.current_span();
        self.expect(&TokenKind::Fallback)?;
        let body = self.parse_block()?;

        Ok(FallbackDef {
            id: self.next_id(),
            span: start_span.merge(self.current_span()),
            body,
        })
    }

    fn parse_receive(&mut self) -> ParseResult<ReceiveDef> {
        let start_span = self.current_span();
        self.expect(&TokenKind::Receive)?;
        let body = self.parse_block()?;

        Ok(ReceiveDef {
            id: self.next_id(),
            span: start_span.merge(self.current_span()),
            body,
        })
    }

    fn parse_modifier(&mut self) -> ParseResult<ModifierDef> {
        let start_span = self.current_span();
        self.expect(&TokenKind::Modifier)?;

        let name = self.expect_identifier()?;

        self.expect(&TokenKind::LParen)?;
        let params = self.parse_params()?;
        self.expect(&TokenKind::RParen)?;

        let body = self.parse_block()?;

        Ok(ModifierDef {
            id: self.next_id(),
            span: start_span.merge(self.current_span()),
            name,
            params,
            body,
        })
    }

    // ========================================================================
    // Types
    // ========================================================================

    fn parse_type(&mut self) -> ParseResult<Type> {
        let start_span = self.current_span();

        let kind = match self.current_kind().clone() {
            // Primitive types
            TokenKind::Bool => {
                self.advance();
                TypeKind::Primitive(PrimitiveType::Bool)
            }
            TokenKind::U8 => {
                self.advance();
                TypeKind::Primitive(PrimitiveType::U8)
            }
            TokenKind::U16 => {
                self.advance();
                TypeKind::Primitive(PrimitiveType::U16)
            }
            TokenKind::U32 => {
                self.advance();
                TypeKind::Primitive(PrimitiveType::U32)
            }
            TokenKind::U64 => {
                self.advance();
                TypeKind::Primitive(PrimitiveType::U64)
            }
            TokenKind::U128 => {
                self.advance();
                TypeKind::Primitive(PrimitiveType::U128)
            }
            TokenKind::U256 => {
                self.advance();
                TypeKind::Primitive(PrimitiveType::U256)
            }
            TokenKind::I8 => {
                self.advance();
                TypeKind::Primitive(PrimitiveType::I8)
            }
            TokenKind::I16 => {
                self.advance();
                TypeKind::Primitive(PrimitiveType::I16)
            }
            TokenKind::I32 => {
                self.advance();
                TypeKind::Primitive(PrimitiveType::I32)
            }
            TokenKind::I64 => {
                self.advance();
                TypeKind::Primitive(PrimitiveType::I64)
            }
            TokenKind::I128 => {
                self.advance();
                TypeKind::Primitive(PrimitiveType::I128)
            }
            TokenKind::I256 => {
                self.advance();
                TypeKind::Primitive(PrimitiveType::I256)
            }
            TokenKind::Address => {
                self.advance();
                TypeKind::Primitive(PrimitiveType::Address)
            }
            TokenKind::Bytes => {
                self.advance();
                TypeKind::Primitive(PrimitiveType::Bytes)
            }
            TokenKind::String_ => {
                self.advance();
                TypeKind::Primitive(PrimitiveType::String)
            }

            // Mapping type
            TokenKind::Mapping => {
                self.advance();
                self.expect(&TokenKind::LParen)?;
                let key = self.parse_type()?;
                self.expect(&TokenKind::FatArrow)?;
                let value = self.parse_type()?;
                self.expect(&TokenKind::RParen)?;
                TypeKind::Mapping(Box::new(key), Box::new(value))
            }

            // Option type
            TokenKind::Option_ => {
                self.advance();
                self.expect(&TokenKind::Lt)?;
                let inner = self.parse_type()?;
                self.expect(&TokenKind::Gt)?;
                TypeKind::Option(Box::new(inner))
            }

            // Result type
            TokenKind::Result_ => {
                self.advance();
                self.expect(&TokenKind::Lt)?;
                let ok = self.parse_type()?;
                self.expect(&TokenKind::Comma)?;
                let err = self.parse_type()?;
                self.expect(&TokenKind::Gt)?;
                TypeKind::Result(Box::new(ok), Box::new(err))
            }

            // Vec type
            TokenKind::Vec_ => {
                self.advance();
                self.expect(&TokenKind::Lt)?;
                let inner = self.parse_type()?;
                self.expect(&TokenKind::Gt)?;
                TypeKind::Slice(Box::new(inner))
            }

            // Reference type
            TokenKind::Ampersand => {
                self.advance();
                let is_mut = self.eat(&TokenKind::Mut);
                let inner = self.parse_type()?;
                TypeKind::Reference(Box::new(inner), is_mut)
            }

            // Array or slice type
            TokenKind::LBracket => {
                self.advance();
                let elem = self.parse_type()?;
                if self.eat(&TokenKind::Semi) {
                    let size = self.parse_expr()?;
                    self.expect(&TokenKind::RBracket)?;
                    TypeKind::Array(Box::new(elem), Box::new(size))
                } else {
                    self.expect(&TokenKind::RBracket)?;
                    TypeKind::Slice(Box::new(elem))
                }
            }

            // Tuple type
            TokenKind::LParen => {
                self.advance();
                let mut types = Vec::new();
                if !self.check(&TokenKind::RParen) {
                    types.push(self.parse_type()?);
                    while self.eat(&TokenKind::Comma) {
                        if self.check(&TokenKind::RParen) {
                            break;
                        }
                        types.push(self.parse_type()?);
                    }
                }
                self.expect(&TokenKind::RParen)?;
                TypeKind::Tuple(types)
            }

            // Self type
            TokenKind::Self_ => {
                self.advance();
                TypeKind::SelfType
            }

            // Function type: fn(T1, T2) -> R
            TokenKind::Fn => {
                self.advance();
                self.expect(&TokenKind::LParen)?;
                let mut param_types = Vec::new();
                if !self.check(&TokenKind::RParen) {
                    param_types.push(self.parse_type()?);
                    while self.eat(&TokenKind::Comma) {
                        param_types.push(self.parse_type()?);
                    }
                }
                self.expect(&TokenKind::RParen)?;
                let return_type = if self.eat(&TokenKind::Arrow) {
                    Some(Box::new(self.parse_type()?))
                } else {
                    None
                };
                TypeKind::Function(param_types, return_type)
            }

            // Named type (path)
            TokenKind::Identifier(_) => {
                let path = self.parse_type_path()?;
                TypeKind::Path(path)
            }

            _ => {
                let span = self.current_span();
                return Err(ParseError::InvalidType(span.line, span.column));
            }
        };

        Ok(Type {
            id: self.next_id(),
            span: start_span.merge(self.current_span()),
            kind,
        })
    }

    fn parse_type_path(&mut self) -> ParseResult<TypePath> {
        let start_span = self.current_span();
        let mut segments = vec![self.parse_path_segment()?];

        while self.eat(&TokenKind::ColonColon) {
            segments.push(self.parse_path_segment()?);
        }

        Ok(TypePath {
            segments,
            span: start_span.merge(self.current_span()),
        })
    }

    fn parse_path_segment(&mut self) -> ParseResult<PathSegment> {
        let start_span = self.current_span();
        let ident = self.expect_identifier()?;

        let generics = if self.eat(&TokenKind::Lt) {
            let mut types = vec![self.parse_type()?];
            while self.eat(&TokenKind::Comma) {
                types.push(self.parse_type()?);
            }
            self.expect(&TokenKind::Gt)?;
            Some(types)
        } else {
            None
        };

        Ok(PathSegment {
            ident,
            generics,
            span: start_span.merge(self.current_span()),
        })
    }

    // ========================================================================
    // Structs and Enums
    // ========================================================================

    fn parse_struct(&mut self, visibility: Visibility) -> ParseResult<StructDef> {
        let start_span = self.current_span();

        // Check for resource keyword
        let is_resource = self.eat(&TokenKind::Resource);
        if !is_resource {
            self.expect(&TokenKind::Struct)?;
        }

        let name = self.expect_identifier()?;
        let generics = self.parse_optional_generics()?;

        // Parse abilities if resource
        let abilities = if is_resource {
            self.parse_abilities()?
        } else {
            Vec::new()
        };

        self.expect(&TokenKind::LBrace)?;

        let mut fields = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.is_eof() {
            fields.push(self.parse_struct_field()?);
        }

        self.expect(&TokenKind::RBrace)?;

        Ok(StructDef {
            id: self.next_id(),
            span: start_span.merge(self.current_span()),
            visibility,
            name,
            generics,
            abilities,
            fields,
        })
    }

    fn parse_struct_field(&mut self) -> ParseResult<StructField> {
        let start_span = self.current_span();
        let visibility = self.parse_visibility();
        let name = self.expect_identifier()?;
        self.expect(&TokenKind::Colon)?;
        let ty = self.parse_type()?;
        self.expect(&TokenKind::Comma)?;

        Ok(StructField {
            id: self.next_id(),
            span: start_span.merge(self.current_span()),
            visibility,
            name,
            ty,
        })
    }

    fn parse_abilities(&mut self) -> ParseResult<Vec<ResourceAbility>> {
        let mut abilities = Vec::new();

        if self.eat(&TokenKind::Colon) {
            loop {
                let ability = match self.current_kind() {
                    TokenKind::Copy => {
                        self.advance();
                        ResourceAbility::Copy
                    }
                    TokenKind::Drop => {
                        self.advance();
                        ResourceAbility::Drop
                    }
                    TokenKind::Store => {
                        self.advance();
                        ResourceAbility::Store
                    }
                    TokenKind::Key => {
                        self.advance();
                        ResourceAbility::Key
                    }
                    _ => break,
                };
                abilities.push(ability);

                if !self.eat(&TokenKind::Plus) {
                    break;
                }
            }
        }

        Ok(abilities)
    }

    fn parse_enum(&mut self, visibility: Visibility) -> ParseResult<EnumDef> {
        let start_span = self.current_span();
        self.expect(&TokenKind::Enum)?;

        let name = self.expect_identifier()?;
        let generics = self.parse_optional_generics()?;

        self.expect(&TokenKind::LBrace)?;

        let mut variants = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.is_eof() {
            variants.push(self.parse_enum_variant()?);
        }

        self.expect(&TokenKind::RBrace)?;

        Ok(EnumDef {
            id: self.next_id(),
            span: start_span.merge(self.current_span()),
            visibility,
            name,
            generics,
            variants,
        })
    }

    fn parse_enum_variant(&mut self) -> ParseResult<EnumVariant> {
        let start_span = self.current_span();
        let name = self.expect_identifier()?;

        let fields = if self.check(&TokenKind::LParen) {
            // Tuple variant
            self.advance();
            let mut types = Vec::new();
            if !self.check(&TokenKind::RParen) {
                types.push(self.parse_type()?);
                while self.eat(&TokenKind::Comma) {
                    if self.check(&TokenKind::RParen) {
                        break;
                    }
                    types.push(self.parse_type()?);
                }
            }
            self.expect(&TokenKind::RParen)?;
            VariantFields::Tuple(types)
        } else if self.check(&TokenKind::LBrace) {
            // Struct variant
            self.advance();
            let mut fields = Vec::new();
            while !self.check(&TokenKind::RBrace) && !self.is_eof() {
                fields.push(self.parse_struct_field()?);
            }
            self.expect(&TokenKind::RBrace)?;
            VariantFields::Struct(fields)
        } else {
            // Unit variant
            VariantFields::Unit
        };

        self.eat(&TokenKind::Comma);

        Ok(EnumVariant {
            id: self.next_id(),
            span: start_span.merge(self.current_span()),
            name,
            fields,
        })
    }

    fn parse_type_alias(&mut self, visibility: Visibility) -> ParseResult<TypeAlias> {
        let start_span = self.current_span();
        self.expect(&TokenKind::Type)?;

        let name = self.expect_identifier()?;
        let generics = self.parse_optional_generics()?;

        self.expect(&TokenKind::Eq)?;
        let ty = self.parse_type()?;
        self.expect(&TokenKind::Semi)?;

        Ok(TypeAlias {
            id: self.next_id(),
            span: start_span.merge(self.current_span()),
            visibility,
            name,
            generics,
            ty,
        })
    }

    fn parse_const(&mut self, visibility: Visibility) -> ParseResult<ConstItem> {
        let start_span = self.current_span();
        self.expect(&TokenKind::Const)?;

        let name = self.expect_identifier()?;
        self.expect(&TokenKind::Colon)?;
        let ty = self.parse_type()?;
        self.expect(&TokenKind::Eq)?;
        let value = self.parse_expr()?;
        self.expect(&TokenKind::Semi)?;

        Ok(ConstItem {
            id: self.next_id(),
            span: start_span.merge(self.current_span()),
            visibility,
            name,
            ty,
            value,
        })
    }

    // ========================================================================
    // Generics
    // ========================================================================

    fn parse_optional_generics(&mut self) -> ParseResult<Option<Generics>> {
        if !self.check(&TokenKind::Lt) {
            return Ok(None);
        }

        let start_span = self.current_span();
        self.advance();

        let mut params = Vec::new();
        if !self.check(&TokenKind::Gt) {
            params.push(self.parse_generic_param()?);
            while self.eat(&TokenKind::Comma) {
                if self.check(&TokenKind::Gt) {
                    break;
                }
                params.push(self.parse_generic_param()?);
            }
        }

        self.expect(&TokenKind::Gt)?;

        let where_clause = if self.check_keyword(&TokenKind::Where) {
            Some(self.parse_where_clause()?)
        } else {
            None
        };

        Ok(Some(Generics {
            params,
            where_clause,
            span: start_span.merge(self.current_span()),
        }))
    }

    fn parse_generic_param(&mut self) -> ParseResult<GenericParam> {
        if self.check_keyword(&TokenKind::Const) {
            self.advance();
            let start_span = self.current_span();
            let name = self.expect_identifier()?;
            self.expect(&TokenKind::Colon)?;
            let ty = self.parse_type()?;
            let default = if self.eat(&TokenKind::Eq) {
                Some(self.parse_expr()?)
            } else {
                None
            };
            Ok(GenericParam::Const(ConstParam {
                id: self.next_id(),
                span: start_span.merge(self.current_span()),
                name,
                ty,
                default,
            }))
        } else {
            let start_span = self.current_span();
            let name = self.expect_identifier()?;

            let bounds = if self.eat(&TokenKind::Colon) {
                self.parse_type_bounds()?
            } else {
                Vec::new()
            };

            let default = if self.eat(&TokenKind::Eq) {
                Some(self.parse_type()?)
            } else {
                None
            };

            Ok(GenericParam::Type(TypeParam {
                id: self.next_id(),
                span: start_span.merge(self.current_span()),
                name,
                bounds,
                default,
            }))
        }
    }

    fn parse_type_bounds(&mut self) -> ParseResult<Vec<TypeBound>> {
        let mut bounds = vec![self.parse_type_bound()?];
        while self.eat(&TokenKind::Plus) {
            bounds.push(self.parse_type_bound()?);
        }
        Ok(bounds)
    }

    fn parse_type_bound(&mut self) -> ParseResult<TypeBound> {
        let path = self.parse_type_path()?;
        let span = path.span;
        Ok(TypeBound { path, span })
    }

    fn parse_where_clause(&mut self) -> ParseResult<WhereClause> {
        let start_span = self.current_span();
        self.expect(&TokenKind::Where)?;

        let mut predicates = vec![self.parse_where_predicate()?];
        while self.eat(&TokenKind::Comma) {
            if self.check(&TokenKind::LBrace) {
                break;
            }
            predicates.push(self.parse_where_predicate()?);
        }

        Ok(WhereClause {
            predicates,
            span: start_span.merge(self.current_span()),
        })
    }

    fn parse_where_predicate(&mut self) -> ParseResult<WherePredicate> {
        let start_span = self.current_span();
        let ty = self.parse_type()?;
        self.expect(&TokenKind::Colon)?;
        let bounds = self.parse_type_bounds()?;

        Ok(WherePredicate {
            ty,
            bounds,
            span: start_span.merge(self.current_span()),
        })
    }

    // ========================================================================
    // Statements
    // ========================================================================

    fn parse_block(&mut self) -> ParseResult<Block> {
        let start_span = self.current_span();
        self.expect(&TokenKind::LBrace)?;

        let mut stmts = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.is_eof() {
            stmts.push(self.parse_stmt()?);
        }

        self.expect(&TokenKind::RBrace)?;

        Ok(Block {
            id: self.next_id(),
            span: start_span.merge(self.current_span()),
            stmts,
        })
    }

    fn parse_stmt(&mut self) -> ParseResult<Stmt> {
        let start_span = self.current_span();

        let kind = match self.current_kind() {
            TokenKind::Let => StmtKind::Local(self.parse_local_stmt()?),
            TokenKind::Semi => {
                self.advance();
                StmtKind::Empty
            }
            _ => {
                let expr = self.parse_expr()?;
                if self.eat(&TokenKind::Semi) {
                    StmtKind::Semi(expr)
                } else {
                    StmtKind::Expr(expr)
                }
            }
        };

        Ok(Stmt {
            id: self.next_id(),
            span: start_span.merge(self.current_span()),
            kind,
        })
    }

    fn parse_local_stmt(&mut self) -> ParseResult<LocalStmt> {
        let start_span = self.current_span();
        self.expect(&TokenKind::Let)?;

        let is_mutable = self.eat(&TokenKind::Mut);
        let pattern = self.parse_pattern()?;

        let ty = if self.eat(&TokenKind::Colon) {
            Some(self.parse_type()?)
        } else {
            None
        };

        let init = if self.eat(&TokenKind::Eq) {
            Some(self.parse_expr()?)
        } else {
            None
        };

        self.expect(&TokenKind::Semi)?;

        Ok(LocalStmt {
            id: self.next_id(),
            span: start_span.merge(self.current_span()),
            pattern,
            ty,
            init,
            is_mutable,
        })
    }

    // ========================================================================
    // Expressions (Pratt parser)
    // ========================================================================

    fn parse_expr(&mut self) -> ParseResult<Expr> {
        self.parse_expr_with_precedence(0)
    }

    fn parse_expr_with_precedence(&mut self, min_prec: u8) -> ParseResult<Expr> {
        let mut left = self.parse_unary_expr()?;

        loop {
            let op = match self.current_kind() {
                TokenKind::Plus => BinaryOp::Add,
                TokenKind::Minus => BinaryOp::Sub,
                TokenKind::Star => BinaryOp::Mul,
                TokenKind::Slash => BinaryOp::Div,
                TokenKind::Percent => BinaryOp::Rem,
                TokenKind::StarStar => BinaryOp::Pow,
                TokenKind::EqEq => BinaryOp::Eq,
                TokenKind::NotEq => BinaryOp::Ne,
                TokenKind::Lt => BinaryOp::Lt,
                TokenKind::LtEq => BinaryOp::Le,
                TokenKind::Gt => BinaryOp::Gt,
                TokenKind::GtEq => BinaryOp::Ge,
                TokenKind::And => BinaryOp::And,
                TokenKind::Or => BinaryOp::Or,
                TokenKind::Ampersand => BinaryOp::BitAnd,
                TokenKind::Pipe => BinaryOp::BitOr,
                TokenKind::Caret => BinaryOp::BitXor,
                TokenKind::Shl => BinaryOp::Shl,
                TokenKind::Shr => BinaryOp::Shr,
                TokenKind::Eq => BinaryOp::Assign,
                TokenKind::PlusEq => BinaryOp::AddAssign,
                TokenKind::MinusEq => BinaryOp::SubAssign,
                TokenKind::StarEq => BinaryOp::MulAssign,
                TokenKind::SlashEq => BinaryOp::DivAssign,
                TokenKind::PercentEq => BinaryOp::RemAssign,
                TokenKind::AmpersandEq => BinaryOp::BitAndAssign,
                TokenKind::PipeEq => BinaryOp::BitOrAssign,
                TokenKind::CaretEq => BinaryOp::BitXorAssign,
                TokenKind::ShlEq => BinaryOp::ShlAssign,
                TokenKind::ShrEq => BinaryOp::ShrAssign,
                _ => break,
            };

            let (left_prec, right_prec) = binary_precedence(op);
            if left_prec < min_prec {
                break;
            }

            self.advance();
            let right = self.parse_expr_with_precedence(right_prec)?;
            let span = left.span.merge(right.span);

            left = Expr {
                id: self.next_id(),
                span,
                kind: ExprKind::Binary(op, Box::new(left), Box::new(right)),
            };
        }

        Ok(left)
    }

    fn parse_unary_expr(&mut self) -> ParseResult<Expr> {
        let start_span = self.current_span();

        match self.current_kind() {
            TokenKind::Minus => {
                self.advance();
                let expr = self.parse_unary_expr()?;
                Ok(Expr {
                    id: self.next_id(),
                    span: start_span.merge(expr.span),
                    kind: ExprKind::Unary(UnaryOp::Neg, Box::new(expr)),
                })
            }
            TokenKind::Not => {
                self.advance();
                let expr = self.parse_unary_expr()?;
                Ok(Expr {
                    id: self.next_id(),
                    span: start_span.merge(expr.span),
                    kind: ExprKind::Unary(UnaryOp::Not, Box::new(expr)),
                })
            }
            TokenKind::Tilde => {
                self.advance();
                let expr = self.parse_unary_expr()?;
                Ok(Expr {
                    id: self.next_id(),
                    span: start_span.merge(expr.span),
                    kind: ExprKind::Unary(UnaryOp::BitNot, Box::new(expr)),
                })
            }
            TokenKind::Ampersand => {
                self.advance();
                let is_mut = self.eat(&TokenKind::Mut);
                let expr = self.parse_unary_expr()?;
                Ok(Expr {
                    id: self.next_id(),
                    span: start_span.merge(expr.span),
                    kind: ExprKind::Reference(Box::new(expr), is_mut),
                })
            }
            TokenKind::Star => {
                self.advance();
                let expr = self.parse_unary_expr()?;
                Ok(Expr {
                    id: self.next_id(),
                    span: start_span.merge(expr.span),
                    kind: ExprKind::Deref(Box::new(expr)),
                })
            }
            _ => self.parse_postfix_expr(),
        }
    }

    fn parse_postfix_expr(&mut self) -> ParseResult<Expr> {
        let mut expr = self.parse_primary_expr()?;

        loop {
            let start_span = expr.span;
            match self.current_kind() {
                TokenKind::LParen => {
                    // Function call
                    self.advance();
                    let args = self.parse_call_args()?;
                    self.expect(&TokenKind::RParen)?;
                    expr = Expr {
                        id: self.next_id(),
                        span: start_span.merge(self.current_span()),
                        kind: ExprKind::Call(Box::new(expr), args),
                    };
                }
                TokenKind::LBracket => {
                    // Index
                    self.advance();
                    let index = self.parse_expr()?;
                    self.expect(&TokenKind::RBracket)?;
                    expr = Expr {
                        id: self.next_id(),
                        span: start_span.merge(self.current_span()),
                        kind: ExprKind::Index(Box::new(expr), Box::new(index)),
                    };
                }
                TokenKind::Dot => {
                    self.advance();
                    let field = self.expect_identifier()?;

                    // Check for method call
                    if self.check(&TokenKind::LParen) {
                        self.advance();
                        let args = self.parse_call_args()?;
                        self.expect(&TokenKind::RParen)?;
                        expr = Expr {
                            id: self.next_id(),
                            span: start_span.merge(self.current_span()),
                            kind: ExprKind::MethodCall(Box::new(expr), field, args),
                        };
                    } else {
                        expr = Expr {
                            id: self.next_id(),
                            span: start_span.merge(self.current_span()),
                            kind: ExprKind::Field(Box::new(expr), field),
                        };
                    }
                }
                TokenKind::Question => {
                    self.advance();
                    expr = Expr {
                        id: self.next_id(),
                        span: start_span.merge(self.current_span()),
                        kind: ExprKind::Try(Box::new(expr)),
                    };
                }
                TokenKind::As => {
                    self.advance();
                    let ty = self.parse_type()?;
                    expr = Expr {
                        id: self.next_id(),
                        span: start_span.merge(self.current_span()),
                        kind: ExprKind::Cast(Box::new(expr), ty),
                    };
                }
                _ => break,
            }
        }

        Ok(expr)
    }

    fn parse_call_args(&mut self) -> ParseResult<Vec<Expr>> {
        let mut args = Vec::new();
        if !self.check(&TokenKind::RParen) {
            args.push(self.parse_expr()?);
            while self.eat(&TokenKind::Comma) {
                if self.check(&TokenKind::RParen) {
                    break;
                }
                args.push(self.parse_expr()?);
            }
        }
        Ok(args)
    }

    fn parse_primary_expr(&mut self) -> ParseResult<Expr> {
        let start_span = self.current_span();

        match self.current_kind().clone() {
            // Literals
            TokenKind::IntLiteral(s) => {
                self.advance();
                Ok(Expr {
                    id: self.next_id(),
                    span: start_span,
                    kind: ExprKind::Literal(Literal::Int(s, None)),
                })
            }
            TokenKind::FloatLiteral(s) => {
                self.advance();
                Ok(Expr {
                    id: self.next_id(),
                    span: start_span,
                    kind: ExprKind::Literal(Literal::Float(s)),
                })
            }
            TokenKind::StringLiteral(s) => {
                self.advance();
                Ok(Expr {
                    id: self.next_id(),
                    span: start_span,
                    kind: ExprKind::Literal(Literal::String(s)),
                })
            }
            TokenKind::ByteStringLiteral(b) => {
                self.advance();
                Ok(Expr {
                    id: self.next_id(),
                    span: start_span,
                    kind: ExprKind::Literal(Literal::ByteString(b)),
                })
            }
            TokenKind::BoolLiteral(b) => {
                self.advance();
                Ok(Expr {
                    id: self.next_id(),
                    span: start_span,
                    kind: ExprKind::Literal(Literal::Bool(b)),
                })
            }
            TokenKind::AddressLiteral(s) => {
                self.advance();
                Ok(Expr {
                    id: self.next_id(),
                    span: start_span,
                    kind: ExprKind::Literal(Literal::Address(s)),
                })
            }

            // Grouped expression or tuple
            TokenKind::LParen => {
                self.advance();
                if self.check(&TokenKind::RParen) {
                    self.advance();
                    return Ok(Expr {
                        id: self.next_id(),
                        span: start_span.merge(self.current_span()),
                        kind: ExprKind::Tuple(Vec::new()),
                    });
                }

                let expr = self.parse_expr()?;

                if self.eat(&TokenKind::Comma) {
                    // Tuple
                    let mut elements = vec![expr];
                    if !self.check(&TokenKind::RParen) {
                        elements.push(self.parse_expr()?);
                        while self.eat(&TokenKind::Comma) {
                            if self.check(&TokenKind::RParen) {
                                break;
                            }
                            elements.push(self.parse_expr()?);
                        }
                    }
                    self.expect(&TokenKind::RParen)?;
                    Ok(Expr {
                        id: self.next_id(),
                        span: start_span.merge(self.current_span()),
                        kind: ExprKind::Tuple(elements),
                    })
                } else {
                    // Grouped
                    self.expect(&TokenKind::RParen)?;
                    Ok(Expr {
                        id: self.next_id(),
                        span: start_span.merge(self.current_span()),
                        kind: ExprKind::Paren(Box::new(expr)),
                    })
                }
            }

            // Array
            TokenKind::LBracket => {
                self.advance();
                if self.check(&TokenKind::RBracket) {
                    self.advance();
                    return Ok(Expr {
                        id: self.next_id(),
                        span: start_span.merge(self.current_span()),
                        kind: ExprKind::Array(Vec::new()),
                    });
                }

                let first = self.parse_expr()?;

                if self.eat(&TokenKind::Semi) {
                    // Repeat array: [expr; count]
                    let count = self.parse_expr()?;
                    self.expect(&TokenKind::RBracket)?;
                    Ok(Expr {
                        id: self.next_id(),
                        span: start_span.merge(self.current_span()),
                        kind: ExprKind::ArrayRepeat(Box::new(first), Box::new(count)),
                    })
                } else {
                    // Regular array
                    let mut elements = vec![first];
                    while self.eat(&TokenKind::Comma) {
                        if self.check(&TokenKind::RBracket) {
                            break;
                        }
                        elements.push(self.parse_expr()?);
                    }
                    self.expect(&TokenKind::RBracket)?;
                    Ok(Expr {
                        id: self.next_id(),
                        span: start_span.merge(self.current_span()),
                        kind: ExprKind::Array(elements),
                    })
                }
            }

            // Block
            TokenKind::LBrace => {
                let block = self.parse_block()?;
                Ok(Expr {
                    id: self.next_id(),
                    span: start_span.merge(self.current_span()),
                    kind: ExprKind::Block(block),
                })
            }

            // If expression
            TokenKind::If => self.parse_if_expr(),

            // Match expression
            TokenKind::Match => self.parse_match_expr(),

            // Loop expressions
            TokenKind::For => self.parse_for_expr(),
            TokenKind::While => self.parse_while_expr(),
            TokenKind::Loop => self.parse_loop_expr(),

            // Control flow
            TokenKind::Break => {
                self.advance();
                let value = if !self.check(&TokenKind::Semi)
                    && !self.check(&TokenKind::RBrace)
                    && !self.check(&TokenKind::Comma)
                {
                    Some(Box::new(self.parse_expr()?))
                } else {
                    None
                };
                Ok(Expr {
                    id: self.next_id(),
                    span: start_span.merge(self.current_span()),
                    kind: ExprKind::Break(value),
                })
            }
            TokenKind::Continue => {
                self.advance();
                Ok(Expr {
                    id: self.next_id(),
                    span: start_span,
                    kind: ExprKind::Continue,
                })
            }
            TokenKind::Return => {
                self.advance();
                let value = if !self.check(&TokenKind::Semi) && !self.check(&TokenKind::RBrace) {
                    Some(Box::new(self.parse_expr()?))
                } else {
                    None
                };
                Ok(Expr {
                    id: self.next_id(),
                    span: start_span.merge(self.current_span()),
                    kind: ExprKind::Return(value),
                })
            }

            // Emit event
            TokenKind::Emit => {
                self.advance();
                let path = self.parse_type_path()?;
                self.expect(&TokenKind::LBrace)?;
                let fields = self.parse_field_inits()?;
                self.expect(&TokenKind::RBrace)?;
                Ok(Expr {
                    id: self.next_id(),
                    span: start_span.merge(self.current_span()),
                    kind: ExprKind::Emit(path, fields),
                })
            }

            // Revert
            TokenKind::Revert => {
                self.advance();
                let path = self.parse_type_path()?;
                self.expect(&TokenKind::LBrace)?;
                let fields = self.parse_field_inits()?;
                self.expect(&TokenKind::RBrace)?;
                Ok(Expr {
                    id: self.next_id(),
                    span: start_span.merge(self.current_span()),
                    kind: ExprKind::Revert(path, fields),
                })
            }

            // Move expression
            TokenKind::Move_ => {
                self.advance();
                let expr = self.parse_expr()?;
                Ok(Expr {
                    id: self.next_id(),
                    span: start_span.merge(self.current_span()),
                    kind: ExprKind::Move(Box::new(expr)),
                })
            }

            // Identifier or path
            TokenKind::Identifier(_) | TokenKind::Self_ | TokenKind::Super => {
                self.parse_path_expr()
            }

            _ => {
                let span = self.current_span();
                Err(ParseError::InvalidExpression(span.line, span.column))
            }
        }
    }

    fn parse_path_expr(&mut self) -> ParseResult<Expr> {
        let start_span = self.current_span();
        let mut segments = Vec::new();

        // First segment
        match self.current_kind().clone() {
            TokenKind::Identifier(name) => {
                segments.push(Ident::new(name, self.current_span()));
                self.advance();
            }
            TokenKind::Self_ => {
                segments.push(Ident::new("self".to_string(), self.current_span()));
                self.advance();
            }
            TokenKind::Super => {
                segments.push(Ident::new("super".to_string(), self.current_span()));
                self.advance();
            }
            _ => {
                return Err(ParseError::ExpectedIdentifier(
                    start_span.line,
                    start_span.column,
                ))
            }
        }

        // Additional segments
        while self.eat(&TokenKind::ColonColon) {
            let ident = self.expect_identifier()?;
            segments.push(ident);
        }

        // Check for struct construction: Path { fields }
        if self.check(&TokenKind::LBrace) && segments.len() >= 1 {
            // This could be a struct construction
            let path = TypePath {
                segments: segments
                    .into_iter()
                    .map(|ident| PathSegment {
                        span: ident.span,
                        ident,
                        generics: None,
                    })
                    .collect(),
                span: start_span.merge(self.current_span()),
            };

            self.advance(); // consume {
            let fields = self.parse_field_inits()?;
            self.expect(&TokenKind::RBrace)?;

            return Ok(Expr {
                id: self.next_id(),
                span: start_span.merge(self.current_span()),
                kind: ExprKind::Struct(path, fields),
            });
        }

        Ok(Expr {
            id: self.next_id(),
            span: start_span.merge(self.current_span()),
            kind: ExprKind::Path(ExprPath {
                segments,
                span: start_span.merge(self.current_span()),
            }),
        })
    }

    fn parse_field_inits(&mut self) -> ParseResult<Vec<FieldInit>> {
        let mut fields = Vec::new();

        while !self.check(&TokenKind::RBrace) && !self.is_eof() {
            let start_span = self.current_span();
            let name = self.expect_identifier()?;

            let value = if self.eat(&TokenKind::Colon) {
                Some(self.parse_expr()?)
            } else {
                None
            };

            fields.push(FieldInit {
                name,
                value,
                span: start_span.merge(self.current_span()),
            });

            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }

        Ok(fields)
    }

    fn parse_if_expr(&mut self) -> ParseResult<Expr> {
        let start_span = self.current_span();
        self.expect(&TokenKind::If)?;

        let condition = self.parse_expr()?;
        let then_block = self.parse_block()?;

        let else_branch = if self.eat(&TokenKind::Else) {
            if self.check_keyword(&TokenKind::If) {
                Some(Box::new(self.parse_if_expr()?))
            } else {
                let block = self.parse_block()?;
                Some(Box::new(Expr {
                    id: self.next_id(),
                    span: block.span,
                    kind: ExprKind::Block(block),
                }))
            }
        } else {
            None
        };

        Ok(Expr {
            id: self.next_id(),
            span: start_span.merge(self.current_span()),
            kind: ExprKind::If(Box::new(condition), then_block, else_branch),
        })
    }

    fn parse_match_expr(&mut self) -> ParseResult<Expr> {
        let start_span = self.current_span();
        self.expect(&TokenKind::Match)?;

        let scrutinee = self.parse_expr()?;
        self.expect(&TokenKind::LBrace)?;

        let mut arms = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.is_eof() {
            arms.push(self.parse_match_arm()?);
        }

        self.expect(&TokenKind::RBrace)?;

        Ok(Expr {
            id: self.next_id(),
            span: start_span.merge(self.current_span()),
            kind: ExprKind::Match(Box::new(scrutinee), arms),
        })
    }

    fn parse_match_arm(&mut self) -> ParseResult<MatchArm> {
        let start_span = self.current_span();
        let pattern = self.parse_pattern()?;

        let guard = if self.eat(&TokenKind::If) {
            Some(self.parse_expr()?)
        } else {
            None
        };

        self.expect(&TokenKind::FatArrow)?;
        let body = self.parse_expr()?;
        self.eat(&TokenKind::Comma);

        Ok(MatchArm {
            pattern,
            guard,
            body,
            span: start_span.merge(self.current_span()),
        })
    }

    fn parse_for_expr(&mut self) -> ParseResult<Expr> {
        let start_span = self.current_span();
        self.expect(&TokenKind::For)?;

        let pattern = self.parse_pattern()?;
        self.expect(&TokenKind::In)?;
        let iter = self.parse_expr()?;
        let body = self.parse_block()?;

        Ok(Expr {
            id: self.next_id(),
            span: start_span.merge(self.current_span()),
            kind: ExprKind::For(pattern, Box::new(iter), body),
        })
    }

    fn parse_while_expr(&mut self) -> ParseResult<Expr> {
        let start_span = self.current_span();
        self.expect(&TokenKind::While)?;

        let condition = self.parse_expr()?;
        let body = self.parse_block()?;

        Ok(Expr {
            id: self.next_id(),
            span: start_span.merge(self.current_span()),
            kind: ExprKind::While(Box::new(condition), body),
        })
    }

    fn parse_loop_expr(&mut self) -> ParseResult<Expr> {
        let start_span = self.current_span();
        self.expect(&TokenKind::Loop)?;

        let body = self.parse_block()?;

        Ok(Expr {
            id: self.next_id(),
            span: start_span.merge(self.current_span()),
            kind: ExprKind::Loop(body),
        })
    }

    // ========================================================================
    // Patterns
    // ========================================================================

    fn parse_pattern(&mut self) -> ParseResult<Pattern> {
        let start_span = self.current_span();

        let kind = match self.current_kind().clone() {
            TokenKind::Underscore => {
                self.advance();
                PatternKind::Wildcard
            }

            TokenKind::Mut => {
                self.advance();
                let ident = self.expect_identifier()?;
                PatternKind::Ident(ident, true)
            }

            TokenKind::Identifier(name) => {
                let ident = Ident::new(name, self.current_span());
                self.advance();

                // Check for struct or tuple struct pattern
                if self.check(&TokenKind::LBrace) || self.check(&TokenKind::LParen) {
                    let path = TypePath {
                        segments: vec![PathSegment {
                            ident,
                            generics: None,
                            span: start_span,
                        }],
                        span: start_span,
                    };

                    if self.eat(&TokenKind::LBrace) {
                        // Struct pattern
                        let fields = self.parse_field_patterns()?;
                        self.expect(&TokenKind::RBrace)?;
                        PatternKind::Struct(path, fields)
                    } else {
                        // Tuple struct pattern
                        self.advance(); // consume (
                        let mut patterns = Vec::new();
                        if !self.check(&TokenKind::RParen) {
                            patterns.push(self.parse_pattern()?);
                            while self.eat(&TokenKind::Comma) {
                                if self.check(&TokenKind::RParen) {
                                    break;
                                }
                                patterns.push(self.parse_pattern()?);
                            }
                        }
                        self.expect(&TokenKind::RParen)?;
                        PatternKind::TupleStruct(path, patterns)
                    }
                } else {
                    PatternKind::Ident(ident, false)
                }
            }

            TokenKind::LParen => {
                self.advance();
                if self.check(&TokenKind::RParen) {
                    self.advance();
                    PatternKind::Tuple(Vec::new())
                } else {
                    let mut patterns = vec![self.parse_pattern()?];
                    while self.eat(&TokenKind::Comma) {
                        if self.check(&TokenKind::RParen) {
                            break;
                        }
                        patterns.push(self.parse_pattern()?);
                    }
                    self.expect(&TokenKind::RParen)?;
                    PatternKind::Tuple(patterns)
                }
            }

            TokenKind::IntLiteral(s) => {
                self.advance();
                PatternKind::Literal(Literal::Int(s, None))
            }

            TokenKind::StringLiteral(s) => {
                self.advance();
                PatternKind::Literal(Literal::String(s))
            }

            TokenKind::BoolLiteral(b) => {
                self.advance();
                PatternKind::Literal(Literal::Bool(b))
            }

            TokenKind::DotDot => {
                self.advance();
                PatternKind::Rest
            }

            TokenKind::Ampersand => {
                self.advance();
                let is_mut = self.eat(&TokenKind::Mut);
                let inner = self.parse_pattern()?;
                PatternKind::Ref(Box::new(inner), is_mut)
            }

            _ => {
                let span = self.current_span();
                return Err(ParseError::InvalidPattern(span.line, span.column));
            }
        };

        // Check for or pattern
        if self.eat(&TokenKind::Pipe) {
            let first = Pattern {
                id: self.next_id(),
                span: start_span.merge(self.current_span()),
                kind,
            };
            let mut alternatives = vec![first];

            loop {
                alternatives.push(self.parse_pattern()?);
                if !self.eat(&TokenKind::Pipe) {
                    break;
                }
            }

            return Ok(Pattern {
                id: self.next_id(),
                span: start_span.merge(self.current_span()),
                kind: PatternKind::Or(alternatives),
            });
        }

        Ok(Pattern {
            id: self.next_id(),
            span: start_span.merge(self.current_span()),
            kind,
        })
    }

    fn parse_field_patterns(&mut self) -> ParseResult<Vec<FieldPattern>> {
        let mut fields = Vec::new();

        while !self.check(&TokenKind::RBrace) && !self.is_eof() {
            let start_span = self.current_span();

            if self.eat(&TokenKind::DotDot) {
                // Rest pattern in struct
                break;
            }

            let name = self.expect_identifier()?;

            let pattern = if self.eat(&TokenKind::Colon) {
                Some(self.parse_pattern()?)
            } else {
                None
            };

            fields.push(FieldPattern {
                name,
                pattern,
                span: start_span.merge(self.current_span()),
            });

            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }

        Ok(fields)
    }

    // ========================================================================
    // Formal verification
    // ========================================================================

    fn parse_spec_block(&mut self) -> ParseResult<SpecBlock> {
        let start_span = self.current_span();
        self.expect(&TokenKind::Spec)?;
        self.expect(&TokenKind::LBrace)?;

        let mut items = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.is_eof() {
            items.push(self.parse_spec_item()?);
        }

        self.expect(&TokenKind::RBrace)?;

        Ok(SpecBlock {
            id: self.next_id(),
            span: start_span.merge(self.current_span()),
            items,
        })
    }

    fn parse_spec_item(&mut self) -> ParseResult<SpecItem> {
        match self.current_kind() {
            TokenKind::Requires => {
                self.advance();
                let expr = self.parse_expr()?;
                self.expect(&TokenKind::Semi)?;
                Ok(SpecItem::Requires(expr))
            }
            TokenKind::Ensures => {
                self.advance();
                let expr = self.parse_expr()?;
                self.expect(&TokenKind::Semi)?;
                Ok(SpecItem::Ensures(expr))
            }
            TokenKind::Invariant => {
                self.advance();
                let expr = self.parse_expr()?;
                self.expect(&TokenKind::Semi)?;
                Ok(SpecItem::Invariant(expr))
            }
            TokenKind::Assume => {
                self.advance();
                let expr = self.parse_expr()?;
                self.expect(&TokenKind::Semi)?;
                Ok(SpecItem::Assume(expr))
            }
            TokenKind::Assert => {
                self.advance();
                let expr = self.parse_expr()?;
                self.expect(&TokenKind::Semi)?;
                Ok(SpecItem::Assert(expr))
            }
            _ => {
                let span = self.current_span();
                Err(ParseError::UnexpectedToken {
                    expected: "spec item".to_string(),
                    found: format!("{}", self.current_kind()),
                    line: span.line,
                    column: span.column,
                })
            }
        }
    }
}

/// Get precedence for binary operators (left precedence, right precedence)
fn binary_precedence(op: BinaryOp) -> (u8, u8) {
    match op {
        // Assignment (right-associative)
        BinaryOp::Assign
        | BinaryOp::AddAssign
        | BinaryOp::SubAssign
        | BinaryOp::MulAssign
        | BinaryOp::DivAssign
        | BinaryOp::RemAssign
        | BinaryOp::BitAndAssign
        | BinaryOp::BitOrAssign
        | BinaryOp::BitXorAssign
        | BinaryOp::ShlAssign
        | BinaryOp::ShrAssign => (2, 1),

        // Logical or
        BinaryOp::Or => (4, 5),

        // Logical and
        BinaryOp::And => (6, 7),

        // Comparison
        BinaryOp::Eq | BinaryOp::Ne | BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge => {
            (8, 9)
        }

        // Bitwise or
        BinaryOp::BitOr => (10, 11),

        // Bitwise xor
        BinaryOp::BitXor => (12, 13),

        // Bitwise and
        BinaryOp::BitAnd => (14, 15),

        // Shift
        BinaryOp::Shl | BinaryOp::Shr => (16, 17),

        // Additive
        BinaryOp::Add | BinaryOp::Sub => (18, 19),

        // Multiplicative
        BinaryOp::Mul | BinaryOp::Div | BinaryOp::Rem => (20, 21),

        // Power (right-associative)
        BinaryOp::Pow => (23, 22),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;

    fn parse(source: &str) -> ParseResult<SourceFile> {
        let tokens = Lexer::new(source).tokenize().unwrap();
        Parser::new(tokens).parse_file()
    }

    #[test]
    fn test_parse_contract() {
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
        let ast = parse(source).unwrap();
        assert_eq!(ast.items.len(), 1);
        match &ast.items[0] {
            Item::Contract(c) => {
                assert_eq!(c.name.name, "Token");
                assert_eq!(c.items.len(), 2);
            }
            _ => panic!("expected contract"),
        }
    }

    #[test]
    fn test_parse_function() {
        let source = r#"
            pub fn add(a: u256, b: u256) -> u256 {
                return a + b;
            }
        "#;
        let ast = parse(source).unwrap();
        assert_eq!(ast.items.len(), 1);
    }

    #[test]
    fn test_parse_struct() {
        let source = r#"
            struct Point {
                x: u256,
                y: u256,
            }
        "#;
        let ast = parse(source).unwrap();
        assert_eq!(ast.items.len(), 1);
        match &ast.items[0] {
            Item::Struct(s) => {
                assert_eq!(s.name.name, "Point");
                assert_eq!(s.fields.len(), 2);
            }
            _ => panic!("expected struct"),
        }
    }

    #[test]
    fn test_parse_resource() {
        let source = r#"
            resource Token: store + drop {
                balance: u256,
            }
        "#;
        let ast = parse(source).unwrap();
        match &ast.items[0] {
            Item::Struct(s) => {
                assert_eq!(s.name.name, "Token");
                assert_eq!(s.abilities.len(), 2);
            }
            _ => panic!("expected struct"),
        }
    }

    #[test]
    fn test_parse_expressions() {
        let source = r#"
            fn test() {
                let x = 1 + 2 * 3;
                let y = x == 5 && true;
                let z = foo.bar(1, 2);
            }
        "#;
        let ast = parse(source).unwrap();
        assert_eq!(ast.items.len(), 1);
    }
}
