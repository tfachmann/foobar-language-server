//! LSP UTF-16 <-> rope char <-> rope byte <-> tree-sitter Point conversion.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use ropey::{Rope, RopeSlice};
use tower_lsp_server::ls_types::{Position, Range, TextDocumentContentChangeEvent, Uri};
use tree_sitter::{InputEdit, Node, Point};

/// Convert a `file://` URI into a filesystem path. `None` for other schemes.
pub fn uri_to_path(uri: &Uri) -> Option<PathBuf> {
    // `to_file_path` ignores the scheme, so check it first.
    if !uri.scheme().as_str().eq_ignore_ascii_case("file") {
        return None;
    }
    uri.to_file_path().map(Cow::into_owned)
}

/// Build a `file://` URI from an absolute filesystem path.
pub fn path_to_uri(path: &Path) -> Option<Uri> {
    Uri::from_file_path(path)
}

/// Coordinates of a single position, expressed in every system we care about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextPos {
    pub byte: usize,
    pub char: usize,
    pub point: Point,
}

/// A normalized, ready-to-apply edit derived from an LSP change event.
#[derive(Clone, Debug)]
pub struct ComputedEdit<'a> {
    pub start_char: usize,
    pub end_char: usize,
    pub input_edit: InputEdit,
    pub text: &'a str,
}

/// Helper trait to improve text operations.
pub trait RopeExt {
    fn lsp_position_to_text_pos(&self, pos: Position) -> TextPos;
    fn byte_to_lsp_position(&self, byte: usize) -> Position;
    fn byte_range_to_lsp_range(&self, byte_start: usize, byte_end: usize) -> Range;
    fn compute_edit<'a>(&self, change: &'a TextDocumentContentChangeEvent) -> ComputedEdit<'a>;
    fn apply_edit(&mut self, edit: &ComputedEdit);
    fn node_text(&self, node: Node) -> String;
    fn node_range(&self, node: Node) -> Range;
}

impl RopeExt for Rope {
    fn lsp_position_to_text_pos(&self, pos: Position) -> TextPos {
        // Out-of-range positions clamp to the last line and to the line end (LSP spec).
        let row = (pos.line as usize).min(self.len_lines() - 1);
        let line = self.line(row);
        let line_end_utf16 = line.char_to_utf16_cu(line.len_chars() - line_break_len(line));
        let col_utf16 = (pos.character as usize).min(line_end_utf16);

        let char = self.line_to_char(row) + line.utf16_cu_to_char(col_utf16);
        let byte = self.char_to_byte(char);

        TextPos {
            byte,
            char,
            // tree-sitter columns are bytes from the line start.
            point: Point::new(row, byte - self.line_to_byte(row)),
        }
    }

    fn byte_to_lsp_position(&self, byte: usize) -> Position {
        let line = self.byte_to_line(byte);
        let line_char = self.line_to_char(line);
        let line_utf16 = self.char_to_utf16_cu(line_char);
        let char = self.byte_to_char(byte);
        let utf16 = self.char_to_utf16_cu(char);
        Position::new(line as u32, (utf16 - line_utf16) as u32)
    }

    fn byte_range_to_lsp_range(&self, byte_start: usize, byte_end: usize) -> Range {
        Range::new(
            self.byte_to_lsp_position(byte_start),
            self.byte_to_lsp_position(byte_end),
        )
    }

    fn compute_edit<'a>(&self, change: &'a TextDocumentContentChangeEvent) -> ComputedEdit<'a> {
        let text = change.text.as_str();
        let text_bytes = text.len();

        let range = change.range.unwrap_or_else(|| {
            Range::new(
                self.byte_to_lsp_position(0),
                self.byte_to_lsp_position(self.len_bytes()),
            )
        });

        let start = self.lsp_position_to_text_pos(range.start);
        let old_end = self.lsp_position_to_text_pos(range.end);

        let new_end_byte = start.byte + text_bytes;
        let new_end_position = end_point_after_insert(start.point, text);

        let input_edit = InputEdit {
            start_byte: start.byte,
            old_end_byte: old_end.byte,
            new_end_byte,
            start_position: start.point,
            old_end_position: old_end.point,
            new_end_position,
        };

        ComputedEdit {
            start_char: start.char,
            end_char: old_end.char,
            input_edit,
            text,
        }
    }

    fn apply_edit(&mut self, edit: &ComputedEdit) {
        if edit.end_char > edit.start_char {
            self.remove(edit.start_char..edit.end_char);
        }
        if !edit.text.is_empty() {
            self.insert(edit.start_char, edit.text);
        }
    }

    fn node_text(&self, node: Node) -> String {
        self.byte_slice(node.start_byte()..node.end_byte())
            .to_string()
    }

    fn node_range(&self, node: Node) -> Range {
        self.byte_range_to_lsp_range(node.start_byte(), node.end_byte())
    }
}

/// Number of chars in the line break that ends `line` (`\n`, `\r\n` or `\r`).
fn line_break_len(line: RopeSlice) -> usize {
    let n = line.len_chars();
    let last = |k: usize| n.checked_sub(k).map(|i| line.char(i));
    match (last(2), last(1)) {
        (Some('\r'), Some('\n')) => 2,
        (_, Some('\n' | '\r')) => 1,
        _ => 0,
    }
}

/// Compute the tree-sitter `Point` after inserting `text` starting at `start`.
fn end_point_after_insert(start: Point, text: &str) -> Point {
    let newlines = text.bytes().filter(|&b| b == b'\n').count();
    match text.rfind('\n') {
        Some(nl) => Point::new(start.row + newlines, text.len() - nl - 1),
        None => Point::new(start.row, start.column + text.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn position_conversion_multibyte() {
        // "é" is 2 bytes in UTF-8 but 1 UTF-16 code unit; "😀" is 4 bytes and 2 code units.
        // Line 1 starts after them, so every coordinate system disagrees there.
        let rope = Rope::from_str("é😀\nab😀c");
        let pos = Position::new(1, 4); // before 'c'
        let tp = rope.lsp_position_to_text_pos(pos);
        assert_eq!(tp.byte, 7 + 6);
        assert_eq!(tp.char, 3 + 3);
        assert_eq!(tp.point, Point::new(1, 6));
        assert_eq!(rope.byte_to_lsp_position(tp.byte), pos);
    }

    #[test]
    fn end_point_after_multiline_insert() {
        assert_eq!(
            end_point_after_insert(Point::new(0, 3), "abc"),
            Point::new(0, 6)
        );
        assert_eq!(
            end_point_after_insert(Point::new(1, 2), "x\ny\nz"),
            Point::new(3, 1)
        );
    }

    #[test]
    fn uri_path_roundtrip_with_space_and_percent() {
        let p = PathBuf::from("/tmp/has space/100%.foobar");
        let uri = path_to_uri(&p).unwrap();
        assert!(uri.as_str().ends_with("/has%20space/100%25.foobar"));
        assert_eq!(uri_to_path(&uri).unwrap(), p);
    }
}
