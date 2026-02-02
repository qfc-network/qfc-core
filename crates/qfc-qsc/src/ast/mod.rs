//! Abstract Syntax Tree definitions for QuantumScript
//!
//! This module defines all AST node types that represent the structure
//! of QuantumScript source code after parsing.

use crate::lexer::Span;

/// A unique identifier for AST nodes
pub type NodeId = u32;

/// Source file representation
#[derive(Debug, Clone)]
pub struct SourceFile {
    pub id: NodeId,
    pub span: Span,
    pub items: Vec<Item>,
}

// ============================================================================
// Top-level items
// ============================================================================

/// Top-level item in a source file
#[derive(Debug, Clone)]
pub enum Item {
    Import(ImportItem),
    Contract(ContractDef),
    Interface(InterfaceDef),
    Library(LibraryDef),
    Struct(StructDef),
    Enum(EnumDef),
    TypeAlias(TypeAlias),
    Const(ConstItem),
    Function(FunctionDef),
}

/// Import statement
#[derive(Debug, Clone)]
pub struct ImportItem {
    pub id: NodeId,
    pub span: Span,
    pub path: ImportPath,
    pub alias: Option<Ident>,
}

/// Import path (e.g., `std::token::ERC20`)
#[derive(Debug, Clone)]
pub struct ImportPath {
    pub segments: Vec<Ident>,
    pub span: Span,
}

/// Contract definition
#[derive(Debug, Clone)]
pub struct ContractDef {
    pub id: NodeId,
    pub span: Span,
    pub name: Ident,
    pub generics: Option<Generics>,
    pub inherits: Vec<TypePath>,
    pub items: Vec<ContractItem>,
}

/// Items that can appear inside a contract
#[derive(Debug, Clone)]
pub enum ContractItem {
    Storage(StorageBlock),
    Event(EventDef),
    Error(ErrorDef),
    Modifier(ModifierDef),
    Function(FunctionDef),
    Constructor(ConstructorDef),
    Fallback(FallbackDef),
    Receive(ReceiveDef),
    Const(ConstItem),
    Struct(StructDef),
    Enum(EnumDef),
}

/// Interface definition
#[derive(Debug, Clone)]
pub struct InterfaceDef {
    pub id: NodeId,
    pub span: Span,
    pub name: Ident,
    pub generics: Option<Generics>,
    pub extends: Vec<TypePath>,
    pub items: Vec<InterfaceItem>,
}

/// Items that can appear inside an interface
#[derive(Debug, Clone)]
pub enum InterfaceItem {
    Function(FunctionSig),
    Event(EventDef),
    Error(ErrorDef),
}

/// Library definition
#[derive(Debug, Clone)]
pub struct LibraryDef {
    pub id: NodeId,
    pub span: Span,
    pub name: Ident,
    pub items: Vec<LibraryItem>,
}

/// Items that can appear inside a library
#[derive(Debug, Clone)]
pub enum LibraryItem {
    Function(FunctionDef),
    Struct(StructDef),
    Const(ConstItem),
}

// ============================================================================
// Storage
// ============================================================================

/// Storage block (state variables)
#[derive(Debug, Clone)]
pub struct StorageBlock {
    pub id: NodeId,
    pub span: Span,
    pub fields: Vec<StorageField>,
}

/// A field in the storage block
#[derive(Debug, Clone)]
pub struct StorageField {
    pub id: NodeId,
    pub span: Span,
    pub visibility: Visibility,
    pub name: Ident,
    pub ty: Type,
    pub default: Option<Expr>,
}

// ============================================================================
// Events and Errors
// ============================================================================

/// Event definition
#[derive(Debug, Clone)]
pub struct EventDef {
    pub id: NodeId,
    pub span: Span,
    pub name: Ident,
    pub fields: Vec<EventField>,
}

/// Event field
#[derive(Debug, Clone)]
pub struct EventField {
    pub id: NodeId,
    pub span: Span,
    pub indexed: bool,
    pub name: Ident,
    pub ty: Type,
}

