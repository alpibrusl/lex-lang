//! `lex-lsp` binary — LSP server over stdin/stdout (#304 phases 1–2a).
//!
//! Phase 1 (read-only diagnostics) plus phase 2a (hover, definition,
//! completion) of the rollout in #304. Phase 2b (cross-file
//! navigation, references), phase 3 (code actions backed by #280's
//! typed transforms), and phase 4 (RepairHint surface) are queued
//! as follow-up slices.

use lex_lsp::{
    analyze_source, completions, definition_at, diagnostics_for_source, hover_at, Documents,
};
use lsp_server::{Connection, Message, Notification, Request, Response};
use lsp_types::notification::{
    DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, DidSaveTextDocument,
    Notification as NotificationTrait, PublishDiagnostics,
};
use lsp_types::request::{Completion, GotoDefinition, HoverRequest, Request as RequestTrait};
use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionOptions, CompletionParams, CompletionResponse,
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DidSaveTextDocumentParams, GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverContents,
    HoverParams, HoverProviderCapability, InitializeParams, InitializeResult, Location,
    MarkupContent, MarkupKind, OneOf, PublishDiagnosticsParams, Range, ServerCapabilities,
    ServerInfo, TextDocumentSyncCapability, TextDocumentSyncKind, Uri,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("lex-lsp starting (phases 1+2a: diagnostics, hover, definition, completion)");

    let (connection, io_threads) = Connection::stdio();
    let server_capabilities = ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        definition_provider: Some(OneOf::Left(true)),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        completion_provider: Some(CompletionOptions {
            // Trigger on `.` so `io.<TAB>` completes module members
            // when phase 2b lands. Phase 2a only returns the imports
            // themselves on a bare cursor.
            trigger_characters: Some(vec![".".to_string()]),
            ..Default::default()
        }),
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
                handle_request(&connection, &docs, req)?;
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

fn handle_request(
    connection: &Connection,
    docs: &Documents,
    req: Request,
) -> Result<(), Box<dyn std::error::Error>> {
    let resp = match req.method.as_str() {
        m if m == HoverRequest::METHOD => {
            let params: HoverParams = serde_json::from_value(req.params)?;
            let uri = params
                .text_document_position_params
                .text_document
                .uri
                .to_string();
            let pos = params.text_document_position_params.position;
            let result = docs
                .get(&uri)
                .and_then(|src| {
                    let file = analyze_source(src)?;
                    let md = hover_at(&file, src, pos)?;
                    Some(Hover {
                        contents: HoverContents::Markup(MarkupContent {
                            kind: MarkupKind::Markdown,
                            value: md,
                        }),
                        range: None,
                    })
                });
            Response {
                id: req.id,
                result: Some(serde_json::to_value(result)?),
                error: None,
            }
        }
        m if m == GotoDefinition::METHOD => {
            let params: GotoDefinitionParams = serde_json::from_value(req.params)?;
            let td = &params.text_document_position_params.text_document;
            let uri_str = td.uri.to_string();
            let pos = params.text_document_position_params.position;
            let result: Option<GotoDefinitionResponse> = docs
                .get(&uri_str)
                .and_then(|src| {
                    let file = analyze_source(src)?;
                    let def = definition_at(&file, src, pos)?;
                    Some(GotoDefinitionResponse::Scalar(Location {
                        uri: td.uri.clone(),
                        range: Range { start: def, end: def },
                    }))
                });
            Response {
                id: req.id,
                result: Some(serde_json::to_value(result)?),
                error: None,
            }
        }
        m if m == Completion::METHOD => {
            let params: CompletionParams = serde_json::from_value(req.params)?;
            let uri = params.text_document_position.text_document.uri.to_string();
            let items: Vec<CompletionItem> = docs
                .get(&uri)
                .and_then(analyze_source)
                .map(|file| {
                    completions(&file)
                        .into_iter()
                        .map(|(label, detail, kind)| CompletionItem {
                            label,
                            detail: Some(detail),
                            kind: completion_kind_from_code(kind),
                            ..Default::default()
                        })
                        .collect()
                })
                .unwrap_or_default();
            Response {
                id: req.id,
                result: Some(serde_json::to_value(CompletionResponse::Array(items))?),
                error: None,
            }
        }
        _ => Response {
            id: req.id,
            result: None,
            error: Some(lsp_server::ResponseError {
                code: lsp_server::ErrorCode::MethodNotFound as i32,
                message: format!("`{}` not supported by lex-lsp", req.method),
                data: None,
            }),
        },
    };
    connection.sender.send(Message::Response(resp))?;
    Ok(())
}

fn completion_kind_from_code(code: u8) -> Option<CompletionItemKind> {
    match code {
        3 => Some(CompletionItemKind::FUNCTION),
        9 => Some(CompletionItemKind::MODULE),
        _ => None,
    }
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
            let uri_str = params.text_document.uri.to_string();
            if let Some(text) = docs.get(&uri_str) {
                let text = text.to_string();
                publish(connection, &params.text_document.uri, &text)?;
            }
        }
        m if m == DidCloseTextDocument::METHOD => {
            let params: DidCloseTextDocumentParams = serde_json::from_value(note.params)?;
            docs.remove(&params.text_document.uri.to_string());
            let empty = PublishDiagnosticsParams {
                uri: params.text_document.uri,
                diagnostics: Vec::new(),
                version: None,
            };
            send_notification::<PublishDiagnostics>(connection, empty)?;
        }
        _ => {
            // Silently ignore notifications we don't handle yet.
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
