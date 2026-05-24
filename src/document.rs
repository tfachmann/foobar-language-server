//! Per-document state: rope, parser, tree, symbol index.

use anyhow::{Context, Result};
use ropey::Rope;
use tower_lsp_server::ls_types::{DidChangeTextDocumentParams, DidOpenTextDocumentParams, Uri};
use tree_sitter::{Parser, Tree};

use crate::symbol::SymbolIndex;
use crate::text::RopeExt;

/// Text handle and symbol information for a single Document.
///
/// A document equals to one file.
pub struct Document {
    pub uri: Uri,
    /// To track text invalidation.
    pub version: i32,
    /// Handle to text.
    pub rope: Rope,
    /// Parsed tree (symbol-agnostic).
    pub tree: Tree,
    /// Parsed symbols.
    pub symbols: SymbolIndex,
    /// Per-document [`Parser`].
    parser: Parser,
}

impl Document {
    pub fn open(params: DidOpenTextDocumentParams) -> Result<Self> {
        let item = params.text_document;
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_foobar::LANGUAGE.into())
            .context("loading foobar grammar")?;

        let rope = Rope::from_str(&item.text);
        let tree = parser
            .parse(&item.text, None)
            .context("initial parse returned None")?;
        let symbols = SymbolIndex::from_tree(&tree, &rope);

        Ok(Self {
            uri: item.uri,
            version: item.version,
            rope,
            tree,
            symbols,
            parser,
        })
    }

    pub fn apply_change(&mut self, params: DidChangeTextDocumentParams) -> Result<()> {
        self.version = params.text_document.version;

        for change in params.content_changes {
            let edit = self.rope.compute_edit(&change);
            self.rope.apply_edit(&edit);
            self.tree.edit(&edit.input_edit);
        }

        // Re-parse using the rope as text provider to avoid full-buffer allocation.
        let rope = &self.rope;
        let new_tree = self
            .parser
            .parse_with_options(
                &mut |byte, _| {
                    if byte >= rope.len_bytes() {
                        return "";
                    }
                    let (chunk, chunk_start, _, _) = rope.chunk_at_byte(byte);
                    &chunk[byte - chunk_start..]
                },
                Some(&self.tree),
                None,
            )
            .context("reparse returned None")?;
        self.tree = new_tree;
        self.symbols = SymbolIndex::from_tree(&self.tree, &self.rope);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;
    use tower_lsp_server::ls_types::{
        Position, Range, TextDocumentContentChangeEvent, TextDocumentItem,
        VersionedTextDocumentIdentifier,
    };

    fn uri() -> Uri {
        Uri::from_str("file:///test.foobar").unwrap()
    }

    fn open(text: &str) -> Document {
        Document::open(DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: uri(),
                language_id: "foobar".to_string(),
                version: 1,
                text: text.to_string(),
            },
        })
        .unwrap()
    }

    fn change(doc: &mut Document, version: i32, range: Option<Range>, text: &str) {
        doc.apply_change(DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier {
                uri: uri(),
                version,
            },
            content_changes: vec![TextDocumentContentChangeEvent {
                range,
                range_length: None,
                text: text.to_string(),
            }],
        })
        .unwrap();
    }

    fn assert_consistent(doc: &Document) {
        // Tree byte range must equal rope byte length.
        let tree_end = doc.tree.root_node().end_byte();
        assert_eq!(tree_end, doc.rope.len_bytes(), "tree end vs rope bytes");
    }

    #[test]
    fn open_then_no_changes() {
        let doc = open("defines\nname: \"x\"\n");
        assert_consistent(&doc);
        assert!(doc.symbols.defines.contains_key("name"));
    }

    #[test]
    fn insert_at_end() {
        let mut doc = open("defines\nname: \"x\"\n");
        let end_pos = doc.rope.byte_to_lsp_position(doc.rope.len_bytes());
        change(
            &mut doc,
            2,
            Some(Range::new(end_pos, end_pos)),
            "other: \"y\"\n",
        );
        assert_consistent(&doc);
        assert!(doc.symbols.defines.contains_key("other"));
    }

    #[test]
    fn replace_value() {
        let mut doc = open("defines\nname: \"old\"\n");
        // Replace the value "old" with "new".
        let start = Position::new(1, 7);
        let end = Position::new(1, 10);
        change(&mut doc, 2, Some(Range::new(start, end)), "new");
        assert_consistent(&doc);
        assert_eq!(doc.symbols.defines.get("name").unwrap().value_text, "new");
    }

    #[test]
    fn insert_at_start() {
        let mut doc = open("name: \"x\"\n");
        let zero = Position::new(0, 0);
        change(&mut doc, 2, Some(Range::new(zero, zero)), "defines\n");
        assert_consistent(&doc);
        assert!(doc.symbols.defines.contains_key("name"));
    }

    #[test]
    fn multibyte_edit() {
        // "héllo" has é (2 bytes UTF-8, 1 UTF-16 cu)
        let mut doc = open("defines\ngreet: \"héllo\"\n");
        assert_consistent(&doc);
        // Replace entire value "héllo" with "world"
        let start = Position::new(1, 8);
        let end = Position::new(1, 13);
        change(&mut doc, 2, Some(Range::new(start, end)), "world");
        assert_consistent(&doc);
        assert_eq!(
            doc.symbols.defines.get("greet").unwrap().value_text,
            "world"
        );
    }

    #[test]
    fn full_replace_no_range() {
        let mut doc = open("defines\na: \"1\"\n");
        change(&mut doc, 2, None, "defines\nb: \"2\"\n");
        assert_consistent(&doc);
        assert!(doc.symbols.defines.contains_key("b"));
        assert!(!doc.symbols.defines.contains_key("a"));
    }
}
