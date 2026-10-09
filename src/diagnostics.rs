//! Per-document diagnostic computation.
//!
//! Pushed to the client after a debounced delay.

use ropey::Rope;
use tower_lsp_server::ls_types::{
    Diagnostic, DiagnosticRelatedInformation, DiagnosticSeverity, DiagnosticTag, Location,
    NumberOrString, Range, Uri,
};
use tree_sitter::Tree;

use crate::document::Document;
use crate::symbol::SymbolIndex;
use crate::text::{RopeExt, path_to_uri, uri_to_path};
use crate::workspace::Workspace;

pub async fn compute(workspace: &Workspace, doc: &Document) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    collect_parse_errors(&doc.tree, &doc.rope, &mut out);
    collect_undefined_variables(workspace, &doc.uri, &doc.symbols, &mut out).await;
    collect_missing_includes(workspace, &doc.uri, &doc.symbols, &mut out);
    collect_cycles(workspace, doc, &mut out).await;
    collect_includes_diags(workspace, doc, &mut out).await;
    collect_unused_defines(workspace, doc, &mut out).await;
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

async fn collect_undefined_variables(
    workspace: &Workspace,
    base_uri: &Uri,
    symbols: &SymbolIndex,
    out: &mut Vec<Diagnostic>,
) {
    for (name, range) in &symbols.var_refs {
        if !workspace.has_define(base_uri, symbols, name).await {
            out.push(make_diag(
                *range,
                DiagnosticSeverity::ERROR,
                "undefined-variable",
                format!("undefined variable: {name}"),
            ));
        }
    }
}

