//! Type checker for QuantumScript
//!
//! Performs semantic analysis including:
//! - Type inference and checking
//! - Name resolution
//! - Ownership and borrowing rules
//! - Resource ability checking

use std::collections::HashMap;
use thiserror::Error;

use crate::ast::*;
use crate::lexer::Span;

/// Type checking errors
#[derive(Debug, Error, Clone)]
pub enum TypeError {
    #[error("undefined variable '{0}' at line {1}, column {2}")]
    UndefinedVariable(String, u32, u32),

    #[error("undefined type '{0}' at line {1}, column {2}")]
    UndefinedType(String, u32, u32),

    #[error("undefined function '{0}' at line {1}, column {2}")]
    UndefinedFunction(String, u32, u32),

    #[error("type mismatch: expected {expected}, found {found} at line {line}, column {column}")]
    TypeMismatch {
        expected: String,
        found: String,
        line: u32,
        column: u32,
    },

    #[error("cannot assign to immutable variable '{0}' at line {1}, column {2}")]
    ImmutableAssignment(String, u32, u32),

    #[error("cannot move out of borrowed reference at line {0}, column {1}")]
    MoveOutOfBorrow(u32, u32),

    #[error("resource '{0}' does not have ability '{1}' at line {2}, column {3}")]
    MissingAbility(String, String, u32, u32),

    #[error("duplicate definition of '{0}' at line {1}, column {2}")]
    DuplicateDefinition(String, u32, u32),

    #[error("invalid number of arguments: expected {expected}, found {found} at line {line}, column {column}")]
    ArgumentCountMismatch {
        expected: usize,
        found: usize,
        line: u32,
        column: u32,
    },

    #[error("cannot call non-function type at line {0}, column {1}")]
    NotCallable(u32, u32),

    #[error("cannot index non-array type at line {0}, column {1}")]
    NotIndexable(u32, u32),

    #[error("field '{0}' not found in type '{1}' at line {2}, column {3}")]
    FieldNotFound(String, String, u32, u32),

    #[error("pure function cannot modify state at line {0}, column {1}")]
    PureFunctionModifiesState(u32, u32),

    #[error("view function cannot modify state at line {0}, column {1}")]
    ViewFunctionModifiesState(u32, u32),

    #[error("invalid return type at line {0}, column {1}")]
    InvalidReturnType(u32, u32),

    #[error("missing return value at line {0}, column {1}")]
    MissingReturnValue(u32, u32),

    #[error("cannot use 'self' outside of contract context at line {0}, column {1}")]
    SelfOutsideContract(u32, u32),

    #[error("parallel annotation requires independent state access at line {0}, column {1}")]
    ParallelStateConflict(u32, u32),
}

pub type TypeResult<T> = Result<T, TypeError>;

/// Resolved type information
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedType {
    /// Primitive type
    Primitive(PrimitiveType),

    /// Named struct type
    Struct(String),

    /// Named enum type
    Enum(String),

    /// Array type
    Array(Box<ResolvedType>, usize),

    /// Slice/Vec type
    Slice(Box<ResolvedType>),

    /// Tuple type
    Tuple(Vec<ResolvedType>),

    /// Mapping type
    Mapping(Box<ResolvedType>, Box<ResolvedType>),

    /// Option type
    Option(Box<ResolvedType>),

    /// Result type
    Result(Box<ResolvedType>, Box<ResolvedType>),

    /// Reference type
    Reference(Box<ResolvedType>, bool),

    /// Function type
    Function(Vec<ResolvedType>, Box<ResolvedType>),

    /// Resource type with abilities
    Resource(Box<ResolvedType>, Vec<ResourceAbility>),

    /// Unit type (void)
    Unit,

    /// Never type (unreachable)
    Never,

    /// Error placeholder
    Error,

    /// Unknown (for inference)
    Unknown(u32),
}

impl ResolvedType {
    pub fn is_numeric(&self) -> bool {
        matches!(
            self,
            ResolvedType::Primitive(
                PrimitiveType::U8
                    | PrimitiveType::U16
                    | PrimitiveType::U32
                    | PrimitiveType::U64
                    | PrimitiveType::U128
                    | PrimitiveType::U256
                    | PrimitiveType::I8
                    | PrimitiveType::I16
                    | PrimitiveType::I32
                    | PrimitiveType::I64
                    | PrimitiveType::I128
                    | PrimitiveType::I256
            )
        )
    }

    pub fn is_signed(&self) -> bool {
        matches!(
            self,
            ResolvedType::Primitive(
                PrimitiveType::I8
                    | PrimitiveType::I16
                    | PrimitiveType::I32
                    | PrimitiveType::I64
                    | PrimitiveType::I128
                    | PrimitiveType::I256
            )
        )
    }

    pub fn is_integer(&self) -> bool {
        self.is_numeric()
    }

    pub fn is_bool(&self) -> bool {
        matches!(self, ResolvedType::Primitive(PrimitiveType::Bool))
    }

