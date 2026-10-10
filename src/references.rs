//! Find references matching by name across the workspace
//!
//! - defines (variables)
//! - templates

use tower_lsp_server::ls_types::{Location, Range, ReferenceParams};
use tree_sitter::Node;

use crate::document::Document;
use crate::symbol::template_param;
use crate::text::RopeExt;
use crate::workspace::Workspace;

/// What the identifier under the cursor names.
enum Target<'a> {
    Define(String),
    Template(String),
    /// A parameter of the given `section_template`.
    Param(Node<'a>, String),
}

pub fn references(
    workspace: &Workspace,
    doc: &Document,
    params: &ReferenceParams,
) -> Option<Vec<Location>> {
    let node = doc.node_at(params.text_document_position.position)?;
    let declaration = params.context.include_declaration;
    let mut found: Vec<Location> = match target(doc, node)? {
        Target::Define(name) => workspace
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
        Target::Template(name) => workspace
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
        Target::Param(template, name) => param_refs(doc, template, &name, declaration),
    };
    found.sort_by(|a, b| {
        let key = |l: &Location| (l.uri.as_str().to_owned(), l.range.start);
        key(a).cmp(&key(b))
    });
    Some(found)
}

fn target<'a>(doc: &Document, node: Node<'a>) -> Option<Target<'a>> {
    if node.kind() != "identifier" {
        return None;
    }
    let name = doc.rope.node_text(node);
    let parent = node.parent()?;
    match parent.kind() {
        "variable" => match template_param(node, &doc.rope) {
            Some(param) => Some(Target::Param(param.parent()?.parent()?, name)),
            None => Some(Target::Define(name)),
        },
        "parameters" => Some(Target::Param(parent.parent()?, name)),
        "entry" if parent.parent()?.kind() == "section_defines" => Some(Target::Define(name)),
        "use" | "section_template" => Some(Target::Template(name)),
        _ => None,
    }
}

/// `%(name)` uses inside `template`, plus the parameter itself if `declaration`.
fn param_refs(doc: &Document, template: Node, name: &str, declaration: bool) -> Vec<Location> {
    let mut found = Vec::new();
    let mut stack = vec![template];
    while let Some(node) = stack.pop() {
        let parent = node.parent().map(|p| p.kind());
        let wanted = match parent {
            Some("variable") => true,
            Some("parameters") => declaration,
            _ => false,
        };
        if node.kind() == "identifier" && wanted && doc.rope.node_text(node) == name {
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
