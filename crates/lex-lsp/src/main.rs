//! `lex-lsp` binary — LSP server over stdin/stdout (#304 phase 1).
//!
//! The minimum viable surface that turns a Lex file open in an
//! LSP-capable editor (VS Code, Cursor, Continue, Zed, JetBrains AI)
//! into a red-squiggle experience for type errors. Subsequent phases
//! add hover, definition, completion, code actions, and repair-hint
//! integration.

use lex_lsp::{diagnostics_for_source, Documents};
use lsp_server::{Connection, Message, Notification, Response};
use lsp_types::notification::{
    DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, DidSaveTextDocument,
    Notification as NotificationTrait, PublishDiagnostics,
};
use lsp_types::{
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DidSaveTextDocumentParams, InitializeParams, InitializeResult, OneOf,
    PublishDiagnosticsParams, ServerCapabilities, ServerInfo, TextDocumentSyncCapability,
    TextDocumentSyncKind, Uri,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // stderr is the standard LSP log channel — editors capture and
    // surface it in their "Output" panes.
    eprintln!("lex-lsp starting (phase 1: read-only diagnostics)");

    let (connection, io_threads) = Connection::stdio();
    let server_capabilities = ServerCapabilities {
        // Full-document sync only for phase 1. Incremental sync is
        // a follow-up; the type checker re-runs the whole program
        // on every change anyway, so the marginal savings are
        // ~zero today.
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        definition_provider: Some(OneOf::Left(false)),
        hover_provider: None,
        completion_provider: None,
        ..Default::default()
    };
    let server_capabilities_json = serde_json::to_value(&server_capabilities)?;

    let initialization_params = connection.initialize(server_capabilities_json)?;
    let _params: InitializeParams = serde_json::from_value(initialization_params)?;
    let _info = InitializeResult {
        capabilities: server_capabilities,
        server_info: Some(ServerInfo {
            name: "lex-lsp".into(),
            version: Some(env!("CARGO_PKG_VERSION").into()),
        }),
    };

    main_loop(connection)?;
    io_threads.join()?;
    eprintln!("lex-lsp shutting down");
    Ok(())
}

fn main_loop(connection: Connection) -> Result<(), Box<dyn std::error::Error>> {
    let mut docs = Documents::new();
    for msg in &connection.receiver {
        match msg {
            Message::Request(req) => {
                if connection.handle_shutdown(&req)? {
                    return Ok(());
                }
                // Phase 2+ requests (hover, definition, completion,
                // codeAction) reply with method-not-found so editors
                // know the capability isn't supported yet.
                let resp = Response {
                    id: req.id,
                    result: None,
                    error: Some(lsp_server::ResponseError {
                        code: lsp_server::ErrorCode::MethodNotFound as i32,
                        message: format!("`{}` not supported by lex-lsp phase 1", req.method),
                        data: None,
                    }),
                };
                connection.sender.send(Message::Response(resp))?;
            }
            Message::Notification(note) => {
                handle_notification(&connection, &mut docs, note)?;
            }
            Message::Response(_) => {
                // We don't send any client → server requests yet.
            }
        }
    }
    Ok(())
}

fn handle_notification(
    connection: &Connection,
    docs: &mut Documents,
    note: Notification,
) -> Result<(), Box<dyn std::error::Error>> {
    match note.method.as_str() {
        m if m == DidOpenTextDocument::METHOD => {
            let params: DidOpenTextDocumentParams = serde_json::from_value(note.params)?;
            let uri = params.text_document.uri.to_string();
            let text = params.text_document.text;
            docs.insert(uri.clone(), text.clone());
            publish(connection, &params.text_document.uri, &text)?;
        }
        m if m == DidChangeTextDocument::METHOD => {
            let params: DidChangeTextDocumentParams = serde_json::from_value(note.params)?;
            let uri = params.text_document.uri.to_string();
            // Phase 1 uses FULL sync: the last content-change is
            // the complete document text. Take the last one.
            if let Some(change) = params.content_changes.into_iter().last() {
                docs.insert(uri.clone(), change.text.clone());
                publish(connection, &params.text_document.uri, &change.text)?;
            }
        }
        m if m == DidSaveTextDocument::METHOD => {
            let params: DidSaveTextDocumentParams = serde_json::from_value(note.params)?;
            // Re-run diagnostics on save in case the editor's
            // on-disk content differs from the in-memory copy
            // (some editors batch saves).
            let uri_str = params.text_document.uri.to_string();
            if let Some(text) = docs.get(&uri_str) {
                let text = text.to_string();
                publish(connection, &params.text_document.uri, &text)?;
            }
        }
        m if m == DidCloseTextDocument::METHOD => {
            let params: DidCloseTextDocumentParams = serde_json::from_value(note.params)?;
            docs.remove(&params.text_document.uri.to_string());
            // Clear any pending diagnostics for the closed doc.
            let empty = PublishDiagnosticsParams {
                uri: params.text_document.uri,
                diagnostics: Vec::new(),
                version: None,
            };
            send_notification::<PublishDiagnostics>(connection, empty)?;
        }
        _ => {
            // Silently ignore notifications we don't handle yet.
            // Includes initialized, didChangeConfiguration, etc.
        }
    }
    Ok(())
}

fn publish(
    connection: &Connection,
    uri: &Uri,
    src: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = uri_to_path(uri);
    let diagnostics = diagnostics_for_source(src, path.as_deref());
    let params = PublishDiagnosticsParams {
        uri: uri.clone(),
        diagnostics,
        version: None,
    };
    send_notification::<PublishDiagnostics>(connection, params)
}

fn send_notification<N: NotificationTrait>(
    connection: &Connection,
    params: N::Params,
) -> Result<(), Box<dyn std::error::Error>>
where
    N::Params: serde::Serialize,
{
    let note = Notification {
        method: N::METHOD.into(),
        params: serde_json::to_value(params)?,
    };
    connection.sender.send(Message::Notification(note))?;
    Ok(())
}

/// Extract the filesystem path from a `file://` URI. Returns `None`
/// for non-file URIs (e.g. `untitled://` scratch buffers).
fn uri_to_path(uri: &Uri) -> Option<String> {
    let s = uri.to_string();
    s.strip_prefix("file://").map(|p| p.to_string())
}
