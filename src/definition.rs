//! Goto-definition dispatch.
//!
//! Two jump cases mirror hover:
//! - `%(name)` -> the matching `name:` define (transitive include + workspace pool).
//! - `include some_other.foobar` -> the included file's start.

use tower_lsp_server::ls_types::{
    GotoDefinitionParams, GotoDefinitionResponse, Location, Position, Range,
};
use tree_sitter::Node;

use crate::document::Document;
use crate::text::{PositionExt, RopeExt, path_to_uri};
use crate::workspace::{DefineSource, Workspace};

pub async fn goto(
    workspace: &Workspace,
    doc: &Document,
    params: &GotoDefinitionParams,
) -> Option<GotoDefinitionResponse> {
    let pos = params.text_document_position_params.position;
    let node = node_at(doc, pos)?;

    // Variable reference: %(name) -> matching `name:` define.
    if node.kind() == "identifier" && node.parent().is_some_and(|p| p.kind() == "variable") {
        let name = doc.rope.node_text(node);
        let hit = workspace.lookup_define(doc, &name).await?;
        return source_to_location(&hit.source, hit.entry.key_range)
            .map(GotoDefinitionResponse::Scalar);
    }

    // Include path: `include some_other.foobar` -> file start.
    if node.kind() == "path" && node.parent().is_some_and(|p| p.kind() == "include") {
        let raw = doc.rope.node_text(node).trim().to_string();
        let resolved = workspace.resolve(&doc.uri, &raw)?;
        if !resolved.exists {
            return None;
        }
        let uri = path_to_uri(&resolved.path)?;
        return Some(GotoDefinitionResponse::Scalar(Location {
            uri,
            range: Range {
                start: Position::new(0, 0),
                end: Position::new(0, 0),
            },
        }));
    }

    None
}

fn node_at(doc: &Document, pos: Position) -> Option<Node<'_>> {
    let point = pos.point();
    doc.tree
        .root_node()
        .descendant_for_point_range(point, point)
}

