//! AST pretty printer.

use crate::ast::*;
use super::config::FormatConfig;

/// The formatter state.
pub struct Formatter {
    config: FormatConfig,
    output: String,
    indent_level: usize,
}

impl Formatter {
    /// Create a new formatter with the given configuration.
    pub fn new(config: FormatConfig) -> Self {
        Self {
            config,
            output: String::new(),
            indent_level: 0,
        }
    }

    /// Format a source file.
    pub fn format_file(&mut self, file: &SourceFile) -> String {
        self.output.clear();
        self.indent_level = 0;

        for (i, item) in file.items.iter().enumerate() {
            if i > 0 {
                for _ in 0..self.config.blank_lines_between_items {
                    self.newline();
                }
            }
            self.format_item(item);
        }

        // Ensure file ends with newline
        if !self.output.ends_with('\n') {
            self.output.push('\n');
        }

        std::mem::take(&mut self.output)
    }

    // ========================================================================
    // Items
    // ========================================================================

    fn format_item(&mut self, item: &Item) {
        match item {
            Item::Import(import) => self.format_import(import),
            Item::Contract(contract) => self.format_contract(contract),
            Item::Interface(interface) => self.format_interface(interface),
            Item::Library(library) => self.format_library(library),
            Item::Struct(struct_def) => self.format_struct(struct_def),
            Item::Enum(enum_def) => self.format_enum(enum_def),
            Item::TypeAlias(alias) => self.format_type_alias(alias),
            Item::Const(const_item) => self.format_const(const_item),
            Item::Function(func) => self.format_function(func),
        }
    }

    fn format_import(&mut self, import: &ImportItem) {
        self.write("use ");
        self.format_import_path(&import.path);
        if let Some(alias) = &import.alias {
            self.write(" as ");
            self.write(&alias.name);
        }
        self.write(";");
        self.newline();
    }

    fn format_import_path(&mut self, path: &ImportPath) {
        for (i, segment) in path.segments.iter().enumerate() {
            if i > 0 {
                self.write("::");
            }
            self.write(&segment.name);
        }
    }

    fn format_contract(&mut self, contract: &ContractDef) {
        self.write("contract ");
        self.write(&contract.name.name);
        if let Some(generics) = &contract.generics {
            self.format_generics(generics);
        }
        if !contract.inherits.is_empty() {
            self.write(" is ");
            for (i, path) in contract.inherits.iter().enumerate() {
                if i > 0 {
                    self.write(", ");
                }
                self.format_type_path(path);
            }
        }
        self.write(" {");
        self.newline();
        self.indent_level += 1;

        for (i, item) in contract.items.iter().enumerate() {
            if i > 0 {
                self.newline();
            }
            self.format_contract_item(item);
        }

        self.indent_level -= 1;
        self.write_indent();
        self.write("}");
        self.newline();
    }

    fn format_contract_item(&mut self, item: &ContractItem) {
        match item {
            ContractItem::Storage(storage) => self.format_storage(storage),
            ContractItem::Event(event) => self.format_event(event),
            ContractItem::Error(error) => self.format_error(error),
            ContractItem::Modifier(modifier) => self.format_modifier(modifier),
            ContractItem::Function(func) => self.format_function(func),
            ContractItem::Constructor(ctor) => self.format_constructor(ctor),
            ContractItem::Fallback(fallback) => self.format_fallback(fallback),
            ContractItem::Receive(receive) => self.format_receive(receive),
            ContractItem::Const(const_item) => self.format_const(const_item),
            ContractItem::Struct(struct_def) => self.format_struct(struct_def),
            ContractItem::Enum(enum_def) => self.format_enum(enum_def),
        }
    }