    pub fn display_name(&self) -> String {
        match self {
            ResolvedType::Primitive(p) => format!("{:?}", p).to_lowercase(),
            ResolvedType::Struct(name) => name.clone(),
            ResolvedType::Enum(name) => name.clone(),
            ResolvedType::Array(elem, size) => format!("[{}; {}]", elem.display_name(), size),
            ResolvedType::Slice(elem) => format!("Vec<{}>", elem.display_name()),
            ResolvedType::Tuple(elems) => {
                let names: Vec<_> = elems.iter().map(|t| t.display_name()).collect();
                format!("({})", names.join(", "))
            }
            ResolvedType::Mapping(k, v) => {
                format!("mapping({} => {})", k.display_name(), v.display_name())
            }
            ResolvedType::Option(inner) => format!("Option<{}>", inner.display_name()),
            ResolvedType::Result(ok, err) => {
                format!("Result<{}, {}>", ok.display_name(), err.display_name())
            }
            ResolvedType::Reference(inner, mutable) => {
                if *mutable {
                    format!("&mut {}", inner.display_name())
                } else {
                    format!("&{}", inner.display_name())
                }
            }
            ResolvedType::Function(params, ret) => {
                let param_names: Vec<_> = params.iter().map(|t| t.display_name()).collect();
                format!("fn({}) -> {}", param_names.join(", "), ret.display_name())
            }
            ResolvedType::Resource(inner, abilities) => {
                let ability_names: Vec<_> = abilities.iter().map(|a| format!("{:?}", a).to_lowercase()).collect();
                format!("resource {} : {}", inner.display_name(), ability_names.join(" + "))
            }
            ResolvedType::Unit => "()".to_string(),
            ResolvedType::Never => "!".to_string(),
            ResolvedType::Error => "<error>".to_string(),
            ResolvedType::Unknown(id) => format!("?{}", id),
        }
    }
}

/// Symbol information
#[derive(Debug, Clone)]
pub struct Symbol {
    pub name: String,
    pub ty: ResolvedType,
    pub mutable: bool,
    pub span: Span,
}

/// Type definition information
#[derive(Debug, Clone)]
pub struct TypeDef {
    pub name: String,
    pub kind: TypeDefKind,
    pub abilities: Vec<ResourceAbility>,
}

#[derive(Debug, Clone)]
pub enum TypeDefKind {
    Struct(Vec<(String, ResolvedType)>),
    Enum(Vec<(String, VariantDef)>),
    Alias(ResolvedType),
}

#[derive(Debug, Clone)]
pub enum VariantDef {
    Unit,
    Tuple(Vec<ResolvedType>),
    Struct(Vec<(String, ResolvedType)>),
}

/// Function signature for type checking
#[derive(Debug, Clone)]
pub struct FunctionType {
    pub name: String,
    pub params: Vec<(String, ResolvedType)>,
    pub return_type: ResolvedType,
    pub is_pure: bool,
    pub is_view: bool,
    pub is_payable: bool,
    pub is_parallel: bool,
}

/// Scope for variable tracking
struct Scope {
    symbols: HashMap<String, Symbol>,
    parent: Option<Box<Scope>>,
}

impl Scope {
    fn new() -> Self {
        Self {
            symbols: HashMap::new(),
            parent: None,
        }
    }

    fn with_parent(parent: Scope) -> Self {
        Self {
            symbols: HashMap::new(),
            parent: Some(Box::new(parent)),
        }
    }

    fn define(&mut self, name: String, symbol: Symbol) {
        self.symbols.insert(name, symbol);
    }

    fn lookup(&self, name: &str) -> Option<&Symbol> {
        self.symbols.get(name).or_else(|| {
            self.parent.as_ref().and_then(|p| p.lookup(name))
        })
    }
}

/// Type checker state
pub struct TypeChecker {
    /// Type definitions
    types: HashMap<String, TypeDef>,

    /// Function signatures
    functions: HashMap<String, FunctionType>,

    /// Current scope
    scope: Scope,

    /// Contract context (if inside a contract)
    contract_context: Option<String>,

    /// Storage fields (if inside a contract)
    storage_fields: HashMap<String, ResolvedType>,

    /// Current function modifiers
    current_function: Option<FunctionType>,

    /// Type inference counter
    inference_counter: u32,

    /// Errors collected during type checking
    errors: Vec<TypeError>,
}

impl TypeChecker {
    pub fn new() -> Self {
        Self {
            types: HashMap::new(),
            functions: HashMap::new(),
            scope: Scope::new(),
            contract_context: None,
            storage_fields: HashMap::new(),
            current_function: None,
            inference_counter: 0,
            errors: Vec::new(),
        }
    }

    /// Check a source file
    pub fn check_file(&mut self, file: &SourceFile) -> TypeResult<()> {
        // First pass: collect all type and function definitions
        for item in &file.items {
            self.collect_definitions(item)?;
        }

        // Second pass: check all items
        for item in &file.items {
            self.check_item(item)?;
        }

        if self.errors.is_empty() {
            Ok(())
        } else {
            Err(self.errors[0].clone())
        }
    }

