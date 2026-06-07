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
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams, Hover,
    HoverParams, HoverProviderCapability, InitializeParams, InitializeResult, InitializedParams,
    MessageType, OneOf, ServerCapabilities, ServerInfo, TextDocumentSyncCapability,
    TextDocumentSyncKind, Uri,
};
use tower_lsp_server::{Client, LanguageServer};

use crate::diagnostics;
use crate::document::Document;
use crate::hover;
use crate::text::uri_to_path;
use crate::workspace::Workspace;

const DIAGNOSTICS_DEBOUNCE_MS: u64 = 250;

pub struct Backend {
    pub client: Client,
    /// Shared Workspace information.
    pub workspace: Arc<Workspace>,
    /// Pending diagnostics-publish tasks per URI. Aborted on subsequent edits.
    pub pending_diags: Arc<DashMap<Uri, JoinHandle<()>>>,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            workspace: Arc::new(Workspace::new()),
            pending_diags: Arc::new(DashMap::new()),
        }
    }

    /// Schedule diagnostics for `uri` and all of its dependenants.
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

    /// Cancel any pending diagnostics publish for `uri` and schedule a fresh one.
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
    let workspace_for_task = workspace.clone();
    let pending_for_task = pending.clone();
    let handle = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(DIAGNOSTICS_DEBOUNCE_MS)).await;
        let Some(entry) = workspace_for_task.docs.get(&uri) else {
            pending_for_task.remove(&uri);
            return;
        };
        let doc = entry.clone();
        drop(entry);
        let guard = doc.read().await;
        let diags = diagnostics::compute(&workspace_for_task, &guard).await;
        let version = guard.version;
        drop(guard);
        client
            .publish_diagnostics(uri.clone(), diags, Some(version))
            .await;
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
                definition_provider: Some(OneOf::Left(false)),
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
            let ws_blk = ws.clone();
            let _ = tokio::task::spawn_blocking(move || ws_blk.scan_workspace()).await;
            client
                .log_message(MessageType::INFO, "workspace scan complete")
                .await;
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
                let doc_arc = Arc::new(RwLock::new(doc));
                self.workspace.docs.insert(uri.clone(), doc_arc.clone());
                let guard = doc_arc.read().await;
                self.workspace.refresh_includes_for(&guard).await;
                drop(guard);
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
        let Some(entry) = self.workspace.docs.get(&uri) else {
            self.client
                .log_message(
                    MessageType::WARNING,
                    format!("did_change on unknown doc: {}", uri.as_str()),
                )
                .await;
            return;
        };
        let doc = entry.clone();
        drop(entry);
        {
            let mut guard = doc.write().await;
            if let Err(e) = guard.apply_change(params) {
                self.client
                    .log_message(MessageType::ERROR, format!("did_change failed: {e:#}"))
                    .await;
                return;
            }
        }
        let guard = doc.read().await;
        self.workspace.refresh_includes_for(&guard).await;
        drop(guard);
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
        let Some(entry) = self.workspace.docs.get(uri) else {
            return Ok(None);
        };
        let doc = entry.clone();
        drop(entry);
        let guard = doc.read().await;
        Ok(hover::hover(&self.workspace, &guard, &params).await)
    }
}

/// Extract workspace roots from `InitializeParams`.
///
/// Prefers `workspace_folders`, falls back to deprecated `root_uri`, then `root_path`.
fn collect_roots(params: &InitializeParams) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let push = |path: PathBuf, out: &mut Vec<PathBuf>, seen: &mut HashSet<PathBuf>| {
        let canon = path.canonicalize().unwrap_or_else(|_| path.clone());
        if seen.insert(canon) {
            out.push(path);
        }
    };

    if let Some(folders) = &params.workspace_folders {
        for folder in folders {
            if let Some(p) = uri_to_path(&folder.uri) {
                push(p, &mut out, &mut seen);
            }
        }
    }
    #[allow(deprecated)]
    if let Some(uri) = &params.root_uri {
        if let Some(p) = uri_to_path(uri) {
            push(p, &mut out, &mut seen);
        }
    }
    #[allow(deprecated)]
    if let Some(s) = &params.root_path {
        push(PathBuf::from(s), &mut out, &mut seen);
    }
    out
}
