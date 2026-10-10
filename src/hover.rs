//! Hover dispatch on the node under the cursor:
//! - keyword tokens (`defines`, `process`, `template`, `use`, `include`)
//! - variable uses `%(name)` (define or template parameter) and define keys
//! - process headers and process entry keys
//! - template names and `use` calls
//! - include paths

use tower_lsp_server::ls_types::{Hover, HoverContents, HoverParams, MarkupContent, MarkupKind};
use tree_sitter::Node;

use crate::document::Document;
use crate::keywords::{keyword_doc, process_entry_key_doc};
use crate::symbol::{TemplateDef, template_param};
use crate::text::{RopeExt, uri_to_path};
use crate::workspace::{FileSnapshot, Origin, Workspace};

pub fn hover(workspace: &Workspace, doc: &Document, params: &HoverParams) -> Option<Hover> {
    let pos = params.text_document_position_params.position;
    let node = doc.node_at(pos)?;
    let md = render(workspace, doc, node)?;
    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: md,
        }),
        range: None,
    })
}

fn render(workspace: &Workspace, doc: &Document, node: Node<'_>) -> Option<String> {
    // Case 1: keyword tokens. Anonymous, parent is the section node.
    if let Some(parent) = node.parent()
        && matches!(
            parent.kind(),
            "section_defines" | "section_process" | "section_template" | "use" | "include"
        )
        && !node.is_named()
        && let Some(doc_md) = keyword_doc(&doc.rope.node_text(node))
    {
        return Some(doc_md.to_string());
    }

    // Case 2: hover on the (path) node of an include.
    if node.kind() == "path" && node.parent().is_some_and(|p| p.kind() == "include") {
        return Some(hover_include_path(workspace, doc, node));
    }

    match (node.kind(), node.parent().map(|p| p.kind())) {
        // Case 3: identifier inside a variable use, e.g. %(file_name).
        ("identifier", Some("variable")) => {
            let name = doc.rope.node_text(node);
            if template_param(node, &doc.rope).is_some() {
                return Some(format!("**{name}** - template parameter"));
            }
            hover_variable_use(workspace, doc, &name)
        }
        // Case 4 / 5: identifier as an entry key.
        ("identifier", Some("entry")) => {
            let name = doc.rope.node_text(node);
            let in_defines = node
                .parent()
                .and_then(|e| e.parent())
                .map(|s| s.kind() == "section_defines")
                .unwrap_or(false);
            if in_defines {
                hover_defines_key(workspace, doc, &name)
            } else {
                hover_process_entry_key(&name)
            }
        }
        // Case 6: process header name.
        ("name", Some("section_process")) => {
            let name = doc.rope.node_text(node).trim().to_string();
            hover_process_header(doc, &name)
        }
        // Case 7: template name in its definition.
        ("identifier", Some("section_template")) => {
            let name = doc.rope.node_text(node);
            let template = doc.snapshot.symbols.templates.get(&name)?;
            Some(format!("**template** `{}`", signature(&name, template)))
        }
        // Case 8: template name in a `use` call.
        ("identifier", Some("use")) => {
            let name = doc.rope.node_text(node);
            let (file, template) = workspace.lookup_template(&doc.snapshot, &name)?;
            Some(format!(
                "**template** `{}`\n---\n{}",
                signature(&name, &template),
                location(doc, &file, template.name_range.start.line + 1),
            ))
        }
        _ => None,
    }
}

fn hover_variable_use(workspace: &Workspace, doc: &Document, name: &str) -> Option<String> {
    let (file, entry) = workspace.lookup_define(&doc.snapshot, name)?;
    Some(format!(
        "**{name}** = `{value}`\n---\n{location}",
        value = entry.value_text,
        location = location(doc, &file, entry.key_range.start.line + 1),
    ))
}

/// "defined at line N [in `file`]" for a symbol found in `file`.
fn location(doc: &Document, file: &FileSnapshot, line: u32) -> String {
    if file.uri == doc.uri {
        return format!("defined at line {line}");
    }
    // Decoded path, so `my file.foobar` does not show as `my%20file.foobar`.
    let path = uri_to_path(&file.uri).unwrap_or_default();
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let open = if file.origin == Origin::Open {
        " (open)"
    } else {
        ""
    };
    format!("defined at line {line} in `{name}`{open}")
}

