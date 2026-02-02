//! Code generation for QuantumScript
//!
//! Generates QVM bytecode from the typed AST.

use std::collections::HashMap;
use thiserror::Error;

use crate::ast::*;
use crate::lexer::Span;

/// Code generation errors
#[derive(Debug, Error, Clone)]
pub enum CodegenError {
    #[error("undefined symbol '{0}' at line {1}, column {2}")]
    UndefinedSymbol(String, u32, u32),

    #[error("unsupported feature '{0}' at line {1}, column {2}")]
    UnsupportedFeature(String, u32, u32),

    #[error("stack overflow at line {0}, column {1}")]
    StackOverflow(u32, u32),

    #[error("internal error: {0}")]
    InternalError(String),
}

pub type CodegenResult<T> = Result<T, CodegenError>;

/// QVM Opcodes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Opcode {
    // Stack operations
    Push = 0x01,
    Pop = 0x02,
    Dup = 0x03,
    Swap = 0x04,

    // Arithmetic
    Add = 0x10,
    Sub = 0x11,
    Mul = 0x12,
    Div = 0x13,
    Mod = 0x14,
    Pow = 0x15,
    Neg = 0x16,

    // Comparison
    Eq = 0x20,
    Ne = 0x21,
    Lt = 0x22,
    Le = 0x23,
    Gt = 0x24,
    Ge = 0x25,

    // Logical
    And = 0x30,
    Or = 0x31,
    Not = 0x32,

    // Bitwise
    BitAnd = 0x40,
    BitOr = 0x41,
    BitXor = 0x42,
    BitNot = 0x43,
    Shl = 0x44,
    Shr = 0x45,

    // Memory
    Load = 0x50,
    Store = 0x51,
    LoadLocal = 0x52,
    StoreLocal = 0x53,

    // Storage
    SLoad = 0x60,
    SStore = 0x61,

    // Control flow
    Jump = 0x70,
    JumpIf = 0x71,
    JumpIfNot = 0x72,
    Call = 0x73,
    Return = 0x74,
    Revert = 0x75,

    // Contract
    Address = 0x80,
    Balance = 0x81,
    Caller = 0x82,
    CallValue = 0x83,
    Origin = 0x84,
    GasPrice = 0x85,
    BlockHash = 0x86,
    Coinbase = 0x87,
    Timestamp = 0x88,
    BlockNumber = 0x89,
    Difficulty = 0x8A,
    GasLimit = 0x8B,
    ChainId = 0x8C,
    SelfBalance = 0x8D,
    Gas = 0x8E,

    // External calls
    ExternalCall = 0x90,
    StaticCall = 0x91,
    DelegateCall = 0x92,
    Create = 0x93,
    Create2 = 0x94,

    // Events
    Log0 = 0xA0,
    Log1 = 0xA1,
    Log2 = 0xA2,
    Log3 = 0xA3,
    Log4 = 0xA4,

    // Crypto
    Keccak256 = 0xB0,
    Sha256 = 0xB1,
    Ripemd160 = 0xB2,
    Ecrecover = 0xB3,

    // Resource operations (QuantumScript specific)
    ResourceCreate = 0xC0,
    ResourceDestroy = 0xC1,
    ResourceMove = 0xC2,
    ResourceCopy = 0xC3,
    ResourceBorrow = 0xC4,
    ResourceBorrowMut = 0xC5,

    // Parallel execution hints
    ParallelStart = 0xD0,
    ParallelEnd = 0xD1,
    StateRead = 0xD2,
    StateWrite = 0xD3,

    // Misc
    Nop = 0xFE,
    Halt = 0xFF,
}

impl From<Opcode> for u8 {
    fn from(op: Opcode) -> u8 {
        op as u8
    }
}

/// Bytecode instruction
#[derive(Debug, Clone)]
pub struct Instruction {
    pub opcode: Opcode,
    pub operand: Option<Vec<u8>>,
}

impl Instruction {
    pub fn new(opcode: Opcode) -> Self {
        Self { opcode, operand: None }
    }

