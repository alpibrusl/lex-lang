//! `lex tool-registry serve` — runtime tool registration over HTTP.
//!
//! The headline pitch of agent-tool says: "the function signature IS
//! the security policy." The tool registry puts that on a network.
//! Anyone can POST a Lex body + an effect declaration and get back a
//! stable `/tools/{id}/invoke` endpoint. Each tool's effect manifest
//! is published at `/tools/{id}` — the contract is auditable by any
//! caller, not buried in the host's source.
//!
//! Endpoints:
//!
//!   POST /tools              register a tool; body is JSON:
//!                              { name, body, allowed_effects?, allow_fs_read?,
//!                                allow_net_host? }
//!                            -> 201 { id, manifest }
//!                            -> 400 { error_kind, detail }   (type-check / policy)
//!
//!   GET  /tools              list registered tools
//!                            -> 200 [ { id, name, allowed_effects } ]
//!
//!   GET  /tools/{id}         tool manifest
//!                            -> 200 { id, name, allowed_effects, ... }
//!                            -> 404
//!
//!   POST /tools/{id}/invoke  run a tool. body is JSON: { input }
//!                            -> 200 { output }
//!                            -> 404  / 5xx { error_kind, detail }
//!
//! Storage is in-memory. The registry is single-process; persistence
//! across restarts is a v1.1 follow-up.

use anyhow::{anyhow, Result};
use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Program, Value};
use lex_runtime::{check_program as check_policy, DefaultHandler, Policy};
use lex_syntax::parse_source;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tiny_http::{Header, Method, Response, Server};

/// One registered tool. We hold the compiled bytecode (`Arc<Program>`)
/// so that invokes don't recompile per request.
struct ToolEntry {
    id: String,
    name: String,
    body: String,
    allowed_effects: Vec<String>,
    allow_fs_read: Vec<PathBuf>,
    allow_net_host: Vec<String>,
    program: Arc<Program>,
}

#[derive(Default)]
struct Registry {
    tools: Mutex<Vec<ToolEntry>>,
    next_id: Mutex<u64>,
}

impl Registry {
    fn alloc_id(&self) -> String {
        let mut n = self.next_id.lock().unwrap();
        *n += 1;
        format!("t{:08x}", *n)
    }
    fn insert(&self, entry: ToolEntry) {
        self.tools.lock().unwrap().push(entry);
    }
    fn get<F, R>(&self, id: &str, f: F) -> Option<R>
    where F: FnOnce(&ToolEntry) -> R
    {
        let g = self.tools.lock().unwrap();
        g.iter().find(|t| t.id == id).map(f)
    }
    fn list(&self) -> Vec<serde_json::Value> {
        self.tools.lock().unwrap().iter().map(|t| json!({
            "id": t.id,
            "name": t.name,
            "allowed_effects": t.allowed_effects,
            "allow_fs_read": t.allow_fs_read.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
            "allow_net_host": t.allow_net_host,
        })).collect()
    }
}

#[derive(Deserialize)]
struct RegisterReq {
    name: String,
    body: String,
    #[serde(default)] allowed_effects: Vec<String>,
    #[serde(default)] allow_fs_read: Vec<String>,
    #[serde(default)] allow_net_host: Vec<String>,
}

#[derive(Deserialize)]
struct InvokeReq {
    #[serde(default)] input: String,
}

#[derive(Serialize)]
struct ManifestOut<'a> {
    id: &'a str,
    name: &'a str,
    allowed_effects: &'a [String],
    allow_fs_read: Vec<String>,
    allow_net_host: &'a [String],
}

/// Subcommand entry point: `lex tool-registry serve --port N`.
pub fn cmd_tool_registry(args: &[String]) -> Result<()> {
    let sub = args.first().ok_or_else(|| anyhow!("usage: lex tool-registry serve [--port N]"))?;
    if sub != "serve" {
        return Err(anyhow!("unknown subcommand `{sub}`; only `serve` is supported"));
    }
    let mut port: u16 = 8300;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--port" => {
                port = args.get(i + 1).ok_or_else(|| anyhow!("--port needs a value"))?
                    .parse().map_err(|e| anyhow!("--port: {e}"))?;
                i += 2;
            }
            other => return Err(anyhow!("unknown flag `{other}`")),
        }
    }
    let registry = Arc::new(Registry::default());
    serve(port, registry)
}

