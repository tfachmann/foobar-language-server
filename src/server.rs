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
    CompletionOptions, CompletionParams, CompletionResponse, DidChangeTextDocumentParams,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, GotoDefinitionParams,
    GotoDefinitionResponse, Hover, HoverParams, HoverProviderCapability, InitializeParams,
    InitializeResult, InitializedParams, MessageType, OneOf, ServerCapabilities, ServerInfo,
    TextDocumentSyncCapability, TextDocumentSyncKind, Uri,
};
use tower_lsp_server::{Client, LanguageServer, LspService, Server};
use tracing_subscriber::EnvFilter;

use crate::completion;
use crate::definition;
use crate::diagnostics;
use crate::document::Document;
use crate::hover;
use crate::text::uri_to_path;
use crate::workspace::{Workspace, uri_key};

const DIAGNOSTICS_DEBOUNCE_MS: u64 = 250;

/// `foobar lsp`: serve the language server over stdin/stdout.
pub fn run() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    tokio::runtime::Runtime::new()?.block_on(async {
        let (service, socket) = LspService::new(Backend::new);
        Server::new(tokio::io::stdin(), tokio::io::stdout(), socket)
            .serve(service)
            .await;
    });
    Ok(())
}

/// Cheap to clone: every field is shared, so spawned tasks get their own handle.
#[derive(Clone)]
pub struct Backend {
    client: Client,
    workspace: Arc<Workspace>,
    /// Edit state of open documents. Only the LSP handlers below ever lock these.
    docs: Arc<DashMap<Uri, Arc<RwLock<Document>>>>,
    /// Pending diagnostics-publish tasks per URI. Aborted on subsequent edits.
    pending_diags: Arc<DashMap<Uri, JoinHandle<()>>>,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            workspace: Arc::new(Workspace::default()),
            docs: Arc::new(DashMap::new()),
            pending_diags: Arc::new(DashMap::new()),
        }
    }

    /// The open document for `uri`, cloned out of the map so no DashMap guard is held.
    fn doc(&self, uri: &Uri) -> Option<Arc<RwLock<Document>>> {
        self.docs.get(uri).map(|e| e.value().clone())
    }

    /// Schedule diagnostics for `uri` and all open docs that include it.
    fn schedule_diagnostics(&self, uri: &Uri) {
        self.schedule_one(uri.clone());
        self.schedule_dependents(uri);
    }

    /// Schedule diagnostics for the open docs that include `uri`.
    fn schedule_dependents(&self, uri: &Uri) {
        let Some(path) = uri_key(uri) else {
            return;
        };
        for dep_uri in self.workspace.dependents_of(&path) {
            if &dep_uri != uri {
                self.schedule_one(dep_uri);
            }
        }
    }

    /// Cancel any pending diagnostics publish for `uri` and schedule a fresh one.
    fn schedule_one(&self, uri: Uri) {
        if let Some((_, prev)) = self.pending_diags.remove(&uri) {
            prev.abort();
        }
        let this = self.clone();
        let key = uri.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(DIAGNOSTICS_DEBOUNCE_MS)).await;
            if let Some(doc) = this.doc(&uri) {
                // Copy what we need and release the lock before computing or sending.
                let (snapshot, version) = {
                    let doc = doc.read().await;
                    (doc.snapshot.clone(), doc.version)
                };
                let mut diags = diagnostics::compute(&this.workspace, &snapshot);
                diags.extend(diagnostics::surface_includes(&this.workspace, &snapshot));
                // `abort()` can't stop the sync compute above; don't undo did_close's clear.
                if doc.read().await.closed {
                    return;
                }
                this.client
                    .publish_diagnostics(uri.clone(), diags, Some(version))
                    .await;
            }
        });
        self.pending_diags.insert(key, handle);
    }
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
                // `(` of `%(` and `use t(`, `,` between arguments; `use ` completes when
                // invoked or while typing the name.
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec!["(".to_string(), ",".to_string()]),
                    ..CompletionOptions::default()
                }),
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
        let this = self.clone();
        tokio::spawn(async move {
            let ws = this.workspace.clone();
            let _ = tokio::task::spawn_blocking(move || ws.scan_workspace()).await;
            this.client
                .log_message(MessageType::INFO, "workspace scan complete")
                .await;
            // Re-check open docs: the scan may resolve previously undefined variables.
            let open_uris: Vec<Uri> = this.docs.iter().map(|e| e.key().clone()).collect();
            for uri in open_uris {
                this.schedule_one(uri);
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
                self.workspace.publish(&doc);
                self.workspace.refresh_includes_for(&doc.snapshot);
                self.docs.insert(uri.clone(), Arc::new(RwLock::new(doc)));
                self.schedule_diagnostics(&uri);
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
        let Some(doc) = self.doc(&uri) else {
            self.client
                .log_message(
                    MessageType::WARNING,
                    format!("did_change on unknown doc: {}", uri.as_str()),
                )
                .await;
            return;
        };
        {
            let mut doc = doc.write().await;
            // A did_change that lost the race against did_close must not publish.
            if doc.closed {
                return;
            }
            if let Err(e) = doc.apply_change(params) {
                drop(doc);
                self.client
                    .log_message(MessageType::ERROR, format!("did_change failed: {e:#}"))
                    .await;
                return;
            }
            // Publish under the write lock so snapshots land in edit order.
            self.workspace.publish(&doc);
            self.workspace.refresh_includes_for(&doc.snapshot);
        }
        self.schedule_diagnostics(&uri);
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        // Tasks scheduled from now on find no document and publish nothing.
        if let Some((_, doc)) = self.docs.remove(&uri) {
            let mut doc = doc.write().await;
            doc.closed = true;
            self.workspace.close(&doc);
        }
        if let Some((_, prev)) = self.pending_diags.remove(&uri) {
            prev.abort();
            let _ = prev.await;
        }
        // The disk version may differ from the closed buffer.
        self.schedule_dependents(&uri);
        // Clear any diagnostics currently shown for this doc on the client.
        self.client.publish_diagnostics(uri, Vec::new(), None).await;
    }

    async fn hover(&self, params: HoverParams) -> RpcResult<Option<Hover>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let Some(doc) = self.doc(uri) else {
            return Ok(None);
        };
        Ok(hover::hover(&self.workspace, &*doc.read().await, &params))
    }

    async fn completion(&self, params: CompletionParams) -> RpcResult<Option<CompletionResponse>> {
        let uri = &params.text_document_position.text_document.uri;
        let Some(doc) = self.doc(uri) else {
            return Ok(None);
        };
        Ok(completion::complete(
            &self.workspace,
            &*doc.read().await,
            &params,
        ))
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> RpcResult<Option<GotoDefinitionResponse>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let Some(doc) = self.doc(uri) else {
            return Ok(None);
        };
        Ok(definition::goto(
            &self.workspace,
            &*doc.read().await,
            &params,
        ))
    }
}

/// Extract workspace roots from `InitializeParams`.
///
/// Prefers `workspace_folders`, falls back to deprecated `root_uri`, then `root_path`.
fn collect_roots(params: &InitializeParams) -> Vec<PathBuf> {
    let folders = params.workspace_folders.iter().flatten().map(|f| &f.uri);
    #[allow(deprecated)]
    let candidates = folders
        .chain(&params.root_uri)
        .filter_map(uri_to_path)
        .chain(params.root_path.iter().map(PathBuf::from));

    let mut seen: HashSet<PathBuf> = HashSet::new();
    candidates
        .filter(|p| seen.insert(p.canonicalize().unwrap_or_else(|_| p.clone())))
        .collect()
}