    fn format_storage(&mut self, storage: &StorageBlock) {
        self.write_indent();
        self.write("storage {");
        self.newline();
        self.indent_level += 1;

        for field in &storage.fields {
            self.write_indent();
            self.format_visibility(field.visibility);
            self.write(&field.name.name);
            self.write(": ");
            self.format_type(&field.ty);
            if let Some(default) = &field.default {
                self.write(" = ");
                self.format_expr(default);
            }
            self.write(",");
            self.newline();
        }

        self.indent_level -= 1;
        self.write_indent();
        self.write("}");
        self.newline();
    }

    fn format_event(&mut self, event: &EventDef) {
        self.write_indent();
        self.write("event ");
        self.write(&event.name.name);
        self.write(" {");

        if event.fields.is_empty() {
            self.write("}");
        } else {
            self.newline();
            self.indent_level += 1;

            for field in &event.fields {
                self.write_indent();
                if field.indexed {
                    self.write("indexed ");
                }
                self.write(&field.name.name);
                self.write(": ");
                self.format_type(&field.ty);
                self.write(",");
                self.newline();
            }

            self.indent_level -= 1;
            self.write_indent();
            self.write("}");
        }
        self.newline();
    }

    fn format_error(&mut self, error: &ErrorDef) {
        self.write_indent();
        self.write("error ");
        self.write(&error.name.name);
        self.write("(");
        for (i, field) in error.fields.iter().enumerate() {
            if i > 0 {
                self.write(", ");
            }
            self.write(&field.name.name);
            self.write(": ");
            self.format_type(&field.ty);
        }
        self.write(");");
        self.newline();
    }

    fn format_interface(&mut self, interface: &InterfaceDef) {
        self.write("interface ");
        self.write(&interface.name.name);
        if let Some(generics) = &interface.generics {
            self.format_generics(generics);
        }
        if !interface.extends.is_empty() {
            self.write(" is ");
            for (i, path) in interface.extends.iter().enumerate() {
                if i > 0 {
                    self.write(", ");
                }
                self.format_type_path(path);
            }
        }
        self.write(" {");
        self.newline();
        self.indent_level += 1;

        for item in &interface.items {
            match item {
                InterfaceItem::Function(sig) => {
                    self.write_indent();
                    self.format_function_sig(sig);
                    self.write(";");
                    self.newline();
                }
                InterfaceItem::Event(event) => self.format_event(event),
                InterfaceItem::Error(error) => self.format_error(error),
            }
        }

        self.indent_level -= 1;
        self.write_indent();
        self.write("}");
        self.newline();
    }

    fn format_library(&mut self, library: &LibraryDef) {
        self.write("library ");
        self.write(&library.name.name);
        self.write(" {");
        self.newline();
        self.indent_level += 1;

        for item in &library.items {
            match item {
                LibraryItem::Function(func) => self.format_function(func),
                LibraryItem::Struct(struct_def) => self.format_struct(struct_def),
                LibraryItem::Const(const_item) => self.format_const(const_item),
            }
        }

        self.indent_level -= 1;
        self.write_indent();
        self.write("}");
        self.newline();
    }

    fn format_struct(&mut self, struct_def: &StructDef) {
        self.write_indent();
        self.format_visibility(struct_def.visibility);
        self.write("struct ");
        self.write(&struct_def.name.name);
        if let Some(generics) = &struct_def.generics {
            self.format_generics(generics);
        }
        if !struct_def.abilities.is_empty() {
            self.write(" has ");
            for (i, ability) in struct_def.abilities.iter().enumerate() {
                if i > 0 {
                    self.write(", ");
                }
                self.format_ability(*ability);
            }
        }
        self.write(" {");
        self.newline();
        self.indent_level += 1;

        for field in &struct_def.fields {
            self.write_indent();
            self.format_visibility(field.visibility);
            self.write(&field.name.name);
            self.write(": ");
            self.format_type(&field.ty);
            self.write(",");
            self.newline();
        }

        self.indent_level -= 1;
        self.write_indent();
        self.write("}");
        self.newline();
    }

