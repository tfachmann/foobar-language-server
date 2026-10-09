//! Per-document symbol index. Rebuilt after each successful parse.

use std::collections::HashMap;

use ropey::Rope;
use tower_lsp_server::ls_types::Range;
use tree_sitter::{Node, Tree};

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
    pub includes: Vec<IncludeRef>,
    /// Every `%(name)` reference encountered.
    pub var_refs: Vec<(String, Range)>,
}

impl SymbolIndex {
    /// Builds symbol index from [`tree_sitter::Tree`].
    pub fn from_tree(tree: &Tree, rope: &Rope) -> Self {
        let root = tree.root_node();

        let mut defines: HashMap<String, DefineEntry> = HashMap::new();
        let mut processes: Vec<ProcessHeader> = Vec::new();
        let mut includes: Vec<IncludeRef> = Vec::new();

        let mut cursor = root.walk();
        for section in root.children(&mut cursor) {
            match section.kind() {
                "section_defines" => defines.extend(collect_defines(section, rope)),
                "section_process" => processes.push(collect_process(section, rope)),
                "include" => {
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
            includes,
            var_refs: collect_refs(root, rope),
        }
    }
}

/// Collects the `key: value` entries of a `defines` section.
fn collect_defines(section: Node, rope: &Rope) -> Vec<(String, DefineEntry)> {
    let mut cursor = section.walk();
    section
        .children(&mut cursor)
        .filter(|c| c.kind() == "entry")
        .filter_map(|entry| {
            let key_node = entry
                .children(&mut entry.walk())
                .find(|c| c.kind() == "identifier")?;
            let value_text = entry
                .children(&mut entry.walk())
                .find(|c| matches!(c.kind(), "string" | "raw_string"))
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

/// Collects the name and entry count of a `process` section.
fn collect_process(section: Node, rope: &Rope) -> ProcessHeader {
    let mut cursor = section.walk();
    let children: Vec<Node> = section.children(&mut cursor).collect();
    ProcessHeader {
        name: children
            .iter()
            .find(|c| c.kind() == "name")
            .map(|n| rope.node_text(*n).trim().to_string())
            .unwrap_or_default(),
        entry_count: children.iter().filter(|c| c.kind() == "entry").count(),
    }
}

/// Extracts the include reference (raw path + range) from an `(include ...)` section.
fn collect_include(section: Node, rope: &Rope) -> Option<IncludeRef> {
    let path_node = section
        .children(&mut section.walk())
        .find(|c| c.kind() == "path")?;
    Some(IncludeRef {
        raw_path: rope.node_text(path_node).trim().to_string(),
        path_range: rope.node_range(path_node),
    })
}

/// Collects every `%(name)` use anywhere in the tree.
fn collect_refs(root: Node, rope: &Rope) -> Vec<(String, Range)> {
    let mut out: Vec<(String, Range)> = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "variable" {
            let mut walker = node.walk();
            if let Some(id) = node
                .children(&mut walker)
                .find(|c| c.kind() == "identifier")
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