/// Error definition
#[derive(Debug, Clone)]
pub struct ErrorDef {
    pub id: NodeId,
    pub span: Span,
    pub name: Ident,
    pub fields: Vec<ErrorField>,
}

/// Error field
#[derive(Debug, Clone)]
pub struct ErrorField {
    pub id: NodeId,
    pub span: Span,
    pub name: Ident,
    pub ty: Type,
}

// ============================================================================
// Functions
// ============================================================================

/// Function definition
#[derive(Debug, Clone)]
pub struct FunctionDef {
    pub id: NodeId,
    pub span: Span,
    pub sig: FunctionSig,
    pub body: Block,
    pub specs: Vec<SpecBlock>,
}

/// Function signature (without body)
#[derive(Debug, Clone)]
pub struct FunctionSig {
    pub id: NodeId,
    pub span: Span,
    pub visibility: Visibility,
    pub modifiers: FunctionModifiers,
    pub name: Ident,
    pub generics: Option<Generics>,
    pub params: Vec<Param>,
    pub return_type: Option<Type>,
}

/// Function modifiers (pure, view, payable, parallel)
#[derive(Debug, Clone, Default)]
pub struct FunctionModifiers {
    pub is_pure: bool,
    pub is_view: bool,
    pub is_payable: bool,
    pub is_parallel: bool,
    pub custom_modifiers: Vec<ModifierCall>,
}

/// Call to a custom modifier
#[derive(Debug, Clone)]
pub struct ModifierCall {
    pub name: Ident,
    pub args: Vec<Expr>,
    pub span: Span,
}

/// Function parameter
#[derive(Debug, Clone)]
pub struct Param {
    pub id: NodeId,
    pub span: Span,
    pub pattern: Pattern,
    pub ty: Type,
}

/// Constructor definition
#[derive(Debug, Clone)]
pub struct ConstructorDef {
    pub id: NodeId,
    pub span: Span,
    pub visibility: Visibility,
    pub modifiers: FunctionModifiers,
    pub params: Vec<Param>,
    pub body: Block,
}

/// Fallback function
#[derive(Debug, Clone)]
pub struct FallbackDef {
    pub id: NodeId,
    pub span: Span,
    pub body: Block,
}

/// Receive function
#[derive(Debug, Clone)]
pub struct ReceiveDef {
    pub id: NodeId,
    pub span: Span,
    pub body: Block,
}

/// Modifier definition
#[derive(Debug, Clone)]
pub struct ModifierDef {
    pub id: NodeId,
    pub span: Span,
    pub name: Ident,
    pub params: Vec<Param>,
    pub body: Block,
}

// ============================================================================
// Types
// ============================================================================

/// Type representation
#[derive(Debug, Clone)]
pub struct Type {
    pub id: NodeId,
    pub span: Span,
    pub kind: TypeKind,
}

/// Type kinds
#[derive(Debug, Clone)]
pub enum TypeKind {
    /// Primitive types (u8, u256, bool, address, etc.)
    Primitive(PrimitiveType),

    /// Named type (possibly with generics)
    Path(TypePath),

    /// Array type: [T; N]
    Array(Box<Type>, Box<Expr>),

    /// Slice type: [T]
    Slice(Box<Type>),

    /// Tuple type: (T1, T2, ...)
    Tuple(Vec<Type>),

    /// Mapping type: mapping(K => V)
    Mapping(Box<Type>, Box<Type>),

    /// Option type: Option<T>
    Option(Box<Type>),

    /// Result type: Result<T, E>
    Result(Box<Type>, Box<Type>),

    /// Reference type: &T or &mut T
    Reference(Box<Type>, bool),

    /// Resource type with abilities
    Resource(Box<Type>, Vec<ResourceAbility>),

    /// Function type: fn(T1, T2) -> R
    Function(Vec<Type>, Option<Box<Type>>),

    /// Inferred type (placeholder)
    Infer,

    /// Never type: !
    Never,

    /// Self type
    SelfType,
}

/// Primitive types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrimitiveType {
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
    String,
}

