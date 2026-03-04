//! Token definitions for QuantumScript lexer

use std::fmt;

/// Source location span
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Span {
    /// Start byte offset
    pub start: usize,
    /// End byte offset (exclusive)
    pub end: usize,
    /// Line number (1-indexed)
    pub line: u32,
    /// Column number (1-indexed)
    pub column: u32,
}

impl Span {
    pub fn new(start: usize, end: usize, line: u32, column: u32) -> Self {
        Self {
            start,
            end,
            line,
            column,
        }
    }

    pub fn merge(self, other: Span) -> Span {
        Span {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
            line: self.line.min(other.line),
            column: if self.line <= other.line {
                self.column
            } else {
                other.column
            },
        }
    }
}

impl Default for Span {
    fn default() -> Self {
        Self {
            start: 0,
            end: 0,
            line: 1,
            column: 1,
        }
    }
}

/// A token with its span
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

impl Token {
    pub fn new(kind: TokenKind, span: Span) -> Self {
        Self { kind, span }
    }
}

/// Token kinds for QuantumScript
#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    // Literals
    /// Integer literal (decimal, hex, binary, octal)
    IntLiteral(String),
    /// Floating point literal
    FloatLiteral(String),
    /// String literal
    StringLiteral(String),
    /// Byte string literal (b"...")
    ByteStringLiteral(Vec<u8>),
    /// Boolean literal
    BoolLiteral(bool),
    /// Address literal (0x...)
    AddressLiteral(String),

    // Identifiers
    Identifier(String),

    // Keywords - Contract Structure
    Contract,
    Interface,
    Library,
    Module,
    Import,
    From,
    As,
    Pub,
    Extern,

    // Keywords - Types
    Bool,
    U8,
    U16,
    U32,
    U64,
    U128,
    U256,
    I8,
    I16,
    I32,
    I64,
    I128,
    I256,
    Address,
    Bytes,
    String_,
    Mapping,
    Vec_,
    Option_,
    Result_,

    // Keywords - Resource Types
    Resource,
    Copy,
    Drop,
    Store,
    Key,
    Move_,

    // Keywords - Functions
    Fn,
    Pure,
    View,
    Payable,
    Parallel,
    Constructor,
    Fallback,
    Receive,
    Returns,

    // Keywords - Modifiers and Events
    Modifier,
    Event,
    Error,
    Emit,
    Revert,

    // Keywords - Control Flow
    If,
    Else,
    Match,
    For,
    While,
    Loop,
    Break,
    Continue,
    Return,
    In,

    // Keywords - Variable Declaration
    Let,
    Mut,
    Const,
    Static,

    // Keywords - Memory/Storage
    Storage,
    Memory,
    Calldata,

    // Keywords - Formal Verification
    Spec,
    Invariant,
    Requires,
    Ensures,
    Assert,
    Assume,
    Forall,
    Exists,

    // Keywords - Other
    Self_,
    Super,
    Impl,
    Trait,
    Where,
    Type,
    Struct,
    Enum,

    // Operators - Arithmetic
    Plus,     // +
    Minus,    // -
    Star,     // *
    Slash,    // /
    Percent,  // %
    StarStar, // **

    // Operators - Comparison
    EqEq,  // ==
    NotEq, // !=
    Lt,    // <
    Gt,    // >
    LtEq,  // <=
    GtEq,  // >=

    // Operators - Logical
    And, // &&
    Or,  // ||
    Not, // !

    // Operators - Bitwise
    Ampersand, // &
    Pipe,      // |
    Caret,     // ^
    Tilde,     // ~
    Shl,       // <<
    Shr,       // >>

    // Operators - Assignment
    Eq,          // =
    PlusEq,      // +=
    MinusEq,     // -=
    StarEq,      // *=
    SlashEq,     // /=
    PercentEq,   // %=
    AmpersandEq, // &=
    PipeEq,      // |=
    CaretEq,     // ^=
    ShlEq,       // <<=
    ShrEq,       // >>=

    // Operators - Other
    Arrow,      // ->
    FatArrow,   // =>
    Question,   // ?
    Colon,      // :
    ColonColon, // ::
    Dot,        // .
    DotDot,     // ..
    DotDotEq,   // ..=
    At,         // @
    Hash,       // #
    Dollar,     // $

    // Delimiters
    LParen,   // (
    RParen,   // )
    LBracket, // [
    RBracket, // ]
    LBrace,   // {
    RBrace,   // }

    // Punctuation
    Comma,      // ,
    Semi,       // ;
    Underscore, // _ (when standalone)

    // Special
    Eof,
    Newline,
    DocComment(String),
    Comment,
}