fn collect_missing_includes(
    workspace: &Workspace,
    base_uri: &Uri,
    symbols: &SymbolIndex,
    out: &mut Vec<Diagnostic>,
) {
    for inc in &symbols.includes {
        match workspace.resolve(base_uri, &inc.raw_path) {
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

async fn collect_cycles(workspace: &Workspace, doc: &Document, out: &mut Vec<Diagnostic>) {
    let Some(doc_path) = uri_to_path(&doc.uri).and_then(|p| p.canonicalize().ok()) else {
        return;
    };

    // Walk each include separately so the diagnostic lands on the include that starts the cycle.
    for root in &doc.symbols.includes {
        let cyclic = workspace
            .reachable(&doc.uri, std::slice::from_ref(root))
            .await
            .iter()
            .any(|r| r.path == doc_path);
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

/// Walk doc's direct includes and surface each included file's diagnostics.
async fn collect_includes_diags(workspace: &Workspace, doc: &Document, out: &mut Vec<Diagnostic>) {
    let self_path = uri_to_path(&doc.uri).and_then(|p| p.canonicalize().ok());

    for inc in &doc.symbols.includes {
        let Some(resolved) = workspace.resolve(&doc.uri, &inc.raw_path) else {
            continue;
        };
        if !resolved.exists || self_path.as_deref() == Some(resolved.path.as_path()) {
            continue;
        }

        let mut sub_diags: Vec<Diagnostic> = Vec::new();
        let included_uri = if let Some(open) = workspace.doc_at(&resolved.path) {
            let open = open.read().await;
            collect_parse_errors(&open.tree, &open.rope, &mut sub_diags);
            collect_undefined_variables(workspace, &open.uri, &open.symbols, &mut sub_diags).await;
            collect_missing_includes(workspace, &open.uri, &open.symbols, &mut sub_diags);
            open.uri.clone()
        } else if let Some(file) = workspace.disk.get(&resolved.path).map(|f| f.clone())
            && let Some(uri) = path_to_uri(&resolved.path)
        {
            sub_diags.extend(file.parse_diags.iter().cloned());
            collect_undefined_variables(workspace, &uri, &file.symbols, &mut sub_diags).await;
            collect_missing_includes(workspace, &uri, &file.symbols, &mut sub_diags);
            uri
        } else {
            continue;
        };

        out.extend(
            sub_diags
                .iter()
                .map(|d| wrap_for_parent(&included_uri, &inc.raw_path, inc.path_range, d)),
        );
    }
}

/// Warning for defines that are never referenced anywhere in the workspace.
async fn collect_unused_defines(workspace: &Workspace, doc: &Document, out: &mut Vec<Diagnostic>) {
    for (name, entry) in &doc.symbols.defines {
        if workspace.count_refs(name).await == 0 {
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
    use std::fs;
    use std::str::FromStr;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::sync::RwLock;
    use tower_lsp_server::ls_types::Position;

    fn with_code<'a>(diags: &'a [Diagnostic], code: &str) -> Vec<&'a Diagnostic> {
        let code = NumberOrString::String(code.to_string());
        diags
            .iter()
            .filter(|d| d.code.as_ref() == Some(&code))
            .collect()
    }

    /// Writes `inc.foobar` and `main.foobar` into a temp dir and opens main.
    async fn main_with_include(
        ws: &Workspace,
        inc_text: &str,
        main_text: &str,
    ) -> (TempDir, Document) {
        let dir = TempDir::new().unwrap();
        let main_path = dir.path().join("main.foobar");
        fs::write(dir.path().join("inc.foobar"), inc_text).unwrap();
        fs::write(&main_path, main_text).unwrap();
        let doc = Document::from_text(path_to_uri(&main_path).unwrap(), main_text);
        ws.refresh_includes_for(&doc).await;
        (dir, doc)
    }

    #[tokio::test]
    async fn missing_value_is_a_parse_error() {
        let ws = Workspace::default();
        // tree-sitter recovers from `name:` by inserting a zero-width MISSING value node.
        let doc = Document::from_text(
            Uri::from_str("file:///x.foobar").unwrap(),
            "defines\nname:\n",
        );
        let diags = compute(&ws, &doc).await;
        let missing = with_code(&diags, "missing-token");
        assert_eq!(missing.len(), 1, "got: {diags:?}");
        assert_eq!(missing[0].range.start, Position::new(1, 5));
    }

    #[tokio::test]
    async fn undefined_variable() {
        let ws = Workspace::default();
        let text = "defines\nx: \"1\"\ncmd: \"%(x) %(unknown)\"\n";
        let doc = Document::from_text(Uri::from_str("file:///x.foobar").unwrap(), text);
        let diags = compute(&ws, &doc).await;
        let undefined = with_code(&diags, "undefined-variable");
        assert_eq!(undefined.len(), 1, "got: {diags:?}");
        assert_eq!(undefined[0].message, "undefined variable: unknown");
    }

    #[tokio::test]
    async fn defined_via_include() {
        let ws = Workspace::default();
        let (_dir, doc) = main_with_include(
            &ws,
            "defines\nx: \"1\"\n",
            "include inc.foobar\n\ndefines\ncmd: \"%(x)\"\n",
        )
        .await;
        let diags = compute(&ws, &doc).await;
        assert!(
            with_code(&diags, "undefined-variable").is_empty(),
            "got: {diags:?}"
        );
    }

    #[tokio::test]
    async fn error_in_included_file_is_anchored_at_include() {
        let ws = Workspace::default();
        let (_dir, doc) = main_with_include(
            &ws,
            "defines\nval: \"%(missing)\"\n",
            "# main\ninclude inc.foobar\n",
        )
        .await;
        let diags = compute(&ws, &doc).await;
        let surfaced = with_code(&diags, "included-error");
        assert_eq!(surfaced.len(), 1, "got: {diags:?}");
        assert_eq!(surfaced[0].range.start.line, 1);
        let related = &surfaced[0].related_information.as_ref().unwrap()[0];
        assert!(related.location.uri.as_str().ends_with("inc.foobar"));
        assert_eq!(related.location.range.start.line, 1);
    }

    #[tokio::test]
    async fn cyclic_include_is_reported_in_current_doc() {
        let ws = Workspace::default();
        let (_dir, doc) = main_with_include(
            &ws,
            "include main.foobar\n",
            "# comment\n# another comment\ninclude inc.foobar\n",
        )
        .await;
        let diags = compute(&ws, &doc).await;
        let cycles = with_code(&diags, "cyclic-include");
        assert_eq!(cycles.len(), 1, "got: {diags:?}");
        // The range of `inc.foobar` in main, not of `main.foobar` in inc.
        assert_eq!(
            cycles[0].range,
            Range::new(Position::new(2, 8), Position::new(2, 18))
        );
    }

    #[tokio::test]
    async fn unused_define_is_tagged_unnecessary() {
        let ws = Workspace::default();
        let doc = Document::from_text(
            Uri::from_str("file:///x.foobar").unwrap(),
            "defines\nfoo: \"1\"\nbar: \"%(foo)\"\n",
        );
        // count_refs only sees documents registered in the workspace (did_open does this).
        let uri = doc.uri.clone();
        let doc = Arc::new(RwLock::new(doc));
        ws.docs.insert(uri, doc.clone());
        let diags = compute(&ws, &*doc.read().await).await;

        let unused = with_code(&diags, "unused-define");
        assert_eq!(unused.len(), 1, "got: {diags:?}");
        assert_eq!(unused[0].message, "`bar` is defined but never referenced");
        assert_eq!(unused[0].tags, Some(vec![DiagnosticTag::UNNECESSARY]));
    }
}