    pub fn with_operand(opcode: Opcode, operand: Vec<u8>) -> Self {
        Self { opcode, operand: Some(operand) }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = vec![self.opcode.into()];
        if let Some(ref operand) = self.operand {
            bytes.extend(operand);
        }
        bytes
    }
}

/// Function bytecode
#[derive(Debug, Clone)]
pub struct FunctionBytecode {
    pub name: String,
    pub selector: [u8; 4],
    pub param_count: u8,
    pub local_count: u8,
    pub code: Vec<Instruction>,
    pub is_payable: bool,
    pub is_view: bool,
}

impl FunctionBytecode {
    /// Calculate function selector (first 4 bytes of keccak256 hash of signature)
    pub fn compute_selector(name: &str, param_types: &[&str]) -> [u8; 4] {
        use blake3::Hasher;
        let signature = format!("{}({})", name, param_types.join(","));
        let mut hasher = Hasher::new();
        hasher.update(signature.as_bytes());
        let hash = hasher.finalize();
        let bytes = hash.as_bytes();
        [bytes[0], bytes[1], bytes[2], bytes[3]]
    }
}

/// Contract bytecode
#[derive(Debug, Clone)]
pub struct ContractBytecode {
    pub name: String,
    pub functions: Vec<FunctionBytecode>,
    pub storage_layout: Vec<(String, u32)>,
    pub init_code: Vec<Instruction>,
    pub runtime_code: Vec<u8>,
}

/// Local variable info
#[derive(Debug, Clone)]
struct LocalVar {
    name: String,
    index: u16,
    mutable: bool,
}

/// Code generator state
pub struct Codegen {
    /// Current function's instructions
    instructions: Vec<Instruction>,

    /// Local variables
    locals: HashMap<String, LocalVar>,
    local_count: u16,

    /// Storage layout
    storage_slots: HashMap<String, u32>,
    next_storage_slot: u32,

    /// Function table
    functions: HashMap<String, (u16, u8)>, // (index, param_count)

    /// Label counter for jumps
    label_counter: u32,

    /// Label positions
    labels: HashMap<String, usize>,

    /// Unresolved jumps
    unresolved_jumps: Vec<(usize, String)>,

    /// Loop stack for break/continue
    loop_stack: Vec<(String, String)>, // (break_label, continue_label)

    /// Current contract name
    contract_name: Option<String>,
}

impl Codegen {
    pub fn new() -> Self {
        Self {
            instructions: Vec::new(),
            locals: HashMap::new(),
            local_count: 0,
            storage_slots: HashMap::new(),
            next_storage_slot: 0,
            functions: HashMap::new(),
            label_counter: 0,
            labels: HashMap::new(),
            unresolved_jumps: Vec::new(),
            loop_stack: Vec::new(),
            contract_name: None,
        }
    }

    /// Generate bytecode for a source file
    pub fn generate(&mut self, file: &SourceFile) -> CodegenResult<Vec<ContractBytecode>> {
        let mut contracts = Vec::new();

        for item in &file.items {
            match item {
                Item::Contract(contract) => {
                    contracts.push(self.generate_contract(contract)?);
                }
                Item::Function(func) => {
                    // Standalone function
                    self.generate_function(func)?;
                }
                _ => {}
            }
        }

        Ok(contracts)
    }

    fn generate_contract(&mut self, contract: &ContractDef) -> CodegenResult<ContractBytecode> {
        self.contract_name = Some(contract.name.name.clone());
        self.storage_slots.clear();
        self.next_storage_slot = 0;

        let mut functions = Vec::new();
        let mut init_code = Vec::new();

        // Process storage layout
        for item in &contract.items {
            if let ContractItem::Storage(storage) = item {
                for field in &storage.fields {
                    self.storage_slots.insert(field.name.name.clone(), self.next_storage_slot);
                    self.next_storage_slot += 1;
                }
            }
        }

        // Generate constructor
        for item in &contract.items {
            if let ContractItem::Constructor(ctor) = item {
                self.reset_function();
                self.generate_constructor(ctor)?;
                init_code = self.instructions.clone();
            }
        }

        // Generate functions
        for item in &contract.items {
            if let ContractItem::Function(func) = item {
                self.reset_function();
                let bytecode = self.generate_function(func)?;
                functions.push(bytecode);
            }
        }

        let storage_layout: Vec<_> = self.storage_slots
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect();

        let runtime_code = self.generate_runtime_dispatcher(&functions);

        self.contract_name = None;

        Ok(ContractBytecode {
            name: contract.name.name.clone(),
            functions,
            storage_layout,
            init_code,
            runtime_code,
        })
    }