fn source_to_location(source: &DefineSource, range: Range) -> Option<Location> {
    match source {
        DefineSource::OpenDoc(uri) => Some(Location {
            uri: uri.clone(),
            range,
        }),
        DefineSource::Disk(path) => path_to_uri(path).map(|uri| Location { uri, range }),
    }
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
        DidOpenTextDocumentParams, PartialResultParams, TextDocumentIdentifier, TextDocumentItem,
        TextDocumentPositionParams, Uri, WorkDoneProgressParams,
    };

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

    async fn goto_at(
        ws: &Workspace,
        d: &Document,
        uri: &Uri,
        line: u32,
        character: u32,
    ) -> Option<GotoDefinitionResponse> {
        let params = GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position: Position::new(line, character),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };
        goto(ws, d, &params).await
    }

    fn scalar(r: GotoDefinitionResponse) -> Location {
        match r {
            GotoDefinitionResponse::Scalar(l) => l,
            other => panic!("expected Scalar, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn goto_variable_local() {
        let ws = Workspace::new();
        let uri = Uri::from_str("file:///x.foobar").unwrap();
        // line 0: defines
        // line 1: foo: "1"
        // line 2: cmd: "%(foo)"
        let d = open_doc(uri.clone(), "defines\nfoo: \"1\"\ncmd: \"%(foo)\"\n");
        // Cursor on `foo` inside %(foo) — line 2, col 9 (inside "foo").
        let loc = scalar(goto_at(&ws, &d, &uri, 2, 9).await.unwrap());
        assert_eq!(loc.uri, uri);
        assert_eq!(loc.range.start.line, 1);
        assert_eq!(loc.range.start.character, 0);
    }

    #[tokio::test]
    async fn goto_variable_via_include_disk() {
        let dir = TempDir::new().unwrap();
        let inc_path = dir.path().join("inc.foobar");
        let main_path = dir.path().join("main.foobar");
        fs::write(&inc_path, "defines\nname: \"timo\"\n").unwrap();
        let main_text = "include inc.foobar\n\ndefines\ncmd: \"%(name)\"\n";
        fs::write(&main_path, main_text).unwrap();
        let main_uri = path_to_uri(&main_path).unwrap();

        let ws = Workspace::new();
        let main_doc = open_doc(main_uri.clone(), main_text);
        ws.refresh_includes_for(&main_doc).await;

        // Line 3, col 8 = inside `name` of %(name).
        let loc = scalar(goto_at(&ws, &main_doc, &main_uri, 3, 8).await.unwrap());
        let inc_canon = inc_path.canonicalize().unwrap();
        assert_eq!(
            crate::text::uri_to_path(&loc.uri)
                .and_then(|p| p.canonicalize().ok())
                .as_deref(),
            Some(inc_canon.as_path())
        );
        assert_eq!(loc.range.start.line, 1, "name: is on line 1");
    }

    #[tokio::test]
    async fn goto_variable_via_include_open() {
        let dir = TempDir::new().unwrap();
        let inc_path = dir.path().join("inc.foobar");
        let main_path = dir.path().join("main.foobar");
        fs::write(&inc_path, "defines\nname: \"timo\"\n").unwrap();
        let main_text = "include inc.foobar\n\ndefines\ncmd: \"%(name)\"\n";
        fs::write(&main_path, main_text).unwrap();
        let main_uri = path_to_uri(&main_path).unwrap();
        let inc_uri = path_to_uri(&inc_path).unwrap();

        let ws = Workspace::new();
        // Open inc explicitly so the source must be OpenDoc, not Disk.
        ws.docs.insert(
            inc_uri.clone(),
            Arc::new(RwLock::new(open_doc(
                inc_uri.clone(),
                &fs::read_to_string(&inc_path).unwrap(),
            ))),
        );
        let main_doc = open_doc(main_uri.clone(), main_text);
        ws.refresh_includes_for(&main_doc).await;

        let loc = scalar(goto_at(&ws, &main_doc, &main_uri, 3, 8).await.unwrap());
        assert_eq!(loc.uri, inc_uri, "should jump to the open-doc URI");
        assert_eq!(loc.range.start.line, 1);
    }

    #[tokio::test]
    async fn goto_include_path() {
        let dir = TempDir::new().unwrap();
        let inc_path = dir.path().join("inc.foobar");
        let main_path = dir.path().join("main.foobar");
        fs::write(&inc_path, "defines\nname: \"timo\"\n").unwrap();
        let main_text = "include inc.foobar\n";
        fs::write(&main_path, main_text).unwrap();
        let main_uri = path_to_uri(&main_path).unwrap();

        let ws = Workspace::new();
        let main_doc = open_doc(main_uri.clone(), main_text);
        ws.refresh_includes_for(&main_doc).await;

        // Line 0, col 10 = inside `inc.foobar`.
        let loc = scalar(goto_at(&ws, &main_doc, &main_uri, 0, 10).await.unwrap());
        let inc_canon = inc_path.canonicalize().unwrap();
        assert_eq!(
            crate::text::uri_to_path(&loc.uri)
                .and_then(|p| p.canonicalize().ok())
                .as_deref(),
            Some(inc_canon.as_path())
        );
        assert_eq!(loc.range.start.line, 0);
        assert_eq!(loc.range.start.character, 0);
    }

    #[tokio::test]
    async fn goto_unknown_variable_returns_none() {
        let ws = Workspace::new();
        let uri = Uri::from_str("file:///x.foobar").unwrap();
        let d = open_doc(uri.clone(), "defines\ncmd: \"%(ghost)\"\n");
        // Inside `ghost`.
        assert!(goto_at(&ws, &d, &uri, 1, 9).await.is_none());
    }

    #[tokio::test]
    async fn goto_missing_include_returns_none() {
        let dir = TempDir::new().unwrap();
        let main_path = dir.path().join("main.foobar");
        let main_text = "include nope.foobar\n";
        fs::write(&main_path, main_text).unwrap();
        let main_uri = path_to_uri(&main_path).unwrap();

        let ws = Workspace::new();
        let main_doc = open_doc(main_uri.clone(), main_text);
        // Inside `nope.foobar`.
        assert!(goto_at(&ws, &main_doc, &main_uri, 0, 10).await.is_none());
    }
}