impl TokenKind {
    /// Check if this token is a keyword
    pub fn is_keyword(&self) -> bool {
        matches!(
            self,
            TokenKind::Contract
                | TokenKind::Interface
                | TokenKind::Library
                | TokenKind::Module
                | TokenKind::Import
                | TokenKind::From
                | TokenKind::As
                | TokenKind::Pub
                | TokenKind::Extern
                | TokenKind::Bool
                | TokenKind::U8
                | TokenKind::U16
                | TokenKind::U32
                | TokenKind::U64
                | TokenKind::U128
                | TokenKind::U256
                | TokenKind::I8
                | TokenKind::I16
                | TokenKind::I32
                | TokenKind::I64
                | TokenKind::I128
                | TokenKind::I256
                | TokenKind::Address
                | TokenKind::Bytes
                | TokenKind::String_
                | TokenKind::Mapping
                | TokenKind::Vec_
                | TokenKind::Option_
                | TokenKind::Result_
                | TokenKind::Resource
                | TokenKind::Copy
                | TokenKind::Drop
                | TokenKind::Store
                | TokenKind::Key
                | TokenKind::Move_
                | TokenKind::Fn
                | TokenKind::Pure
                | TokenKind::View
                | TokenKind::Payable
                | TokenKind::Parallel
                | TokenKind::Constructor
                | TokenKind::Fallback
                | TokenKind::Receive
                | TokenKind::Returns
                | TokenKind::Modifier
                | TokenKind::Event
                | TokenKind::Error
                | TokenKind::Emit
                | TokenKind::Revert
                | TokenKind::If
                | TokenKind::Else
                | TokenKind::Match
                | TokenKind::For
                | TokenKind::While
                | TokenKind::Loop
                | TokenKind::Break
                | TokenKind::Continue
                | TokenKind::Return
                | TokenKind::In
                | TokenKind::Let
                | TokenKind::Mut
                | TokenKind::Const
                | TokenKind::Static
                | TokenKind::Storage
                | TokenKind::Memory
                | TokenKind::Calldata
                | TokenKind::Spec
                | TokenKind::Invariant
                | TokenKind::Requires
                | TokenKind::Ensures
                | TokenKind::Assert
                | TokenKind::Assume
                | TokenKind::Forall
                | TokenKind::Exists
                | TokenKind::Self_
                | TokenKind::Super
                | TokenKind::Impl
                | TokenKind::Trait
                | TokenKind::Where
                | TokenKind::Type
                | TokenKind::Struct
                | TokenKind::Enum
        )
    }

