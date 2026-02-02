//! Document management for the language server.

use ropey::Rope;
use tower_lsp::lsp_types::{Position, Range};

/// A managed document with rope-based text storage.
#[derive(Debug, Clone)]
pub struct Document {
    /// The document content as a rope for efficient editing.
    pub content: Rope,
    /// The document version.
    pub version: i32,
}

impl Document {
    /// Create a new document from text content.
    pub fn new(content: &str, version: i32) -> Self {
        Self {
            content: Rope::from_str(content),
            version,
        }
    }

    /// Get the full text content.
    pub fn text(&self) -> String {
        self.content.to_string()
    }

    /// Apply a full content change.
    pub fn apply_full_change(&mut self, new_content: &str, version: i32) {
        self.content = Rope::from_str(new_content);
        self.version = version;
    }

    /// Apply an incremental change at a range.
    pub fn apply_incremental_change(&mut self, range: Range, new_text: &str, version: i32) {
        let start_idx = self.position_to_offset(range.start);
        let end_idx = self.position_to_offset(range.end);

        if let (Some(start), Some(end)) = (start_idx, end_idx) {
            self.content.remove(start..end);
            self.content.insert(start, new_text);
        }

        self.version = version;
    }

    /// Convert an LSP position to a byte offset.
    pub fn position_to_offset(&self, position: Position) -> Option<usize> {
        let line = position.line as usize;
        let character = position.character as usize;

        if line >= self.content.len_lines() {
            return None;
        }

        let line_start = self.content.line_to_char(line);
        let line_text = self.content.line(line);

        // Handle UTF-16 code units (LSP uses UTF-16)
        let mut utf16_offset = 0;
        let mut char_offset = 0;

        for ch in line_text.chars() {
            if utf16_offset >= character {
                break;
            }
            utf16_offset += ch.len_utf16();
            char_offset += 1;
        }

        Some(self.content.char_to_byte(line_start + char_offset))
    }

    /// Convert a byte offset to an LSP position.
    pub fn offset_to_position(&self, offset: usize) -> Option<Position> {
        if offset > self.content.len_bytes() {
            return None;
        }

        let char_offset = self.content.byte_to_char(offset);
        let line = self.content.char_to_line(char_offset);
        let line_start = self.content.line_to_char(line);

        // Calculate UTF-16 offset for the character position
        let mut utf16_offset = 0;
        for ch in self.content.slice(line_start..char_offset).chars() {
            utf16_offset += ch.len_utf16();
        }

        Some(Position {
            line: line as u32,
            character: utf16_offset as u32,
        })
    }

    /// Get a range of text as a string.
    pub fn get_range(&self, range: Range) -> Option<String> {
        let start = self.position_to_offset(range.start)?;
        let end = self.position_to_offset(range.end)?;
        let start_char = self.content.byte_to_char(start);
        let end_char = self.content.byte_to_char(end);
        Some(self.content.slice(start_char..end_char).to_string())
    }

    /// Get the word at a position.
    pub fn word_at_position(&self, position: Position) -> Option<(String, Range)> {
        let offset = self.position_to_offset(position)?;
        let char_offset = self.content.byte_to_char(offset);

        // Find word boundaries
        let mut start = char_offset;
        let mut end = char_offset;

        // Search backward for word start
        while start > 0 {
            let ch = self.content.char(start - 1);
            if !is_identifier_char(ch) {
                break;
            }
            start -= 1;
        }

        // Search forward for word end
        while end < self.content.len_chars() {
            let ch = self.content.char(end);
            if !is_identifier_char(ch) {
                break;
            }
            end += 1;
        }

        if start == end {
            return None;
        }

        let word = self.content.slice(start..end).to_string();
        let start_pos = self.offset_to_position(self.content.char_to_byte(start))?;
        let end_pos = self.offset_to_position(self.content.char_to_byte(end))?;

        Some((word, Range::new(start_pos, end_pos)))
    }
}

fn is_identifier_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_document_creation() {
        let doc = Document::new("hello\nworld", 1);
        assert_eq!(doc.text(), "hello\nworld");
        assert_eq!(doc.version, 1);
    }

    #[test]
    fn test_position_conversion() {
        let doc = Document::new("hello\nworld\n", 1);

        // First line, first char
        let pos = Position::new(0, 0);
        let offset = doc.position_to_offset(pos).unwrap();
        assert_eq!(offset, 0);

        // Second line, first char
        let pos = Position::new(1, 0);
        let offset = doc.position_to_offset(pos).unwrap();
        assert_eq!(offset, 6); // "hello\n" = 6 bytes

        // Convert back
        let pos_back = doc.offset_to_position(6).unwrap();
        assert_eq!(pos_back.line, 1);
        assert_eq!(pos_back.character, 0);
    }

    #[test]
    fn test_word_at_position() {
        let doc = Document::new("let foo = 42;", 1);

        // Position at 'f' in 'foo'
        let (word, _range) = doc.word_at_position(Position::new(0, 4)).unwrap();
        assert_eq!(word, "foo");
    }

    #[test]
    fn test_incremental_change() {
        let mut doc = Document::new("hello world", 1);

        // Replace "world" with "rust"
        doc.apply_incremental_change(
            Range::new(Position::new(0, 6), Position::new(0, 11)),
            "rust",
            2,
        );

        assert_eq!(doc.text(), "hello rust");
        assert_eq!(doc.version, 2);
    }
}
