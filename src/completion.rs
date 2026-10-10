//! Completion: defines and template parameters after `%(`, template names after `use`.

use std::collections::HashSet;

use tower_lsp_server::ls_types::{
    CompletionItem, CompletionItemKind, CompletionParams, CompletionResponse,
};

use crate::document::Document;
use crate::text::RopeExt;
use crate::workspace::Workspace;

/// What is being typed at the cursor.
enum Context {
    /// An identifier after `%(`.
    Variable,
    /// A template name after `use`.
    Template,
}

pub fn complete(
    workspace: &Workspace,
    doc: &Document,
    params: &CompletionParams,
) -> Option<CompletionResponse> {
    let pos = params.text_document_position.position;
    let node = doc.node_at(pos)?;
    // `%(` in a raw string or a comment is plain text.
    if matches!(node.kind(), "raw_string" | "comment") {
        return None;
    }
    let cursor = doc.rope.lsp_position_to_text_pos(pos);
    let line_start = doc.rope.line_to_char(cursor.point.row);
    let before = doc.rope.slice(line_start..cursor.char).to_string();
    let context = context(&before)?;

    let mut items: Vec<CompletionItem> = Vec::new();
    let mut seen = HashSet::new();
    // `rank` puts nearer symbols first; the nearest of equal names wins.
    let mut add = |rank: usize, label: &str, kind: CompletionItemKind, detail: String| {
        if seen.insert(label.to_string()) {
            items.push(CompletionItem {
                label: label.to_string(),
                kind: Some(kind),
                detail: Some(detail),
                sort_text: Some(format!("{rank:04}{label}")),
                ..CompletionItem::default()
            });
        }
    };
    let files = workspace.visible(&doc.snapshot);
    match context {
        Context::Variable => {
            // Template parameters shadow defines of the same name.
            let template = std::iter::successors(Some(node), |n| n.parent())
                .find(|n| n.kind() == "section_template")
                .and_then(|t| t.named_child(0))
                .and_then(|name| {
                    let name = doc.rope.node_text(name);
                    doc.snapshot.symbols.templates.get(&name)
                });
            for param in template.iter().flat_map(|t| &t.params) {
                add(
                    0,
                    param,
                    CompletionItemKind::VARIABLE,
                    "template parameter".to_string(),
                );
            }
            for (rank, file) in files.iter().enumerate() {
                for (name, entry) in &file.symbols.defines {
                    let detail = format!("\"{}\"", entry.value_text);
                    add(rank + 1, name, CompletionItemKind::CONSTANT, detail);
                }
            }
        }
        Context::Template => {
            for (rank, file) in files.iter().enumerate() {
                for (name, template) in &file.symbols.templates {
                    let detail = template.signature(name);
                    add(rank, name, CompletionItemKind::FUNCTION, detail);
                }
            }
        }
    }
    Some(CompletionResponse::Array(items))
}

/// What the text between line start and cursor is about to become.
fn context(before: &str) -> Option<Context> {
    // Drop the partially typed identifier; the client filters by it.
    let typed = before.trim_end_matches(|c: char| c.is_ascii_alphanumeric() || c == '_');
    if typed.ends_with("%(") {
        return Some(Context::Variable);
    }
    let rest = typed.trim_start().strip_prefix("use")?;
    (!rest.is_empty() && rest.trim().is_empty()).then_some(Context::Template)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;
    use tower_lsp_server::ls_types::{
        PartialResultParams, Position, TextDocumentIdentifier, TextDocumentPositionParams, Uri,
        WorkDoneProgressParams,
    };

    /// Completion items at `line:character`, in the order an editor shows them.
    fn items_at(text: &str, line: u32, character: u32) -> Option<Vec<CompletionItem>> {
        let doc = Document::from_text(Uri::from_str("file:///x.foobar").unwrap(), text);
        let params = CompletionParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier {
                    uri: doc.uri.clone(),
                },
                position: Position::new(line, character),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
            context: None,
        };
        match complete(&Workspace::default(), &doc, &params)? {
            CompletionResponse::Array(mut items) => {
                items.sort_by(|a, b| a.sort_text.cmp(&b.sort_text));
                Some(items)
            }
            CompletionResponse::List(_) => None,
        }
    }

    fn labels(items: Option<Vec<CompletionItem>>) -> Option<Vec<String>> {
        items.map(|items| items.into_iter().map(|i| i.label).collect())
    }

    /// Right after `%(` the tree only has an ERROR node, yet the template scope is still known.
    #[test]
    fn variables_after_percent_paren() {
        let text = "defines\n  name: \"x\"\ntemplate t(src)\n  cmd: \"%()\"\n  raw: '%()'\n";
        assert_eq!(
            labels(items_at(text, 3, 10)),
            Some(vec!["src".to_string(), "name".to_string()])
        );
        // Raw strings don't interpolate.
        assert_eq!(labels(items_at(text, 4, 10)), None);
    }

    #[test]
    fn templates_after_use() {
        let text = "template compile(src, dst)\n  cmd: \"x\"\n\nprocess p\n  use co\n";
        let items = items_at(text, 4, 8).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "compile");
        assert_eq!(items[0].detail.as_deref(), Some("compile(src, dst)"));
    }
}
