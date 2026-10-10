//! Formatter for foobar source. Syntax only: it needs the grammar, not the workspace.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, anyhow, ensure};
use topiary_core::{Language, Operation, TopiaryQuery, formatter_str};

use crate::syntax;

/// Formatting rules (a Topiary query) plus the indentation string.
pub struct Formatter {
    language: Language,
}

impl Formatter {
    /// Rules from a Topiary query; `indent` defaults to two spaces.
    pub fn new(query: &str, indent: Option<&str>) -> Result<Self> {
        let grammar = topiary_tree_sitter_facade::Language::from(tree_sitter_foobar::LANGUAGE);
        let formatting_query = TopiaryQuery::new(&grammar, query).map_err(topiary_error)?;
        Ok(Self {
            language: Language {
                name: "foobar".to_string(),
                formatting_query,
                injection_query: None,
                grammar,
                indent: Some(indent.unwrap_or("  ").to_string()),
            },
        })
    }

    /// The query file at `query`, or the rules shipped with the grammar.
    pub fn load(query: Option<&Path>, indent: Option<&str>) -> Result<Self> {
        let Some(path) = query else {
            return Self::new(tree_sitter_foobar::FORMATTING_QUERY, indent);
        };
        let text =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::new(&text, indent).with_context(|| format!("formatting query {}", path.display()))
    }

    /// `source` formatted, or `None` if it has syntax errors (a recovered tree can't be trusted).
    ///
    /// Fails if the rules change more than whitespace, which a custom query can do.
    pub fn format(&self, source: &str) -> Result<Option<String>> {
        // Topiary only rejects ERROR nodes; MISSING ones (`name:` without a value) count here too.
        if syntax::parse(source).root_node().has_error() {
            return Ok(None);
        }
        let operation = Operation::Format {
            skip_idempotence: false,
            tolerate_parsing_errors: false,
            skip_stage: None,
        };
        let mut out = Vec::new();
        formatter_str(source, &mut out, &self.language, operation, None).map_err(topiary_error)?;
        let out = String::from_utf8(out)?;
        ensure!(
            tokens(&out) == tokens(source),
            "the formatting rules changed more than whitespace"
        );
        Ok(Some(out))
    }
}

/// The first line of a Topiary report; the rest are internal source locations and spans.
fn topiary_error(report: impl std::fmt::Display) -> anyhow::Error {
    let text = report.to_string();
    let first = text
        .lines()
        .map(|line| line.trim().trim_start_matches('●').trim())
        .find(|line| !line.is_empty());
    anyhow!("{}", first.unwrap_or("Topiary failed"))
}

/// Kind and text of every token in order: equal iff only whitespace between tokens differs.
fn tokens(source: &str) -> Vec<(u16, &str)> {
    let tree = syntax::parse(source);
    let mut out = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.child_count() == 0 {
            out.push((node.kind_id(), &source[node.byte_range()]));
        }
        let mut cursor = node.walk();
        let children: Vec<_> = node.children(&mut cursor).collect();
        stack.extend(children.into_iter().rev());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only whitespace moves, formatting twice changes nothing, and broken input is refused.
    #[test]
    fn keeps_the_tokens_and_is_idempotent() {
        let fmt = Formatter::load(None, None).unwrap();
        let src = "defines\nname:\"eurorust\"   # note\nprocess  compile it\n     use t( a:'x',b:\"y\" )\n";
        let once = fmt.format(src).unwrap().unwrap();
        assert_eq!(
            once,
            "defines\n  name: \"eurorust\" # note\nprocess compile it\n  use t(a: 'x', b: \"y\")\n"
        );
        assert_eq!(tokens(&once), tokens(src));
        assert_eq!(fmt.format(&once).unwrap().unwrap(), once);
        assert_eq!(fmt.format("defines\n  name:\n").unwrap(), None);
    }

    /// A custom query or indent is used; a query that drops content or doesn't compile is refused.
    #[test]
    fn custom_rules() {
        let src = "defines\n  name: \"x\" # note\n";
        let wide = Formatter::new(tree_sitter_foobar::FORMATTING_QUERY, Some("    ")).unwrap();
        assert_eq!(
            wide.format(src).unwrap().unwrap(),
            "defines\n    name: \"x\" # note\n"
        );
        let deleting = format!(
            "{}\n(comment) @delete\n",
            tree_sitter_foobar::FORMATTING_QUERY
        );
        assert!(
            Formatter::new(&deleting, None)
                .unwrap()
                .format(src)
                .is_err()
        );
        assert!(Formatter::new("(no_such_node) @leaf", None).is_err());
    }
}
