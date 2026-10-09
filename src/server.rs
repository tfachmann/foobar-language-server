//! tower-lsp-server [`Backend`]: stores docs, delegates feature impls.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tower_lsp_server::jsonrpc::Result as RpcResult;
use tower_lsp_server::ls_types::{
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverParams, HoverProviderCapability,
    InitializeParams, InitializeResult, InitializedParams, MessageType, OneOf, ServerCapabilities,
    ServerInfo, TextDocumentSyncCapability, TextDocumentSyncKind, Uri,
};
use tower_lsp_server::{Client, LanguageServer};

use crate::definition;
use crate::diagnostics;
use crate::document::Document;
use crate::hover;
use crate::text::uri_to_path;
use crate::workspace::Workspace;

const DIAGNOSTICS_DEBOUNCE_MS: u64 = 250;

pub struct Backend {
    client: Client,
    workspace: Arc<Workspace>,
    /// Pending diagnostics-publish tasks per URI. Aborted on subsequent edits.
    pending_diags: Arc<DashMap<Uri, JoinHandle<()>>>,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            workspace: Arc::new(Workspace::default()),
            pending_diags: Arc::new(DashMap::new()),
        }
    }

    /// Schedule diagnostics for `uri` and all open docs that include it.
    fn schedule_diagnostics(&self, uri: Uri) {
        self.schedule_one(uri.clone());

        let Some(path) = uri_to_path(&uri).and_then(|p| p.canonicalize().ok()) else {
            return;
        };
        for dep_uri in self.workspace.dependents_of(&path) {
            if dep_uri != uri {
                self.schedule_one(dep_uri);
            }
        }
    }

    fn schedule_one(&self, uri: Uri) {
        schedule_one(
            self.workspace.clone(),
            self.client.clone(),
            self.pending_diags.clone(),
            uri,
        );
    }
}

/// Cancel any pending diagnostics publish for `uri` and schedule a fresh one.
fn schedule_one(
    workspace: Arc<Workspace>,
    client: Client,
    pending: Arc<DashMap<Uri, JoinHandle<()>>>,
    uri: Uri,
) {
    if let Some((_, prev)) = pending.remove(&uri) {
        prev.abort();
    }
    let key = uri.clone();
    let pending_for_task = pending.clone();
    let handle = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(DIAGNOSTICS_DEBOUNCE_MS)).await;
        if let Some(doc) = workspace.doc(&uri) {
            let doc = doc.read().await;
            let diags = diagnostics::compute(&workspace, &doc).await;
            client
                .publish_diagnostics(uri.clone(), diags, Some(doc.version))
                .await;
        }
        pending_for_task.remove(&uri);
    });
    pending.insert(key, handle);
}

impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> RpcResult<InitializeResult> {
        self.workspace.set_roots(collect_roots(&params));
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::INCREMENTAL,
                )),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
                ..ServerCapabilities::default()
            },
            server_info: Some(ServerInfo {
                name: "foobar-language-server".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
            ..InitializeResult::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        let banner = format!(
            "foobar-language-server v{} ready",
            env!("CARGO_PKG_VERSION"),
        );
        self.client.log_message(MessageType::INFO, &banner).await;
        let ws = self.workspace.clone();
        let client = self.client.clone();
        let pending = self.pending_diags.clone();
        tokio::spawn(async move {
            let scan_ws = ws.clone();
            let _ = tokio::task::spawn_blocking(move || scan_ws.scan_workspace()).await;
            client
                .log_message(MessageType::INFO, "workspace scan complete")
                .await;
            // Re-check open docs: the scan may resolve previously undefined variables.
            let open_uris: Vec<Uri> = ws.docs.iter().map(|e| e.key().clone()).collect();
            for uri in open_uris {
                schedule_one(ws.clone(), client.clone(), pending.clone(), uri);
            }
        });
    }

    async fn shutdown(&self) -> RpcResult<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        match Document::open(params) {
            Ok(doc) => {
                let doc = Arc::new(RwLock::new(doc));
                self.workspace.docs.insert(uri.clone(), doc.clone());
                self.workspace
                    .refresh_includes_for(&*doc.read().await)
                    .await;
                self.schedule_diagnostics(uri);
            }
            Err(e) => {
                self.client
                    .log_message(MessageType::ERROR, format!("did_open failed: {e:#}"))
                    .await;
            }
        }
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        let Some(doc) = self.workspace.doc(&uri) else {
            self.client
                .log_message(
                    MessageType::WARNING,
                    format!("did_change on unknown doc: {}", uri.as_str()),
                )
                .await;
            return;
        };
        if let Err(e) = doc.write().await.apply_change(params) {
            self.client
                .log_message(MessageType::ERROR, format!("did_change failed: {e:#}"))
                .await;
            return;
        }
        self.workspace
            .refresh_includes_for(&*doc.read().await)
            .await;
        self.schedule_diagnostics(uri);
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        if let Some((_, prev)) = self.pending_diags.remove(&uri) {
            prev.abort();
        }
        self.workspace.docs.remove(&uri);
        self.workspace.forget_doc(&uri);
        // Clear any diagnostics currently shown for this doc on the client.
        self.client.publish_diagnostics(uri, Vec::new(), None).await;
    }

    async fn hover(&self, params: HoverParams) -> RpcResult<Option<Hover>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let Some(doc) = self.workspace.doc(uri) else {
            return Ok(None);
        };
        Ok(hover::hover(&self.workspace, &*doc.read().await, &params).await)
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> RpcResult<Option<GotoDefinitionResponse>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let Some(doc) = self.workspace.doc(uri) else {
            return Ok(None);
        };
        Ok(definition::goto(&self.workspace, &*doc.read().await, &params).await)
    }
}

/// Extract workspace roots from `InitializeParams`.
///
/// Prefers `workspace_folders`, falls back to deprecated `root_uri`, then `root_path`.
#[allow(deprecated)]
fn collect_roots(params: &InitializeParams) -> Vec<PathBuf> {
    let folders = params.workspace_folders.iter().flatten().map(|f| &f.uri);
    let candidates = folders
        .chain(&params.root_uri)
        .filter_map(uri_to_path)
        .chain(params.root_path.iter().map(PathBuf::from));

    let mut seen: HashSet<PathBuf> = HashSet::new();
    candidates
        .filter(|p| seen.insert(p.canonicalize().unwrap_or_else(|_| p.clone())))
        .collect()
}
