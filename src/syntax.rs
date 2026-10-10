//! The one place that builds a foobar parser.

use tree_sitter::{Parser, Tree};

/// A parser for the foobar grammar.
pub fn parser() -> Parser {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_foobar::LANGUAGE.into())
        .expect("tree-sitter-foobar matches the tree-sitter ABI");
    parser
}

/// Parse `text` from scratch.
pub fn parse(text: &str) -> Tree {
    // `parse` only returns `None` on cancellation or timeout, and neither is set.
    parser()
        .parse(text, None)
        .expect("parse without a timeout returns a tree")
}