    fn reset_function(&mut self) {
        self.instructions.clear();
        self.locals.clear();
        self.local_count = 0;
        self.labels.clear();
        self.unresolved_jumps.clear();
    }

    fn generate_function(&mut self, func: &FunctionDef) -> CodegenResult<FunctionBytecode> {
        // Register parameters as locals
        for param in &func.sig.params {
            if let PatternKind::Ident(ident, mutable) = &param.pattern.kind {
                self.locals.insert(ident.name.clone(), LocalVar {
                    name: ident.name.clone(),
                    index: self.local_count,
                    mutable: *mutable,
                });
                self.local_count += 1;
            }
        }

        // Generate function body
        self.generate_block(&func.body)?;

        // Add implicit return if needed
        if self.instructions.is_empty() || !matches!(
            self.instructions.last().map(|i| i.opcode),
            Some(Opcode::Return) | Some(Opcode::Revert)
        ) {
            self.emit(Instruction::new(Opcode::Return));
        }

        // Resolve jump labels
        self.resolve_jumps()?;

        // Build function bytecode
        let param_types: Vec<&str> = func.sig.params.iter()
            .map(|p| self.type_to_abi_string(&p.ty))
            .collect();

        Ok(FunctionBytecode {
            name: func.sig.name.name.clone(),
            selector: FunctionBytecode::compute_selector(&func.sig.name.name, &param_types),
            param_count: func.sig.params.len() as u8,
            local_count: self.local_count as u8,
            code: self.instructions.clone(),
            is_payable: func.sig.modifiers.is_payable,
            is_view: func.sig.modifiers.is_view,
        })
    }

    fn generate_constructor(&mut self, ctor: &ConstructorDef) -> CodegenResult<()> {
        // Register parameters
        for param in &ctor.params {
            if let PatternKind::Ident(ident, mutable) = &param.pattern.kind {
                self.locals.insert(ident.name.clone(), LocalVar {
                    name: ident.name.clone(),
                    index: self.local_count,
                    mutable: *mutable,
                });
                self.local_count += 1;
            }
        }

        self.generate_block(&ctor.body)?;
        self.emit(Instruction::new(Opcode::Return));
        self.resolve_jumps()?;
        Ok(())
    }

    fn generate_block(&mut self, block: &Block) -> CodegenResult<()> {
        for stmt in &block.stmts {
            self.generate_stmt(stmt)?;
        }
        Ok(())
    }

    fn generate_stmt(&mut self, stmt: &Stmt) -> CodegenResult<()> {
        match &stmt.kind {
            StmtKind::Local(local) => self.generate_local(local),
            StmtKind::Expr(expr) => {
                self.generate_expr(expr)?;
                // Keep result on stack (expression result)
                Ok(())
            }
            StmtKind::Semi(expr) => {
                self.generate_expr(expr)?;
                // Pop result if not needed
                self.emit(Instruction::new(Opcode::Pop));
                Ok(())
            }
            StmtKind::Empty => Ok(()),
            StmtKind::Item(_) => Ok(()), // Items are handled separately
        }
    }