/// Resource abilities
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceAbility {
    Copy,
    Drop,
    Store,
    Key,
}

/// Type path (e.g., `std::token::ERC20<u256>`)
#[derive(Debug, Clone)]
pub struct TypePath {
    pub segments: Vec<PathSegment>,
    pub span: Span,
}

/// Path segment with optional generic arguments
#[derive(Debug, Clone)]
pub struct PathSegment {
    pub ident: Ident,
    pub generics: Option<Vec<Type>>,
    pub span: Span,
}

// ============================================================================
// Structs and Enums
// ============================================================================

/// Struct definition
#[derive(Debug, Clone)]
pub struct StructDef {
    pub id: NodeId,
    pub span: Span,
    pub visibility: Visibility,
    pub name: Ident,
    pub generics: Option<Generics>,
    pub abilities: Vec<ResourceAbility>,
    pub fields: Vec<StructField>,
}

/// Struct field
#[derive(Debug, Clone)]
pub struct StructField {
    pub id: NodeId,
    pub span: Span,
    pub visibility: Visibility,
    pub name: Ident,
    pub ty: Type,
}

/// Enum definition
#[derive(Debug, Clone)]
pub struct EnumDef {
    pub id: NodeId,
    pub span: Span,
    pub visibility: Visibility,
    pub name: Ident,
    pub generics: Option<Generics>,
    pub variants: Vec<EnumVariant>,
}

/// Enum variant
#[derive(Debug, Clone)]
pub struct EnumVariant {
    pub id: NodeId,
    pub span: Span,
    pub name: Ident,
    pub fields: VariantFields,
}

/// Variant field styles
#[derive(Debug, Clone)]
pub enum VariantFields {
    Unit,
    Tuple(Vec<Type>),
    Struct(Vec<StructField>),
}

/// Type alias
#[derive(Debug, Clone)]
pub struct TypeAlias {
    pub id: NodeId,
    pub span: Span,
    pub visibility: Visibility,
    pub name: Ident,
    pub generics: Option<Generics>,
    pub ty: Type,
}

/// Constant item
#[derive(Debug, Clone)]
pub struct ConstItem {
    pub id: NodeId,
    pub span: Span,
    pub visibility: Visibility,
    pub name: Ident,
    pub ty: Type,
    pub value: Expr,
}

// ============================================================================
// Generics
// ============================================================================

/// Generic parameters
#[derive(Debug, Clone)]
pub struct Generics {
    pub params: Vec<GenericParam>,
    pub where_clause: Option<WhereClause>,
    pub span: Span,
}

/// Generic parameter
#[derive(Debug, Clone)]
pub enum GenericParam {
    Type(TypeParam),
    Const(ConstParam),
}

/// Type parameter
#[derive(Debug, Clone)]
pub struct TypeParam {
    pub id: NodeId,
    pub span: Span,
    pub name: Ident,
    pub bounds: Vec<TypeBound>,
    pub default: Option<Type>,
}

/// Const parameter
#[derive(Debug, Clone)]
pub struct ConstParam {
    pub id: NodeId,
    pub span: Span,
    pub name: Ident,
    pub ty: Type,
    pub default: Option<Expr>,
}

/// Type bound
#[derive(Debug, Clone)]
pub struct TypeBound {
    pub path: TypePath,
    pub span: Span,
}

/// Where clause
#[derive(Debug, Clone)]
pub struct WhereClause {
    pub predicates: Vec<WherePredicate>,
    pub span: Span,
}

/// Where predicate
#[derive(Debug, Clone)]
pub struct WherePredicate {
    pub ty: Type,
    pub bounds: Vec<TypeBound>,
    pub span: Span,
}

// ============================================================================
// Statements
// ============================================================================

/// Statement
#[derive(Debug, Clone)]
pub struct Stmt {
    pub id: NodeId,
    pub span: Span,
    pub kind: StmtKind,
}

/// Statement kinds
#[derive(Debug, Clone)]
pub enum StmtKind {
    /// Local variable declaration: `let x = expr;`
    Local(LocalStmt),