fn signature(name: &str, template: &TemplateDef) -> String {
    format!("{name}({})", template.params.join(", "))
}

fn hover_defines_key(workspace: &Workspace, doc: &Document, name: &str) -> Option<String> {
    let entry = doc.snapshot.symbols.defines.get(name)?;
    let n = workspace.count_refs(name);
    let usage = match n {
        0 => "_unused_".to_string(),
        1 => "_used 1 time_".to_string(),
        _ => format!("_used {n} times_"),
    };
    Some(format!(
        "**{name}** = `{value}`  \n---\n{usage}",
        value = entry.value_text,
    ))
}

fn hover_process_header(doc: &Document, name: &str) -> Option<String> {
    let p = doc
        .snapshot
        .symbols
        .processes
        .iter()
        .find(|p| p.name == name)?;
    Some(format!(
        "**process** `{name}` ({n} entr{plural})",
        name = name,
        n = p.entry_count,
        plural = if p.entry_count == 1 { "y" } else { "ies" },
    ))
}

fn hover_process_entry_key(name: &str) -> Option<String> {
    process_entry_key_doc(name)
        .map(|s| s.to_string())
        .or_else(|| Some(format!("**{name}** - process entry key (no documentation)")))
}

fn hover_include_path(workspace: &Workspace, doc: &Document, node: Node<'_>) -> String {
    let raw = doc.rope.node_text(node).trim().to_string();
    let resolved = workspace.resolve(&doc.uri, &raw);
    let mut body = format!("**include** `{raw}`\n---\n");
    match resolved {
        Some(r) => {
            body.push_str(&format!("Resolved to `{}`\n---\n", r.path.display()));
            if !r.exists {
                body.push_str("_file not found_");
            } else if let Some(n) = workspace.defines_count_at(&r.path) {
                body.push_str(&format!("_{n} define{}_", if n == 1 { "" } else { "s" }));
            } else {
                body.push_str("_file found_");
            }
        }
        None => {
            body.push_str("_could not resolve path (base URI is not file://)_");
        }
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::str::FromStr;
    use tempfile::TempDir;
    use tower_lsp_server::ls_types::{
        Position, TextDocumentIdentifier, TextDocumentPositionParams, Uri, WorkDoneProgressParams,
    };

    use crate::text::path_to_uri;

    fn hover_at(ws: &Workspace, doc: &Document, line: u32, character: u32) -> Option<String> {
        let params = HoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier {
                    uri: doc.uri.clone(),
                },
                position: Position::new(line, character),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        };
        hover(ws, doc, &params).map(|h| match h.contents {
            HoverContents::Markup(m) => m.value,
            _ => String::new(),
        })
    }

    const SRC: &str = "defines\nfile_name: \"slides\"\n\nprocess compile presentation\ncommand: \"%(file_name).typ\"\n";

    #[test]
    fn hover_on_keyword() {
        let doc = Document::from_text(Uri::from_str("file:///x.foobar").unwrap(), SRC);
        let s = hover_at(&Workspace::default(), &doc, 3, 2).unwrap();
        assert!(s.starts_with("**process**"), "got: {s}");
    }

    #[test]
    fn hover_on_variable_use() {
        let doc = Document::from_text(Uri::from_str("file:///x.foobar").unwrap(), SRC);
        // Inside `file_name` of `%(file_name)`.
        let s = hover_at(&Workspace::default(), &doc, 4, 14).unwrap();
        assert_eq!(s, "**file_name** = `slides`\n---\ndefined at line 2");
    }

    #[test]
    fn hover_on_include_path() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("inc.foobar"),
            "defines\na: \"1\"\nb: \"2\"\n",
        )
        .unwrap();
        let main_uri = path_to_uri(&dir.path().join("main.foobar")).unwrap();
        let doc = Document::from_text(main_uri, "include inc.foobar\n");
        let ws = Workspace::default();
        ws.refresh_includes_for(&doc.snapshot);

        let s = hover_at(&ws, &doc, 0, 10).unwrap();
        assert!(s.contains("_2 defines_"), "got: {s}");
    }
}
