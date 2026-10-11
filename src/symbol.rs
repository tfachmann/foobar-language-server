//! Per-document symbol index. Rebuilt after each successful parse.

use std::collections::{HashMap, HashSet};

use ropey::Rope;
use tower_lsp_server::ls_types::Range;
use tree_sitter::{Node, Tree};
use tree_sitter_foobar::kind;

use crate::text::RopeExt;

#[derive(Clone, Debug)]
pub struct DefineEntry {
    pub value_text: String,
    pub key_range: Range,
}

#[derive(Clone, Debug)]
pub struct ProcessHeader {
    pub name: String,
    pub entry_count: usize,
}

#[derive(Clone, Debug)]
pub struct TemplateDef {
    pub name_range: Range,
    pub params: Vec<String>,
    pub unused_params: Vec<(String, Range)>,
}

impl TemplateDef {
    /// `name(param, ...)`, as shown by hover and completion.
    pub fn signature(&self, name: &str) -> String {
        format!("{name}({})", self.params.join(", "))
    }
}

/// A `use name(arg: "...", ...)` call inside a process.
#[derive(Clone, Debug)]
pub struct UseRef {
    pub name: String,
    pub name_range: Range,
    /// Argument names with their ranges.
    pub args: Vec<(String, Range)>,
}

#[derive(Clone, Debug)]
pub struct IncludeRef {
    /// Raw path text as written in source, trimmed of surrounding whitespace.
    pub raw_path: String,
    /// Range of the `(path)` node.
    pub path_range: Range,
}

#[derive(Clone, Debug, Default)]
pub struct SymbolIndex {
    pub defines: HashMap<String, DefineEntry>,
    pub processes: Vec<ProcessHeader>,
    pub templates: HashMap<String, TemplateDef>,
    pub uses: Vec<UseRef>,
    pub includes: Vec<IncludeRef>,
    /// Every `%(name)` reference to a define (template parameters excluded).
    pub var_refs: Vec<(String, Range)>,
    /// Keys of `process` and `template` entries; only documented process keys are valid.
    pub entry_keys: Vec<(String, Range)>,
}

impl SymbolIndex {
    /// Builds symbol index from [`tree_sitter::Tree`].
    pub fn from_tree(tree: &Tree, rope: &Rope) -> Self {
        let root = tree.root_node();

        let mut defines: HashMap<String, DefineEntry> = HashMap::new();
        let mut processes: Vec<ProcessHeader> = Vec::new();
        let mut templates: HashMap<String, TemplateDef> = HashMap::new();
        let mut uses: Vec<UseRef> = Vec::new();
        let mut includes: Vec<IncludeRef> = Vec::new();
        let mut entry_keys: Vec<(String, Range)> = Vec::new();

        let mut cursor = root.walk();
        for section in root.children(&mut cursor) {
            match section.kind() {
                kind::SECTION_DEFINES => defines.extend(collect_defines(section, rope)),
                kind::SECTION_PROCESS => {
                    processes.push(collect_process(section, rope));
                    entry_keys.extend(collect_entry_keys(section, rope));
                    uses.extend(children(section, kind::USE).map(|u| collect_use(u, rope)));
                }
                kind::SECTION_TEMPLATE => {
                    templates.extend(collect_template(section, rope));
                    entry_keys.extend(collect_entry_keys(section, rope));
                }
                kind::INCLUDE => {
                    if let Some(r) = collect_include(section, rope) {
                        includes.push(r);
                    }
                }
                _ => {}
            }
        }

        Self {
            defines,
            processes,
            templates,
            uses,
            includes,
            var_refs: collect_refs(root, rope),
            entry_keys,
        }
    }
}

/// Keys of the entries of a `process` or `template` section.
fn collect_entry_keys(section: Node, rope: &Rope) -> Vec<(String, Range)> {
    children(section, kind::ENTRY)
        .filter_map(|entry| children(entry, kind::IDENTIFIER).next())
        .map(|key| (rope.node_text(key), rope.node_range(key)))
        .collect()
}

/// Collects the `key: value` entries of a `defines` section.
fn collect_defines(section: Node, rope: &Rope) -> Vec<(String, DefineEntry)> {
    let mut cursor = section.walk();
    section
        .children(&mut cursor)
        .filter(|c| c.kind() == kind::ENTRY)
        .filter_map(|entry| {
            let key_node = entry
                .children(&mut entry.walk())
                .find(|c| c.kind() == kind::IDENTIFIER)?;
            let value_text = entry
                .children(&mut entry.walk())
                .find(|c| matches!(c.kind(), kind::STRING | kind::RAW_STRING))
                .map(|v| strip_quotes(&rope.node_text(v)))
                .unwrap_or_default();
            Some((
                rope.node_text(key_node),
                DefineEntry {
                    value_text,
                    key_range: rope.node_range(key_node),
                },
            ))
        })
        .collect()
}

