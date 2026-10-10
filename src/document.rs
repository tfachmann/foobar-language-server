//! Per-document edit state: rope, parser, tree, and the snapshot published from them.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use ropey::Rope;
use tower_lsp_server::ls_types::{
    DidChangeTextDocumentParams, DidOpenTextDocumentParams, Position, Uri,
};
use tree_sitter::{Node, Parser, Tree};

use crate::text::RopeExt;
use crate::workspace::{FileSnapshot, Origin, uri_key};

/// Text handle and symbol information for a single Document.
///
/// A document equals to one file.
pub struct Document {
    pub uri: Uri,
    /// Workspace map key, computed once at open so it stays stable while the file is saved,
    /// renamed or deleted. `None` for non-`file://` buffers.
    pub key: Option<PathBuf>,
    /// To track text invalidation.
    pub version: i32,
    /// Handle to text.
    pub rope: Rope,
    /// Parsed tree (symbol-agnostic).
    pub tree: Tree,
    /// Symbols and parse errors of the latest parse; shared with other files via the workspace.
    pub snapshot: Arc<FileSnapshot>,
    /// Set by `did_close`. A `did_change` that arrives late must not touch a closed document.
    pub closed: bool,
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
        let snapshot = Arc::new(FileSnapshot::new(
            item.uri.clone(),
            Origin::Open,
            &tree,
            &rope,
        ));

        Ok(Self {
            key: uri_key(&item.uri),
            uri: item.uri,
            version: item.version,
            rope,
            tree,
            snapshot,
            closed: false,
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
        self.snapshot = Arc::new(FileSnapshot::new(
            self.uri.clone(),
            Origin::Open,
            &self.tree,
            &self.rope,
        ));
        Ok(())
    }

    /// Smallest tree-sitter node at the LSP cursor position.
    ///
    /// Looks up by byte: tree-sitter rows only count `\n`, LSP lines also break at `\r`.
    pub fn node_at(&self, pos: Position) -> Option<Node<'_>> {
        let byte = self.rope.lsp_position_to_text_pos(pos).byte;
        self.tree.root_node().descendant_for_byte_range(byte, byte)
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
        assert_eq!(doc.snapshot.symbols.defines["greet"].value_text, "wörld");
        assert!(doc.snapshot.symbols.defines.contains_key("user"));
    }

    #[test]
    fn full_replace_without_range() {
        let mut doc = Document::from_text(uri(), "defines\na: \"1\"\n");
        change(&mut doc, None, "defines\nb: \"2\"\n");
        assert!(doc.snapshot.symbols.defines.contains_key("b"));
        assert!(!doc.snapshot.symbols.defines.contains_key("a"));
    }

    /// Lines break at `\n`, `\r\n` and `\r` only (as in LSP), and positions clamp to the line end.
    #[test]
    fn positions_follow_lsp_lines() {
        // U+2028 is not a line break for LSP; the lone `\r` is.
        let text = "# a\u{2028}b\ndefines\rx: \"%(y)\"\n";
        let doc = Document::from_text(uri(), text);
        let node = doc.node_at(Position::new(2, 6)).unwrap();
        assert_eq!(doc.rope.node_text(node), "y");

        let byte = |line, character| {
            doc.rope
                .lsp_position_to_text_pos(Position::new(line, character))
                .byte
        };
        assert_eq!(byte(1, 99), text.find('\r').unwrap());
        assert_eq!(byte(99, 0), text.len());
    }
}
