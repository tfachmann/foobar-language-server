//! LSP UTF-16 <-> rope char <-> rope byte <-> tree-sitter Point conversion.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};
use ropey::Rope;
use tower_lsp_server::ls_types::{Position, Range, TextDocumentContentChangeEvent, Uri};
use tree_sitter::{InputEdit, Node, Point};

pub fn uri_to_path(uri: &Uri) -> Option<PathBuf> {
    let s = uri.as_str();
    let rest = s.strip_prefix("file://")?;
    let path_part = match rest.find('/') {
        Some(0) => rest,
        Some(slash) => &rest[slash..],
        None => return None,
    };
    let decoded = percent_decode_str(path_part).decode_utf8().ok()?;
    Some(PathBuf::from(decoded.as_ref()))
}

/// Build a `file://` URI from an absolute filesystem path.
pub fn path_to_uri(path: &Path) -> Option<Uri> {
    let p = path.to_str()?;
    if !p.starts_with('/') {
        return None;
    }
    // Encode the path component; keep '/' separator.
    const SAFE: &AsciiSet = &CONTROLS
        .add(b' ')
        .add(b'"')
        .add(b'#')
        .add(b'<')
        .add(b'>')
        .add(b'?')
        .add(b'`')
        .add(b'{')
        .add(b'}');
    let encoded: String = utf8_percent_encode(p, SAFE).collect();
    Uri::from_str(&format!("file://{encoded}")).ok()
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

pub trait PositionExt {
    fn point(&self) -> Point;
}

impl PositionExt for Position {
    fn point(&self) -> Point {
        Point {
            row: self.line as usize,
            column: self.character as usize,
        }
    }
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
        let row = pos.line as usize;
        let utf16_col = pos.character as usize;

        let row_char = self.line_to_char(row);
        let col_char = self.utf16_cu_to_char(utf16_col);

        let row_byte = self.line_to_byte(row);
        let col_byte = self.char_to_byte(col_char);

        TextPos {
            byte: row_byte + col_byte,
            char: row_char + col_char,
            point: Point::new(row, col_byte),
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

/// Compute the tree-sitter `Point` after inserting `text` starting at `start`.
fn end_point_after_insert(start: Point, text: &str) -> Point {
    let mut row = start.row;
    let mut col = start.column;
    let mut last_newline: Option<usize> = None;

    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            row += 1;
            last_newline = Some(i);
        }
    }

    col = match last_newline {
        Some(nl) => text.len() - nl - 1,
        None => col + text.len(),
    };

    Point::new(row, col)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn end_point_single_line() {
        let p = end_point_after_insert(Point::new(0, 3), "abc");
        assert_eq!(p, Point::new(0, 6));
    }

    #[test]
    fn end_point_multi_line() {
        let p = end_point_after_insert(Point::new(1, 2), "x\ny\nz");
        assert_eq!(p, Point::new(3, 1));
    }

    #[test]
    fn end_point_trailing_newline() {
        let p = end_point_after_insert(Point::new(0, 0), "abc\n");
        assert_eq!(p, Point::new(1, 0));
    }

    #[test]
    fn position_roundtrip_ascii() {
        let rope = Rope::from_str("line0\nline1\nline2");
        let pos = Position::new(1, 3);
        let tp = rope.lsp_position_to_text_pos(pos);
        assert_eq!(tp.byte, 9);
        assert_eq!(rope.byte_to_lsp_position(9), pos);
    }

    #[test]
    fn position_roundtrip_multibyte() {
        // "héllo" — é is 2 bytes UTF-8, 1 char, 1 UTF-16 code unit
        let rope = Rope::from_str("héllo");
        let pos = Position::new(0, 2); // after 'é'
        let tp = rope.lsp_position_to_text_pos(pos);
        assert_eq!(tp.byte, 3);
        assert_eq!(rope.byte_to_lsp_position(3), pos);
    }

    #[test]
    fn uri_path_roundtrip_simple() {
        let p = PathBuf::from("/tmp/foo.foobar");
        let uri = path_to_uri(&p).unwrap();
        assert_eq!(uri_to_path(&uri).unwrap(), p);
    }

    #[test]
    fn uri_path_roundtrip_with_space() {
        let p = PathBuf::from("/tmp/has space/bar.foobar");
        let uri = path_to_uri(&p).unwrap();
        assert!(uri.as_str().contains("%20"));
        assert_eq!(uri_to_path(&uri).unwrap(), p);
    }

    #[test]
    fn uri_to_path_rejects_non_file() {
        let uri = Uri::from_str("https://example.com/x").unwrap();
        assert!(uri_to_path(&uri).is_none());
    }
}
