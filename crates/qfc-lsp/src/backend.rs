//! LSP backend implementation.

use dashmap::DashMap;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};

use qfc_qsc::{lexer::Lexer, parser::Parser, typeck::TypeChecker};

use crate::diagnostics::{
    lexer_error_to_diagnostic, parse_error_to_diagnostic, type_error_to_diagnostic,
};
use crate::document::Document;

/// The LSP backend state.
pub struct Backend {
    /// The LSP client for sending notifications.
    client: Client,
    /// Open documents indexed by URI.
    documents: DashMap<Url, Document>,
}

impl Backend {
    /// Create a new backend.
    pub fn new(client: Client) -> Self {
        Self {
            client,
            documents: DashMap::new(),
        }
    }

    /// Analyze a document and publish diagnostics.
    async fn analyze_document(&self, uri: &Url) {
        let doc = match self.documents.get(uri) {
            Some(doc) => doc.clone(),
            None => return,
        };

        let text = doc.text();
        let mut diagnostics = Vec::new();

        // Lexical analysis
        let lexer = Lexer::new(&text);
        let tokens = match lexer.tokenize() {
            Ok(tokens) => tokens,
            Err(error) => {
                diagnostics.push(lexer_error_to_diagnostic(&error, &doc));
                // Can't continue without valid tokens
                self.client
                    .publish_diagnostics(uri.clone(), diagnostics, Some(doc.version))
                    .await;
                return;
            }
        };

        // Parsing
        let mut parser = Parser::new(tokens);
        let ast = match parser.parse_file() {
            Ok(ast) => ast,
            Err(error) => {
                diagnostics.push(parse_error_to_diagnostic(&error, &doc));
                self.client
                    .publish_diagnostics(uri.clone(), diagnostics, Some(doc.version))
                    .await;
                return;
            }
        };

        // Type checking
        let mut type_checker = TypeChecker::new();
        if let Err(error) = type_checker.check_file(&ast) {
            diagnostics.push(type_error_to_diagnostic(&error, &doc));
        }

        self.client
            .publish_diagnostics(uri.clone(), diagnostics, Some(doc.version))
            .await;
    }

    /// Get completions for a position.
    fn get_completions(&self, _uri: &Url, _position: Position) -> Vec<CompletionItem> {
        // Keywords
        let keywords = vec![
            (
                "contract",
                "Define a new contract",
                CompletionItemKind::KEYWORD,
            ),
            ("fn", "Define a function", CompletionItemKind::KEYWORD),
            ("pub", "Public visibility", CompletionItemKind::KEYWORD),
            ("let", "Variable declaration", CompletionItemKind::KEYWORD),
            ("mut", "Mutable variable", CompletionItemKind::KEYWORD),
            ("if", "Conditional statement", CompletionItemKind::KEYWORD),
            ("else", "Else branch", CompletionItemKind::KEYWORD),
            ("for", "For loop", CompletionItemKind::KEYWORD),
            ("while", "While loop", CompletionItemKind::KEYWORD),
            ("return", "Return statement", CompletionItemKind::KEYWORD),
            ("struct", "Define a struct", CompletionItemKind::KEYWORD),
            ("enum", "Define an enum", CompletionItemKind::KEYWORD),
            ("event", "Define an event", CompletionItemKind::KEYWORD),
            ("emit", "Emit an event", CompletionItemKind::KEYWORD),
            ("storage", "Storage block", CompletionItemKind::KEYWORD),
            ("mapping", "Mapping type", CompletionItemKind::KEYWORD),
            ("require", "Require condition", CompletionItemKind::KEYWORD),
            (
                "view",
                "View function modifier",
                CompletionItemKind::KEYWORD,
            ),
            (
                "payable",
                "Payable function modifier",
                CompletionItemKind::KEYWORD,
            ),
            (
                "parallel",
                "Parallel execution attribute",
                CompletionItemKind::KEYWORD,
            ),
            ("resource", "Resource type", CompletionItemKind::KEYWORD),
            ("spec", "Specification block", CompletionItemKind::KEYWORD),
            (
                "invariant",
                "Contract invariant",
                CompletionItemKind::KEYWORD,
            ),
        ];

        // Types
        let types = vec![
            (
                "u8",
                "8-bit unsigned integer",
                CompletionItemKind::TYPE_PARAMETER,
            ),
            (
                "u16",
                "16-bit unsigned integer",
                CompletionItemKind::TYPE_PARAMETER,
            ),
            (
                "u32",
                "32-bit unsigned integer",
                CompletionItemKind::TYPE_PARAMETER,
            ),
            (
                "u64",
                "64-bit unsigned integer",
                CompletionItemKind::TYPE_PARAMETER,
            ),
            (
                "u128",
                "128-bit unsigned integer",
                CompletionItemKind::TYPE_PARAMETER,
            ),
            (
                "u256",
                "256-bit unsigned integer",
                CompletionItemKind::TYPE_PARAMETER,
            ),
            (
                "i8",
                "8-bit signed integer",
                CompletionItemKind::TYPE_PARAMETER,
            ),
            (
                "i16",
                "16-bit signed integer",
                CompletionItemKind::TYPE_PARAMETER,
            ),
            (
                "i32",
                "32-bit signed integer",
                CompletionItemKind::TYPE_PARAMETER,
            ),
            (
                "i64",
                "64-bit signed integer",
                CompletionItemKind::TYPE_PARAMETER,
            ),
            (
                "i128",
                "128-bit signed integer",
                CompletionItemKind::TYPE_PARAMETER,
            ),
            (
                "i256",
                "256-bit signed integer",
                CompletionItemKind::TYPE_PARAMETER,
            ),
            ("bool", "Boolean type", CompletionItemKind::TYPE_PARAMETER),
            (
                "address",
                "Blockchain address",
                CompletionItemKind::TYPE_PARAMETER,
            ),
            (
                "bytes",
                "Dynamic byte array",
                CompletionItemKind::TYPE_PARAMETER,
            ),
            (
                "bytes32",
                "32-byte fixed array",
                CompletionItemKind::TYPE_PARAMETER,
            ),
            ("string", "String type", CompletionItemKind::TYPE_PARAMETER),
        ];

        // Built-in functions
        let builtins = vec![
            (
                "msg.sender",
                "Transaction sender address",
                CompletionItemKind::VARIABLE,
            ),
            (
                "msg.value",
                "Transaction value",
                CompletionItemKind::VARIABLE,
            ),
            (
                "block.number",
                "Current block number",
                CompletionItemKind::VARIABLE,
            ),
            (
                "block.timestamp",
                "Current block timestamp",
                CompletionItemKind::VARIABLE,
            ),
            (
                "keccak256",
                "Keccak-256 hash function",
                CompletionItemKind::FUNCTION,
            ),
            (
                "sha256",
                "SHA-256 hash function",
                CompletionItemKind::FUNCTION,
            ),
            (
                "ecrecover",
                "Recover signer from signature",
                CompletionItemKind::FUNCTION,
            ),
        ];

        let mut completions = Vec::new();

        for (label, detail, kind) in keywords.into_iter().chain(types).chain(builtins) {
            completions.push(CompletionItem {
                label: label.to_string(),
                kind: Some(kind),
                detail: Some(detail.to_string()),
                ..Default::default()
            });
        }

        completions
    }