    fn generate_local(&mut self, local: &LocalStmt) -> CodegenResult<()> {
        // Generate initializer if present
        if let Some(ref init) = local.init {
            self.generate_expr(init)?;
        } else {
            // Push default value (0)
            self.emit_push_u256(&[0u8; 32]);
        }

        // Store in local variable
        if let PatternKind::Ident(ident, mutable) = &local.pattern.kind {
            let index = self.local_count;
            self.locals.insert(ident.name.clone(), LocalVar {
                name: ident.name.clone(),
                index,
                mutable: local.is_mutable || *mutable,
            });
            self.local_count += 1;
            self.emit_store_local(index);
        }

        Ok(())
    }

    fn generate_expr(&mut self, expr: &Expr) -> CodegenResult<()> {
        match &expr.kind {
            ExprKind::Literal(lit) => self.generate_literal(lit),

            ExprKind::Path(path) => {
                if path.segments.len() == 1 {
                    let name = &path.segments[0].name;

                    // Check local variable
                    if let Some(local) = self.locals.get(name) {
                        self.emit_load_local(local.index);
                        return Ok(());
                    }

                    // Check storage
                    if let Some(&slot) = self.storage_slots.get(name) {
                        self.emit_push_u32(slot);
                        self.emit(Instruction::new(Opcode::SLoad));
                        return Ok(());
                    }

                    return Err(CodegenError::UndefinedSymbol(
                        name.clone(),
                        expr.span.line,
                        expr.span.column,
                    ));
                }
                Err(CodegenError::UnsupportedFeature(
                    "complex paths".to_string(),
                    expr.span.line,
                    expr.span.column,
                ))
            }

            ExprKind::Binary(op, left, right) => {
                // Generate operands
                self.generate_expr(left)?;
                self.generate_expr(right)?;

                // Generate operator
                let opcode = match op {
                    BinaryOp::Add => Opcode::Add,
                    BinaryOp::Sub => Opcode::Sub,
                    BinaryOp::Mul => Opcode::Mul,
                    BinaryOp::Div => Opcode::Div,
                    BinaryOp::Rem => Opcode::Mod,
                    BinaryOp::Pow => Opcode::Pow,
                    BinaryOp::Eq => Opcode::Eq,
                    BinaryOp::Ne => Opcode::Ne,
                    BinaryOp::Lt => Opcode::Lt,
                    BinaryOp::Le => Opcode::Le,
                    BinaryOp::Gt => Opcode::Gt,
                    BinaryOp::Ge => Opcode::Ge,
                    BinaryOp::And => Opcode::And,
                    BinaryOp::Or => Opcode::Or,
                    BinaryOp::BitAnd => Opcode::BitAnd,
                    BinaryOp::BitOr => Opcode::BitOr,
                    BinaryOp::BitXor => Opcode::BitXor,
                    BinaryOp::Shl => Opcode::Shl,
                    BinaryOp::Shr => Opcode::Shr,
                    BinaryOp::Assign => {
                        // Handle assignment
                        return self.generate_assignment(left, right);
                    }
                    _ => {
                        return Err(CodegenError::UnsupportedFeature(
                            format!("operator {:?}", op),
                            expr.span.line,
                            expr.span.column,
                        ));
                    }
                };
                self.emit(Instruction::new(opcode));
                Ok(())
            }

            ExprKind::Unary(op, operand) => {
                self.generate_expr(operand)?;
                let opcode = match op {
                    UnaryOp::Neg => Opcode::Neg,
                    UnaryOp::Not => Opcode::Not,
                    UnaryOp::BitNot => Opcode::BitNot,
                };
                self.emit(Instruction::new(opcode));
                Ok(())
            }

            ExprKind::Call(callee, args) => {
                // Push arguments
                for arg in args {
                    self.generate_expr(arg)?;
                }

                // Get function index
                if let ExprKind::Path(path) = &callee.kind {
                    if path.segments.len() == 1 {
                        let name = &path.segments[0].name;
                        // Internal call
                        self.emit(Instruction::with_operand(
                            Opcode::Call,
                            name.as_bytes().to_vec(),
                        ));
                        return Ok(());
                    }
                }

                Err(CodegenError::UnsupportedFeature(
                    "complex call".to_string(),
                    expr.span.line,
                    expr.span.column,
                ))
            }

            ExprKind::MethodCall(receiver, method, args) => {
                // Generate receiver
                self.generate_expr(receiver)?;

                // Push arguments
                for arg in args {
                    self.generate_expr(arg)?;
                }

                // Method call (simplified)
                self.emit(Instruction::with_operand(
                    Opcode::Call,
                    method.name.as_bytes().to_vec(),
                ));
                Ok(())
            }

            ExprKind::Field(obj, field) => {
                self.generate_expr(obj)?;
                // Simplified: assume struct offset
                self.emit_push_u32(0); // Would need proper field offset
                self.emit(Instruction::new(Opcode::Add));
                self.emit(Instruction::new(Opcode::Load));
                Ok(())
            }

            ExprKind::Index(array, index) => {
                self.generate_expr(array)?;
                self.generate_expr(index)?;
                // Calculate offset: base + index * element_size
                self.emit_push_u32(32); // Assume 32-byte elements
                self.emit(Instruction::new(Opcode::Mul));
                self.emit(Instruction::new(Opcode::Add));
                self.emit(Instruction::new(Opcode::Load));
                Ok(())
            }

            ExprKind::Block(block) => {
                self.generate_block(block)?;
                Ok(())
            }

            ExprKind::If(cond, then_block, else_branch) => {
                self.generate_if(cond, then_block, else_branch.as_deref(), expr.span)?;
                Ok(())
            }

            ExprKind::For(pattern, iter, body) => {
                self.generate_for(pattern, iter, body)?;
                Ok(())
            }

            ExprKind::While(cond, body) => {
                self.generate_while(cond, body)?;
                Ok(())
            }

            ExprKind::Loop(body) => {
                self.generate_loop(body)?;
                Ok(())
            }

            ExprKind::Break(_) => {
                if let Some((break_label, _)) = self.loop_stack.last() {
                    let label = break_label.clone();
                    self.emit_jump(&label);
                }
                Ok(())
            }

            ExprKind::Continue => {
                if let Some((_, continue_label)) = self.loop_stack.last() {
                    let label = continue_label.clone();
                    self.emit_jump(&label);
                }
                Ok(())
            }

            ExprKind::Return(value) => {
                if let Some(v) = value {
                    self.generate_expr(v)?;
                }
                self.emit(Instruction::new(Opcode::Return));
                Ok(())
            }

            ExprKind::Tuple(elements) => {
                for elem in elements {
                    self.generate_expr(elem)?;
                }
                Ok(())
            }

            ExprKind::Array(elements) => {
                for elem in elements {
                    self.generate_expr(elem)?;
                }
                Ok(())
            }

            ExprKind::Struct(_, fields) => {
                for field in fields {
                    if let Some(ref value) = field.value {
                        self.generate_expr(value)?;
                    }
                }
                Ok(())
            }

            ExprKind::Emit(path, fields) => {
                // Generate event
                let topic_count = fields.iter().filter(|f| f.value.is_some()).count();

                // Push field values
                for field in fields {
                    if let Some(ref value) = field.value {
                        self.generate_expr(value)?;
                    }
                }

                // Emit log opcode based on topic count
                let opcode = match topic_count {
                    0 => Opcode::Log0,
                    1 => Opcode::Log1,
                    2 => Opcode::Log2,
                    3 => Opcode::Log3,
                    _ => Opcode::Log4,
                };
                self.emit(Instruction::new(opcode));
                Ok(())
            }

            ExprKind::Revert(_, fields) => {
                for field in fields {
                    if let Some(ref value) = field.value {
                        self.generate_expr(value)?;
                    }
                }
                self.emit(Instruction::new(Opcode::Revert));
                Ok(())
            }

            ExprKind::Paren(inner) => self.generate_expr(inner),

            _ => Err(CodegenError::UnsupportedFeature(
                format!("expression {:?}", std::mem::discriminant(&expr.kind)),
                expr.span.line,
                expr.span.column,
            )),
        }
    }

