//! Per-document diagnostic computation.
//!
//! Pushed to the client after a debounced delay.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};

use ropey::Rope;
use tower_lsp_server::ls_types::{
    Diagnostic, DiagnosticRelatedInformation, DiagnosticSeverity, DiagnosticTag, Location,
    NumberOrString, Range, Uri,
};
use tree_sitter::Tree;

use crate::document::Document;
use crate::symbol::{IncludeRef, SymbolIndex};
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

    for root in &doc.symbols.includes {
        if reaches_back(workspace, &doc.uri, &doc_path, root).await {
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

/// BFS from a single root include in `doc`.
///
/// Returns true if any path leads back to `doc_path`.
async fn reaches_back(
    workspace: &Workspace,
    doc_uri: &Uri,
    doc_path: &Path,
    root: &IncludeRef,
) -> bool {
    let mut visited: HashSet<PathBuf> = HashSet::new();
    visited.insert(doc_path.to_path_buf());

    let mut queue: VecDeque<(Uri, IncludeRef)> = VecDeque::new();
    queue.push_back((doc_uri.clone(), root.clone()));

    while let Some((base, inc)) = queue.pop_front() {
        let Some(resolved) = workspace.resolve(&base, &inc.raw_path) else {
            continue;
        };
        if !resolved.exists {
            continue;
        }
        if resolved.path == doc_path {
            return true;
        }
        if !visited.insert(resolved.path.clone()) {
            continue;
        }

        // Pull the included file's own includes (open-doc first, else disk cache).
        let next_includes: Vec<IncludeRef> =
            if let Some(open_uri) = workspace.find_open_doc(&resolved.path) {
                if let Some(open_doc) = workspace.docs.get(&open_uri) {
                    let guard = open_doc.read().await;
                    guard.symbols.includes.clone()
                } else {
                    Vec::new()
                }
            } else if let Some(file) = workspace.disk.get(&resolved.path) {
                file.symbols.includes.clone()
            } else {
                Vec::new()
            };

        let next_base = path_to_uri(&resolved.path).unwrap_or(base);
        for nxt in next_includes {
            queue.push_back((next_base.clone(), nxt));
        }
    }
    false
}

/// Walk doc's direct includes and surface each included file's diagnostics.
async fn collect_includes_diags(workspace: &Workspace, doc: &Document, out: &mut Vec<Diagnostic>) {
    let self_path = uri_to_path(&doc.uri).and_then(|p| p.canonicalize().ok());

    for inc in &doc.symbols.includes {
        let Some(resolved) = workspace.resolve(&doc.uri, &inc.raw_path) else {
            continue;
        };
        if !resolved.exists {
            continue;
        }
        if self_path.as_deref() == Some(resolved.path.as_path()) {
            continue;
        }

        let mut sub_diags: Vec<Diagnostic> = Vec::new();
        let included_uri = if let Some(open_uri) = workspace.find_open_doc(&resolved.path) {
            if let Some(open_doc) = workspace.docs.get(&open_uri) {
                let guard = open_doc.read().await;
                collect_parse_errors(&guard.tree, &guard.rope, &mut sub_diags);
                collect_undefined_variables(workspace, &guard.uri, &guard.symbols, &mut sub_diags)
                    .await;
                collect_missing_includes(workspace, &guard.uri, &guard.symbols, &mut sub_diags);
            }
            open_uri
        } else if let Some(file) = workspace.disk.get(&resolved.path) {
            let uri = match path_to_uri(&resolved.path) {
                Some(u) => u,
                None => continue,
            };
            sub_diags.extend(file.parse_diags.iter().cloned());
            collect_undefined_variables(workspace, &uri, &file.symbols, &mut sub_diags).await;
            collect_missing_includes(workspace, &uri, &file.symbols, &mut sub_diags);
            uri
        } else {
            continue;
        };

        for d in sub_diags {
            out.push(wrap_for_parent(
                &included_uri,
                &inc.raw_path,
                inc.path_range,
                &d,
            ));
        }
    }
}

/// Warning for unused defines (over workspace).
async fn collect_unused_defines(workspace: &Workspace, doc: &Document, out: &mut Vec<Diagnostic>) {
    for (name, entry) in &doc.symbols.defines {
        if workspace.count_refs(name).await == 0 {
            out.push(Diagnostic {
                range: entry.key_range,
                severity: Some(DiagnosticSeverity::WARNING),
                code: Some(NumberOrString::String("unused-define".to_string())),
                code_description: None,
                source: Some("foobar".to_string()),
                message: format!("`{name}` is defined but never referenced"),
                related_information: None,
                tags: Some(vec![DiagnosticTag::UNNECESSARY]),
                data: None,
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
    use tower_lsp_server::ls_types::{DidOpenTextDocumentParams, TextDocumentItem};

    fn open_doc(uri: Uri, text: &str) -> Document {
        Document::open(DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri,
                language_id: "foobar".to_string(),
                version: 1,
                text: text.to_string(),
            },
        })
        .unwrap()
    }

    fn code_of(d: &Diagnostic) -> &str {
        match d.code.as_ref() {
            Some(NumberOrString::String(s)) => s.as_str(),
            _ => "",
        }
    }

    #[tokio::test]
    async fn empty_doc_no_diagnostics() {
        let ws = Workspace::new();
        let doc = open_doc(Uri::from_str("file:///x.foobar").unwrap(), "");
        let diags = compute(&ws, &doc).await;
        assert!(diags.is_empty(), "got: {diags:?}");
    }

    #[tokio::test]
    async fn parse_error_emits_diagnostic() {
        let ws = Workspace::new();
        // `name:` with no value -> parse error / missing.
        let doc = open_doc(
            Uri::from_str("file:///x.foobar").unwrap(),
            "defines\nname:\n",
        );
        let diags = compute(&ws, &doc).await;
        assert!(
            diags
                .iter()
                .any(|d| matches!(code_of(d), "parse-error" | "missing-token")),
            "got: {diags:?}"
        );
    }

    #[tokio::test]
    async fn undefined_variable_local_only() {
        let ws = Workspace::new();
        let doc = open_doc(
            Uri::from_str("file:///x.foobar").unwrap(),
            "defines\ncmd: \"%(unknown)\"\n",
        );
        let diags = compute(&ws, &doc).await;
        let undefined: Vec<_> = diags
            .iter()
            .filter(|d| code_of(d) == "undefined-variable")
            .collect();
        assert_eq!(undefined.len(), 1, "got: {diags:?}");
        assert!(undefined[0].message.contains("unknown"));
    }

    #[tokio::test]
    async fn defined_locally_no_diagnostic() {
        let ws = Workspace::new();
        let doc = open_doc(
            Uri::from_str("file:///x.foobar").unwrap(),
            "defines\nx: \"1\"\ncmd: \"%(x)\"\n",
        );
        let diags = compute(&ws, &doc).await;
        assert!(
            !diags.iter().any(|d| code_of(d) == "undefined-variable"),
            "got: {diags:?}"
        );
    }

    #[tokio::test]
    async fn defined_via_include_no_diagnostic() {
        let dir = TempDir::new().unwrap();
        let inc_path = dir.path().join("inc.foobar");
        let main_path = dir.path().join("main.foobar");
        fs::write(&inc_path, "defines\nx: \"1\"\n").unwrap();
        let main_text = "include inc.foobar\n\ndefines\ncmd: \"%(x)\"\n";
        fs::write(&main_path, main_text).unwrap();
        let main_uri = path_to_uri(&main_path).unwrap();

        let ws = Workspace::new();
        let main_doc = open_doc(main_uri.clone(), main_text);
        ws.refresh_includes_for(&main_doc).await;
        let diags = compute(&ws, &main_doc).await;
        assert!(
            !diags.iter().any(|d| code_of(d) == "undefined-variable"),
            "got: {diags:?}"
        );
    }

    #[tokio::test]
    async fn missing_include_diagnostic() {
        let dir = TempDir::new().unwrap();
        let main_path = dir.path().join("main.foobar");
        fs::write(&main_path, "include nope.foobar\n").unwrap();
        let main_uri = path_to_uri(&main_path).unwrap();

        let ws = Workspace::new();
        let main_doc = open_doc(main_uri, "include nope.foobar\n");
        let diags = compute(&ws, &main_doc).await;
        assert!(
            diags.iter().any(|d| code_of(d) == "missing-include"),
            "got: {diags:?}"
        );
    }

    #[tokio::test]
    async fn cyclic_include_diagnostic() {
        let dir = TempDir::new().unwrap();
        let a_path = dir.path().join("a.foobar");
        let b_path = dir.path().join("b.foobar");
        fs::write(&a_path, "include b.foobar\n").unwrap();
        fs::write(&b_path, "include a.foobar\n").unwrap();
        let a_uri = path_to_uri(&a_path).unwrap();

        let ws = Workspace::new();
        let a_doc = open_doc(a_uri.clone(), &fs::read_to_string(&a_path).unwrap());
        // Need workspace.docs to include `a` so cycle walk can find b's includes via disk cache
        ws.docs.insert(
            a_uri.clone(),
            Arc::new(RwLock::new(open_doc(
                a_uri,
                &fs::read_to_string(&a_path).unwrap(),
            ))),
        );
        ws.refresh_includes_for(&a_doc).await;
        let diags = compute(&ws, &a_doc).await;
        let cycles: Vec<_> = diags
            .iter()
            .filter(|d| code_of(d) == "cyclic-include")
            .collect();
        assert_eq!(cycles.len(), 1, "got: {diags:?}");
        assert_eq!(cycles[0].severity, Some(DiagnosticSeverity::WARNING));
    }

    #[tokio::test]
    async fn cyclic_include_range_is_in_current_doc() {
        let dir = TempDir::new().unwrap();
        let a_path = dir.path().join("a.foobar");
        let b_path = dir.path().join("b.foobar");
        let a_text = "# comment\n# another comment\ninclude b.foobar\n";
        let b_text = "include a.foobar\n";
        fs::write(&a_path, a_text).unwrap();
        fs::write(&b_path, b_text).unwrap();
        let a_uri = path_to_uri(&a_path).unwrap();

        let ws = Workspace::new();
        let a_doc = open_doc(a_uri.clone(), a_text);
        ws.docs.insert(
            a_uri.clone(),
            Arc::new(RwLock::new(open_doc(a_uri, a_text))),
        );
        ws.refresh_includes_for(&a_doc).await;
        let diags = compute(&ws, &a_doc).await;
        let cycles: Vec<_> = diags
            .iter()
            .filter(|d| code_of(d) == "cyclic-include")
            .collect();
        assert_eq!(cycles.len(), 1, "got: {diags:?}");
        assert_eq!(cycles[0].range.start.line, 2, "got: {:?}", cycles[0].range);
        assert_eq!(cycles[0].range.start.character, 8);
        assert_eq!(cycles[0].range.end.line, 2);
        assert_eq!(cycles[0].range.end.character, 16);
        assert!(cycles[0].message.contains("b.foobar"));
    }

    #[tokio::test]
    async fn parse_error_in_included_file_surfaces() {
        let dir = TempDir::new().unwrap();
        let inc_path = dir.path().join("inc.foobar");
        let main_path = dir.path().join("main.foobar");
        // `name:` with no value - parse error / missing token in inc.
        fs::write(&inc_path, "defines\nname:\n").unwrap();
        let main_text = "include inc.foobar\n";
        fs::write(&main_path, main_text).unwrap();
        let main_uri = path_to_uri(&main_path).unwrap();

        let ws = Workspace::new();
        let main_doc = open_doc(main_uri.clone(), main_text);
        ws.refresh_includes_for(&main_doc).await;
        let diags = compute(&ws, &main_doc).await;

        let included: Vec<_> = diags
            .iter()
            .filter(|d| code_of(d) == "included-error")
            .collect();
        assert!(!included.is_empty(), "got: {diags:?}");
        // Diag anchored at main's include statement (line 0).
        assert_eq!(included[0].range.start.line, 0);
        // related_information points to inc.foobar at its own error location.
        let rel = included[0].related_information.as_ref().unwrap();
        let inc_canon = inc_path.canonicalize().unwrap();
        assert_eq!(
            uri_to_path(&rel[0].location.uri)
                .and_then(|p| p.canonicalize().ok())
                .as_deref(),
            Some(inc_canon.as_path())
        );
    }

    #[tokio::test]
    async fn undefined_variable_in_included_file_surfaces() {
        let dir = TempDir::new().unwrap();
        let inc_path = dir.path().join("inc.foobar");
        let main_path = dir.path().join("main.foobar");
        // inc.foobar uses %(missing) but missing is defined nowhere.
        fs::write(&inc_path, "defines\nval: \"%(missing)\"\n").unwrap();
        let main_text = "include inc.foobar\n";
        fs::write(&main_path, main_text).unwrap();
        let main_uri = path_to_uri(&main_path).unwrap();

        let ws = Workspace::new();
        let main_doc = open_doc(main_uri.clone(), main_text);
        ws.refresh_includes_for(&main_doc).await;
        let diags = compute(&ws, &main_doc).await;

        let surfaced: Vec<_> = diags
            .iter()
            .filter(|d| code_of(d) == "included-error" && d.message.contains("missing"))
            .collect();
        assert_eq!(surfaced.len(), 1, "got: {diags:?}");
        assert_eq!(surfaced[0].range.start.line, 0);
        let rel = surfaced[0].related_information.as_ref().unwrap();
        assert!(rel[0].message.contains("missing"));
    }

    #[tokio::test]
    async fn surfaced_diag_uses_open_doc_when_available() {
        let dir = TempDir::new().unwrap();
        let inc_path = dir.path().join("inc.foobar");
        let main_path = dir.path().join("main.foobar");
        fs::write(&inc_path, "defines\nname: \"ok\"\n").unwrap(); // clean on disk
        let main_text = "include inc.foobar\n";
        fs::write(&main_path, main_text).unwrap();
        let main_uri = path_to_uri(&main_path).unwrap();
        let inc_uri = path_to_uri(&inc_path).unwrap();

        let ws = Workspace::new();
        // Open inc with a buffer that has a parse error.
        ws.docs.insert(
            inc_uri.clone(),
            Arc::new(RwLock::new(open_doc(inc_uri, "defines\nname:\n"))),
        );
        let main_doc = open_doc(main_uri.clone(), main_text);
        ws.docs.insert(
            main_uri.clone(),
            Arc::new(RwLock::new(open_doc(main_uri, main_text))),
        );
        ws.refresh_includes_for(&main_doc).await;
        let diags = compute(&ws, &main_doc).await;
        assert!(
            diags.iter().any(|d| code_of(d) == "included-error"),
            "got: {diags:?}"
        );
    }

    #[tokio::test]
    async fn unused_define_emits_warning_with_unnecessary_tag() {
        use tower_lsp_server::ls_types::DiagnosticTag;
        let uri = Uri::from_str("file:///x.foobar").unwrap();
        let text = "defines\nfoo: \"1\"\nbar: \"%(foo)\"\n";
        let doc = open_doc(uri.clone(), text);
        let ws = Workspace::new();
        // Insert into workspace.docs so count_refs sees the local refs.
        ws.docs
            .insert(uri.clone(), Arc::new(RwLock::new(open_doc(uri, text))));
        let diags = compute(&ws, &doc).await;
        let unused: Vec<_> = diags
            .iter()
            .filter(|d| code_of(d) == "unused-define")
            .collect();
        // `foo` is used once (in bar); `bar` is unused.
        assert_eq!(unused.len(), 1, "got: {diags:?}");
        assert_eq!(unused[0].severity, Some(DiagnosticSeverity::WARNING));
        assert_eq!(
            unused[0].tags.as_deref(),
            Some(&[DiagnosticTag::UNNECESSARY][..]),
        );
        assert!(
            unused[0].message.contains("bar"),
            "got: {}",
            unused[0].message
        );
    }

    #[tokio::test]
    async fn unused_define_silenced_by_cross_file_ref() {
        let dir = TempDir::new().unwrap();
        let inc_path = dir.path().join("inc.foobar");
        let main_path = dir.path().join("main.foobar");
        fs::write(&inc_path, "defines\nfile_name: \"slides\"\n").unwrap();
        let main_text = "include inc.foobar\n\ndefines\nout: \"%(file_name).pdf\"\n";
        fs::write(&main_path, main_text).unwrap();
        let inc_uri = path_to_uri(&inc_path).unwrap();

        let ws = Workspace::new();
        let inc_doc = open_doc(inc_uri.clone(), &fs::read_to_string(&inc_path).unwrap());
        ws.docs.insert(
            inc_uri.clone(),
            Arc::new(RwLock::new(open_doc(
                inc_uri,
                &fs::read_to_string(&inc_path).unwrap(),
            ))),
        );
        ws.set_roots(vec![dir.path().to_path_buf()]);
        ws.scan_workspace();

        let diags = compute(&ws, &inc_doc).await;
        let unused: Vec<_> = diags
            .iter()
            .filter(|d| code_of(d) == "unused-define")
            .collect();
        assert!(
            unused.iter().all(|d| !d.message.contains("file_name")),
            "file_name flagged as unused despite cross-file ref: {unused:?}"
        );
    }
}
