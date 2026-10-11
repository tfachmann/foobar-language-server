//! Goto-definition dispatch.
//!
//! Jump cases mirror hover:
//! - `%(name)` -> the template parameter `name`, else the matching `name:` define.
//! - `use name(...)` -> the template definition.
//! - `include some_other.foobar` -> the included file's start.

use tower_lsp_server::ls_types::{GotoDefinitionParams, GotoDefinitionResponse, Location, Range};

use crate::document::Document;
use crate::target::{Target, target};
use crate::text::{RopeExt, path_to_uri};
use crate::workspace::Workspace;

pub fn goto(
    workspace: &Workspace,
    doc: &Document,
    params: &GotoDefinitionParams,
) -> Option<GotoDefinitionResponse> {
    let node = doc.node_at(params.text_document_position_params.position)?;
    let location = match target(node, &doc.rope)? {
        Target::Param { decl, .. } => Location {
            uri: doc.uri.clone(),
            range: doc.rope.node_range(decl),
        },
        Target::DefineUse(name) => {
            let (file, entry) = workspace.lookup_define(&doc.snapshot, &name)?;
            Location {
                uri: file.uri.clone(),
                range: entry.key_range,
            }
        }
        Target::TemplateCall(name) => {
            let (file, template) = workspace.lookup_template(&doc.snapshot, &name)?;
            Location {
                uri: file.uri.clone(),
                range: template.name_range,
            }
        }
        Target::IncludePath(raw) => {
            let resolved = workspace.resolve(&doc.uri, &raw)?;
            if !resolved.exists {
                return None;
            }
            // Prefer the editor's URI if the file is open.
            let uri = match workspace.file(&resolved.path) {
                Some(file) => file.uri.clone(),
                None => path_to_uri(&resolved.path)?,
            };
            Location {
                uri,
                range: Range::default(),
            }
        }
        _ => return None,
    };
    Some(GotoDefinitionResponse::Scalar(location))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::str::FromStr;
    use tempfile::TempDir;
    use tower_lsp_server::ls_types::{
        PartialResultParams, Position, TextDocumentIdentifier, TextDocumentPositionParams, Uri,
        WorkDoneProgressParams,
    };

    fn goto_at(ws: &Workspace, doc: &Document, line: u32, character: u32) -> Location {
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
        match goto(ws, doc, &params) {
            Some(GotoDefinitionResponse::Scalar(loc)) => loc,
            other => panic!("expected a single location, got {other:?}"),
        }
    }

    /// `main.foobar` includes `inc.foobar`, which defines `name` on line 1.
    fn setup() -> (TempDir, Workspace, Document, Uri) {
        let dir = TempDir::new().unwrap();
        let inc_path = dir.path().join("inc.foobar");
        fs::write(&inc_path, "defines\nname: \"timo\"\n").unwrap();
        let main_uri = path_to_uri(&dir.path().join("main.foobar")).unwrap();
        let doc = Document::from_text(
            main_uri,
            "include inc.foobar\n\ndefines\ncmd: \"%(name)\"\n",
        );
        let ws = Workspace::default();
        ws.refresh_includes_for(&doc.snapshot);
        let inc_uri = path_to_uri(&inc_path.canonicalize().unwrap()).unwrap();
        (dir, ws, doc, inc_uri)
    }

    #[test]
    fn goto_variable_via_include() {
        let (_dir, ws, doc, inc_uri) = setup();
        // Inside `name` of `%(name)`.
        let loc = goto_at(&ws, &doc, 3, 8);
        assert_eq!(loc.uri, inc_uri);
        assert_eq!(loc.range.start, Position::new(1, 0));
    }

    #[test]
    fn goto_include_path() {
        let (_dir, ws, doc, inc_uri) = setup();
        // Inside `inc.foobar`.
        let loc = goto_at(&ws, &doc, 0, 10);
        assert_eq!(loc.uri, inc_uri);
        assert_eq!(loc.range, Range::default());
    }

    #[test]
    fn goto_template_param_and_use() {
        let uri = Uri::from_str("file:///x.foobar").unwrap();
        let text = "defines\n  src: 'outer'\ntemplate t(src)\n  cmd: \"%(src)\"\n\
                    process p\n  use t(src: 'x')\n";
        let doc = Document::from_text(uri, text);
        let ws = Workspace::default();

        // `%(src)` in the template body is the parameter, not the define of the same name.
        let loc = goto_at(&ws, &doc, 3, 11);
        assert_eq!(
            loc.range,
            Range::new(Position::new(2, 11), Position::new(2, 14))
        );

        // `use t` jumps to the template name.
        let loc = goto_at(&ws, &doc, 5, 6);
        assert_eq!(
            loc.range,
            Range::new(Position::new(2, 9), Position::new(2, 10))
        );
    }
}
