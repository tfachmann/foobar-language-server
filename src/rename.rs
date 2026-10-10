//! Rename: every reference of the symbol under the cursor, declarations included.

use std::collections::HashMap;

use tower_lsp_server::jsonrpc::{Error, Result};
use tower_lsp_server::ls_types::{
    PrepareRenameResponse, RenameParams, TextDocumentPositionParams, TextEdit, Uri, WorkspaceEdit,
};

use crate::document::Document;
use crate::references;
use crate::workspace::Workspace;

/// The identifier's range, if the cursor is on something renamable.
pub fn prepare(
    doc: &Document,
    params: &TextDocumentPositionParams,
) -> Option<PrepareRenameResponse> {
    references::symbol_range(doc, params.position).map(PrepareRenameResponse::Range)
}

pub fn rename(
    workspace: &Workspace,
    doc: &Document,
    params: &RenameParams,
) -> Result<Option<WorkspaceEdit>> {
    let new_name = &params.new_name;
    if !is_identifier(new_name) {
        return Err(Error::invalid_params(format!(
            "`{new_name}` is not a name: a lowercase letter, then letters, digits or `_`"
        )));
    }
    let pos = params.text_document_position.position;
    let Some(found) = references::find(workspace, doc, pos, true) else {
        return Ok(None);
    };
    let mut changes: HashMap<Uri, Vec<TextEdit>> = HashMap::new();
    for location in found {
        let edit = TextEdit::new(location.range, new_name.clone());
        changes.entry(location.uri).or_default().push(edit);
    }
    Ok(Some(WorkspaceEdit {
        changes: Some(changes),
        ..WorkspaceEdit::default()
    }))
}

/// The grammar's `identifier`: `[a-z][a-zA-Z0-9_]*`.
fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;
    use tower_lsp_server::ls_types::{Position, TextDocumentIdentifier, WorkDoneProgressParams};

    fn rename_at(doc: &Document, line: u32, character: u32, new_name: &str) -> Result<Vec<u32>> {
        let ws = Workspace::default();
        ws.publish(doc);
        let params = RenameParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier {
                    uri: doc.uri.clone(),
                },
                position: Position::new(line, character),
            },
            new_name: new_name.to_string(),
            work_done_progress_params: WorkDoneProgressParams::default(),
        };
        let edit = rename(&ws, doc, &params)?.unwrap();
        let edits = &edit.changes.unwrap()[&doc.uri];
        Ok(edits.iter().map(|e| e.range.start.line).collect())
    }

    /// A parameter is renamed in its declaration, its uses and the argument names of calls.
    #[test]
    fn renames_parameter_with_call_arguments() {
        let doc = Document::from_text(
            Uri::from_str("file:///x.foobar").unwrap(),
            "template t(src)\n  cmd: \"%(src)\"\nprocess p\n  use t(src: 'x')\n",
        );
        assert_eq!(rename_at(&doc, 1, 11, "input").unwrap(), [0, 1, 3]);
        assert!(rename_at(&doc, 1, 11, "Input").is_err());
    }
}
