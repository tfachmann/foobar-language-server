//! tower-lsp-server [`Backend`]: stores docs, delegates feature impls.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use dashmap::DashMap;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tower_lsp_server::jsonrpc::Result as RpcResult;
use tower_lsp_server::ls_types::{
    CompletionOptions, CompletionParams, CompletionResponse, DidChangeTextDocumentParams,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, DocumentFormattingParams,
    GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverParams, HoverProviderCapability,
    InitializeParams, InitializeResult, InitializedParams, Location, MessageType, OneOf, Position,
    PrepareRenameResponse, Range, ReferenceParams, RenameOptions, RenameParams, ServerCapabilities,
    ServerInfo, TextDocumentPositionParams, TextDocumentSyncCapability, TextDocumentSyncKind,
    TextEdit, Uri, WorkDoneProgressOptions, WorkspaceEdit,
};
use tower_lsp_server::{Client, LanguageServer, LspService, Server};
use tracing_subscriber::EnvFilter;

use crate::completion;
use crate::definition;
use crate::diagnostics;
use crate::document::Document;
use crate::format::Formatter;
use crate::hover;
use crate::references;
use crate::rename;
use crate::text::{RopeExt, uri_to_path};
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
    /// Set once from `initializationOptions` in `initialize`.
    format_options: Arc<OnceLock<FormatOptions>>,
}

/// Formatting settings from `initializationOptions`.
#[derive(Clone, Default)]
struct FormatOptions {
    /// `formattingQuery`: a Topiary query file, relative to the first workspace root.
    query: Option<PathBuf>,
    /// `formattingIndent`: the indentation string.
    indent: Option<String>,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            workspace: Arc::new(Workspace::default()),
            docs: Arc::new(DashMap::new()),
            pending_diags: Arc::new(DashMap::new()),
            format_options: Arc::new(OnceLock::new()),
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
        let roots = collect_roots(&params);
        let options = params.initialization_options.as_ref();
        let option = |key: &str| {
            let value = options.and_then(|o| o.get(key))?;
            value.as_str().map(str::to_string)
        };
        let _ = self.format_options.set(FormatOptions {
            query: option("formattingQuery").map(|query| match roots.first() {
                Some(root) => root.join(query),
                None => PathBuf::from(query),
            }),
            indent: option("formattingIndent"),
        });
        self.workspace.set_roots(roots);
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::INCREMENTAL,
                )),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
                document_formatting_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                })),
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

    /// One edit replacing the whole buffer. Nothing on syntax errors.
    async fn formatting(
        &self,
        params: DocumentFormattingParams,
    ) -> RpcResult<Option<Vec<TextEdit>>> {
        let Some(doc) = self.doc(&params.text_document.uri) else {
            return Ok(None);
        };
        let (text, end) = {
            let doc = doc.read().await;
            let end = doc.rope.byte_to_lsp_position(doc.rope.len_bytes());
            (doc.rope.to_string(), end)
        };
        let options = self.format_options.get().cloned().unwrap_or_default();
        // Loaded per request, so edits to a custom query apply without a restart.
        let formatted = Formatter::load(options.query.as_deref(), options.indent.as_deref())
            .and_then(|formatter| formatter.format(&text));
        match formatted {
            Ok(Some(formatted)) if formatted != text => Ok(Some(vec![TextEdit::new(
                Range::new(Position::new(0, 0), end),
                formatted,
            )])),
            Ok(_) => Ok(None),
            Err(e) => {
                let message = format!("foobar: formatting failed: {e:#}");
                self.client.show_message(MessageType::ERROR, message).await;
                Ok(None)
            }
        }
    }

    async fn references(&self, params: ReferenceParams) -> RpcResult<Option<Vec<Location>>> {
        let uri = &params.text_document_position.text_document.uri;
        let Some(doc) = self.doc(uri) else {
            return Ok(None);
        };
        Ok(references::references(
            &self.workspace,
            &*doc.read().await,
            &params,
        ))
    }

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> RpcResult<Option<PrepareRenameResponse>> {
        let Some(doc) = self.doc(&params.text_document.uri) else {
            return Ok(None);
        };
        Ok(rename::prepare(&*doc.read().await, &params))
    }

    async fn rename(&self, params: RenameParams) -> RpcResult<Option<WorkspaceEdit>> {
        let uri = &params.text_document_position.text_document.uri;
        let Some(doc) = self.doc(uri) else {
            return Ok(None);
        };
        rename::rename(&self.workspace, &*doc.read().await, &params)
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