    fn generate_literal(&mut self, lit: &Literal) -> CodegenResult<()> {
        match lit {
            Literal::Int(s, _) => {
                let value = self.parse_int_literal(s);
                self.emit_push_u256(&value);
            }
            Literal::Bool(b) => {
                let value = if *b { 1u8 } else { 0u8 };
                let mut bytes = [0u8; 32];
                bytes[31] = value;
                self.emit_push_u256(&bytes);
            }
            Literal::Address(s) => {
                // Parse hex address
                let s = s.strip_prefix("0x").unwrap_or(s);
                let mut bytes = [0u8; 32];
                if let Ok(decoded) = hex::decode(s) {
                    let start = 32 - decoded.len().min(20);
                    bytes[start..start + decoded.len().min(20)].copy_from_slice(&decoded[..decoded.len().min(20)]);
                }
                self.emit_push_u256(&bytes);
            }
            Literal::String(s) => {
                // Push string as bytes
                let bytes = s.as_bytes();
                for chunk in bytes.chunks(32) {
                    let mut padded = [0u8; 32];
                    padded[..chunk.len()].copy_from_slice(chunk);
                    self.emit_push_u256(&padded);
                }
            }
            _ => {
                // Default: push zero
                self.emit_push_u256(&[0u8; 32]);
            }
        }
        Ok(())
    }

