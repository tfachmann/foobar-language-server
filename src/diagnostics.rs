//! Per-document diagnostic computation.
//!
//! Pushed to the client after a debounced delay.

use std::sync::Arc;

use ropey::Rope;
use tower_lsp_server::ls_types::{
    Diagnostic, DiagnosticRelatedInformation, DiagnosticSeverity, DiagnosticTag, Location,
    NumberOrString, Range, Uri,
};
use tree_sitter::Tree;

use crate::text::RopeExt;
use crate::workspace::{FileSnapshot, Workspace, uri_key};

/// The file's own diagnostics: what `foobar lint` reports for each file.
pub fn compute(workspace: &Workspace, file: &Arc<FileSnapshot>) -> Vec<Diagnostic> {
    let mut out = file.parse_diags.clone();
    collect_symbol_diags(workspace, file, &mut out);
    collect_cycles(workspace, file, &mut out);
    collect_unused_defines(workspace, file, &mut out);
    out
}

pub(crate) fn collect_parse_errors(tree: &Tree, rope: &Rope, out: &mut Vec<Diagnostic>) {
    let root = tree.root_node();
    if !root.has_error() {
        return;
    }
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.is_error() {
            out.push(make_diag(
                rope.node_range(node),
                DiagnosticSeverity::ERROR,
                "parse-error",
                "syntax error".to_string(),
            ));
        } else if node.is_missing() {
            out.push(make_diag(
                rope.node_range(node),
                DiagnosticSeverity::ERROR,
                "missing-token",
                format!("missing {}", node.kind()),
            ));
        }
        if node.has_error() {
            let mut walker = node.walk();
            for child in node.children(&mut walker) {
                stack.push(child);
            }
        }
    }
}

/// Checks that need only a file's symbol index; shared with included files.
fn collect_symbol_diags(
    workspace: &Workspace,
    file: &Arc<FileSnapshot>,
    out: &mut Vec<Diagnostic>,
) {
    collect_undefined_variables(workspace, file, out);
    collect_missing_includes(workspace, file, out);
    collect_template_uses(workspace, file, out);
}

fn collect_undefined_variables(
    workspace: &Workspace,
    file: &Arc<FileSnapshot>,
    out: &mut Vec<Diagnostic>,
) {
    for (name, range) in &file.symbols.var_refs {
        if !workspace.has_define(file, name) {
            out.push(make_diag(
                *range,
                DiagnosticSeverity::ERROR,
                "undefined-variable",
                format!("undefined variable: {name}"),
            ));
        }
    }
}

/// Every `use` must name a known template and pass exactly its parameters.
fn collect_template_uses(
    workspace: &Workspace,
    file: &Arc<FileSnapshot>,
    out: &mut Vec<Diagnostic>,
) {
    for u in &file.symbols.uses {
        let Some((_, template)) = workspace.lookup_template(file, &u.name) else {
            out.push(make_diag(
                u.name_range,
                DiagnosticSeverity::ERROR,
                "unknown-template",
                format!("unknown template: {}", u.name),
            ));
            continue;
        };
        for (arg, range) in &u.args {
            if !template.params.contains(arg) {
                out.push(make_diag(
                    *range,
                    DiagnosticSeverity::ERROR,
                    "unknown-argument",
                    format!("template `{}` has no parameter `{arg}`", u.name),
                ));
            }
        }
        for param in &template.params {
            if !u.args.iter().any(|(arg, _)| arg == param) {
                out.push(make_diag(
                    u.name_range,
                    DiagnosticSeverity::ERROR,
                    "missing-argument",
                    format!("missing argument `{param}` for template `{}`", u.name),
                ));
            }
        }
    }
}

fn collect_missing_includes(workspace: &Workspace, file: &FileSnapshot, out: &mut Vec<Diagnostic>) {
    for inc in &file.symbols.includes {
        match workspace.resolve(&file.uri, &inc.raw_path) {
            None => out.push(make_diag(
                inc.path_range,
                DiagnosticSeverity::ERROR,
                "unresolvable-include",
                format!("cannot resolve include path: {}", inc.raw_path),
            )),
            Some(r) if !r.exists => out.push(make_diag(
                inc.path_range,
                DiagnosticSeverity::ERROR,
                "missing-include",
                format!("include file not found: {}", r.path.display()),
            )),
            _ => {}
        }
    }
}

fn collect_cycles(workspace: &Workspace, file: &FileSnapshot, out: &mut Vec<Diagnostic>) {
    let Some(path) = uri_key(&file.uri) else {
        return;
    };

    // Walk each include separately so the diagnostic lands on the include that starts the cycle.
    for root in &file.symbols.includes {
        let cyclic = workspace
            .reachable(&file.uri, std::slice::from_ref(root))
            .iter()
            .any(|r| r.path == path);
        if cyclic {
            out.push(make_diag(
                root.path_range,
                DiagnosticSeverity::WARNING,
                "cyclic-include",
                format!(
                    "cyclic include: `{}` refers back to this file",
                    root.raw_path
                ),
            ));
        }
    }
}