    /// Get hover information for a position.
    fn get_hover(&self, uri: &Url, position: Position) -> Option<Hover> {
        let doc = self.documents.get(uri)?;
        let (word, _range) = doc.word_at_position(position)?;

        let contents = match word.as_str() {
            // Keywords
            "contract" => "```quantumscript\ncontract Name { ... }\n```\nDefines a smart contract.",
            "fn" => "```quantumscript\npub fn name(params) -> ReturnType { ... }\n```\nDefines a function.",
            "let" => "```quantumscript\nlet name: Type = value;\n```\nDeclares an immutable variable.",
            "mut" => "```quantumscript\nlet mut name: Type = value;\n```\nDeclares a mutable variable.",
            "storage" => "```quantumscript\nstorage {\n    field: Type,\n}\n```\nDefines contract storage variables.",
            "event" => "```quantumscript\nevent Name { field: Type }\n```\nDefines an event for logging.",
            "emit" => "```quantumscript\nemit EventName { field: value };\n```\nEmits an event.",
            "require" => "```quantumscript\nrequire(condition, \"error message\");\n```\nReverts if condition is false.",
            "view" => "Read-only function modifier. Cannot modify state.",
            "payable" => "Function can receive native tokens via msg.value.",
            "parallel" => "Function can be executed in parallel with proper read/write annotations.",
            "resource" => "Linear type that must be explicitly created, moved, or destroyed.",

            // Types
            "u256" => "256-bit unsigned integer. Range: 0 to 2^256-1.",
            "u128" => "128-bit unsigned integer. Range: 0 to 2^128-1.",
            "u64" => "64-bit unsigned integer. Range: 0 to 2^64-1.",
            "u32" => "32-bit unsigned integer. Range: 0 to 2^32-1.",
            "u16" => "16-bit unsigned integer. Range: 0 to 65535.",
            "u8" => "8-bit unsigned integer. Range: 0 to 255.",
            "i256" => "256-bit signed integer.",
            "i128" => "128-bit signed integer.",
            "i64" => "64-bit signed integer.",
            "i32" => "32-bit signed integer.",
            "i16" => "16-bit signed integer.",
            "i8" => "8-bit signed integer.",
            "bool" => "Boolean type. Values: `true` or `false`.",
            "address" => "20-byte Ethereum-compatible address.",
            "bytes" => "Dynamic-length byte array.",
            "bytes32" => "Fixed 32-byte array, commonly used for hashes.",
            "string" => "UTF-8 encoded string.",
            "mapping" => "```quantumscript\nmapping(KeyType => ValueType)\n```\nKey-value storage mapping.",

            // Built-ins
            "msg" => "Transaction message context.\n- `msg.sender`: address - Transaction sender\n- `msg.value`: u256 - Sent native tokens\n- `msg.data`: bytes - Call data",
            "block" => "Block context.\n- `block.number`: u256 - Current block number\n- `block.timestamp`: u256 - Block timestamp",
            "tx" => "Transaction context.\n- `tx.origin`: address - Original sender\n- `tx.gasprice`: u256 - Gas price",

            _ => return None,
        };

        Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: contents.to_string(),
            }),
            range: None,
        })
    }

    /// Get document symbols.
    fn get_document_symbols(&self, uri: &Url) -> Option<Vec<DocumentSymbol>> {
        let doc = self.documents.get(uri)?;
        let text = doc.text();

        // Simple regex-based symbol extraction
        let mut symbols = Vec::new();

        for (line_num, line) in text.lines().enumerate() {
            let line_trimmed = line.trim();

            // Contract
            if line_trimmed.starts_with("contract ") {
                if let Some(name) = extract_name(line_trimmed, "contract ") {
                    symbols.push(create_symbol(
                        &name,
                        SymbolKind::CLASS,
                        line_num as u32,
                        line,
                    ));
                }
            }
            // Function
            else if line_trimmed.contains("fn ") {
                if let Some(name) = extract_fn_name(line_trimmed) {
                    symbols.push(create_symbol(
                        &name,
                        SymbolKind::FUNCTION,
                        line_num as u32,
                        line,
                    ));
                }
            }
            // Struct
            else if line_trimmed.starts_with("struct ") {
                if let Some(name) = extract_name(line_trimmed, "struct ") {
                    symbols.push(create_symbol(
                        &name,
                        SymbolKind::STRUCT,
                        line_num as u32,
                        line,
                    ));
                }
            }
            // Enum
            else if line_trimmed.starts_with("enum ") {
                if let Some(name) = extract_name(line_trimmed, "enum ") {
                    symbols.push(create_symbol(
                        &name,
                        SymbolKind::ENUM,
                        line_num as u32,
                        line,
                    ));
                }
            }
            // Event
            else if line_trimmed.starts_with("event ") {
                if let Some(name) = extract_name(line_trimmed, "event ") {
                    symbols.push(create_symbol(
                        &name,
                        SymbolKind::EVENT,
                        line_num as u32,
                        line,
                    ));
                }
            }
        }

        Some(symbols)
    }
}

