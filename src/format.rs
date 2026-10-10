//! Formatter for foobar source. Syntax only: it needs the grammar, not the workspace.

use crate::syntax;

/// `source` formatted, or `None` if it has syntax errors (a recovered tree can't be trusted).
///
/// Placeholder: returns the input unchanged until the formatting rules exist.
pub fn format(source: &str) -> Option<String> {
    let tree = syntax::parse(source);
    (!tree.root_node().has_error()).then(|| source.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The contract any formatting rules must keep: only whitespace moves (the tree is
    /// unchanged), formatting twice changes nothing, and broken input is left alone.
    #[test]
    fn keeps_the_tree_and_is_idempotent() {
        let src = "defines\n  name: \"eurorust\"\n\nprocess compile it\n  command: '%(name).typ'\n";
        let once = format(src).unwrap();
        assert_eq!(
            syntax::parse(&once).root_node().to_sexp(),
            syntax::parse(src).root_node().to_sexp()
        );
        assert_eq!(format(&once).unwrap(), once);
        assert_eq!(format("defines\n  name:\n"), None);
    }
}
