//! Completion: defines and template parameters after `%(`, template names after `use`, argument
//! names inside `use name(...)`, and process keys at the start of a process or template entry.

use std::collections::HashSet;

use tower_lsp_server::ls_types::{
    CompletionItem, CompletionItemKind, CompletionItemLabelDetails, CompletionParams,
    CompletionResponse, Documentation, MarkupContent, MarkupKind,
};
use tree_sitter_foobar::{keyword, kind};

use crate::docs::{self, Group};
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
    /// A key at the start of a process or template entry; `use` calls only fit in processes.
    ProcessKey { in_process: bool },
}

pub fn complete(
    workspace: &Workspace,
    doc: &Document,
    params: &CompletionParams,
) -> Option<CompletionResponse> {
    let pos = params.text_document_position.position;
    let node = doc.node_at(pos)?;
    // `%(` in a raw string or a comment is plain text.
    if matches!(node.kind(), kind::RAW_STRING | kind::COMMENT) {
        return None;
    }
    let cursor = doc.rope.lsp_position_to_text_pos(pos);
    let line_start = doc.rope.line_to_char(cursor.point.row);
    let before = doc.rope.slice(line_start..cursor.char).to_string();
    let context = context(&before)
        .or_else(|| argument_context(&call_before(doc, cursor)?))
        .or_else(|| key_context(doc, cursor.point.row, &before))?;

    let mut items: Vec<CompletionItem> = Vec::new();
    let mut seen = HashSet::new();
    // `rank` puts nearer symbols first; the nearest of equal names wins.
    let mut add = |rank: usize, item: CompletionItem| {
        if seen.insert(item.label.clone()) {
            let sort_text = Some(format!("{rank:04}{}", item.label));
            items.push(CompletionItem { sort_text, ..item });
        }
    };
    let files = workspace.visible(&doc.snapshot);
    match context {
        Context::Variable => {
            // Template parameters shadow defines of the same name.
            let template = std::iter::successors(Some(node), |n| n.parent())
                .find(|n| n.kind() == kind::SECTION_TEMPLATE)
                .and_then(|t| t.named_child(0))
                .and_then(|name| {
                    let name = doc.rope.node_text(name);
                    doc.snapshot.symbols.templates.get(&name)
                });
            for param in template.iter().flat_map(|t| &t.params) {
                let label_details = described("template parameter");
                add(
                    0,
                    CompletionItem {
                        label_details,
                        ..item(param, CompletionItemKind::VARIABLE)
                    },
                );
            }
            for (rank, file) in files.iter().enumerate() {
                for (name, entry) in &file.symbols.defines {
                    let detail = Some(format!("\"{}\"", entry.value_text));
                    add(
                        rank + 1,
                        CompletionItem {
                            detail,
                            ..item(name, CompletionItemKind::CONSTANT)
                        },
                    );
                }
            }
            for builtin in docs::all(Group::Builtin) {
                let builtin_item = CompletionItem {
                    label_details: described(builtin.group.label()),
                    documentation: Some(markdown(builtin.documentation())),
                    ..item(&builtin.name, CompletionItemKind::CONSTANT)
                };
                add(files.len() + 1, builtin_item);
            }
        }
        Context::Template => {
            for (rank, file) in files.iter().enumerate() {
                for (name, template) in &file.symbols.templates {
                    let detail = Some(template.signature(name));
                    add(
                        rank,
                        CompletionItem {
                            detail,
                            ..item(name, CompletionItemKind::FUNCTION)
                        },
                    );
                }
            }
        }
        Context::Argument { template, given } => {
            let (_, def) = workspace.lookup_template(&doc.snapshot, &template)?;
            let missing = def.params.iter().filter(|p| !given.contains(p));
            for (i, param) in missing.enumerate() {
                items.push(CompletionItem {
                    detail: Some(def.signature(&template)),
                    // Declaration order; accepting inserts the colon too.
                    sort_text: Some(format!("{i:04}")),
                    insert_text: Some(format!("{param}: ")),
                    ..item(param, CompletionItemKind::PROPERTY)
                });
            }
        }
        Context::ProcessKey { in_process } => {
            for key in docs::all(Group::ProcessKey) {
                items.push(CompletionItem {
                    label_details: described(key.group.label()),
                    documentation: Some(markdown(key.documentation())),
                    insert_text: Some(format!("{}: ", key.name)),
                    ..item(&key.name, CompletionItemKind::PROPERTY)
                });
            }
            if in_process && let Some(doc) = docs::get(Group::Keyword, keyword::USE) {
                items.push(CompletionItem {
                    label_details: described(doc.group.label()),
                    documentation: Some(markdown(doc.documentation())),
                    // The template name follows; typing it completes templates.
                    insert_text: Some(format!("{} ", keyword::USE)),
                    ..item(keyword::USE, CompletionItemKind::KEYWORD)
                });
            }
        }
    }
    Some(CompletionResponse::Array(items))
}

