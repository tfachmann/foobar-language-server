//! Hover dispatch on the node under the cursor:
//! - keyword tokens (`defines`, `process`, `template`, `use`, `include`)
//! - variable uses `%(name)` (define or template parameter) and define keys
//! - process headers and process entry keys
//! - template names and `use` calls
//! - include paths

use tower_lsp_server::ls_types::{Hover, HoverContents, HoverParams, MarkupContent, MarkupKind};
use tree_sitter::Node;

use crate::docs::{self, Group};
use crate::document::Document;
use crate::target::{Target, target};
use crate::text::uri_to_path;
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
    match target(node, &doc.rope)? {
        Target::Keyword(word) => docs::hover(Group::Keyword, &word),
        Target::IncludePath(raw) => Some(hover_include_path(workspace, doc, &raw)),
        Target::Param { name, .. } => Some(format!("**{name}** - template parameter")),
        Target::DefineUse(name) => hover_variable_use(workspace, doc, &name),
        Target::DefineKey(name) => hover_defines_key(workspace, doc, &name),
        Target::EntryKey(name) => Some(
            docs::hover(Group::ProcessKey, &name)
                .unwrap_or_else(|| format!("**{name}**\n---\nprocess key (no documentation)")),
        ),
        Target::ProcessName(name) => hover_process_header(doc, &name),
        Target::TemplateName(name) => {
            let template = doc.snapshot.symbols.templates.get(&name)?;
            Some(format!(
                "**template** `{}`\n---\n{}",
                template.signature(&name),
                usage(workspace.count_uses(&name)),
            ))
        }
        Target::TemplateCall(name) => {
            let (file, template) = workspace.lookup_template(&doc.snapshot, &name)?;
            Some(format!(
                "**template** `{}`\n---\n{}",
                template.signature(&name),
                location(doc, &file, template.name_range.start.line + 1),
            ))
        }
    }
}

fn hover_variable_use(workspace: &Workspace, doc: &Document, name: &str) -> Option<String> {
    // A define hides the built-in variable of the same name.
    let Some((file, entry)) = workspace.lookup_define(&doc.snapshot, name) else {
        return docs::hover(Group::Builtin, name);
    };
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

fn hover_defines_key(workspace: &Workspace, doc: &Document, name: &str) -> Option<String> {
    let entry = doc.snapshot.symbols.defines.get(name)?;
    Some(format!(
        "**{name}** = `{value}`  \n---\n{usage}",
        value = entry.value_text,
        usage = usage(workspace.count_refs(name)),
    ))
}

/// `_unused_`, `_used 1 time_`, `_used N times_`.
fn usage(n: usize) -> String {
    match n {
        0 => "_unused_".to_string(),
        1 => "_used 1 time_".to_string(),
        _ => format!("_used {n} times_"),
    }
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

fn hover_include_path(workspace: &Workspace, doc: &Document, raw: &str) -> String {
    let resolved = workspace.resolve(&doc.uri, raw);
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
