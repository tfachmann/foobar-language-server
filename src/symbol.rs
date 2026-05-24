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
    pub value_range: Range,
    pub refs: Vec<Range>,
}

#[derive(Clone, Debug)]
pub struct ProcessHeader {
    pub name: String,
    pub header_range: Range,
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

        for (name, range) in collect_refs(root, rope) {
            if let Some(entry) = defines.get_mut(&name) {
                entry.refs.push(range);
            }
        }

        Self {
            defines,
            processes,
            includes,
        }
    }
}

/// Processes a define section and builds a symbolic representation.
fn collect_defines(section: Node, rope: &Rope) -> Vec<(String, DefineEntry)> {
    let mut cursor = section.walk();
    section
        .children(&mut cursor)
        .filter(|c| c.kind() == "entry")
        .filter_map(|entry| {
            let key_node = entry
                .children(&mut entry.walk())
                .find(|c| c.kind() == "identifier")?;
            let value_node = entry
                .children(&mut entry.walk())
                .find(|c| matches!(c.kind(), "string" | "raw_string"));
            let (value_text, value_range) = match value_node {
                Some(v) => (strip_quotes(&rope.node_text(v)), rope.node_range(v)),
                None => (String::new(), rope.node_range(entry)),
            };
            Some((
                rope.node_text(key_node),
                DefineEntry {
                    value_text,
                    key_range: rope.node_range(key_node),
                    value_range,
                    refs: Vec::new(),
                },
            ))
        })
        .collect()
}

/// Processes a process section and builds a symbolic representation.
fn collect_process(section: Node, rope: &Rope) -> ProcessHeader {
    let mut cursor = section.walk();
    let mut name = String::new();
    let mut header_end = section.start_byte();
    let mut entry_count = 0usize;

    for child in section.children(&mut cursor) {
        match child.kind() {
            "name" => {
                name = rope.node_text(child).trim().to_string();
                header_end = child.end_byte();
            }
            "entry" => entry_count += 1,
            _ => {}
        }
    }

    ProcessHeader {
        name,
        header_range: rope.byte_range_to_lsp_range(section.start_byte(), header_end),
        entry_count,
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

/// Computes reference (how often a variable is used)
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
