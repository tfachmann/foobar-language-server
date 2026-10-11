//! Find references matching by name across the workspace
//!
//! - defines (variables)
//! - templates
//! - template parameters (their template, plus argument names in calls)

use tower_lsp_server::ls_types::{Location, Position, Range, ReferenceParams};
use tree_sitter::Node;
use tree_sitter_foobar::kind;

use crate::docs;
use crate::document::Document;
use crate::target::{Target, target};
use crate::text::RopeExt;
use crate::workspace::Workspace;

pub fn references(
    workspace: &Workspace,
    doc: &Document,
    params: &ReferenceParams,
) -> Option<Vec<Location>> {
    let pos = params.text_document_position.position;
    find(workspace, doc, pos, params.context.include_declaration)
}

/// The identifier's range at `pos`, if it names a define, template or parameter.
///
/// A built-in variable is not renamable, unless a define hides it.
pub fn symbol_range(workspace: &Workspace, doc: &Document, pos: Position) -> Option<Range> {
    let node = doc.node_at(pos)?;
    match target(node, &doc.rope)? {
        Target::DefineUse(name)
            if docs::is_builtin(&name)
                && workspace.lookup_define(&doc.snapshot, &name).is_none() =>
        {
            None
        }
        Target::DefineUse(_)
        | Target::DefineKey(_)
        | Target::TemplateCall(_)
        | Target::TemplateName(_)
        | Target::Param { .. } => Some(doc.rope.node_range(node)),
        _ => None,
    }
}

/// Every location naming the symbol at `pos`, sorted; `None` if the cursor is not on one.
pub fn find(
    workspace: &Workspace,
    doc: &Document,
    pos: Position,
    declaration: bool,
) -> Option<Vec<Location>> {
    let node = doc.node_at(pos)?;
    let mut found: Vec<Location> = match target(node, &doc.rope)? {
        Target::DefineUse(name) | Target::DefineKey(name) => workspace
            .all_files()
            .iter()
            .flat_map(|file| {
                let uses = file.symbols.var_refs.iter().filter(|(n, _)| *n == name);
                let decl = file.symbols.defines.get(&name).filter(|_| declaration);
                let ranges = uses.map(|(_, r)| *r).chain(decl.map(|d| d.key_range));
                ranges
                    .map(|range| location(&file.uri, range))
                    .collect::<Vec<_>>()
            })
            .collect(),
        Target::TemplateCall(name) | Target::TemplateName(name) => workspace
            .all_files()
            .iter()
            .flat_map(|file| {
                let uses = file.symbols.uses.iter().filter(|u| u.name == name);
                let decl = file.symbols.templates.get(&name).filter(|_| declaration);
                let ranges = uses.map(|u| u.name_range).chain(decl.map(|t| t.name_range));
                ranges
                    .map(|range| location(&file.uri, range))
                    .collect::<Vec<_>>()
            })
            .collect(),
        Target::Param { template, name, .. } => {
            let mut found = param_refs(doc, template, &name, declaration);
            // Argument names in calls: `use t(name: ...)`.
            let called = doc.rope.node_text(template.named_child(0)?);
            found.extend(workspace.all_files().iter().flat_map(|file| {
                let calls = file.symbols.uses.iter().filter(|u| u.name == called);
                let args = calls.flat_map(|u| &u.args).filter(|(arg, _)| *arg == name);
                args.map(|(_, range)| location(&file.uri, *range))
                    .collect::<Vec<_>>()
            }));
            found
        }
        _ => return None,
    };
    found.sort_by(|a, b| {
        let key = |l: &Location| (l.uri.as_str().to_owned(), l.range.start);
        key(a).cmp(&key(b))
    });
    Some(found)
}

/// `%(name)` uses inside `template`, plus the parameter itself if `declaration`.
fn param_refs(doc: &Document, template: Node, name: &str, declaration: bool) -> Vec<Location> {
    let mut found = Vec::new();
    let mut stack = vec![template];
    while let Some(node) = stack.pop() {
        let parent = node.parent().map(|p| p.kind());
        let wanted = match parent {
            Some(kind::VARIABLE) => true,
            Some(kind::PARAMETERS) => declaration,
            _ => false,
        };
        if node.kind() == kind::IDENTIFIER && wanted && doc.rope.node_text(node) == name {
            found.push(location(&doc.uri, doc.rope.node_range(node)));
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    found
}

fn location(uri: &tower_lsp_server::ls_types::Uri, range: Range) -> Location {
    Location {
        uri: uri.clone(),
        range,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;
    use tower_lsp_server::ls_types::{
        PartialResultParams, Position, ReferenceContext, TextDocumentIdentifier,
        TextDocumentPositionParams, Uri, WorkDoneProgressParams,
    };

    /// Lines of every reference at `line:character`, declaration included.
    fn ref_lines(doc: &Document, line: u32, character: u32) -> Vec<u32> {
        let ws = Workspace::default();
        ws.publish(doc);
        let params = ReferenceParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier {
                    uri: doc.uri.clone(),
                },
                position: Position::new(line, character),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
            context: ReferenceContext {
                include_declaration: true,
            },
        };
        let found = references(&ws, doc, &params).unwrap();
        found.iter().map(|l| l.range.start.line).collect()
    }

    /// A template parameter and a define of the same name have separate references.
    #[test]
    fn parameter_and_define_of_the_same_name() {
        let doc = Document::from_text(
            Uri::from_str("file:///x.foobar").unwrap(),
            "template t(src)\n  cmd: \"%(src)\"\ndefines\n  src: \"x\"\n  a: \"%(src)\"\n",
        );
        assert_eq!(ref_lines(&doc, 1, 11), [0, 1]); // `%(src)` in the template body
        assert_eq!(ref_lines(&doc, 3, 2), [3, 4]); // the define `src`
    }
}
