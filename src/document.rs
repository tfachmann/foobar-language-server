//! Per-document state: rope, parser, tree, symbol index.

use anyhow::{Context, Result};
use ropey::Rope;
use tower_lsp_server::ls_types::{
    DidChangeTextDocumentParams, DidOpenTextDocumentParams, Position, Uri,
};
use tree_sitter::{Node, Parser, Tree};

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

    /// Smallest tree-sitter node at the LSP cursor position.
    pub fn node_at(&self, pos: Position) -> Option<Node<'_>> {
        let point = self.rope.lsp_position_to_text_pos(pos).point;
        self.tree
            .root_node()
            .descendant_for_point_range(point, point)
    }

    #[cfg(test)]
    pub fn from_text(uri: Uri, text: &str) -> Self {
        use tower_lsp_server::ls_types::TextDocumentItem;
        Self::open(DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri,
                language_id: "foobar".to_string(),
                version: 1,
                text: text.to_string(),
            },
        })
        .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;
    use tower_lsp_server::ls_types::{
        Range, TextDocumentContentChangeEvent, VersionedTextDocumentIdentifier,
    };

    fn uri() -> Uri {
        Uri::from_str("file:///test.foobar").unwrap()
    }

    fn change(doc: &mut Document, range: Option<Range>, text: &str) {
        doc.apply_change(DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier {
                uri: uri(),
                version: doc.version + 1,
            },
            content_changes: vec![TextDocumentContentChangeEvent {
                range,
                range_length: None,
                text: text.to_string(),
            }],
        })
        .unwrap();
    }

    fn range(l0: u32, c0: u32, l1: u32, c1: u32) -> Range {
        Range::new(Position::new(l0, c0), Position::new(l1, c1))
    }

    /// Incremental edits must leave rope and tree exactly as a fresh parse of the same text would.
    #[test]
    fn incremental_edits_match_fresh_parse() {
        let mut doc = Document::from_text(uri(), "defines\ngreet: \"héllo\"\n");

        change(&mut doc, Some(range(1, 8, 1, 13)), "wörld"); // replace inside a multibyte value
        change(&mut doc, Some(range(2, 0, 2, 0)), "name: \"%(greet)\"\n"); // append a line
        change(&mut doc, Some(range(0, 0, 0, 0)), "# 😀\n"); // multibyte on an earlier line
        change(&mut doc, Some(range(3, 0, 3, 4)), "user"); // rename key below it

        let text = "# 😀\ndefines\ngreet: \"wörld\"\nuser: \"%(greet)\"\n";
        assert_eq!(doc.rope.to_string(), text);
        let fresh = Document::from_text(uri(), text);
        assert_eq!(
            doc.tree.root_node().to_sexp(),
            fresh.tree.root_node().to_sexp()
        );
        assert_eq!(doc.symbols.defines["greet"].value_text, "wörld");
        assert!(doc.symbols.defines.contains_key("user"));
    }

    #[test]
    fn full_replace_without_range() {
        let mut doc = Document::from_text(uri(), "defines\na: \"1\"\n");
        change(&mut doc, None, "defines\nb: \"2\"\n");
        assert!(doc.symbols.defines.contains_key("b"));
        assert!(!doc.symbols.defines.contains_key("a"));
    }
}