    fn format_enum(&mut self, enum_def: &EnumDef) {
        self.write_indent();
        self.format_visibility(enum_def.visibility);
        self.write("enum ");
        self.write(&enum_def.name.name);
        if let Some(generics) = &enum_def.generics {
            self.format_generics(generics);
        }
        self.write(" {");
        self.newline();
        self.indent_level += 1;

        for variant in &enum_def.variants {
            self.write_indent();
            self.write(&variant.name.name);
            match &variant.fields {
                VariantFields::Unit => {}
                VariantFields::Tuple(types) => {
                    self.write("(");
                    for (i, ty) in types.iter().enumerate() {
                        if i > 0 {
                            self.write(", ");
                        }
                        self.format_type(ty);
                    }
                    self.write(")");
                }
                VariantFields::Struct(fields) => {
                    self.write(" {");
                    self.newline();
                    self.indent_level += 1;
                    for field in fields {
                        self.write_indent();
                        self.write(&field.name.name);
                        self.write(": ");
                        self.format_type(&field.ty);
                        self.write(",");
                        self.newline();
                    }
                    self.indent_level -= 1;
                    self.write_indent();
                    self.write("}");
                }
            }
            self.write(",");
            self.newline();
        }

        self.indent_level -= 1;
        self.write_indent();
        self.write("}");
        self.newline();
    }

    fn format_type_alias(&mut self, alias: &TypeAlias) {
        self.write_indent();
        self.format_visibility(alias.visibility);
        self.write("type ");
        self.write(&alias.name.name);
        if let Some(generics) = &alias.generics {
            self.format_generics(generics);
        }
        self.write(" = ");
        self.format_type(&alias.ty);
        self.write(";");
        self.newline();
    }

    fn format_const(&mut self, const_item: &ConstItem) {
        self.write_indent();
        self.format_visibility(const_item.visibility);
        self.write("const ");
        self.write(&const_item.name.name);
        self.write(": ");
        self.format_type(&const_item.ty);
        self.write(" = ");
        self.format_expr(&const_item.value);
        self.write(";");
        self.newline();
    }

    // ========================================================================
    // Functions
    // ========================================================================

    fn format_function(&mut self, func: &FunctionDef) {
        self.write_indent();
        self.format_function_sig(&func.sig);
        self.write(" ");
        self.format_block(&func.body);
        self.newline();
    }

    fn format_function_sig(&mut self, sig: &FunctionSig) {
        self.format_visibility(sig.visibility);
        self.write("fn ");
        self.write(&sig.name.name);
        if let Some(generics) = &sig.generics {
            self.format_generics(generics);
        }
        self.write("(");
        for (i, param) in sig.params.iter().enumerate() {
            if i > 0 {
                self.write(", ");
            }
            self.format_pattern(&param.pattern);
            self.write(": ");
            self.format_type(&param.ty);
        }
        self.write(")");

        if let Some(ret_type) = &sig.return_type {
            self.write(" -> ");
            self.format_type(ret_type);
        }

        self.format_function_modifiers(&sig.modifiers);
    }

    fn format_function_modifiers(&mut self, mods: &FunctionModifiers) {
        if mods.is_pure {
            self.write(" pure");
        }
        if mods.is_view {
            self.write(" view");
        }
        if mods.is_payable {
            self.write(" payable");
        }
        if mods.is_parallel {
            self.write(" parallel");
        }
        for modifier in &mods.custom_modifiers {
            self.write(" ");
            self.write(&modifier.name.name);
            if !modifier.args.is_empty() {
                self.write("(");
                for (i, arg) in modifier.args.iter().enumerate() {
                    if i > 0 {
                        self.write(", ");
                    }
                    self.format_expr(arg);
                }
                self.write(")");
            }
        }
    }

    fn format_constructor(&mut self, ctor: &ConstructorDef) {
        self.write_indent();
        self.format_visibility(ctor.visibility);
        self.write("fn new(");
        for (i, param) in ctor.params.iter().enumerate() {
            if i > 0 {
                self.write(", ");
            }
            self.format_pattern(&param.pattern);
            self.write(": ");
            self.format_type(&param.ty);
        }
        self.write(")");
        self.format_function_modifiers(&ctor.modifiers);
        self.write(" ");
        self.format_block(&ctor.body);
        self.newline();
    }

