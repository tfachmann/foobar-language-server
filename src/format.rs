//! Formatter for foobar source. Syntax only: it needs the grammar, not the workspace.

use std::sync::LazyLock;

use anyhow::{Result, anyhow, ensure};
use topiary_core::{Language, Operation, TopiaryQuery, formatter_str};

use crate::syntax;

/// Topiary's view of foobar: the grammar plus its formatting query, built once.
static LANGUAGE: LazyLock<Language> = LazyLock::new(|| {
    let grammar = topiary_tree_sitter_facade::Language::from(tree_sitter_foobar::LANGUAGE);
    let formatting_query = TopiaryQuery::new(&grammar, tree_sitter_foobar::FORMATTING_QUERY)
        .expect("formatting.scm compiles against the grammar");
    Language {
        name: "foobar".to_string(),
        formatting_query,
        injection_query: None,
        grammar,
        indent: Some("  ".to_string()),
    }
});

/// `source` formatted. Refuses input with syntax errors: a recovered tree can't be trusted.
pub fn format(source: &str) -> Result<String> {
    // Topiary only rejects ERROR nodes; MISSING ones (`name:` without a value) count here too.
    ensure!(
        !syntax::parse(source).root_node().has_error(),
        "syntax errors"
    );
    let operation = Operation::Format {
        skip_idempotence: false,
        tolerate_parsing_errors: false,
        skip_stage: None,
    };
    let mut out = Vec::new();
    formatter_str(source, &mut out, &LANGUAGE, operation, None).map_err(|e| anyhow!("{e}"))?;
    Ok(String::from_utf8(out)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sexp(text: &str) -> String {
        syntax::parse(text).root_node().to_sexp()
    }

    /// Only whitespace moves, formatting twice changes nothing, and broken input is refused.
    #[test]
    fn keeps_the_tree_and_is_idempotent() {
        let src = "defines\nname:\"eurorust\"   # note\nprocess  compile it\n     use t( a:'x',b:\"y\" )\n";
        let once = format(src).unwrap();
        assert_eq!(
            once,
            "defines\n  name: \"eurorust\" # note\nprocess compile it\n  use t(a: 'x', b: \"y\")\n"
        );
        assert_eq!(sexp(&once), sexp(src));
        assert_eq!(format(&once).unwrap(), once);
        assert!(format("defines\n  name:\n").is_err());
    }
}