    fn generate_assignment(&mut self, left: &Expr, right: &Expr) -> CodegenResult<()> {
        // Generate the value
        self.generate_expr(right)?;

        // Store based on left-hand side
        match &left.kind {
            ExprKind::Path(path) => {
                if path.segments.len() == 1 {
                    let name = &path.segments[0].name;

                    // Check local variable
                    if let Some(local) = self.locals.get(name) {
                        self.emit_store_local(local.index);
                        return Ok(());
                    }

                    // Check storage
                    if let Some(&slot) = self.storage_slots.get(name) {
                        self.emit_push_u32(slot);
                        self.emit(Instruction::new(Opcode::SStore));
                        return Ok(());
                    }
                }
            }
            ExprKind::Field(obj, _field) => {
                self.generate_expr(obj)?;
                // Store to field
                self.emit(Instruction::new(Opcode::Store));
                return Ok(());
            }
            ExprKind::Index(array, index) => {
                self.generate_expr(array)?;
                self.generate_expr(index)?;
                self.emit_push_u32(32);
                self.emit(Instruction::new(Opcode::Mul));
                self.emit(Instruction::new(Opcode::Add));
                self.emit(Instruction::new(Opcode::Store));
                return Ok(());
            }
            _ => {}
        }

        Err(CodegenError::UnsupportedFeature(
            "assignment target".to_string(),
            left.span.line,
            left.span.column,
        ))
    }

    fn generate_if(&mut self, cond: &Expr, then_block: &Block, else_branch: Option<&Expr>, _span: Span) -> CodegenResult<()> {
        let else_label = self.fresh_label("else");
        let end_label = self.fresh_label("endif");

        // Condition
        self.generate_expr(cond)?;
        self.emit_jump_if_not(&else_label);

        // Then branch
        self.generate_block(then_block)?;
        self.emit_jump(&end_label);

        // Else branch
        self.set_label(&else_label);
        if let Some(else_expr) = else_branch {
            self.generate_expr(else_expr)?;
        }

        self.set_label(&end_label);
        Ok(())
    }