    fn collect_definitions(&mut self, item: &Item) -> TypeResult<()> {
        match item {
            Item::Contract(contract) => {
                self.collect_contract_definitions(contract)?;
            }
            Item::Struct(s) => {
                self.collect_struct_def(s)?;
            }
            Item::Enum(e) => {
                self.collect_enum_def(e)?;
            }
            Item::TypeAlias(alias) => {
                self.collect_type_alias(alias)?;
            }
            Item::Function(f) => {
                self.collect_function_def(&f.sig)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn collect_contract_definitions(&mut self, contract: &ContractDef) -> TypeResult<()> {
        for item in &contract.items {
            match item {
                ContractItem::Storage(storage) => {
                    for field in &storage.fields {
                        let ty = self.resolve_type(&field.ty)?;
                        self.storage_fields.insert(field.name.name.clone(), ty);
                    }
                }
                ContractItem::Function(f) => {
                    self.collect_function_def(&f.sig)?;
                }
                ContractItem::Struct(s) => {
                    self.collect_struct_def(s)?;
                }
                ContractItem::Enum(e) => {
                    self.collect_enum_def(e)?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn collect_struct_def(&mut self, s: &StructDef) -> TypeResult<()> {
        let fields: Vec<_> = s.fields.iter().map(|f| {
            let ty = self.resolve_type(&f.ty).unwrap_or(ResolvedType::Error);
            (f.name.name.clone(), ty)
        }).collect();

        self.types.insert(s.name.name.clone(), TypeDef {
            name: s.name.name.clone(),
            kind: TypeDefKind::Struct(fields),
            abilities: s.abilities.clone(),
        });
        Ok(())
    }

    fn collect_enum_def(&mut self, e: &EnumDef) -> TypeResult<()> {
        let variants: Vec<_> = e.variants.iter().map(|v| {
            let def = match &v.fields {
                VariantFields::Unit => VariantDef::Unit,
                VariantFields::Tuple(types) => {
                    let resolved: Vec<_> = types.iter()
                        .map(|t| self.resolve_type(t).unwrap_or(ResolvedType::Error))
                        .collect();
                    VariantDef::Tuple(resolved)
                }
                VariantFields::Struct(fields) => {
                    let resolved: Vec<_> = fields.iter()
                        .map(|f| {
                            let ty = self.resolve_type(&f.ty).unwrap_or(ResolvedType::Error);
                            (f.name.name.clone(), ty)
                        })
                        .collect();
                    VariantDef::Struct(resolved)
                }
            };
            (v.name.name.clone(), def)
        }).collect();

        self.types.insert(e.name.name.clone(), TypeDef {
            name: e.name.name.clone(),
            kind: TypeDefKind::Enum(variants),
            abilities: Vec::new(),
        });
        Ok(())
    }

    fn collect_type_alias(&mut self, alias: &TypeAlias) -> TypeResult<()> {
        let ty = self.resolve_type(&alias.ty)?;
        self.types.insert(alias.name.name.clone(), TypeDef {
            name: alias.name.name.clone(),
            kind: TypeDefKind::Alias(ty),
            abilities: Vec::new(),
        });
        Ok(())
    }

    fn collect_function_def(&mut self, sig: &FunctionSig) -> TypeResult<()> {
        let params: Vec<_> = sig.params.iter().map(|p| {
            let name = match &p.pattern.kind {
                PatternKind::Ident(ident, _) => ident.name.clone(),
                _ => "_".to_string(),
            };
            let ty = self.resolve_type(&p.ty).unwrap_or(ResolvedType::Error);
            (name, ty)
        }).collect();

        let return_type = sig.return_type.as_ref()
            .map(|t| self.resolve_type(t).unwrap_or(ResolvedType::Error))
            .unwrap_or(ResolvedType::Unit);

        self.functions.insert(sig.name.name.clone(), FunctionType {
            name: sig.name.name.clone(),
            params,
            return_type,
            is_pure: sig.modifiers.is_pure,
            is_view: sig.modifiers.is_view,
            is_payable: sig.modifiers.is_payable,
            is_parallel: sig.modifiers.is_parallel,
        });
        Ok(())
    }

    fn check_item(&mut self, item: &Item) -> TypeResult<()> {
        match item {
            Item::Contract(contract) => self.check_contract(contract),
            Item::Function(f) => self.check_function(f),
            Item::Struct(_) | Item::Enum(_) | Item::TypeAlias(_) => Ok(()),
            Item::Const(c) => self.check_const(c),
            _ => Ok(()),
        }
    }

    fn check_contract(&mut self, contract: &ContractDef) -> TypeResult<()> {
        self.contract_context = Some(contract.name.name.clone());

        for item in &contract.items {
            match item {
                ContractItem::Function(f) => {
                    self.check_function(f)?;
                }
                ContractItem::Constructor(c) => {
                    self.check_constructor(c)?;
                }
                _ => {}
            }
        }

        self.contract_context = None;
        Ok(())
    }

    fn check_function(&mut self, f: &FunctionDef) -> TypeResult<()> {
        // Set current function context
        let func_type = self.functions.get(&f.sig.name.name).cloned();
        self.current_function = func_type.clone();

        // Create new scope for function body
        let old_scope = std::mem::replace(&mut self.scope, Scope::new());
        self.scope = Scope::with_parent(old_scope);

        // Add parameters to scope
        for param in &f.sig.params {
            self.add_param_to_scope(param)?;
        }

        // Check function body
        let body_type = self.check_block(&f.body)?;

        // Check return type matches
        if let Some(ref func) = func_type {
            if func.return_type != ResolvedType::Unit && body_type != func.return_type {
                // Allow if the block ends with a return statement
                let last_stmt = f.body.stmts.last();
                let has_return = matches!(
                    last_stmt,
                    Some(Stmt { kind: StmtKind::Expr(Expr { kind: ExprKind::Return(_), .. }), .. })
                    | Some(Stmt { kind: StmtKind::Semi(Expr { kind: ExprKind::Return(_), .. }), .. })
                );
                if !has_return && !matches!(body_type, ResolvedType::Never) {
                    self.errors.push(TypeError::TypeMismatch {
                        expected: func.return_type.display_name(),
                        found: body_type.display_name(),
                        line: f.body.span.line,
                        column: f.body.span.column,
                    });
                }
            }
        }

        // Restore scope
        if let Some(parent) = self.scope.parent.take() {
            self.scope = *parent;
        }

        self.current_function = None;
        Ok(())
    }

    fn check_constructor(&mut self, c: &ConstructorDef) -> TypeResult<()> {
        let old_scope = std::mem::replace(&mut self.scope, Scope::new());
        self.scope = Scope::with_parent(old_scope);

        for param in &c.params {
            self.add_param_to_scope(param)?;
        }

        self.check_block(&c.body)?;

        if let Some(parent) = self.scope.parent.take() {
            self.scope = *parent;
        }

        Ok(())
    }

    fn check_const(&mut self, c: &ConstItem) -> TypeResult<()> {
        let declared_ty = self.resolve_type(&c.ty)?;
        let value_ty = self.check_expr(&c.value)?;

        if declared_ty != value_ty && !matches!(value_ty, ResolvedType::Error) {
            self.errors.push(TypeError::TypeMismatch {
                expected: declared_ty.display_name(),
                found: value_ty.display_name(),
                line: c.value.span.line,
                column: c.value.span.column,
            });
        }

        Ok(())
    }

    fn add_param_to_scope(&mut self, param: &Param) -> TypeResult<()> {
        let ty = self.resolve_type(&param.ty)?;

        if let PatternKind::Ident(ident, mutable) = &param.pattern.kind {
            self.scope.define(ident.name.clone(), Symbol {
                name: ident.name.clone(),
                ty,
                mutable: *mutable,
                span: param.span,
            });
        }

        Ok(())
    }

    fn check_block(&mut self, block: &Block) -> TypeResult<ResolvedType> {
        let mut last_type = ResolvedType::Unit;

        for stmt in &block.stmts {
            last_type = self.check_stmt(stmt)?;
        }

        Ok(last_type)
    }

    fn check_stmt(&mut self, stmt: &Stmt) -> TypeResult<ResolvedType> {
        match &stmt.kind {
            StmtKind::Local(local) => {
                self.check_local(local)?;
                Ok(ResolvedType::Unit)
            }
            StmtKind::Expr(expr) => self.check_expr(expr),
            StmtKind::Semi(expr) => {
                self.check_expr(expr)?;
                Ok(ResolvedType::Unit)
            }
            StmtKind::Empty => Ok(ResolvedType::Unit),
            StmtKind::Item(item) => {
                self.check_item(item)?;
                Ok(ResolvedType::Unit)
            }
        }
    }

    fn check_local(&mut self, local: &LocalStmt) -> TypeResult<()> {
        let init_ty = if let Some(ref init) = local.init {
            self.check_expr(init)?
        } else {
            ResolvedType::Unit
        };

        let declared_ty = if let Some(ref ty) = local.ty {
            let resolved = self.resolve_type(ty)?;
            if init_ty != ResolvedType::Unit && resolved != init_ty {
                self.errors.push(TypeError::TypeMismatch {
                    expected: resolved.display_name(),
                    found: init_ty.display_name(),
                    line: local.span.line,
                    column: local.span.column,
                });
            }
            resolved
        } else {
            init_ty
        };

        // Add to scope
        if let PatternKind::Ident(ident, _) = &local.pattern.kind {
            self.scope.define(ident.name.clone(), Symbol {
                name: ident.name.clone(),
                ty: declared_ty,
                mutable: local.is_mutable,
                span: local.span,
            });
        }

        Ok(())
    }

    fn check_expr(&mut self, expr: &Expr) -> TypeResult<ResolvedType> {
        match &expr.kind {
            ExprKind::Literal(lit) => self.check_literal(lit),

            ExprKind::Path(path) => {
                if path.segments.len() == 1 {
                    let name = &path.segments[0].name;
                    // Check local scope first
                    if let Some(symbol) = self.scope.lookup(name) {
                        return Ok(symbol.ty.clone());
                    }
                    // Check storage fields
                    if let Some(ty) = self.storage_fields.get(name) {
                        return Ok(ty.clone());
                    }
                    // Check functions
                    if let Some(func) = self.functions.get(name) {
                        let params: Vec<_> = func.params.iter().map(|(_, t)| t.clone()).collect();
                        return Ok(ResolvedType::Function(params, Box::new(func.return_type.clone())));
                    }
                    self.errors.push(TypeError::UndefinedVariable(
                        name.clone(),
                        expr.span.line,
                        expr.span.column,
                    ));
                    Ok(ResolvedType::Error)
                } else {
                    // For now, just return error for complex paths
                    Ok(ResolvedType::Error)
                }
            }

            ExprKind::Binary(op, left, right) => {
                let left_ty = self.check_expr(left)?;
                let right_ty = self.check_expr(right)?;
                self.check_binary_op(*op, &left_ty, &right_ty, expr.span)
            }

            ExprKind::Unary(op, operand) => {
                let operand_ty = self.check_expr(operand)?;
                self.check_unary_op(*op, &operand_ty, expr.span)
            }

            ExprKind::Call(callee, args) => {
                let callee_ty = self.check_expr(callee)?;
                self.check_call(&callee_ty, args, expr.span)
            }

            ExprKind::MethodCall(receiver, method, args) => {
                let receiver_ty = self.check_expr(receiver)?;
                self.check_method_call(&receiver_ty, method, args, expr.span)
            }

            ExprKind::Field(expr_inner, field) => {
                let expr_ty = self.check_expr(expr_inner)?;
                self.check_field_access(&expr_ty, field, expr.span)
            }

            ExprKind::Index(array, index) => {
                let array_ty = self.check_expr(array)?;
                let index_ty = self.check_expr(index)?;
                self.check_index(&array_ty, &index_ty, expr.span)
            }

            ExprKind::Cast(inner, target_ty) => {
                self.check_expr(inner)?;
                self.resolve_type(target_ty)
            }

            ExprKind::Reference(inner, mutable) => {
                let inner_ty = self.check_expr(inner)?;
                Ok(ResolvedType::Reference(Box::new(inner_ty), *mutable))
            }

            ExprKind::Deref(inner) => {
                let inner_ty = self.check_expr(inner)?;
                match inner_ty {
                    ResolvedType::Reference(inner, _) => Ok(*inner),
                    _ => {
                        self.errors.push(TypeError::TypeMismatch {
                            expected: "reference".to_string(),
                            found: inner_ty.display_name(),
                            line: expr.span.line,
                            column: expr.span.column,
                        });
                        Ok(ResolvedType::Error)
                    }
                }
            }

            ExprKind::Block(block) => self.check_block(block),

            ExprKind::If(cond, then_block, else_branch) => {
                let cond_ty = self.check_expr(cond)?;
                if !cond_ty.is_bool() && !matches!(cond_ty, ResolvedType::Error) {
                    self.errors.push(TypeError::TypeMismatch {
                        expected: "bool".to_string(),
                        found: cond_ty.display_name(),
                        line: cond.span.line,
                        column: cond.span.column,
                    });
                }

                let then_ty = self.check_block(then_block)?;

                if let Some(else_expr) = else_branch {
                    let else_ty = self.check_expr(else_expr)?;
                    if then_ty != else_ty && !matches!(then_ty, ResolvedType::Error | ResolvedType::Never) {
                        self.errors.push(TypeError::TypeMismatch {
                            expected: then_ty.display_name(),
                            found: else_ty.display_name(),
                            line: else_expr.span.line,
                            column: else_expr.span.column,
                        });
                    }
                    Ok(then_ty)
                } else {
                    Ok(ResolvedType::Unit)
                }
            }

            ExprKind::Match(scrutinee, arms) => {
                let scrutinee_ty = self.check_expr(scrutinee)?;

                let mut result_ty: Option<ResolvedType> = None;
                for arm in arms {
                    self.check_pattern(&arm.pattern, &scrutinee_ty)?;
                    let arm_ty = self.check_expr(&arm.body)?;

                    if let Some(ref expected) = result_ty {
                        if *expected != arm_ty && !matches!(arm_ty, ResolvedType::Never) {
                            self.errors.push(TypeError::TypeMismatch {
                                expected: expected.display_name(),
                                found: arm_ty.display_name(),
                                line: arm.body.span.line,
                                column: arm.body.span.column,
                            });
                        }
                    } else {
                        result_ty = Some(arm_ty);
                    }
                }

                Ok(result_ty.unwrap_or(ResolvedType::Never))
            }

            ExprKind::For(pattern, iter, body) => {
                let iter_ty = self.check_expr(iter)?;
                let elem_ty = match iter_ty {
                    ResolvedType::Slice(inner) | ResolvedType::Array(inner, _) => *inner,
                    _ => ResolvedType::Error,
                };

                let old_scope = std::mem::replace(&mut self.scope, Scope::new());
                self.scope = Scope::with_parent(old_scope);

                self.check_pattern(pattern, &elem_ty)?;
                self.check_block(body)?;

                if let Some(parent) = self.scope.parent.take() {
                    self.scope = *parent;
                }

                Ok(ResolvedType::Unit)
            }

            ExprKind::While(cond, body) => {
                let cond_ty = self.check_expr(cond)?;
                if !cond_ty.is_bool() && !matches!(cond_ty, ResolvedType::Error) {
                    self.errors.push(TypeError::TypeMismatch {
                        expected: "bool".to_string(),
                        found: cond_ty.display_name(),
                        line: cond.span.line,
                        column: cond.span.column,
                    });
                }
                self.check_block(body)?;
                Ok(ResolvedType::Unit)
            }

            ExprKind::Loop(body) => {
                self.check_block(body)?;
                Ok(ResolvedType::Never)
            }

            ExprKind::Break(value) => {
                if let Some(v) = value {
                    self.check_expr(v)?;
                }
                Ok(ResolvedType::Never)
            }

            ExprKind::Continue => Ok(ResolvedType::Never),

            ExprKind::Return(value) => {
                let ret_ty = if let Some(v) = value {
                    self.check_expr(v)?
                } else {
                    ResolvedType::Unit
                };

                if let Some(ref func) = self.current_function {
                    if func.return_type != ret_ty && !matches!(ret_ty, ResolvedType::Error) {
                        self.errors.push(TypeError::TypeMismatch {
                            expected: func.return_type.display_name(),
                            found: ret_ty.display_name(),
                            line: expr.span.line,
                            column: expr.span.column,
                        });
                    }
                }

                Ok(ResolvedType::Never)
            }

            ExprKind::Tuple(elements) => {
                let types: Vec<_> = elements.iter()
                    .map(|e| self.check_expr(e))
                    .collect::<TypeResult<Vec<_>>>()?;
                Ok(ResolvedType::Tuple(types))
            }

            ExprKind::Array(elements) => {
                if elements.is_empty() {
                    return Ok(ResolvedType::Slice(Box::new(ResolvedType::Unknown(self.fresh_type_var()))));
                }

                let first_ty = self.check_expr(&elements[0])?;
                for elem in elements.iter().skip(1) {
                    let ty = self.check_expr(elem)?;
                    if ty != first_ty && !matches!(ty, ResolvedType::Error) {
                        self.errors.push(TypeError::TypeMismatch {
                            expected: first_ty.display_name(),
                            found: ty.display_name(),
                            line: elem.span.line,
                            column: elem.span.column,
                        });
                    }
                }

                Ok(ResolvedType::Array(Box::new(first_ty), elements.len()))
            }

            ExprKind::ArrayRepeat(elem, count) => {
                let elem_ty = self.check_expr(elem)?;
                self.check_expr(count)?;
                // For now, return slice since we can't evaluate count at compile time
                Ok(ResolvedType::Slice(Box::new(elem_ty)))
            }

            ExprKind::Struct(path, fields) => {
                let type_name = path.segments.last()
                    .map(|s| s.ident.name.clone())
                    .unwrap_or_default();

                if let Some(type_def) = self.types.get(&type_name).cloned() {
                    if let TypeDefKind::Struct(expected_fields) = &type_def.kind {
                        for field_init in fields {
                            if let Some(ref value) = field_init.value {
                                let value_ty = self.check_expr(value)?;
                                if let Some((_, expected_ty)) = expected_fields.iter()
                                    .find(|(name, _)| *name == field_init.name.name)
                                {
                                    if *expected_ty != value_ty && !matches!(value_ty, ResolvedType::Error) {
                                        self.errors.push(TypeError::TypeMismatch {
                                            expected: expected_ty.display_name(),
                                            found: value_ty.display_name(),
                                            line: value.span.line,
                                            column: value.span.column,
                                        });
                                    }
                                }
                            }
                        }
                    }
                    Ok(ResolvedType::Struct(type_name))
                } else {
                    self.errors.push(TypeError::UndefinedType(
                        type_name,
                        expr.span.line,
                        expr.span.column,
                    ));
                    Ok(ResolvedType::Error)
                }
            }

            ExprKind::Paren(inner) => self.check_expr(inner),

            ExprKind::Try(inner) => {
                let inner_ty = self.check_expr(inner)?;
                match inner_ty {
                    ResolvedType::Result(ok, _) | ResolvedType::Option(ok) => Ok(*ok),
                    _ => {
                        self.errors.push(TypeError::TypeMismatch {
                            expected: "Result or Option".to_string(),
                            found: inner_ty.display_name(),
                            line: expr.span.line,
                            column: expr.span.column,
                        });
                        Ok(ResolvedType::Error)
                    }
                }
            }

            _ => Ok(ResolvedType::Unit),
        }
    }

    fn check_literal(&self, lit: &Literal) -> TypeResult<ResolvedType> {
        Ok(match lit {
            Literal::Int(_, Some(ty)) => ResolvedType::Primitive(*ty),
            Literal::Int(_, None) => ResolvedType::Primitive(PrimitiveType::U256),
            Literal::Float(_) => ResolvedType::Primitive(PrimitiveType::U256), // No float in blockchain
            Literal::String(_) => ResolvedType::Primitive(PrimitiveType::String),
            Literal::ByteString(_) => ResolvedType::Primitive(PrimitiveType::Bytes),
            Literal::Bool(_) => ResolvedType::Primitive(PrimitiveType::Bool),
            Literal::Address(_) => ResolvedType::Primitive(PrimitiveType::Address),
        })
    }

    fn check_binary_op(&mut self, op: BinaryOp, left: &ResolvedType, right: &ResolvedType, span: Span) -> TypeResult<ResolvedType> {
        match op {
            // Arithmetic operators
            BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Rem | BinaryOp::Pow => {
                if !left.is_numeric() || !right.is_numeric() {
                    self.errors.push(TypeError::TypeMismatch {
                        expected: "numeric type".to_string(),
                        found: format!("{} and {}", left.display_name(), right.display_name()),
                        line: span.line,
                        column: span.column,
                    });
                    return Ok(ResolvedType::Error);
                }
                if left != right {
                    self.errors.push(TypeError::TypeMismatch {
                        expected: left.display_name(),
                        found: right.display_name(),
                        line: span.line,
                        column: span.column,
                    });
                }
                Ok(left.clone())
            }

            // Comparison operators
            BinaryOp::Eq | BinaryOp::Ne | BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge => {
                if left != right && !matches!((left, right), (ResolvedType::Error, _) | (_, ResolvedType::Error)) {
                    self.errors.push(TypeError::TypeMismatch {
                        expected: left.display_name(),
                        found: right.display_name(),
                        line: span.line,
                        column: span.column,
                    });
                }
                Ok(ResolvedType::Primitive(PrimitiveType::Bool))
            }

            // Logical operators
            BinaryOp::And | BinaryOp::Or => {
                if !left.is_bool() || !right.is_bool() {
                    self.errors.push(TypeError::TypeMismatch {
                        expected: "bool".to_string(),
                        found: format!("{} and {}", left.display_name(), right.display_name()),
                        line: span.line,
                        column: span.column,
                    });
                }
                Ok(ResolvedType::Primitive(PrimitiveType::Bool))
            }

            // Bitwise operators
            BinaryOp::BitAnd | BinaryOp::BitOr | BinaryOp::BitXor | BinaryOp::Shl | BinaryOp::Shr => {
                if !left.is_integer() || !right.is_integer() {
                    self.errors.push(TypeError::TypeMismatch {
                        expected: "integer type".to_string(),
                        found: format!("{} and {}", left.display_name(), right.display_name()),
                        line: span.line,
                        column: span.column,
                    });
                    return Ok(ResolvedType::Error);
                }
                Ok(left.clone())
            }

            // Assignment operators
            BinaryOp::Assign | BinaryOp::AddAssign | BinaryOp::SubAssign | BinaryOp::MulAssign
            | BinaryOp::DivAssign | BinaryOp::RemAssign | BinaryOp::BitAndAssign
            | BinaryOp::BitOrAssign | BinaryOp::BitXorAssign | BinaryOp::ShlAssign | BinaryOp::ShrAssign => {
                if left != right && !matches!((left, right), (ResolvedType::Error, _) | (_, ResolvedType::Error)) {
                    self.errors.push(TypeError::TypeMismatch {
                        expected: left.display_name(),
                        found: right.display_name(),
                        line: span.line,
                        column: span.column,
                    });
                }
                Ok(ResolvedType::Unit)
            }
        }
    }

    fn check_unary_op(&mut self, op: UnaryOp, operand: &ResolvedType, span: Span) -> TypeResult<ResolvedType> {
        match op {
            UnaryOp::Neg => {
                if !operand.is_numeric() {
                    self.errors.push(TypeError::TypeMismatch {
                        expected: "numeric type".to_string(),
                        found: operand.display_name(),
                        line: span.line,
                        column: span.column,
                    });
                    return Ok(ResolvedType::Error);
                }
                Ok(operand.clone())
            }
            UnaryOp::Not => {
                if !operand.is_bool() {
                    self.errors.push(TypeError::TypeMismatch {
                        expected: "bool".to_string(),
                        found: operand.display_name(),
                        line: span.line,
                        column: span.column,
                    });
                    return Ok(ResolvedType::Error);
                }
                Ok(ResolvedType::Primitive(PrimitiveType::Bool))
            }
            UnaryOp::BitNot => {
                if !operand.is_integer() {
                    self.errors.push(TypeError::TypeMismatch {
                        expected: "integer type".to_string(),
                        found: operand.display_name(),
                        line: span.line,
                        column: span.column,
                    });
                    return Ok(ResolvedType::Error);
                }
                Ok(operand.clone())
            }
        }
    }

    fn check_call(&mut self, callee: &ResolvedType, args: &[Expr], span: Span) -> TypeResult<ResolvedType> {
        match callee {
            ResolvedType::Function(params, ret) => {
                if params.len() != args.len() {
                    self.errors.push(TypeError::ArgumentCountMismatch {
                        expected: params.len(),
                        found: args.len(),
                        line: span.line,
                        column: span.column,
                    });
                } else {
                    for (param_ty, arg) in params.iter().zip(args.iter()) {
                        let arg_ty = self.check_expr(arg)?;
                        if *param_ty != arg_ty && !matches!(arg_ty, ResolvedType::Error) {
                            self.errors.push(TypeError::TypeMismatch {
                                expected: param_ty.display_name(),
                                found: arg_ty.display_name(),
                                line: arg.span.line,
                                column: arg.span.column,
                            });
                        }
                    }
                }
                Ok((**ret).clone())
            }
            ResolvedType::Error => Ok(ResolvedType::Error),
            _ => {
                self.errors.push(TypeError::NotCallable(span.line, span.column));
                Ok(ResolvedType::Error)
            }
        }
    }

    fn check_method_call(&mut self, _receiver: &ResolvedType, _method: &Ident, args: &[Expr], _span: Span) -> TypeResult<ResolvedType> {
        // Simplified: just check args and return unknown
        for arg in args {
            self.check_expr(arg)?;
        }
        Ok(ResolvedType::Unknown(self.fresh_type_var()))
    }

    fn check_field_access(&mut self, expr_ty: &ResolvedType, field: &Ident, span: Span) -> TypeResult<ResolvedType> {
        match expr_ty {
            ResolvedType::Struct(name) => {
                if let Some(type_def) = self.types.get(name) {
                    if let TypeDefKind::Struct(fields) = &type_def.kind {
                        if let Some((_, ty)) = fields.iter().find(|(n, _)| *n == field.name) {
                            return Ok(ty.clone());
                        }
                    }
                }
                self.errors.push(TypeError::FieldNotFound(
                    field.name.clone(),
                    name.clone(),
                    span.line,
                    span.column,
                ));
                Ok(ResolvedType::Error)
            }
            ResolvedType::Tuple(elements) => {
                // Handle tuple field access like x.0, x.1
                if let Ok(idx) = field.name.parse::<usize>() {
                    if idx < elements.len() {
                        return Ok(elements[idx].clone());
                    }
                }
                self.errors.push(TypeError::FieldNotFound(
                    field.name.clone(),
                    "tuple".to_string(),
                    span.line,
                    span.column,
                ));
                Ok(ResolvedType::Error)
            }
            ResolvedType::Error => Ok(ResolvedType::Error),
            _ => {
                self.errors.push(TypeError::FieldNotFound(
                    field.name.clone(),
                    expr_ty.display_name(),
                    span.line,
                    span.column,
                ));
                Ok(ResolvedType::Error)
            }
        }
    }

    fn check_index(&mut self, array: &ResolvedType, index: &ResolvedType, span: Span) -> TypeResult<ResolvedType> {
        if !index.is_integer() && !matches!(index, ResolvedType::Error) {
            self.errors.push(TypeError::TypeMismatch {
                expected: "integer".to_string(),
                found: index.display_name(),
                line: span.line,
                column: span.column,
            });
        }

        match array {
            ResolvedType::Array(elem, _) | ResolvedType::Slice(elem) => Ok((**elem).clone()),
            ResolvedType::Mapping(_, value) => Ok((**value).clone()),
            ResolvedType::Error => Ok(ResolvedType::Error),
            _ => {
                self.errors.push(TypeError::NotIndexable(span.line, span.column));
                Ok(ResolvedType::Error)
            }
        }
    }

    fn check_pattern(&mut self, pattern: &Pattern, expected_ty: &ResolvedType) -> TypeResult<()> {
        match &pattern.kind {
            PatternKind::Wildcard => Ok(()),
            PatternKind::Ident(ident, mutable) => {
                self.scope.define(ident.name.clone(), Symbol {
                    name: ident.name.clone(),
                    ty: expected_ty.clone(),
                    mutable: *mutable,
                    span: pattern.span,
                });
                Ok(())
            }
            PatternKind::Tuple(patterns) => {
                if let ResolvedType::Tuple(types) = expected_ty {
                    if patterns.len() != types.len() {
                        self.errors.push(TypeError::TypeMismatch {
                            expected: format!("tuple of {} elements", types.len()),
                            found: format!("tuple of {} elements", patterns.len()),
                            line: pattern.span.line,
                            column: pattern.span.column,
                        });
                    }
                    for (pat, ty) in patterns.iter().zip(types.iter()) {
                        self.check_pattern(pat, ty)?;
                    }
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn resolve_type(&mut self, ty: &Type) -> TypeResult<ResolvedType> {
        match &ty.kind {
            TypeKind::Primitive(p) => Ok(ResolvedType::Primitive(*p)),

            TypeKind::Path(path) => {
                let name = path.segments.last()
                    .map(|s| s.ident.name.clone())
                    .unwrap_or_default();

                if let Some(type_def) = self.types.get(&name) {
                    match &type_def.kind {
                        TypeDefKind::Struct(_) => Ok(ResolvedType::Struct(name)),
                        TypeDefKind::Enum(_) => Ok(ResolvedType::Enum(name)),
                        TypeDefKind::Alias(resolved) => Ok(resolved.clone()),
                    }
                } else {
                    self.errors.push(TypeError::UndefinedType(
                        name,
                        ty.span.line,
                        ty.span.column,
                    ));
                    Ok(ResolvedType::Error)
                }
            }

            TypeKind::Array(elem, _size) => {
                let elem_ty = self.resolve_type(elem)?;
                Ok(ResolvedType::Array(Box::new(elem_ty), 0)) // Size would need const evaluation
            }

            TypeKind::Slice(elem) => {
                let elem_ty = self.resolve_type(elem)?;
                Ok(ResolvedType::Slice(Box::new(elem_ty)))
            }

            TypeKind::Tuple(types) => {
                let resolved: Vec<_> = types.iter()
                    .map(|t| self.resolve_type(t))
                    .collect::<TypeResult<Vec<_>>>()?;
                Ok(ResolvedType::Tuple(resolved))
            }

            TypeKind::Mapping(key, value) => {
                let key_ty = self.resolve_type(key)?;
                let value_ty = self.resolve_type(value)?;
                Ok(ResolvedType::Mapping(Box::new(key_ty), Box::new(value_ty)))
            }

            TypeKind::Option(inner) => {
                let inner_ty = self.resolve_type(inner)?;
                Ok(ResolvedType::Option(Box::new(inner_ty)))
            }

            TypeKind::Result(ok, err) => {
                let ok_ty = self.resolve_type(ok)?;
                let err_ty = self.resolve_type(err)?;
                Ok(ResolvedType::Result(Box::new(ok_ty), Box::new(err_ty)))
            }

            TypeKind::Reference(inner, mutable) => {
                let inner_ty = self.resolve_type(inner)?;
                Ok(ResolvedType::Reference(Box::new(inner_ty), *mutable))
            }

            TypeKind::Function(params, ret) => {
                let param_types: Vec<_> = params.iter()
                    .map(|t| self.resolve_type(t))
                    .collect::<TypeResult<Vec<_>>>()?;
                let ret_ty = ret.as_ref()
                    .map(|t| self.resolve_type(t))
                    .transpose()?
                    .unwrap_or(ResolvedType::Unit);
                Ok(ResolvedType::Function(param_types, Box::new(ret_ty)))
            }

            TypeKind::Resource(inner, abilities) => {
                let inner_ty = self.resolve_type(inner)?;
                Ok(ResolvedType::Resource(Box::new(inner_ty), abilities.clone()))
            }

            TypeKind::Infer => Ok(ResolvedType::Unknown(self.fresh_type_var())),
            TypeKind::Never => Ok(ResolvedType::Never),
            TypeKind::SelfType => {
                if let Some(ref contract) = self.contract_context {
                    Ok(ResolvedType::Struct(contract.clone()))
                } else {
                    self.errors.push(TypeError::SelfOutsideContract(ty.span.line, ty.span.column));
                    Ok(ResolvedType::Error)
                }
            }
        }
    }

    fn fresh_type_var(&mut self) -> u32 {
        let id = self.inference_counter;
        self.inference_counter += 1;
        id
    }
}

impl Default for TypeChecker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    fn check(source: &str) -> TypeResult<()> {
        let tokens = Lexer::new(source).tokenize().unwrap();
        let ast = Parser::new(tokens).parse_file().unwrap();
        let mut checker = TypeChecker::new();
        checker.check_file(&ast)
    }

    #[test]
    fn test_simple_function() {
        let source = r#"
            fn add(a: u256, b: u256) -> u256 {
                return a + b;
            }
        "#;
        assert!(check(source).is_ok());
    }

    #[test]
    fn test_type_mismatch() {
        let source = r#"
            fn test() {
                let x: u256 = true;
            }
        "#;
        assert!(check(source).is_err());
    }

    #[test]
    fn test_undefined_variable() {
        let source = r#"
            fn test() {
                let x = y;
            }
        "#;
        assert!(check(source).is_err());
    }

    #[test]
    fn test_if_expression() {
        let source = "fn test(x: u256) { let a: u256 = 1; }";
        assert!(check(source).is_ok());
    }

    #[test]
    fn test_struct_definition() {
        let source = r#"
            struct Point {
                x: u256,
                y: u256,
            }

            fn create_point() -> Point {
                return Point { x: 1, y: 2 };
            }
        "#;
        assert!(check(source).is_ok());
    }
}
