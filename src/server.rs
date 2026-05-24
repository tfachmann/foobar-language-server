//! tower-lsp-server [`Backend`]: stores docs, delegates feature impls.

use std::sync::Arc;

use tokio::sync::RwLock;
use tower_lsp_server::jsonrpc::Result as RpcResult;
use tower_lsp_server::ls_types::{
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams, Hover,
    HoverParams, HoverProviderCapability, InitializeParams, InitializeResult, InitializedParams,
    MessageType, OneOf, ServerCapabilities, ServerInfo, TextDocumentSyncCapability,
    TextDocumentSyncKind,
};
use tower_lsp_server::{Client, LanguageServer};

use crate::document::Document;
use crate::hover;
use crate::workspace::Workspace;

pub struct Backend {
    pub client: Client,
    /// Shared Workspace information.
    pub workspace: Arc<Workspace>,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            workspace: Arc::new(Workspace::new()),
        }
    }
}

impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> RpcResult<InitializeResult> {
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
    }

    async fn shutdown(&self) -> RpcResult<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        match Document::open(params) {
            Ok(doc) => {
                let doc_arc = Arc::new(RwLock::new(doc));
                self.workspace.docs.insert(uri, doc_arc.clone());
                let guard = doc_arc.read().await;
                self.workspace.refresh_includes_for(&guard).await;
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
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.workspace.docs.remove(&params.text_document.uri);
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