    fn format_fallback(&mut self, fallback: &FallbackDef) {
        self.write_indent();
        self.write("fallback() ");
        self.format_block(&fallback.body);
        self.newline();
    }

    fn format_receive(&mut self, receive: &ReceiveDef) {
        self.write_indent();
        self.write("receive() ");
        self.format_block(&receive.body);
        self.newline();
    }

    fn format_modifier(&mut self, modifier: &ModifierDef) {
        self.write_indent();
        self.write("modifier ");
        self.write(&modifier.name.name);
        self.write("(");
        for (i, param) in modifier.params.iter().enumerate() {
            if i > 0 {
                self.write(", ");
            }
            self.format_pattern(&param.pattern);
            self.write(": ");
            self.format_type(&param.ty);
        }
        self.write(") ");
        self.format_block(&modifier.body);
        self.newline();
    }

    // ========================================================================
    // Types
    // ========================================================================

    fn format_type(&mut self, ty: &Type) {
        match &ty.kind {
            TypeKind::Primitive(prim) => self.format_primitive_type(*prim),
            TypeKind::Path(path) => self.format_type_path(path),
            TypeKind::Array(elem, size) => {
                self.write("[");
                self.format_type(elem);
                self.write("; ");
                self.format_expr(size);
                self.write("]");
            }
            TypeKind::Slice(elem) => {
                self.write("[");
                self.format_type(elem);
                self.write("]");
            }
            TypeKind::Tuple(types) => {
                self.write("(");
                for (i, ty) in types.iter().enumerate() {
                    if i > 0 {
                        self.write(", ");
                    }
                    self.format_type(ty);
                }
                self.write(")");
            }
            TypeKind::Mapping(key, value) => {
                self.write("mapping(");
                self.format_type(key);
                self.write(" => ");
                self.format_type(value);
                self.write(")");
            }
            TypeKind::Option(inner) => {
                self.write("Option<");
                self.format_type(inner);
                self.write(">");
            }
            TypeKind::Result(ok, err) => {
                self.write("Result<");
                self.format_type(ok);
                self.write(", ");
                self.format_type(err);
                self.write(">");
            }
            TypeKind::Reference(inner, mutable) => {
                if *mutable {
                    self.write("&mut ");
                } else {
                    self.write("&");
                }
                self.format_type(inner);
            }
            TypeKind::Resource(inner, abilities) => {
                self.format_type(inner);
                if !abilities.is_empty() {
                    self.write(" has ");
                    for (i, ability) in abilities.iter().enumerate() {
                        if i > 0 {
                            self.write(", ");
                        }
                        self.format_ability(*ability);
                    }
                }
            }
            TypeKind::Function(params, ret) => {
                self.write("fn(");
                for (i, ty) in params.iter().enumerate() {
                    if i > 0 {
                        self.write(", ");
                    }
                    self.format_type(ty);
                }
                self.write(")");
                if let Some(ret) = ret {
                    self.write(" -> ");
                    self.format_type(ret);
                }
            }
            TypeKind::Infer => self.write("_"),
            TypeKind::Never => self.write("!"),
            TypeKind::SelfType => self.write("Self"),
        }
    }

    fn format_primitive_type(&mut self, prim: PrimitiveType) {
        let s = match prim {
            PrimitiveType::Bool => "bool",
            PrimitiveType::U8 => "u8",
            PrimitiveType::U16 => "u16",
            PrimitiveType::U32 => "u32",
            PrimitiveType::U64 => "u64",
            PrimitiveType::U128 => "u128",
            PrimitiveType::U256 => "u256",
            PrimitiveType::I8 => "i8",
            PrimitiveType::I16 => "i16",
            PrimitiveType::I32 => "i32",
            PrimitiveType::I64 => "i64",
            PrimitiveType::I128 => "i128",
            PrimitiveType::I256 => "i256",
            PrimitiveType::Address => "address",
            PrimitiveType::Bytes => "bytes",
            PrimitiveType::String => "string",
        };
        self.write(s);
    }