    /// Expression statement: `expr;`
    Expr(Expr),

    /// Semi expression (with semicolon)
    Semi(Expr),

    /// Item statement (nested function, struct, etc.)
    Item(Box<Item>),

    /// Empty statement `;`
    Empty,
}

/// Local variable declaration
#[derive(Debug, Clone)]
pub struct LocalStmt {
    pub id: NodeId,
    pub span: Span,
    pub pattern: Pattern,
    pub ty: Option<Type>,
    pub init: Option<Expr>,
    pub is_mutable: bool,
}

/// Code block
#[derive(Debug, Clone)]
pub struct Block {
    pub id: NodeId,
    pub span: Span,
    pub stmts: Vec<Stmt>,
}

// ============================================================================
// Expressions
// ============================================================================

/// Expression
#[derive(Debug, Clone)]
pub struct Expr {
    pub id: NodeId,
    pub span: Span,
    pub kind: ExprKind,
}

/// Expression kinds
#[derive(Debug, Clone)]
pub enum ExprKind {
    /// Literal value
    Literal(Literal),

    /// Path expression (variable, constant, function)
    Path(ExprPath),

    /// Binary operation: `a + b`
    Binary(BinaryOp, Box<Expr>, Box<Expr>),

    /// Unary operation: `-a`, `!a`
    Unary(UnaryOp, Box<Expr>),

    /// Function call: `f(args)`
    Call(Box<Expr>, Vec<Expr>),

    /// Method call: `obj.method(args)`
    MethodCall(Box<Expr>, Ident, Vec<Expr>),

    /// Field access: `obj.field`
    Field(Box<Expr>, Ident),

    /// Index: `arr[idx]`
    Index(Box<Expr>, Box<Expr>),

    /// Cast: `expr as Type`
    Cast(Box<Expr>, Type),

    /// Reference: `&expr` or `&mut expr`
    Reference(Box<Expr>, bool),

    /// Dereference: `*expr`
    Deref(Box<Expr>),

    /// Block expression: `{ stmts }`
    Block(Block),

    /// If expression: `if cond { } else { }`
    If(Box<Expr>, Block, Option<Box<Expr>>),

    /// Match expression
    Match(Box<Expr>, Vec<MatchArm>),

    /// For loop: `for pat in expr { }`
    For(Pattern, Box<Expr>, Block),

    /// While loop: `while cond { }`
    While(Box<Expr>, Block),

    /// Infinite loop: `loop { }`
    Loop(Block),

    /// Break: `break` or `break expr`
    Break(Option<Box<Expr>>),

    /// Continue
    Continue,

    /// Return: `return` or `return expr`
    Return(Option<Box<Expr>>),

    /// Tuple: `(a, b, c)`
    Tuple(Vec<Expr>),

    /// Array: `[a, b, c]`
    Array(Vec<Expr>),

    /// Array with repeat: `[expr; count]`
    ArrayRepeat(Box<Expr>, Box<Expr>),

    /// Struct construction: `Point { x: 1, y: 2 }`
    Struct(TypePath, Vec<FieldInit>),

    /// Range: `a..b` or `a..=b`
    Range(Option<Box<Expr>>, Option<Box<Expr>>, bool),

    /// Closure: `|args| expr`
    Closure(Vec<ClosureParam>, Box<Expr>),

    /// Move expression: `move expr`
    Move(Box<Expr>),

    /// Emit event: `emit Event { ... }`
    Emit(TypePath, Vec<FieldInit>),

    /// Revert: `revert Error { ... }`
    Revert(TypePath, Vec<FieldInit>),

    /// Assert: `assert!(cond)` or `assert!(cond, msg)`
    Assert(Box<Expr>, Option<Box<Expr>>),

    /// Try expression: `expr?`
    Try(Box<Expr>),

    /// Grouped expression: `(expr)`
    Paren(Box<Expr>),

    /// Await expression (for async): `expr.await`
    Await(Box<Expr>),
}

/// Expression path
#[derive(Debug, Clone)]
pub struct ExprPath {
    pub segments: Vec<Ident>,
    pub span: Span,
}