fn serve(port: u16, registry: Arc<Registry>) -> Result<()> {
    let server = Server::http(("127.0.0.1", port))
        .map_err(|e| anyhow!("bind {port}: {e}"))?;
    eprintln!("lex tool-registry: listening on http://127.0.0.1:{port}");
    for req in server.incoming_requests() {
        let registry = Arc::clone(&registry);
        std::thread::spawn(move || handle(req, registry));
    }
    Ok(())
}

fn json_response(status: u16, body: serde_json::Value) -> Response<std::io::Cursor<Vec<u8>>> {
    let body_str = body.to_string();
    let header = Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap();
    Response::from_string(body_str).with_status_code(status).with_header(header)
}

fn handle(mut req: tiny_http::Request, registry: Arc<Registry>) {
    let method = req.method().clone();
    let url = req.url().to_string();
    let path = url.split('?').next().unwrap_or(&url).to_string();

    let mut body_str = String::new();
    use std::io::Read;
    let _ = Read::read_to_string(req.as_reader(), &mut body_str);

    let resp = match (&method, path.as_str()) {
        (Method::Post, "/tools") => post_tools(&body_str, &registry),
        (Method::Get, "/tools")  => json_response(200, json!(registry.list())),
        (Method::Get, p) if p.starts_with("/tools/") => {
            let rest = &p["/tools/".len()..];
            if let Some(id) = rest.strip_suffix("/invoke") {
                json_response(405, json!({ "error": "use POST", "id": id }))
            } else {
                get_tool(rest, &registry)
            }
        }
        (Method::Post, p) if p.starts_with("/tools/") && p.ends_with("/invoke") => {
            let id = &p["/tools/".len()..p.len() - "/invoke".len()];
            invoke_tool(id, &body_str, &registry)
        }
        _ => json_response(404, json!({ "error": "not found", "path": path })),
    };
    let _ = req.respond(resp);
}

fn post_tools(body_str: &str, registry: &Registry) -> Response<std::io::Cursor<Vec<u8>>> {
    let parsed: RegisterReq = match serde_json::from_str(body_str) {
        Ok(p) => p,
        Err(e) => return json_response(400, json!({
            "error_kind": "bad_json", "detail": e.to_string(),
        })),
    };

    // 1) Build the same tool program agent-tool builds, splice the
    // body into a fixed signature with the declared effects.
    let src = build_tool_program(&parsed.body, &parsed.allowed_effects);

    // 2) Parse + type-check.
    let prog = match parse_source(&src) {
        Ok(p) => p,
        Err(e) => return json_response(400, json!({
            "error_kind": "parse", "detail": e.to_string(),
        })),
    };
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        let detail: Vec<String> = errs.iter().map(|e| e.to_string()).collect();
        return json_response(400, json!({
            "error_kind": "type_check", "detail": detail,
        }));
    }

    // 3) Compile + policy gate.
    let bc = compile_program(&stages);
    let policy = build_policy(&parsed);
    if let Err(violations) = check_policy(&bc, &policy) {
        let detail: Vec<String> = violations.iter().map(|v| v.to_string()).collect();
        return json_response(400, json!({
            "error_kind": "policy", "detail": detail,
        }));
    }

    let entry = ToolEntry {
        id: registry.alloc_id(),
        name: parsed.name.clone(),
        body: parsed.body.clone(),
        allowed_effects: parsed.allowed_effects.clone(),
        allow_fs_read: parsed.allow_fs_read.iter().map(PathBuf::from).collect(),
        allow_net_host: parsed.allow_net_host.clone(),
        program: Arc::new(bc),
    };
    let id = entry.id.clone();
    let manifest = ManifestOut {
        id: &entry.id,
        name: &entry.name,
        allowed_effects: &entry.allowed_effects,
        allow_fs_read: entry.allow_fs_read.iter().map(|p| p.display().to_string()).collect(),
        allow_net_host: &entry.allow_net_host,
    };
    let manifest_json = serde_json::to_value(&manifest).unwrap_or(json!({}));
    registry.insert(entry);
    json_response(201, json!({
        "id": id,
        "manifest": manifest_json,
        "endpoint": format!("/tools/{id}/invoke"),
    }))
}