    fn format_type_path(&mut self, path: &TypePath) {
        for (i, segment) in path.segments.iter().enumerate() {
            if i > 0 {
                self.write("::");
            }
            self.write(&segment.ident.name);
            if let Some(generics) = &segment.generics {
                self.write("<");
                for (i, ty) in generics.iter().enumerate() {
                    if i > 0 {
                        self.write(", ");
                    }
                    self.format_type(ty);
                }
                self.write(">");
            }
        }
    }

    fn format_ability(&mut self, ability: ResourceAbility) {
        let s = match ability {
            ResourceAbility::Copy => "copy",
            ResourceAbility::Drop => "drop",
            ResourceAbility::Store => "store",
            ResourceAbility::Key => "key",
        };
        self.write(s);
    }

    fn format_generics(&mut self, generics: &Generics) {
        if generics.params.is_empty() {
            return;
        }
        self.write("<");
        for (i, param) in generics.params.iter().enumerate() {
            if i > 0 {
                self.write(", ");
            }
            match param {
                GenericParam::Type(tp) => {
                    self.write(&tp.name.name);
                    if !tp.bounds.is_empty() {
                        self.write(": ");
                        for (i, bound) in tp.bounds.iter().enumerate() {
                            if i > 0 {
                                self.write(" + ");
                            }
                            self.format_type_path(&bound.path);
                        }
                    }
                }
                GenericParam::Const(cp) => {
                    self.write("const ");
                    self.write(&cp.name.name);
                    self.write(": ");
                    self.format_type(&cp.ty);
                }
            }
        }
        self.write(">");
    }

    fn format_visibility(&mut self, vis: Visibility) {
        match vis {
            Visibility::Private => {}
            Visibility::Public => self.write("pub "),
        }
    }

    // ========================================================================
    // Statements
    // ========================================================================

    fn format_block(&mut self, block: &Block) {
        self.write("{");
        if block.stmts.is_empty() {
            self.write("}");
            return;
        }

        self.newline();
        self.indent_level += 1;

        for stmt in &block.stmts {
            self.format_stmt(stmt);
        }

        self.indent_level -= 1;
        self.write_indent();
        self.write("}");
    }

    fn format_stmt(&mut self, stmt: &Stmt) {
        match &stmt.kind {
            StmtKind::Local(local) => {
                self.write_indent();
                self.write("let ");
                if local.is_mutable {
                    self.write("mut ");
                }
                self.format_pattern(&local.pattern);
                if let Some(ty) = &local.ty {
                    self.write(": ");
                    self.format_type(ty);
                }
                if let Some(init) = &local.init {
                    self.write(" = ");
                    self.format_expr(init);
                }
                self.write(";");
                self.newline();
            }
            StmtKind::Expr(expr) => {
                self.write_indent();
                self.format_expr(expr);
                self.newline();
            }
            StmtKind::Semi(expr) => {
                self.write_indent();
                self.format_expr(expr);
                self.write(";");
                self.newline();
            }
            StmtKind::Item(item) => {
                self.format_item(item);
            }
            StmtKind::Empty => {
                self.write_indent();
                self.write(";");
                self.newline();
            }
        }
    }

    // ========================================================================
    // Expressions
    // ========================================================================