/// Errors inside the file's direct includes, anchored at each `include` line.
pub fn surface_includes(workspace: &Workspace, file: &FileSnapshot) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let self_path = uri_key(&file.uri);

    for inc in &file.symbols.includes {
        let Some(resolved) = workspace.resolve(&file.uri, &inc.raw_path) else {
            continue;
        };
        if !resolved.exists || self_path.as_ref() == Some(&resolved.path) {
            continue;
        }
        let Some(included) = workspace.file(&resolved.path) else {
            continue;
        };

        let mut sub_diags = included.parse_diags.clone();
        collect_symbol_diags(workspace, &included, &mut sub_diags);
        out.extend(
            sub_diags
                .iter()
                .map(|d| wrap_for_parent(&included.uri, &inc.raw_path, inc.path_range, d)),
        );
    }
    out
}

/// Warning for defines that are never referenced anywhere in the workspace.
fn collect_unused_defines(workspace: &Workspace, file: &FileSnapshot, out: &mut Vec<Diagnostic>) {
    for (name, entry) in &file.symbols.defines {
        if workspace.count_refs(name) == 0 {
            out.push(Diagnostic {
                // Editors render UNNECESSARY as grayed-out text.
                tags: Some(vec![DiagnosticTag::UNNECESSARY]),
                ..make_diag(
                    entry.key_range,
                    DiagnosticSeverity::WARNING,
                    "unused-define",
                    format!("`{name}` is defined but never referenced"),
                )
            });
        }
    }
}

fn wrap_for_parent(
    included_uri: &Uri,
    raw_path: &str,
    parent_range: Range,
    d: &Diagnostic,
) -> Diagnostic {
    Diagnostic {
        range: parent_range,
        severity: d.severity,
        code: Some(NumberOrString::String("included-error".to_string())),
        code_description: None,
        source: Some("foobar".to_string()),
        message: format!("error in included file `{}`: {}", raw_path, d.message),
        related_information: Some(vec![DiagnosticRelatedInformation {
            location: Location {
                uri: included_uri.clone(),
                range: d.range,
            },
            message: d.message.clone(),
        }]),
        tags: None,
        data: None,
    }
}