/// Collects the name and entry count (`use` lines included) of a `process` section.
fn collect_process(section: Node, rope: &Rope) -> ProcessHeader {
    let mut cursor = section.walk();
    let children: Vec<Node> = section.children(&mut cursor).collect();
    ProcessHeader {
        name: children
            .iter()
            .find(|c| c.kind() == kind::NAME)
            .map(|n| rope.node_text(*n).trim().to_string())
            .unwrap_or_default(),
        entry_count: children
            .iter()
            .filter(|c| matches!(c.kind(), kind::ENTRY | kind::USE))
            .count(),
    }
}

/// Collects the name and parameter list of a `template` section.
fn collect_template(section: Node, rope: &Rope) -> Option<(String, TemplateDef)> {
    let name = children(section, kind::IDENTIFIER).next()?;
    let params: Vec<Node> = children(section, kind::PARAMETERS)
        .flat_map(|p| children(p, kind::IDENTIFIER))
        .collect();
    let used = variable_names(section, rope);
    let unused_params = params
        .iter()
        .filter(|p| !used.contains(&rope.node_text(**p)))
        .map(|p| (rope.node_text(*p), rope.node_range(*p)))
        .collect();
    Some((
        rope.node_text(name),
        TemplateDef {
            name_range: rope.node_range(name),
            params: params.iter().map(|p| rope.node_text(*p)).collect(),
            unused_params,
        },
    ))
}

/// Names of every `%(name)` below `node`.
fn variable_names(node: Node, rope: &Rope) -> HashSet<String> {
    let mut names = HashSet::new();
    let mut stack = vec![node];
    while let Some(node) = stack.pop() {
        if node.kind() == kind::VARIABLE
            && let Some(ident) = children(node, kind::IDENTIFIER).next()
        {
            names.insert(rope.node_text(ident));
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    names
}

/// Collects the template name and argument names of a `use` call.
fn collect_use(node: Node, rope: &Rope) -> UseRef {
    let name = children(node, kind::IDENTIFIER).next();
    let args = children(node, kind::ARGUMENTS)
        .flat_map(|a| children(a, kind::ARGUMENT))
        .filter_map(|a| children(a, kind::IDENTIFIER).next())
        .map(|id| (rope.node_text(id), rope.node_range(id)))
        .collect();
    UseRef {
        name: name.map(|n| rope.node_text(n)).unwrap_or_default(),
        name_range: rope.node_range(name.unwrap_or(node)),
        args,
    }
}

/// Extracts the include reference (raw path + range) from an `(include ...)` section.
fn collect_include(section: Node, rope: &Rope) -> Option<IncludeRef> {
    let path_node = section
        .children(&mut section.walk())
        .find(|c| c.kind() == kind::PATH)?;
    Some(IncludeRef {
        raw_path: rope.node_text(path_node).trim().to_string(),
        path_range: rope.node_range(path_node),
    })
}

/// Collects every `%(name)` use anywhere in the tree, except uses of template parameters.
fn collect_refs(root: Node, rope: &Rope) -> Vec<(String, Range)> {
    let mut out: Vec<(String, Range)> = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == kind::VARIABLE {
            let mut walker = node.walk();
            if let Some(id) = node
                .children(&mut walker)
                .find(|c| c.kind() == kind::IDENTIFIER)
                && template_param(id, rope).is_none()
            {
                out.push((rope.node_text(id), rope.node_range(id)));
            }
        }
        let mut walker = node.walk();
        for child in node.children(&mut walker) {
            stack.push(child);
        }
    }
    out
}

/// The template parameter that the identifier of a `%(name)` refers to, if any.
///
/// Inside a template body, `%(name)` means the parameter when the template declares `name`.
pub fn template_param<'a>(ident: Node<'a>, rope: &Rope) -> Option<Node<'a>> {
    let template = std::iter::successors(ident.parent(), |n| n.parent())
        .find(|n| n.kind() == kind::SECTION_TEMPLATE)?;
    let name = rope.node_text(ident);
    children(template, kind::PARAMETERS)
        .flat_map(|p| children(p, kind::IDENTIFIER))
        .find(|p| rope.node_text(*p) == name)
}

/// Direct children of `node` with the given kind.
fn children<'a>(node: Node<'a>, kind: &'static str) -> impl Iterator<Item = Node<'a>> {
    let mut cursor = node.walk();
    let all: Vec<Node<'a>> = node.children(&mut cursor).collect();
    all.into_iter().filter(move |c| c.kind() == kind)
}

fn strip_quotes(s: &str) -> String {
    let s = s.trim();
    let bytes = s.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}