    fn format_expr(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Literal(lit) => self.format_literal(lit),
            ExprKind::Path(path) => self.format_expr_path(path),
            ExprKind::Binary(op, lhs, rhs) => {
                self.format_expr(lhs);
                self.write(" ");
                self.format_binary_op(*op);
                self.write(" ");
                self.format_expr(rhs);
            }
            ExprKind::Unary(op, operand) => {
                self.format_unary_op(*op);
                self.format_expr(operand);
            }
            ExprKind::Call(func, args) => {
                self.format_expr(func);
                self.write("(");
                for (i, arg) in args.iter().enumerate() {
                    if i > 0 {
                        self.write(", ");
                    }
                    self.format_expr(arg);
                }
                self.write(")");
            }
            ExprKind::MethodCall(obj, method, args) => {
                self.format_expr(obj);
                self.write(".");
                self.write(&method.name);
                self.write("(");
                for (i, arg) in args.iter().enumerate() {
                    if i > 0 {
                        self.write(", ");
                    }
                    self.format_expr(arg);
                }
                self.write(")");
            }
            ExprKind::Field(obj, field) => {
                self.format_expr(obj);
                self.write(".");
                self.write(&field.name);
            }
            ExprKind::Index(obj, idx) => {
                self.format_expr(obj);
                self.write("[");
                self.format_expr(idx);
                self.write("]");
            }
            ExprKind::Cast(expr, ty) => {
                self.format_expr(expr);
                self.write(" as ");
                self.format_type(ty);
            }
            ExprKind::Reference(expr, mutable) => {
                if *mutable {
                    self.write("&mut ");
                } else {
                    self.write("&");
                }
                self.format_expr(expr);
            }
            ExprKind::Deref(expr) => {
                self.write("*");
                self.format_expr(expr);
            }
            ExprKind::Block(block) => {
                self.format_block(block);
            }
            ExprKind::If(cond, then_block, else_expr) => {
                self.write("if ");
                self.format_expr(cond);
                self.write(" ");
                self.format_block(then_block);
                if let Some(else_expr) = else_expr {
                    self.write(" else ");
                    self.format_expr(else_expr);
                }
            }
            ExprKind::Match(expr, arms) => {
                self.write("match ");
                self.format_expr(expr);
                self.write(" {");
                self.newline();
                self.indent_level += 1;

                for arm in arms {
                    self.write_indent();
                    self.format_pattern(&arm.pattern);
                    if let Some(guard) = &arm.guard {
                        self.write(" if ");
                        self.format_expr(guard);
                    }
                    self.write(" => ");
                    self.format_expr(&arm.body);
                    self.write(",");
                    self.newline();
                }

                self.indent_level -= 1;
                self.write_indent();
                self.write("}");
            }
            ExprKind::For(pattern, iter, body) => {
                self.write("for ");
                self.format_pattern(pattern);
                self.write(" in ");
                self.format_expr(iter);
                self.write(" ");
                self.format_block(body);
            }
            ExprKind::While(cond, body) => {
                self.write("while ");
                self.format_expr(cond);
                self.write(" ");
                self.format_block(body);
            }
            ExprKind::Loop(body) => {
                self.write("loop ");
                self.format_block(body);
            }
            ExprKind::Break(expr) => {
                self.write("break");
                if let Some(expr) = expr {
                    self.write(" ");
                    self.format_expr(expr);
                }
            }
            ExprKind::Continue => {
                self.write("continue");
            }
            ExprKind::Return(expr) => {
                self.write("return");
                if let Some(expr) = expr {
                    self.write(" ");
                    self.format_expr(expr);
                }
            }
            ExprKind::Tuple(exprs) => {
                self.write("(");
                for (i, expr) in exprs.iter().enumerate() {
                    if i > 0 {
                        self.write(", ");
                    }
                    self.format_expr(expr);
                }
                self.write(")");
            }
            ExprKind::Array(exprs) => {
                self.write("[");
                for (i, expr) in exprs.iter().enumerate() {
                    if i > 0 {
                        self.write(", ");
                    }
                    self.format_expr(expr);
                }
                self.write("]");
            }
            ExprKind::ArrayRepeat(expr, count) => {
                self.write("[");
                self.format_expr(expr);
                self.write("; ");
                self.format_expr(count);
                self.write("]");
            }
            ExprKind::Struct(path, fields) => {
                self.format_type_path(path);
                self.write(" { ");
                for (i, field) in fields.iter().enumerate() {
                    if i > 0 {
                        self.write(", ");
                    }
                    self.write(&field.name.name);
                    if let Some(value) = &field.value {
                        self.write(": ");
                        self.format_expr(value);
                    }
                }
                self.write(" }");
            }
            ExprKind::Range(start, end, inclusive) => {
                if let Some(start) = start {
                    self.format_expr(start);
                }
                if *inclusive {
                    self.write("..=");
                } else {
                    self.write("..");
                }
                if let Some(end) = end {
                    self.format_expr(end);
                }
            }
            ExprKind::Closure(params, body) => {
                self.write("|");
                for (i, param) in params.iter().enumerate() {
                    if i > 0 {
                        self.write(", ");
                    }
                    self.format_pattern(&param.pattern);
                    if let Some(ty) = &param.ty {
                        self.write(": ");
                        self.format_type(ty);
                    }
                }
                self.write("| ");
                self.format_expr(body);
            }
            ExprKind::Move(expr) => {
                self.write("move ");
                self.format_expr(expr);
            }
            ExprKind::Emit(path, fields) => {
                self.write("emit ");
                self.format_type_path(path);
                self.write(" { ");
                for (i, field) in fields.iter().enumerate() {
                    if i > 0 {
                        self.write(", ");
                    }
                    self.write(&field.name.name);
                    if let Some(value) = &field.value {
                        self.write(": ");
                        self.format_expr(value);
                    }
                }
                self.write(" }");
            }
            ExprKind::Revert(path, fields) => {
                self.write("revert ");
                self.format_type_path(path);
                self.write(" { ");
                for (i, field) in fields.iter().enumerate() {
                    if i > 0 {
                        self.write(", ");
                    }
                    self.write(&field.name.name);
                    if let Some(value) = &field.value {
                        self.write(": ");
                        self.format_expr(value);
                    }
                }
                self.write(" }");
            }
            ExprKind::Assert(cond, msg) => {
                self.write("assert!(");
                self.format_expr(cond);
                if let Some(msg) = msg {
                    self.write(", ");
                    self.format_expr(msg);
                }
                self.write(")");
            }
            ExprKind::Try(expr) => {
                self.format_expr(expr);
                self.write("?");
            }
            ExprKind::Paren(expr) => {
                self.write("(");
                self.format_expr(expr);
                self.write(")");
            }
            ExprKind::Await(expr) => {
                self.format_expr(expr);
                self.write(".await");
            }
        }
    }

    fn format_expr_path(&mut self, path: &ExprPath) {
        for (i, segment) in path.segments.iter().enumerate() {
            if i > 0 {
                self.write("::");
            }
            self.write(&segment.name);
        }
    }

    fn format_literal(&mut self, lit: &Literal) {
        match lit {
            Literal::Int(value, suffix) => {
                self.write(value);
                if let Some(ty) = suffix {
                    self.format_primitive_type(*ty);
                }
            }
            Literal::Float(value) => self.write(value),
            Literal::String(s) => {
                self.write("\"");
                self.write(&escape_string(s));
                self.write("\"");
            }
            Literal::ByteString(bytes) => {
                self.write("b\"");
                for byte in bytes {
                    if byte.is_ascii_graphic() || *byte == b' ' {
                        self.output.push(*byte as char);
                    } else {
                        self.write(&format!("\\x{:02x}", byte));
                    }
                }
                self.write("\"");
            }
            Literal::Bool(b) => {
                self.write(if *b { "true" } else { "false" });
            }
            Literal::Address(addr) => {
                self.write(addr);
            }
        }
    }

    fn format_binary_op(&mut self, op: BinaryOp) {
        let s = match op {
            BinaryOp::Add => "+",
            BinaryOp::Sub => "-",
            BinaryOp::Mul => "*",
            BinaryOp::Div => "/",
            BinaryOp::Rem => "%",
            BinaryOp::Pow => "**",
            BinaryOp::Eq => "==",
            BinaryOp::Ne => "!=",
            BinaryOp::Lt => "<",
            BinaryOp::Le => "<=",
            BinaryOp::Gt => ">",
            BinaryOp::Ge => ">=",
            BinaryOp::And => "&&",
            BinaryOp::Or => "||",
            BinaryOp::BitAnd => "&",
            BinaryOp::BitOr => "|",
            BinaryOp::BitXor => "^",
            BinaryOp::Shl => "<<",
            BinaryOp::Shr => ">>",
            BinaryOp::Assign => "=",
            BinaryOp::AddAssign => "+=",
            BinaryOp::SubAssign => "-=",
            BinaryOp::MulAssign => "*=",
            BinaryOp::DivAssign => "/=",
            BinaryOp::RemAssign => "%=",
            BinaryOp::BitAndAssign => "&=",
            BinaryOp::BitOrAssign => "|=",
            BinaryOp::BitXorAssign => "^=",
            BinaryOp::ShlAssign => "<<=",
            BinaryOp::ShrAssign => ">>=",
        };
        self.write(s);
    }

    fn format_unary_op(&mut self, op: UnaryOp) {
        let s = match op {
            UnaryOp::Neg => "-",
            UnaryOp::Not => "!",
            UnaryOp::BitNot => "~",
        };
        self.write(s);
    }

    // ========================================================================
    // Patterns
    // ========================================================================

    fn format_pattern(&mut self, pattern: &Pattern) {
        match &pattern.kind {
            PatternKind::Wildcard => self.write("_"),
            PatternKind::Ident(ident, mutable) => {
                if *mutable {
                    self.write("mut ");
                }
                self.write(&ident.name);
            }
            PatternKind::Literal(lit) => self.format_literal(lit),
            PatternKind::Tuple(patterns) => {
                self.write("(");
                for (i, p) in patterns.iter().enumerate() {
                    if i > 0 {
                        self.write(", ");
                    }
                    self.format_pattern(p);
                }
                self.write(")");
            }
            PatternKind::Struct(path, fields) => {
                self.format_type_path(path);
                self.write(" { ");
                for (i, field) in fields.iter().enumerate() {
                    if i > 0 {
                        self.write(", ");
                    }
                    self.write(&field.name.name);
                    if let Some(pattern) = &field.pattern {
                        self.write(": ");
                        self.format_pattern(pattern);
                    }
                }
                self.write(" }");
            }
            PatternKind::TupleStruct(path, patterns) => {
                self.format_type_path(path);
                self.write("(");
                for (i, p) in patterns.iter().enumerate() {
                    if i > 0 {
                        self.write(", ");
                    }
                    self.format_pattern(p);
                }
                self.write(")");
            }
            PatternKind::Or(patterns) => {
                for (i, p) in patterns.iter().enumerate() {
                    if i > 0 {
                        self.write(" | ");
                    }
                    self.format_pattern(p);
                }
            }
            PatternKind::Ref(pattern, mutable) => {
                if *mutable {
                    self.write("&mut ");
                } else {
                    self.write("&");
                }
                self.format_pattern(pattern);
            }
            PatternKind::Range(start, end, inclusive) => {
                self.format_pattern(start);
                if *inclusive {
                    self.write("..=");
                } else {
                    self.write("..");
                }
                self.format_pattern(end);
            }
            PatternKind::Rest => self.write(".."),
            PatternKind::Path(path) => self.format_type_path(path),
        }
    }

    // ========================================================================
    // Helpers
    // ========================================================================

    fn write(&mut self, s: &str) {
        self.output.push_str(s);
    }

    fn newline(&mut self) {
        self.output.push('\n');
    }

    fn write_indent(&mut self) {
        self.output.push_str(&self.config.indent_n(self.indent_level));
    }
}

fn escape_string(s: &str) -> String {
    let mut result = String::new();
    for c in s.chars() {
        match c {
            '"' => result.push_str("\\\""),
            '\\' => result.push_str("\\\\"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            c => result.push(c),
        }
    }
    result
}
