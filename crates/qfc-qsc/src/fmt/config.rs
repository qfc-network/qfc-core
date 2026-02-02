//! Formatter configuration.

/// Configuration for the code formatter.
#[derive(Debug, Clone)]
pub struct FormatConfig {
    /// Number of spaces per indentation level.
    pub indent_size: usize,

    /// Maximum line width before wrapping.
    pub max_width: usize,

    /// Use tabs instead of spaces.
    pub use_tabs: bool,

    /// Add trailing commas in multi-line constructs.
    pub trailing_commas: bool,

    /// Put opening brace on same line.
    pub brace_same_line: bool,

    /// Spaces inside braces: `{ x }` vs `{x}`.
    pub spaces_in_braces: bool,

    /// Blank lines between top-level items.
    pub blank_lines_between_items: usize,

    /// Sort imports alphabetically.
    pub sort_imports: bool,
}

impl Default for FormatConfig {
    fn default() -> Self {
        Self {
            indent_size: 4,
            max_width: 100,
            use_tabs: false,
            trailing_commas: true,
            brace_same_line: true,
            spaces_in_braces: true,
            blank_lines_between_items: 1,
            sort_imports: true,
        }
    }
}

impl FormatConfig {
    /// Create a new configuration with default values.
    pub fn new() -> Self {
        Self::default()
    }

    /// Get the indentation string for one level.
    pub fn indent_str(&self) -> String {
        if self.use_tabs {
            "\t".to_string()
        } else {
            " ".repeat(self.indent_size)
        }
    }

    /// Get the indentation string for n levels.
    pub fn indent_n(&self, n: usize) -> String {
        self.indent_str().repeat(n)
    }
}