fn extract_name(line: &str, prefix: &str) -> Option<String> {
    let after_prefix = line.strip_prefix(prefix)?;
    let name: String = after_prefix
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

fn extract_fn_name(line: &str) -> Option<String> {
    let fn_pos = line.find("fn ")?;
    let after_fn = &line[fn_pos + 3..];
    let name: String = after_fn
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

#[allow(deprecated)]
fn create_symbol(name: &str, kind: SymbolKind, line: u32, line_text: &str) -> DocumentSymbol {
    let start_col = line_text.find(name).unwrap_or(0) as u32;
    let end_col = start_col + name.len() as u32;

    DocumentSymbol {
        name: name.to_string(),
        detail: None,
        kind,
        tags: None,
        deprecated: None,
        range: Range::new(
            Position::new(line, 0),
            Position::new(line, line_text.len() as u32),
        ),
        selection_range: Range::new(Position::new(line, start_col), Position::new(line, end_col)),
        children: None,
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![".".to_string(), ":".to_string()]),
                    ..Default::default()
                }),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: "qsc-lsp".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        tracing::info!("QuantumScript Language Server initialized");
    }

    async fn shutdown(&self) -> Result<()> {
        tracing::info!("QuantumScript Language Server shutting down");
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri;
        let text = params.text_document.text;
        let version = params.text_document.version;

        tracing::debug!("Document opened: {}", uri);

        self.documents
            .insert(uri.clone(), Document::new(&text, version));
        self.analyze_document(&uri).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        let version = params.text_document.version;

        if let Some(mut doc) = self.documents.get_mut(&uri) {
            for change in params.content_changes {
                if let Some(range) = change.range {
                    doc.apply_incremental_change(range, &change.text, version);
                } else {
                    doc.apply_full_change(&change.text, version);
                }
            }
        }

        self.analyze_document(&uri).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        tracing::debug!("Document closed: {}", uri);
        self.documents.remove(&uri);

        // Clear diagnostics
        self.client.publish_diagnostics(uri, vec![], None).await;
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;

        let completions = self.get_completions(&uri, position);
        Ok(Some(CompletionResponse::Array(completions)))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        Ok(self.get_hover(&uri, position))
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let uri = params.text_document.uri;

        if let Some(symbols) = self.get_document_symbols(&uri) {
            Ok(Some(DocumentSymbolResponse::Nested(symbols)))
        } else {
            Ok(None)
        }
    }
}