/// Field initialization
#[derive(Debug, Clone)]
pub struct FieldInit {
    pub name: Ident,
    pub value: Option<Expr>,
    pub span: Span,
}

/// Match arm
#[derive(Debug, Clone)]
pub struct MatchArm {
    pub pattern: Pattern,
    pub guard: Option<Expr>,
    pub body: Expr,
    pub span: Span,
}

/// Closure parameter
#[derive(Debug, Clone)]
pub struct ClosureParam {
    pub pattern: Pattern,
    pub ty: Option<Type>,
    pub span: Span,
}

// ============================================================================
// Patterns
// ============================================================================

/// Pattern
#[derive(Debug, Clone)]
pub struct Pattern {
    pub id: NodeId,
    pub span: Span,
    pub kind: PatternKind,
}

/// Pattern kinds
#[derive(Debug, Clone)]
pub enum PatternKind {
    /// Wildcard: `_`
    Wildcard,

    /// Binding: `x` or `mut x`
    Ident(Ident, bool),

    /// Literal pattern
    Literal(Literal),

    /// Tuple pattern: `(a, b)`
    Tuple(Vec<Pattern>),

    /// Struct pattern: `Point { x, y }`
    Struct(TypePath, Vec<FieldPattern>),

    /// Enum variant pattern: `Some(x)`
    TupleStruct(TypePath, Vec<Pattern>),

    /// Or pattern: `A | B`
    Or(Vec<Pattern>),

    /// Reference pattern: `&x`
    Ref(Box<Pattern>, bool),

    /// Range pattern: `1..=10`
    Range(Box<Pattern>, Box<Pattern>, bool),

    /// Rest pattern: `..`
    Rest,

    /// Path pattern (unit enum variant, constant)
    Path(TypePath),
}

/// Field pattern
#[derive(Debug, Clone)]
pub struct FieldPattern {
    pub name: Ident,
    pub pattern: Option<Pattern>,
    pub span: Span,
}

// ============================================================================
// Operators
// ============================================================================

/// Binary operators
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    // Arithmetic
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Pow,

    // Comparison
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,

    // Logical
    And,
    Or,

    // Bitwise
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,

    // Assignment
    Assign,
    AddAssign,
    SubAssign,
    MulAssign,
    DivAssign,
    RemAssign,
    BitAndAssign,
    BitOrAssign,
    BitXorAssign,
    ShlAssign,
    ShrAssign,
}

/// Unary operators
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    /// Negation: `-`
    Neg,
    /// Logical not: `!`
    Not,
    /// Bitwise not: `~`
    BitNot,
}

// ============================================================================
// Literals
// ============================================================================

/// Literal values
#[derive(Debug, Clone)]
pub enum Literal {
    /// Integer literal
    Int(String, Option<PrimitiveType>),

    /// Float literal
    Float(String),

    /// String literal
    String(String),

    /// Byte string literal
    ByteString(Vec<u8>),

    /// Boolean literal
    Bool(bool),

    /// Address literal
    Address(String),
}

// ============================================================================
// Common types
// ============================================================================

/// Identifier
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Ident {
    pub name: String,
    pub span: Span,
}

impl Ident {
    pub fn new(name: String, span: Span) -> Self {
        Self { name, span }
    }
}

/// Visibility
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Visibility {
    #[default]
    Private,
    Public,
}

// ============================================================================
// Formal verification
// ============================================================================

/// Specification block
#[derive(Debug, Clone)]
pub struct SpecBlock {
    pub id: NodeId,
    pub span: Span,
    pub items: Vec<SpecItem>,
}

/// Specification items
#[derive(Debug, Clone)]
pub enum SpecItem {
    /// Precondition: `requires expr;`
    Requires(Expr),

    /// Postcondition: `ensures expr;`
    Ensures(Expr),

    /// Invariant: `invariant expr;`
    Invariant(Expr),

    /// Assumption: `assume expr;`
    Assume(Expr),

    /// Assertion: `assert expr;`
    Assert(Expr),
}