fn item(label: &str, kind: CompletionItemKind) -> CompletionItem {
    CompletionItem {
        label: label.to_string(),
        kind: Some(kind),
        ..CompletionItem::default()
    }
}

/// Editors render `detail` as code in the file's language.
fn described(text: &str) -> Option<CompletionItemLabelDetails> {
    Some(CompletionItemLabelDetails {
        detail: None,
        description: Some(text.to_string()),
    })
}

fn markdown(value: String) -> Documentation {
    Documentation::MarkupContent(MarkupContent {
        kind: MarkupKind::Markdown,
        value,
    })
}

/// What the text between line start and cursor is about to become.
fn context(before: &str) -> Option<Context> {
    // Drop the partially typed identifier; the client filters by it.
    let typed = before.trim_end_matches(|c: char| c.is_ascii_alphanumeric() || c == '_');
    if typed.ends_with("%(") {
        return Some(Context::Variable);
    }
    let rest = typed.trim_start().strip_prefix(keyword::USE)?;
    (!rest.is_empty() && rest.trim().is_empty()).then_some(Context::Template)
}

/// The start of a process or template entry: only a partial key before the cursor.
fn key_context(doc: &Document, row: usize, before: &str) -> Option<Context> {
    let partial = before.trim_start();
    if !partial
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return None;
    }
    // The nearest section keyword above decides; indentation means nothing in foobar.
    let sections = [
        keyword::DEFINES,
        keyword::PROCESS,
        keyword::TEMPLATE,
        keyword::INCLUDE,
    ];
    let section = (0..row).rev().find_map(|r| {
        let line = doc.rope.line(r).to_string();
        let first = line.split_whitespace().next()?;
        sections.into_iter().find(|s| *s == first)
    })?;
    match section {
        keyword::PROCESS => Some(Context::ProcessKey { in_process: true }),
        keyword::TEMPLATE => Some(Context::ProcessKey { in_process: false }),
        _ => None,
    }
}

/// Text from the start of the `use` line up to the cursor: a call may span lines.
fn call_before(doc: &Document, cursor: TextPos) -> Option<String> {
    let start = (0..=cursor.point.row).rev().find_map(|row| {
        match doc.rope.line(row).to_string().split_whitespace().next() {
            Some(keyword::USE) => Some(Some(row)),
            Some(keyword::DEFINES | keyword::PROCESS | keyword::TEMPLATE | keyword::INCLUDE) => {
                Some(None)
            }
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
    let (template, args) = call
        .trim_start()
        .strip_prefix(keyword::USE)?
        .split_once('(')?;
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
    ///
    /// Built-in variables come last.
    #[test]
    fn variables_after_percent_paren() {
        let text = "defines\n  name: \"x\"\ntemplate t(src)\n  cmd: \"%()\"\n  raw: '%()'\n";
        assert_eq!(
            labels(items_at(text, 3, 10)),
            Some(
                ["src", "name", "curdir", "hostname"]
                    .map(String::from)
                    .to_vec()
            )
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

    /// The documented process keys (and `use` in processes) at an entry start, not in `defines`.
    #[test]
    fn process_keys_at_entry_start() {
        let text = "process p\n  co\ndefines\n  co\ntemplate t()\n  co\n";
        let items = items_at(text, 1, 4).unwrap();
        let command = items.iter().find(|i| i.label == "command").unwrap();
        assert_eq!(command.insert_text.as_deref(), Some("command: "));
        assert!(items.iter().any(|i| i.label == "use"));
        assert!(items_at(text, 3, 4).is_none());
        // Templates take entries only, no `use` calls.
        let in_template = items_at(text, 5, 4).unwrap();
        assert!(in_template.iter().all(|i| i.label != "use"));
    }
}