fn get_tool(id: &str, registry: &Registry) -> Response<std::io::Cursor<Vec<u8>>> {
    match registry.get(id, |t| json!({
        "id": t.id,
        "name": t.name,
        "body": t.body,
        "allowed_effects": t.allowed_effects,
        "allow_fs_read": t.allow_fs_read.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        "allow_net_host": t.allow_net_host,
    })) {
        Some(v) => json_response(200, v),
        None => json_response(404, json!({ "error": "not found", "id": id })),
    }
}

fn invoke_tool(id: &str, body_str: &str, registry: &Registry) -> Response<std::io::Cursor<Vec<u8>>> {
    let parsed: InvokeReq = match serde_json::from_str(body_str) {
        Ok(p) => p,
        Err(e) => return json_response(400, json!({
            "error_kind": "bad_json", "detail": e.to_string(),
        })),
    };
    // Pull out the program + policy under the lock; release before
    // running so the VM doesn't hold the registry mutex.
    let setup = registry.get(id, |t| (
        Arc::clone(&t.program),
        build_policy_from_entry(t),
    ));
    let (prog, policy) = match setup {
        Some(x) => x,
        None => return json_response(404, json!({ "error": "not found", "id": id })),
    };
    let handler = DefaultHandler::new(policy).with_program(Arc::clone(&prog));
    let mut vm = Vm::with_handler(&prog, Box::new(handler));
    vm.set_step_limit(1_000_000);
    match vm.call("tool", vec![Value::Str(parsed.input)]) {
        Ok(Value::Str(s)) => json_response(200, json!({ "output": s })),
        Ok(other) => json_response(200, json!({ "output": format!("{other:?}") })),
        Err(e) => {
            let msg = format!("{e}");
            let kind = if msg.contains("step limit") { "step_limit" }
                else if msg.contains("outside --allow-fs-read")
                    || msg.contains("not in --allow-net-host") { "policy_runtime" }
                else { "runtime" };
            json_response(500, json!({ "error_kind": kind, "detail": msg }))
        }
    }
}

fn build_policy(req: &RegisterReq) -> Policy {
    let mut policy = Policy::pure();
    policy.allow_effects = req.allowed_effects.iter().cloned().collect::<BTreeSet<_>>();
    policy.allow_fs_read = req.allow_fs_read.iter().map(PathBuf::from).collect();
    policy.allow_net_host = req.allow_net_host.clone();
    policy
}

fn build_policy_from_entry(t: &ToolEntry) -> Policy {
    let mut policy = Policy::pure();
    policy.allow_effects = t.allowed_effects.iter().cloned().collect::<BTreeSet<_>>();
    policy.allow_fs_read = t.allow_fs_read.clone();
    policy.allow_net_host = t.allow_net_host.clone();
    policy
}

/// Build the same fixed-signature program the agent-tool flow uses.
/// Kept here rather than cross-imported from main.rs because the
/// registry needs it via a stable public-ish path; if main.rs's
/// version drifts, this becomes the canonical one.
fn build_tool_program(body: &str, allowed_effects: &[String]) -> String {
    let imports = [
        "import \"std.io\"    as io",
        "import \"std.net\"   as net",
        "import \"std.str\"   as str",
        "import \"std.int\"   as int",
        "import \"std.float\" as float",
        "import \"std.list\"  as list",
        "import \"std.json\"  as json",
        "import \"std.bytes\" as bytes",
        "import \"std.time\"  as time",
    ].join("\n");
    let effects = if allowed_effects.is_empty() {
        String::new()
    } else {
        format!("[{}] ", allowed_effects.join(", "))
    };
    format!("{imports}\n\nfn tool(input :: Str) -> {effects}Str {{\n{body}\n}}\n")
}