fn make_diag(
    range: Range,
    severity: DiagnosticSeverity,
    code: &str,
    message: String,
) -> Diagnostic {
    Diagnostic {
        range,
        severity: Some(severity),
        code: Some(NumberOrString::String(code.to_string())),
        code_description: None,
        source: Some("foobar".to_string()),
        message,
        related_information: None,
        tags: None,
        data: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::Document;
    use crate::text::path_to_uri;
    use std::fs;
    use std::str::FromStr;
    use tempfile::TempDir;
    use tower_lsp_server::ls_types::Position;

    fn with_code<'a>(diags: &'a [Diagnostic], code: &str) -> Vec<&'a Diagnostic> {
        let code = NumberOrString::String(code.to_string());
        diags
            .iter()
            .filter(|d| d.code.as_ref() == Some(&code))
            .collect()
    }

    /// Writes `inc.foobar` and `main.foobar` into a temp dir and opens main.
    fn main_with_include(ws: &Workspace, inc_text: &str, main_text: &str) -> (TempDir, Document) {
        let dir = TempDir::new().unwrap();
        let main_path = dir.path().join("main.foobar");
        fs::write(dir.path().join("inc.foobar"), inc_text).unwrap();
        fs::write(&main_path, main_text).unwrap();
        let doc = Document::from_text(path_to_uri(&main_path).unwrap(), main_text);
        ws.refresh_includes_for(&doc.snapshot);
        (dir, doc)
    }

    #[test]
    fn missing_value_is_a_parse_error() {
        let ws = Workspace::default();
        // tree-sitter recovers from `name:` by inserting a zero-width MISSING value node.
        let doc = Document::from_text(
            Uri::from_str("file:///x.foobar").unwrap(),
            "defines\nname:\n",
        );
        let diags = compute(&ws, &doc.snapshot);
        let missing = with_code(&diags, "missing-token");
        assert_eq!(missing.len(), 1, "got: {diags:?}");
        assert_eq!(missing[0].range.start, Position::new(1, 5));
    }

    #[test]
    fn undefined_variable() {
        let ws = Workspace::default();
        let text = "defines\nx: \"1\"\ncmd: \"%(x) %(unknown)\"\n";
        let doc = Document::from_text(Uri::from_str("file:///x.foobar").unwrap(), text);
        let diags = compute(&ws, &doc.snapshot);
        let undefined = with_code(&diags, "undefined-variable");
        assert_eq!(undefined.len(), 1, "got: {diags:?}");
        assert_eq!(undefined[0].message, "undefined variable: unknown");
    }

    #[test]
    fn defined_via_include() {
        let ws = Workspace::default();
        let (_dir, doc) = main_with_include(
            &ws,
            "defines\nx: \"1\"\n",
            "include inc.foobar\n\ndefines\ncmd: \"%(x)\"\n",
        );
        let diags = compute(&ws, &doc.snapshot);
        assert!(
            with_code(&diags, "undefined-variable").is_empty(),
            "got: {diags:?}"
        );
    }

    #[test]
    fn error_in_included_file_is_anchored_at_include() {
        let ws = Workspace::default();
        let (_dir, doc) = main_with_include(
            &ws,
            "defines\nval: \"%(missing)\"\n",
            "# main\ninclude inc.foobar\n",
        );
        let diags = surface_includes(&ws, &doc.snapshot);
        let surfaced = with_code(&diags, "included-error");
        assert_eq!(surfaced.len(), 1, "got: {diags:?}");
        assert_eq!(surfaced[0].range.start.line, 1);
        let related = &surfaced[0].related_information.as_ref().unwrap()[0];
        assert!(related.location.uri.as_str().ends_with("inc.foobar"));
        assert_eq!(related.location.range.start.line, 1);
    }

    #[test]
    fn cyclic_include_is_reported_in_current_doc() {
        let ws = Workspace::default();
        let (_dir, doc) = main_with_include(
            &ws,
            "include main.foobar\n",
            "# comment\n# another comment\ninclude inc.foobar\n",
        );
        let diags = compute(&ws, &doc.snapshot);
        let cycles = with_code(&diags, "cyclic-include");
        assert_eq!(cycles.len(), 1, "got: {diags:?}");
        // The range of `inc.foobar` in main, not of `main.foobar` in inc.
        assert_eq!(
            cycles[0].range,
            Range::new(Position::new(2, 8), Position::new(2, 18))
        );
    }

    #[test]
    fn unused_define_is_tagged_unnecessary() {
        let ws = Workspace::default();
        let doc = Document::from_text(
            Uri::from_str("file:///x.foobar").unwrap(),
            "defines\nfoo: \"1\"\nbar: \"%(foo)\"\n",
        );
        // count_refs only sees published files (did_open does this).
        ws.publish(&doc);
        let diags = compute(&ws, &doc.snapshot);

        let unused = with_code(&diags, "unused-define");
        assert_eq!(unused.len(), 1, "got: {diags:?}");
        assert_eq!(unused[0].message, "`bar` is defined but never referenced");
        assert_eq!(unused[0].tags, Some(vec![DiagnosticTag::UNNECESSARY]));
    }

    #[test]
    fn template_uses_are_checked() {
        let ws = Workspace::default();
        let text = "template t(a, b)\n  cmd: \"%(a) %(b)\"\n\n\
                    process p\n  use t(a: '1', c: '2')\n  use nope()\n";
        let doc = Document::from_text(Uri::from_str("file:///x.foobar").unwrap(), text);
        let diags = compute(&ws, &doc.snapshot);

        // `%(a)` and `%(b)` are parameters, not undefined variables.
        assert!(
            with_code(&diags, "undefined-variable").is_empty(),
            "got: {diags:?}"
        );
        let unknown_arg = with_code(&diags, "unknown-argument");
        assert_eq!(unknown_arg.len(), 1, "got: {diags:?}");
        assert_eq!(unknown_arg[0].message, "template `t` has no parameter `c`");
        let missing = with_code(&diags, "missing-argument");
        assert_eq!(missing.len(), 1, "got: {diags:?}");
        assert_eq!(missing[0].message, "missing argument `b` for template `t`");
        assert_eq!(
            with_code(&diags, "unknown-template").len(),
            1,
            "got: {diags:?}"
        );
    }

    /// A valid template example (the one shown in the talk) produces no diagnostics.
    #[test]
    fn template_example_is_clean() {
        let text = r#"defines
  name: "eurorust"
  program_compile: "typst"

template compile(src, dst)
  command: "%(program_compile) compile %(src) %(dst)"

process compile presentation
  use compile(src: "%(name).typ", dst: "%(name).pdf")
  cwd: 'slides'
"#;
        let ws = Workspace::default();
        let doc = Document::from_text(Uri::from_str("file:///x.foobar").unwrap(), text);
        // count_refs only sees published files (did_open does this).
        ws.publish(&doc);
        let diags = compute(&ws, &doc.snapshot);
        assert!(diags.is_empty(), "got: {diags:?}");
    }
}
