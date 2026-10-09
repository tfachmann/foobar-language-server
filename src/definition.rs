//! Goto-definition dispatch.
//!
//! Two jump cases mirror hover:
//! - `%(name)` -> the matching `name:` define (local or via transitive includes).
//! - `include some_other.foobar` -> the included file's start.

use tower_lsp_server::ls_types::{GotoDefinitionParams, GotoDefinitionResponse, Location, Range};

use crate::document::Document;
use crate::text::{RopeExt, path_to_uri};
use crate::workspace::{DefineSource, Workspace};

pub async fn goto(
    workspace: &Workspace,
    doc: &Document,
    params: &GotoDefinitionParams,
) -> Option<GotoDefinitionResponse> {
    let pos = params.text_document_position_params.position;
    let node = doc.node_at(pos)?;

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
            range: Range::default(),
        }));
    }

    None
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
    use tempfile::TempDir;
    use tower_lsp_server::ls_types::{
        PartialResultParams, Position, TextDocumentIdentifier, TextDocumentPositionParams, Uri,
        WorkDoneProgressParams,
    };

    async fn goto_at(ws: &Workspace, doc: &Document, line: u32, character: u32) -> Location {
        let params = GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier {
                    uri: doc.uri.clone(),
                },
                position: Position::new(line, character),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };
        match goto(ws, doc, &params).await {
            Some(GotoDefinitionResponse::Scalar(loc)) => loc,
            other => panic!("expected a single location, got {other:?}"),
        }
    }

    /// `main.foobar` includes `inc.foobar`, which defines `name` on line 1.
    async fn setup() -> (TempDir, Workspace, Document, Uri) {
        let dir = TempDir::new().unwrap();
        let inc_path = dir.path().join("inc.foobar");
        fs::write(&inc_path, "defines\nname: \"timo\"\n").unwrap();
        let main_uri = path_to_uri(&dir.path().join("main.foobar")).unwrap();
        let doc = Document::from_text(
            main_uri,
            "include inc.foobar\n\ndefines\ncmd: \"%(name)\"\n",
        );
        let ws = Workspace::default();
        ws.refresh_includes_for(&doc).await;
        let inc_uri = path_to_uri(&inc_path.canonicalize().unwrap()).unwrap();
        (dir, ws, doc, inc_uri)
    }

    #[tokio::test]
    async fn goto_variable_via_include() {
        let (_dir, ws, doc, inc_uri) = setup().await;
        // Inside `name` of `%(name)`.
        let loc = goto_at(&ws, &doc, 3, 8).await;
        assert_eq!(loc.uri, inc_uri);
        assert_eq!(loc.range.start, Position::new(1, 0));
    }

    #[tokio::test]
    async fn goto_include_path() {
        let (_dir, ws, doc, inc_uri) = setup().await;
        // Inside `inc.foobar`.
        let loc = goto_at(&ws, &doc, 0, 10).await;
        assert_eq!(loc.uri, inc_uri);
        assert_eq!(loc.range, Range::default());
    }
}