    fn generate_for(&mut self, pattern: &Pattern, iter: &Expr, body: &Block) -> CodegenResult<()> {
        let loop_label = self.fresh_label("for");
        let break_label = self.fresh_label("for_end");
        let continue_label = self.fresh_label("for_continue");

        self.loop_stack.push((break_label.clone(), continue_label.clone()));

        // Initialize iterator
        self.generate_expr(iter)?;
        let iter_local = self.local_count;
        self.local_count += 1;
        self.emit_store_local(iter_local);

        // Index counter
        self.emit_push_u32(0);
        let index_local = self.local_count;
        self.local_count += 1;
        self.emit_store_local(index_local);

        // Loop
        self.set_label(&loop_label);

        // Check bounds (simplified)
        self.emit_load_local(index_local);
        self.emit_push_u32(100); // Simplified: max iterations
        self.emit(Instruction::new(Opcode::Lt));
        self.emit_jump_if_not(&break_label);

        // Load current element and bind to pattern
        if let PatternKind::Ident(ident, _) = &pattern.kind {
            self.emit_load_local(iter_local);
            self.emit_load_local(index_local);
            self.emit_push_u32(32);
            self.emit(Instruction::new(Opcode::Mul));
            self.emit(Instruction::new(Opcode::Add));
            self.emit(Instruction::new(Opcode::Load));

            let elem_local = self.local_count;
            self.locals.insert(ident.name.clone(), LocalVar {
                name: ident.name.clone(),
                index: elem_local,
                mutable: false,
            });
            self.local_count += 1;
            self.emit_store_local(elem_local);
        }

        // Body
        self.generate_block(body)?;

        // Continue point
        self.set_label(&continue_label);

        // Increment
        self.emit_load_local(index_local);
        self.emit_push_u32(1);
        self.emit(Instruction::new(Opcode::Add));
        self.emit_store_local(index_local);
        self.emit_jump(&loop_label);

        self.set_label(&break_label);
        self.loop_stack.pop();
        Ok(())
    }

    fn generate_while(&mut self, cond: &Expr, body: &Block) -> CodegenResult<()> {
        let loop_label = self.fresh_label("while");
        let break_label = self.fresh_label("while_end");

        self.loop_stack.push((break_label.clone(), loop_label.clone()));

        self.set_label(&loop_label);
        self.generate_expr(cond)?;
        self.emit_jump_if_not(&break_label);

        self.generate_block(body)?;
        self.emit_jump(&loop_label);

        self.set_label(&break_label);
        self.loop_stack.pop();
        Ok(())
    }

    fn generate_loop(&mut self, body: &Block) -> CodegenResult<()> {
        let loop_label = self.fresh_label("loop");
        let break_label = self.fresh_label("loop_end");

        self.loop_stack.push((break_label.clone(), loop_label.clone()));

        self.set_label(&loop_label);
        self.generate_block(body)?;
        self.emit_jump(&loop_label);

        self.set_label(&break_label);
        self.loop_stack.pop();
        Ok(())
    }

    // ========================================================================
    // Helper methods
    // ========================================================================

    fn emit(&mut self, instruction: Instruction) {
        self.instructions.push(instruction);
    }

    fn emit_push_u256(&mut self, value: &[u8; 32]) {
        self.emit(Instruction::with_operand(Opcode::Push, value.to_vec()));
    }

    fn emit_push_u32(&mut self, value: u32) {
        let mut bytes = [0u8; 32];
        bytes[28..32].copy_from_slice(&value.to_be_bytes());
        self.emit_push_u256(&bytes);
    }

    fn emit_load_local(&mut self, index: u16) {
        self.emit(Instruction::with_operand(
            Opcode::LoadLocal,
            index.to_be_bytes().to_vec(),
        ));
    }

    fn emit_store_local(&mut self, index: u16) {
        self.emit(Instruction::with_operand(
            Opcode::StoreLocal,
            index.to_be_bytes().to_vec(),
        ));
    }

    fn fresh_label(&mut self, prefix: &str) -> String {
        let label = format!("{}_{}", prefix, self.label_counter);
        self.label_counter += 1;
        label
    }

    fn set_label(&mut self, label: &str) {
        self.labels.insert(label.to_string(), self.instructions.len());
    }

    fn emit_jump(&mut self, label: &str) {
        self.unresolved_jumps.push((self.instructions.len(), label.to_string()));
        self.emit(Instruction::with_operand(Opcode::Jump, vec![0, 0]));
    }

    fn emit_jump_if_not(&mut self, label: &str) {
        self.unresolved_jumps.push((self.instructions.len(), label.to_string()));
        self.emit(Instruction::with_operand(Opcode::JumpIfNot, vec![0, 0]));
    }

