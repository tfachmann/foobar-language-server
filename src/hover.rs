//! Hover dispatch for various symbols
//! - keyword tokens
//! - variables
//! - pathes
//! - ...

use std::path::Path;

use tower_lsp_server::ls_types::{
    Hover, HoverContents, HoverParams, MarkupContent, MarkupKind, Position,
};
use tree_sitter::Node;

use crate::document::Document;
use crate::keywords::{keyword_doc, process_entry_key_doc};
use crate::text::{PositionExt, RopeExt};
use crate::workspace::{DefineSource, Workspace};

pub async fn hover(workspace: &Workspace, doc: &Document, params: &HoverParams) -> Option<Hover> {
    let pos = params.text_document_position_params.position;
    let node = node_at(doc, pos)?;
    let md = render(workspace, doc, node).await?;
    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: md,
        }),
        range: None,
    })
}

fn node_at(doc: &Document, pos: Position) -> Option<Node<'_>> {
    let point = pos.point();
    doc.tree
        .root_node()
        .descendant_for_point_range(point, point)
}

async fn render(workspace: &Workspace, doc: &Document, node: Node<'_>) -> Option<String> {
    // Case 1: keyword tokens (defines / process / include). Anonymous, parent is the section node.
    if let Some(parent) = node.parent()
        && matches!(
            parent.kind(),
            "section_defines" | "section_process" | "include"
        )
        && !node.is_named()
        && let Some(doc_md) = keyword_doc(&doc.rope.node_text(node))
    {
        return Some(doc_md.to_string());
    }

    // Case 2: hover on the (path) node of an include.
    if node.kind() == "path" && node.parent().is_some_and(|p| p.kind() == "include") {
        return Some(hover_include_path(workspace, doc, node).await);
    }

    match (node.kind(), node.parent().map(|p| p.kind())) {
        // Case 3: identifier inside a variable use, e.g. %(file_name).
        ("identifier", Some("variable")) => {
            let name = doc.rope.node_text(node);
            hover_variable_use(workspace, doc, &name).await
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
                hover_defines_key(doc, &name)
            } else {
                hover_process_entry_key(&name)
            }
        }
        // Case 6: process header name.
        ("name", Some("section_process")) => {
            let name = doc.rope.node_text(node).trim().to_string();
            hover_process_header(doc, &name)
        }
        _ => None,
    }
}

async fn hover_variable_use(workspace: &Workspace, doc: &Document, name: &str) -> Option<String> {
    let hit = workspace.lookup_define(doc, name).await?;
    let line = hit.entry.key_range.start.line + 1;
    let location = match &hit.source {
        DefineSource::OpenDoc(uri) if uri == &doc.uri => format!("defined at line {line}"),
        DefineSource::OpenDoc(uri) => {
            let file = uri.as_str().rsplit('/').next().unwrap_or(uri.as_str());
            format!("defined at line {line} in `{file}` (open)")
        }
        DefineSource::Disk(path) => {
            let file = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("<unknown>");
            format!("defined at line {line} in `{file}`")
        }
    };
    Some(format!(
        "**{name}** = `{value}`\n---\n{location}",
        value = hit.entry.value_text,
    ))
}

fn hover_defines_key(doc: &Document, name: &str) -> Option<String> {
    let entry = doc.symbols.defines.get(name)?;
    let n = entry.refs.len();
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
    let p = doc.symbols.processes.iter().find(|p| p.name == name)?;
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
        .or_else(|| Some(format!("**{name}** — process entry key (no documentation)")))
}