    /// Convert identifier to keyword if applicable
    pub fn keyword_from_str(s: &str) -> Option<TokenKind> {
        match s {
            // Contract structure
            "contract" => Some(TokenKind::Contract),
            "interface" => Some(TokenKind::Interface),
            "library" => Some(TokenKind::Library),
            "module" => Some(TokenKind::Module),
            "import" => Some(TokenKind::Import),
            "from" => Some(TokenKind::From),
            "as" => Some(TokenKind::As),
            "pub" => Some(TokenKind::Pub),
            "extern" => Some(TokenKind::Extern),

            // Types
            "bool" => Some(TokenKind::Bool),
            "u8" => Some(TokenKind::U8),
            "u16" => Some(TokenKind::U16),
            "u32" => Some(TokenKind::U32),
            "u64" => Some(TokenKind::U64),
            "u128" => Some(TokenKind::U128),
            "u256" => Some(TokenKind::U256),
            "i8" => Some(TokenKind::I8),
            "i16" => Some(TokenKind::I16),
            "i32" => Some(TokenKind::I32),
            "i64" => Some(TokenKind::I64),
            "i128" => Some(TokenKind::I128),
            "i256" => Some(TokenKind::I256),
            "address" => Some(TokenKind::Address),
            "bytes" => Some(TokenKind::Bytes),
            "string" => Some(TokenKind::String_),
            "mapping" => Some(TokenKind::Mapping),
            "Vec" => Some(TokenKind::Vec_),
            "Option" => Some(TokenKind::Option_),
            "Result" => Some(TokenKind::Result_),

            // Resource types
            "resource" => Some(TokenKind::Resource),
            "copy" => Some(TokenKind::Copy),
            "drop" => Some(TokenKind::Drop),
            "store" => Some(TokenKind::Store),
            "key" => Some(TokenKind::Key),
            "move" => Some(TokenKind::Move_),

            // Functions
            "fn" => Some(TokenKind::Fn),
            "pure" => Some(TokenKind::Pure),
            "view" => Some(TokenKind::View),
            "payable" => Some(TokenKind::Payable),
            "parallel" => Some(TokenKind::Parallel),
            "constructor" => Some(TokenKind::Constructor),
            "fallback" => Some(TokenKind::Fallback),
            "receive" => Some(TokenKind::Receive),
            "returns" => Some(TokenKind::Returns),

            // Modifiers and events
            "modifier" => Some(TokenKind::Modifier),
            "event" => Some(TokenKind::Event),
            "error" => Some(TokenKind::Error),
            "emit" => Some(TokenKind::Emit),
            "revert" => Some(TokenKind::Revert),

            // Control flow
            "if" => Some(TokenKind::If),
            "else" => Some(TokenKind::Else),
            "match" => Some(TokenKind::Match),
            "for" => Some(TokenKind::For),
            "while" => Some(TokenKind::While),
            "loop" => Some(TokenKind::Loop),
            "break" => Some(TokenKind::Break),
            "continue" => Some(TokenKind::Continue),
            "return" => Some(TokenKind::Return),
            "in" => Some(TokenKind::In),

            // Variable declaration
            "let" => Some(TokenKind::Let),
            "mut" => Some(TokenKind::Mut),
            "const" => Some(TokenKind::Const),
            "static" => Some(TokenKind::Static),

            // Memory/storage
            "storage" => Some(TokenKind::Storage),
            "memory" => Some(TokenKind::Memory),
            "calldata" => Some(TokenKind::Calldata),

            // Formal verification
            "spec" => Some(TokenKind::Spec),
            "invariant" => Some(TokenKind::Invariant),
            "requires" => Some(TokenKind::Requires),
            "ensures" => Some(TokenKind::Ensures),
            "assert" => Some(TokenKind::Assert),
            "assume" => Some(TokenKind::Assume),
            "forall" => Some(TokenKind::Forall),
            "exists" => Some(TokenKind::Exists),

            // Other
            "self" => Some(TokenKind::Self_),
            "super" => Some(TokenKind::Super),
            "impl" => Some(TokenKind::Impl),
            "trait" => Some(TokenKind::Trait),
            "where" => Some(TokenKind::Where),
            "type" => Some(TokenKind::Type),
            "struct" => Some(TokenKind::Struct),
            "enum" => Some(TokenKind::Enum),

            // Boolean literals
            "true" => Some(TokenKind::BoolLiteral(true)),
            "false" => Some(TokenKind::BoolLiteral(false)),

            _ => None,
        }
    }
}

