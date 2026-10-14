//! foobar language tooling: one grammar, one core, three tools.
//!
//! - Syntax (every tool): [`syntax`], [`text`], [`format`].
//! - Semantic core (language server and linter): [`symbol`], [`workspace`], [`diagnostics`],
//!   [`docs`].
//! - Editor: [`document`], [`target`], [`hover`], [`definition`], [`completion`], [`references`],
//!   [`rename`], [`folding`], [`server`].
//! - Command line: [`cli`].

pub mod cli;
pub mod completion;
pub mod definition;
pub mod diagnostics;
pub mod docs;
pub mod document;
pub mod folding;
pub mod format;
pub mod hover;
pub mod references;
pub mod rename;
pub mod server;
pub mod symbol;
pub mod syntax;
pub mod target;
pub mod text;
pub mod workspace;

#[cfg(test)]
mod tests {
    /// Zed bundles the highlight query in its extension: the copy must match the grammar's.
    #[test]
    fn zed_highlights_match_the_grammar() {
        assert_eq!(
            include_str!("../editors/zed/languages/foobar/highlights.scm"),
            tree_sitter_foobar::HIGHLIGHTS_QUERY,
            "copy tree-sitter-foobar/queries/highlights.scm to editors/zed/languages/foobar/"
        );
    }
}