async fn hover_include_path(workspace: &Workspace, doc: &Document, node: Node<'_>) -> String {
    let raw = doc.rope.node_text(node).trim().to_string();
    let resolved = workspace.resolve(&doc.uri, &raw);
    let mut body = format!("**include** `{raw}`\n---\n");
    match resolved {
        Some(r) => {
            body.push_str(&format!("Resolved to `{}`\n---\n", display_path(&r.path)));
            if !r.exists {
                body.push_str("_file not found_");
            } else if let Some(n) = workspace.defines_count_at(&r.path).await {
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

fn display_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::str::FromStr;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::sync::RwLock;
    use tower_lsp_server::ls_types::{
        DidOpenTextDocumentParams, TextDocumentIdentifier, TextDocumentItem,
        TextDocumentPositionParams, Uri, WorkDoneProgressParams,
    };

    use crate::text::path_to_uri;

    fn open_doc(uri: Uri, text: &str) -> Document {
        Document::open(DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri,
                language_id: "foobar".to_string(),
                version: 1,
                text: text.to_string(),
            },
        })
        .unwrap()
    }

    fn doc(text: &str) -> Document {
        open_doc(Uri::from_str("file:///x.foobar").unwrap(), text)
    }

    async fn hover_at(
        ws: &Workspace,
        d: &Document,
        uri: &Uri,
        line: u32,
        character: u32,
    ) -> Option<String> {
        let params = HoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position: Position::new(line, character),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        };
        hover(ws, d, &params).await.map(|h| match h.contents {
            HoverContents::Markup(m) => m.value,
            _ => String::new(),
        })
    }

    const SRC: &str = "defines\nfile_name: \"slides\"\n\nprocess compile presentation\ncommand: \"%(file_name).typst\"\n";

    #[tokio::test]
    async fn hover_on_variable_use() {
        let ws = Workspace::new();
        let d = doc(SRC);
        let uri = Uri::from_str("file:///x.foobar").unwrap();
        let s = hover_at(&ws, &d, &uri, 4, 14)
            .await
            .expect("hover on %(file_name)");
        assert!(s.contains("file_name"), "got: {s}");
        assert!(s.contains("slides"), "got: {s}");
        assert!(s.contains("defined at line"), "got: {s}");
    }

    #[tokio::test]
    async fn hover_on_defines_key() {
        let ws = Workspace::new();
        let d = doc(SRC);
        let uri = Uri::from_str("file:///x.foobar").unwrap();
        let s = hover_at(&ws, &d, &uri, 1, 3).await.unwrap();
        assert!(s.contains("file_name"));
        assert!(s.contains("slides"));
        assert!(s.contains("used"));
    }

    #[tokio::test]
    async fn hover_on_process_header() {
        let ws = Workspace::new();
        let d = doc(SRC);
        let uri = Uri::from_str("file:///x.foobar").unwrap();
        let s = hover_at(&ws, &d, &uri, 3, 10).await.unwrap();
        assert!(s.contains("process"));
        assert!(s.contains("compile presentation"));
    }

    #[tokio::test]
    async fn hover_on_keyword_defines() {
        let ws = Workspace::new();
        let d = doc(SRC);
        let uri = Uri::from_str("file:///x.foobar").unwrap();
        let s = hover_at(&ws, &d, &uri, 0, 2).await.unwrap();
        assert!(s.contains("**defines**"));
    }

    #[tokio::test]
    async fn hover_on_keyword_process() {
        let ws = Workspace::new();
        let d = doc(SRC);
        let uri = Uri::from_str("file:///x.foobar").unwrap();
        let s = hover_at(&ws, &d, &uri, 3, 2).await.unwrap();
        assert!(s.contains("**process**"));
    }

    #[tokio::test]
    async fn hover_on_process_entry_key() {
        let ws = Workspace::new();
        let d = doc(SRC);
        let uri = Uri::from_str("file:///x.foobar").unwrap();
        let s = hover_at(&ws, &d, &uri, 4, 2).await.unwrap();
        assert!(s.contains("command"));
    }

    #[tokio::test]
    async fn hover_on_variable_use_via_include() {
        let dir = TempDir::new().unwrap();
        let inc_path = dir.path().join("inc.foobar");
        let main_path = dir.path().join("main.foobar");
        fs::write(&inc_path, "defines\nname: \"timo\"\n").unwrap();
        let main_text = "include inc.foobar\n\nprocess greet\ncommand: \"hi %(name)\"\n";
        fs::write(&main_path, main_text).unwrap();
        let main_uri = path_to_uri(&main_path).unwrap();

        let ws = Workspace::new();
        let main_doc = open_doc(main_uri.clone(), main_text);
        ws.docs.insert(
            main_uri.clone(),
            Arc::new(RwLock::new(open_doc(main_uri.clone(), main_text))),
        );
        ws.refresh_includes_for(&main_doc).await;

        // %(name) on line 3 starts at col 15 ("hi %(name)" -> `n` of name)
        let s = hover_at(&ws, &main_doc, &main_uri, 3, 15).await.unwrap();
        assert!(s.contains("**name**"), "got: {s}");
        assert!(s.contains("timo"), "got: {s}");
        assert!(s.contains("inc.foobar"), "got: {s}");
    }

    #[tokio::test]
    async fn hover_on_include_path() {
        let dir = TempDir::new().unwrap();
        let inc_path = dir.path().join("inc.foobar");
        let main_path = dir.path().join("main.foobar");
        fs::write(&inc_path, "defines\na: \"1\"\nb: \"2\"\n").unwrap();
        let main_text = "include inc.foobar\n";
        fs::write(&main_path, main_text).unwrap();
        let main_uri = path_to_uri(&main_path).unwrap();

        let ws = Workspace::new();
        let main_doc = open_doc(main_uri.clone(), main_text);
        ws.refresh_includes_for(&main_doc).await;

        // Line 0: "include inc.foobar". Path starts at col 7.
        let s = hover_at(&ws, &main_doc, &main_uri, 0, 10).await.unwrap();
        assert!(s.contains("**include**"), "got: {s}");
        assert!(s.contains("inc.foobar"), "got: {s}");
        assert!(s.contains("2 defines"), "got: {s}");
    }
}
