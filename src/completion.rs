//! Completion: defines and template parameters after `%(`, template names after `use`, and
//! argument names inside `use name(...)`.

use std::collections::HashSet;

use tower_lsp_server::ls_types::{
    CompletionItem, CompletionItemKind, CompletionParams, CompletionResponse,
};

use crate::document::Document;
use crate::text::{RopeExt, TextPos};
use crate::workspace::Workspace;

/// What is being typed at the cursor.
enum Context {
    /// An identifier after `%(`.
    Variable,
    /// A template name after `use`.
    Template,
    /// An argument name inside `use template(...)`; `given` are the arguments already passed.
    Argument {
        template: String,
        given: Vec<String>,
    },
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
    let context = context(&before).or_else(|| argument_context(&call_before(doc, cursor)?))?;

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
        Context::Argument { template, given } => {
            let (_, def) = workspace.lookup_template(&doc.snapshot, &template)?;
            let missing = def.params.iter().filter(|p| !given.contains(p));
            for (i, param) in missing.enumerate() {
                items.push(CompletionItem {
                    label: param.clone(),
                    kind: Some(CompletionItemKind::PROPERTY),
                    detail: Some(def.signature(&template)),
                    // Declaration order; accepting inserts the colon too.
                    sort_text: Some(format!("{i:04}")),
                    insert_text: Some(format!("{param}: ")),
                    ..CompletionItem::default()
                });
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

/// Text from the start of the `use` line up to the cursor: a call may span lines.
fn call_before(doc: &Document, cursor: TextPos) -> Option<String> {
    let start = (0..=cursor.point.row).rev().find_map(|row| {
        match doc.rope.line(row).to_string().split_whitespace().next() {
            Some("use") => Some(Some(row)),
            Some("defines" | "process" | "template" | "include") => Some(None),
            _ => None,
        }
    })??;
    Some(
        doc.rope
            .slice(doc.rope.line_to_char(start)..cursor.char)
            .to_string(),
    )
}

/// `use name(arg: "...", ` + partial argument name, from a [`call_before`] text.
fn argument_context(call: &str) -> Option<Context> {
    let (template, args) = call.trim_start().strip_prefix("use")?.split_once('(')?;
    let mut given = Vec::new();
    // The current argument's text outside quotes; values may contain `,` and `)`.
    let mut current = String::new();
    let mut quote = None;
    for c in args.chars() {
        match (quote, c) {
            (Some(q), _) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, ')') => return None,
            (None, ',') => {
                given.extend(current.split(':').next().map(|n| n.trim().to_string()));
                current.clear();
            }
            (None, _) => current.push(c),
        }
    }
    // Only a partial name so far: no `:`, not inside a value.
    let partial = current.trim_start();
    (quote.is_none()
        && partial
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_'))
    .then(|| Context::Argument {
        template: template.trim().to_string(),
        given,
    })
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

    /// Arguments already passed (even with `,` in a value, even on an earlier line) are skipped.
    #[test]
    fn arguments_inside_use_call() {
        let template = "template add_note(name, kind)\n  note: \"%(kind): %(name)\"\n\nprocess p\n";
        let first = format!("{template}  use add_note(\n");
        assert_eq!(
            labels(items_at(&first, 4, 15)),
            Some(vec!["name".to_string(), "kind".to_string()])
        );
        let later = format!("{template}  use add_note(kind: 'x, y',\n    \n");
        let items = items_at(&later, 5, 4).unwrap();
        assert_eq!(labels(Some(items.clone())), Some(vec!["name".to_string()]));
        assert_eq!(items[0].insert_text.as_deref(), Some("name: "));
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