impl fmt::Display for TokenKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TokenKind::IntLiteral(s) => write!(f, "{}", s),
            TokenKind::FloatLiteral(s) => write!(f, "{}", s),
            TokenKind::StringLiteral(s) => write!(f, "\"{}\"", s),
            TokenKind::ByteStringLiteral(_) => write!(f, "b\"...\""),
            TokenKind::BoolLiteral(b) => write!(f, "{}", b),
            TokenKind::AddressLiteral(s) => write!(f, "{}", s),
            TokenKind::Identifier(s) => write!(f, "{}", s),
            TokenKind::Contract => write!(f, "contract"),
            TokenKind::Interface => write!(f, "interface"),
            TokenKind::Library => write!(f, "library"),
            TokenKind::Module => write!(f, "module"),
            TokenKind::Import => write!(f, "import"),
            TokenKind::From => write!(f, "from"),
            TokenKind::As => write!(f, "as"),
            TokenKind::Pub => write!(f, "pub"),
            TokenKind::Extern => write!(f, "extern"),
            TokenKind::Bool => write!(f, "bool"),
            TokenKind::U8 => write!(f, "u8"),
            TokenKind::U16 => write!(f, "u16"),
            TokenKind::U32 => write!(f, "u32"),
            TokenKind::U64 => write!(f, "u64"),
            TokenKind::U128 => write!(f, "u128"),
            TokenKind::U256 => write!(f, "u256"),
            TokenKind::I8 => write!(f, "i8"),
            TokenKind::I16 => write!(f, "i16"),
            TokenKind::I32 => write!(f, "i32"),
            TokenKind::I64 => write!(f, "i64"),
            TokenKind::I128 => write!(f, "i128"),
            TokenKind::I256 => write!(f, "i256"),
            TokenKind::Address => write!(f, "address"),
            TokenKind::Bytes => write!(f, "bytes"),
            TokenKind::String_ => write!(f, "string"),
            TokenKind::Mapping => write!(f, "mapping"),
            TokenKind::Vec_ => write!(f, "Vec"),
            TokenKind::Option_ => write!(f, "Option"),
            TokenKind::Result_ => write!(f, "Result"),
            TokenKind::Resource => write!(f, "resource"),
            TokenKind::Copy => write!(f, "copy"),
            TokenKind::Drop => write!(f, "drop"),
            TokenKind::Store => write!(f, "store"),
            TokenKind::Key => write!(f, "key"),
            TokenKind::Move_ => write!(f, "move"),
            TokenKind::Fn => write!(f, "fn"),
            TokenKind::Pure => write!(f, "pure"),
            TokenKind::View => write!(f, "view"),
            TokenKind::Payable => write!(f, "payable"),
            TokenKind::Parallel => write!(f, "parallel"),
            TokenKind::Constructor => write!(f, "constructor"),
            TokenKind::Fallback => write!(f, "fallback"),
            TokenKind::Receive => write!(f, "receive"),
            TokenKind::Returns => write!(f, "returns"),
            TokenKind::Modifier => write!(f, "modifier"),
            TokenKind::Event => write!(f, "event"),
            TokenKind::Error => write!(f, "error"),
            TokenKind::Emit => write!(f, "emit"),
            TokenKind::Revert => write!(f, "revert"),
            TokenKind::If => write!(f, "if"),
            TokenKind::Else => write!(f, "else"),
            TokenKind::Match => write!(f, "match"),
            TokenKind::For => write!(f, "for"),
            TokenKind::While => write!(f, "while"),
            TokenKind::Loop => write!(f, "loop"),
            TokenKind::Break => write!(f, "break"),
            TokenKind::Continue => write!(f, "continue"),
            TokenKind::Return => write!(f, "return"),
            TokenKind::In => write!(f, "in"),
            TokenKind::Let => write!(f, "let"),
            TokenKind::Mut => write!(f, "mut"),
            TokenKind::Const => write!(f, "const"),
            TokenKind::Static => write!(f, "static"),
            TokenKind::Storage => write!(f, "storage"),
            TokenKind::Memory => write!(f, "memory"),
            TokenKind::Calldata => write!(f, "calldata"),
            TokenKind::Spec => write!(f, "spec"),
            TokenKind::Invariant => write!(f, "invariant"),
            TokenKind::Requires => write!(f, "requires"),
            TokenKind::Ensures => write!(f, "ensures"),
            TokenKind::Assert => write!(f, "assert"),
            TokenKind::Assume => write!(f, "assume"),
            TokenKind::Forall => write!(f, "forall"),
            TokenKind::Exists => write!(f, "exists"),
            TokenKind::Self_ => write!(f, "self"),
            TokenKind::Super => write!(f, "super"),
            TokenKind::Impl => write!(f, "impl"),
            TokenKind::Trait => write!(f, "trait"),
            TokenKind::Where => write!(f, "where"),
            TokenKind::Type => write!(f, "type"),
            TokenKind::Struct => write!(f, "struct"),
            TokenKind::Enum => write!(f, "enum"),
            TokenKind::Plus => write!(f, "+"),
            TokenKind::Minus => write!(f, "-"),
            TokenKind::Star => write!(f, "*"),
            TokenKind::Slash => write!(f, "/"),
            TokenKind::Percent => write!(f, "%"),
            TokenKind::StarStar => write!(f, "**"),
            TokenKind::EqEq => write!(f, "=="),
            TokenKind::NotEq => write!(f, "!="),
            TokenKind::Lt => write!(f, "<"),
            TokenKind::Gt => write!(f, ">"),
            TokenKind::LtEq => write!(f, "<="),
            TokenKind::GtEq => write!(f, ">="),
            TokenKind::And => write!(f, "&&"),
            TokenKind::Or => write!(f, "||"),
            TokenKind::Not => write!(f, "!"),
            TokenKind::Ampersand => write!(f, "&"),
            TokenKind::Pipe => write!(f, "|"),
            TokenKind::Caret => write!(f, "^"),
            TokenKind::Tilde => write!(f, "~"),
            TokenKind::Shl => write!(f, "<<"),
            TokenKind::Shr => write!(f, ">>"),
            TokenKind::Eq => write!(f, "="),
            TokenKind::PlusEq => write!(f, "+="),
            TokenKind::MinusEq => write!(f, "-="),
            TokenKind::StarEq => write!(f, "*="),
            TokenKind::SlashEq => write!(f, "/="),
            TokenKind::PercentEq => write!(f, "%="),
            TokenKind::AmpersandEq => write!(f, "&="),
            TokenKind::PipeEq => write!(f, "|="),
            TokenKind::CaretEq => write!(f, "^="),
            TokenKind::ShlEq => write!(f, "<<="),
            TokenKind::ShrEq => write!(f, ">>="),
            TokenKind::Arrow => write!(f, "->"),
            TokenKind::FatArrow => write!(f, "=>"),
            TokenKind::Question => write!(f, "?"),
            TokenKind::Colon => write!(f, ":"),
            TokenKind::ColonColon => write!(f, "::"),
            TokenKind::Dot => write!(f, "."),
            TokenKind::DotDot => write!(f, ".."),
            TokenKind::DotDotEq => write!(f, "..="),
            TokenKind::At => write!(f, "@"),
            TokenKind::Hash => write!(f, "#"),
            TokenKind::Dollar => write!(f, "$"),
            TokenKind::LParen => write!(f, "("),
            TokenKind::RParen => write!(f, ")"),
            TokenKind::LBracket => write!(f, "["),
            TokenKind::RBracket => write!(f, "]"),
            TokenKind::LBrace => write!(f, "{{"),
            TokenKind::RBrace => write!(f, "}}"),
            TokenKind::Comma => write!(f, ","),
            TokenKind::Semi => write!(f, ";"),
            TokenKind::Underscore => write!(f, "_"),
            TokenKind::Eof => write!(f, "<EOF>"),
            TokenKind::Newline => write!(f, "<NEWLINE>"),
            TokenKind::DocComment(s) => write!(f, "/// {}", s),
            TokenKind::Comment => write!(f, "<COMMENT>"),
        }
    }
}