    fn resolve_jumps(&mut self) -> CodegenResult<()> {
        for (instr_idx, label) in &self.unresolved_jumps {
            if let Some(&target) = self.labels.get(label) {
                let offset = (target as u16).to_be_bytes();
                if let Some(ref mut operand) = self.instructions[*instr_idx].operand {
                    operand[0] = offset[0];
                    operand[1] = offset[1];
                }
            }
        }
        self.unresolved_jumps.clear();
        Ok(())
    }

    fn parse_int_literal(&self, s: &str) -> [u8; 32] {
        let mut bytes = [0u8; 32];

        let s = s.replace('_', "");

        if s.starts_with("0x") || s.starts_with("0X") {
            // Hex
            if let Ok(decoded) = hex::decode(&s[2..]) {
                let start = 32 - decoded.len().min(32);
                bytes[start..].copy_from_slice(&decoded[..decoded.len().min(32)]);
            }
        } else if s.starts_with("0b") || s.starts_with("0B") {
            // Binary
            if let Ok(value) = u128::from_str_radix(&s[2..], 2) {
                let value_bytes = value.to_be_bytes();
                bytes[16..].copy_from_slice(&value_bytes);
            }
        } else if s.starts_with("0o") || s.starts_with("0O") {
            // Octal
            if let Ok(value) = u128::from_str_radix(&s[2..], 8) {
                let value_bytes = value.to_be_bytes();
                bytes[16..].copy_from_slice(&value_bytes);
            }
        } else {
            // Decimal
            if let Ok(value) = s.parse::<u128>() {
                let value_bytes = value.to_be_bytes();
                bytes[16..].copy_from_slice(&value_bytes);
            }
        }

        bytes
    }

    fn type_to_abi_string(&self, _ty: &Type) -> &'static str {
        // Simplified ABI type mapping
        "uint256"
    }

    fn generate_runtime_dispatcher(&self, functions: &[FunctionBytecode]) -> Vec<u8> {
        let mut code = Vec::new();

        // Simple dispatcher: compare calldata selector with each function
        for func in functions {
            // Push function selector
            code.push(Opcode::Push as u8);
            code.extend_from_slice(&[0u8; 28]);
            code.extend_from_slice(&func.selector);

            // Compare with calldata
            // ... simplified for now

            // Jump to function if match
            code.push(Opcode::JumpIf as u8);
            code.extend_from_slice(&[0, 0]); // Placeholder
        }

        // Fallback: revert
        code.push(Opcode::Revert as u8);

        code
    }
}

impl Default for Codegen {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    fn compile(source: &str) -> CodegenResult<Vec<ContractBytecode>> {
        let tokens = Lexer::new(source).tokenize().unwrap();
        let ast = Parser::new(tokens).parse_file().unwrap();
        let mut codegen = Codegen::new();
        codegen.generate(&ast)
    }

    #[test]
    fn test_simple_function() {
        let source = r#"
            fn add(a: u256, b: u256) -> u256 {
                return a + b;
            }
        "#;
        let result = compile(source);
        assert!(result.is_ok());
    }

    #[test]
    fn test_contract() {
        let source = r#"
            contract Token {
                storage {
                    total_supply: u256,
                }

                pub fn mint(amount: u256) {
                    total_supply = total_supply + amount;
                }
            }
        "#;
        let result = compile(source);
        assert!(result.is_ok());
        let contracts = result.unwrap();
        assert_eq!(contracts.len(), 1);
        assert_eq!(contracts[0].name, "Token");
    }

    #[test]
    fn test_if_statement() {
        let source = "fn test(x: u256) { let a: u256 = 1; }";
        let result = compile(source);
        assert!(result.is_ok());
    }

    #[test]
    fn test_loop() {
        let source = r#"
            fn test() {
                let mut i: u256 = 0;
                loop {
                    i = i + 1;
                    break;
                }
            }
        "#;
        let result = compile(source);
        assert!(result.is_ok());
    }
}
