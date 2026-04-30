//! Request routing for the agent API.
//!
//! Each handler is a synchronous function that returns
//! `Result<serde_json::Value, ApiError>`. The dispatcher wraps the result
//! in an HTTP response — successes as 200 with the JSON body, structured
//! errors as 4xx/5xx with a JSON envelope.

use indexmap::IndexMap;
use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{check_program as check_policy, DefaultHandler, Policy};
use lex_store::Store;
use lex_syntax::parse_source;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tiny_http::{Header, Method, Request, Response};

pub struct State {
    pub store: Mutex<Store>,
}

impl State {
    pub fn open(root: PathBuf) -> anyhow::Result<Self> {
        Ok(Self { store: Mutex::new(Store::open(root)?) })
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct ErrorEnvelope {
    error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<serde_json::Value>,
}

fn json_response(status: u16, body: &serde_json::Value) -> Response<std::io::Cursor<Vec<u8>>> {
    let bytes = serde_json::to_vec(body).unwrap_or_else(|_| b"{}".to_vec());
    Response::from_data(bytes)
        .with_status_code(status)
        .with_header(Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap())
}

fn error_response(status: u16, msg: impl Into<String>) -> Response<std::io::Cursor<Vec<u8>>> {
    json_response(status, &serde_json::to_value(ErrorEnvelope {
        error: msg.into(), detail: None,
    }).unwrap())
}

fn error_with_detail(status: u16, msg: impl Into<String>, detail: serde_json::Value)
    -> Response<std::io::Cursor<Vec<u8>>>
{
    json_response(status, &serde_json::to_value(ErrorEnvelope {
        error: msg.into(), detail: Some(detail),
    }).unwrap())
}

pub fn handle(state: Arc<State>, mut req: Request) -> std::io::Result<()> {
    let method = req.method().clone();
    let url = req.url().to_string();
    let path = url.split('?').next().unwrap_or("").to_string();
    let query = url.split_once('?').map(|(_, q)| q.to_string()).unwrap_or_default();

    let mut body = String::new();
    let _ = req.as_reader().read_to_string(&mut body);

    let resp = route(&state, &method, &path, &query, &body);
    req.respond(resp)
}

fn route(
    state: &State,
    method: &Method,
    path: &str,
    query: &str,
    body: &str,
) -> Response<std::io::Cursor<Vec<u8>>> {
    match (method, path) {
        (Method::Get, "/v1/health") => json_response(200, &serde_json::json!({"ok": true})),
        (Method::Post, "/v1/parse") => parse_handler(body),
        (Method::Post, "/v1/check") => check_handler(body),
        (Method::Post, "/v1/publish") => publish_handler(state, body),
        (Method::Get, p) if p.starts_with("/v1/stage/") => {
            let id = &p["/v1/stage/".len()..];
            stage_handler(state, id)
        }
        (Method::Post, "/v1/run") => run_handler(state, body, false),
        (Method::Post, "/v1/replay") => run_handler(state, body, true),
        (Method::Get, p) if p.starts_with("/v1/trace/") => {
            let id = &p["/v1/trace/".len()..];
            trace_handler(state, id)
        }
        (Method::Get, "/v1/diff") => diff_handler(state, query),
        _ => error_response(404, format!("unknown route: {method:?} {path}")),
    }
}

#[derive(Deserialize)]
struct ParseReq { source: String }

fn parse_handler(body: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    let req: ParseReq = match serde_json::from_str(body) {
        Ok(r) => r, Err(e) => return error_response(400, format!("bad request: {e}")),
    };
    match parse_source(&req.source) {
        Ok(prog) => {
            let stages = canonicalize_program(&prog);
            json_response(200, &serde_json::to_value(&stages).unwrap())
        }
        Err(e) => error_response(400, format!("syntax error: {e}")),
    }
}

fn check_handler(body: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    let req: ParseReq = match serde_json::from_str(body) {
        Ok(r) => r, Err(e) => return error_response(400, format!("bad request: {e}")),
    };
    let prog = match parse_source(&req.source) {
        Ok(p) => p, Err(e) => return error_response(400, format!("syntax error: {e}")),
    };
    let stages = canonicalize_program(&prog);
    match lex_types::check_program(&stages) {
        Ok(_) => json_response(200, &serde_json::json!({"ok": true})),
        Err(errs) => json_response(422, &serde_json::to_value(&errs).unwrap()),
    }
}

#[derive(Deserialize)]
struct PublishReq { source: String, #[serde(default)] activate: bool }

fn publish_handler(state: &State, body: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    let req: PublishReq = match serde_json::from_str(body) {
        Ok(r) => r, Err(e) => return error_response(400, format!("bad request: {e}")),
    };
    let prog = match parse_source(&req.source) {
        Ok(p) => p, Err(e) => return error_response(400, format!("syntax error: {e}")),
    };
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        return error_with_detail(422, "type errors", serde_json::to_value(&errs).unwrap());
    }
    let store = state.store.lock().unwrap();
    let mut out = Vec::new();
    for s in &stages {
        if matches!(s, lex_ast::Stage::Import(_)) { continue; }
        match store.publish(s) {
            Ok(id) => {
                if req.activate {
                    if let Err(e) = store.activate(&id) {
                        return error_response(500, format!("activate: {e}"));
                    }
                }
                let name = match s {
                    lex_ast::Stage::FnDecl(fd) => fd.name.clone(),
                    lex_ast::Stage::TypeDecl(td) => td.name.clone(),
                    _ => "?".into(),
                };
                let sig = lex_ast::sig_id(s).unwrap_or_default();
                let status = format!("{:?}", store.get_status(&id).unwrap_or(lex_store::StageStatus::Draft)).to_lowercase();
                out.push(serde_json::json!({"name": name, "sig_id": sig, "stage_id": id, "status": status}));
            }
            Err(e) => return error_response(500, format!("publish: {e}")),
        }
    }
    json_response(200, &serde_json::Value::Array(out))
}

fn stage_handler(state: &State, id: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    let store = state.store.lock().unwrap();
    let meta = match store.get_metadata(id) {
        Ok(m) => m, Err(e) => return error_response(404, format!("{e}")),
    };
    let ast = match store.get_ast(id) {
        Ok(a) => a, Err(e) => return error_response(404, format!("{e}")),
    };
    let status = format!("{:?}", store.get_status(id).unwrap_or(lex_store::StageStatus::Draft)).to_lowercase();
    json_response(200, &serde_json::json!({
        "metadata": meta,
        "ast": ast,
        "status": status,
    }))
}

#[derive(Deserialize, Default)]
struct PolicyJson {
    #[serde(default)] allow_effects: Vec<String>,
    #[serde(default)] allow_fs_read: Vec<String>,
    #[serde(default)] allow_fs_write: Vec<String>,
    #[serde(default)] budget: Option<u64>,
}

impl PolicyJson {
    fn into_policy(self) -> Policy {
        Policy {
            allow_effects: self.allow_effects.into_iter().collect::<BTreeSet<_>>(),
            allow_fs_read: self.allow_fs_read.into_iter().map(PathBuf::from).collect(),
            allow_fs_write: self.allow_fs_write.into_iter().map(PathBuf::from).collect(),
            budget: self.budget,
        }
    }
}

#[derive(Deserialize)]
struct RunReq {
    source: String,
    #[serde(rename = "fn")] func: String,
    #[serde(default)] args: Vec<serde_json::Value>,
    #[serde(default)] policy: PolicyJson,
    #[serde(default)] overrides: IndexMap<String, serde_json::Value>,
}

fn run_handler(state: &State, body: &str, with_overrides: bool) -> Response<std::io::Cursor<Vec<u8>>> {
    let req: RunReq = match serde_json::from_str(body) {
        Ok(r) => r, Err(e) => return error_response(400, format!("bad request: {e}")),
    };
    let prog = match parse_source(&req.source) {
        Ok(p) => p, Err(e) => return error_response(400, format!("syntax error: {e}")),
    };
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        return error_with_detail(422, "type errors", serde_json::to_value(&errs).unwrap());
    }
    let bc = compile_program(&stages);
    let policy = req.policy.into_policy();
    if let Err(violations) = check_policy(&bc, &policy) {
        return error_with_detail(403, "policy violation", serde_json::to_value(&violations).unwrap());
    }

    let mut recorder = lex_trace::Recorder::new();
    if with_overrides && !req.overrides.is_empty() {
        recorder = recorder.with_overrides(req.overrides);
    }
    let handle = recorder.handle();
    let handler = DefaultHandler::new(policy);
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.set_tracer(Box::new(recorder));

    let vargs: Vec<Value> = req.args.iter().map(json_to_value).collect();
    let started = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let result = vm.call(&req.func, vargs);
    let ended = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();

    let store = state.store.lock().unwrap();
    let (root_out, root_err, status) = match &result {
        Ok(v) => (Some(value_to_json(v)), None, 200u16),
        Err(e) => (None, Some(format!("{e}")), 200u16),
    };
    let tree = handle.finalize(req.func.clone(), serde_json::Value::Null,
        root_out.clone(), root_err.clone(), started, ended);
    let run_id = match store.save_trace(&tree) {
        Ok(id) => id,
        Err(e) => return error_response(500, format!("save_trace: {e}")),
    };

    let mut body = serde_json::json!({
        "run_id": run_id,
        "output": root_out,
    });
    if let Some(err) = root_err {
        body["error"] = serde_json::Value::String(err);
    }
    json_response(status, &body)
}

fn trace_handler(state: &State, id: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    let store = state.store.lock().unwrap();
    match store.load_trace(id) {
        Ok(t) => json_response(200, &serde_json::to_value(&t).unwrap()),
        Err(e) => error_response(404, format!("{e}")),
    }
}

fn diff_handler(state: &State, query: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    let mut a = None;
    let mut b = None;
    for kv in query.split('&') {
        if let Some((k, v)) = kv.split_once('=') {
            match k { "a" => a = Some(v.to_string()), "b" => b = Some(v.to_string()), _ => {} }
        }
    }
    let (Some(a), Some(b)) = (a, b) else {
        return error_response(400, "missing a or b query params");
    };
    let store = state.store.lock().unwrap();
    let ta = match store.load_trace(&a) { Ok(t) => t, Err(e) => return error_response(404, format!("a: {e}")) };
    let tb = match store.load_trace(&b) { Ok(t) => t, Err(e) => return error_response(404, format!("b: {e}")) };
    match lex_trace::diff_runs(&ta, &tb) {
        Some(d) => json_response(200, &serde_json::to_value(&d).unwrap()),
        None => json_response(200, &serde_json::json!({"divergence": null})),
    }
}

fn json_to_value(v: &serde_json::Value) -> Value {
    use serde_json::Value as J;
    match v {
        J::Null => Value::Unit,
        J::Bool(b) => Value::Bool(*b),
        J::Number(n) => {
            if let Some(i) = n.as_i64() { Value::Int(i) }
            else if let Some(f) = n.as_f64() { Value::Float(f) }
            else { Value::Unit }
        }
        J::String(s) => Value::Str(s.clone()),
        J::Array(items) => Value::List(items.iter().map(json_to_value).collect()),
        J::Object(map) => {
            if let (Some(J::String(name)), Some(J::Array(args))) =
                (map.get("$variant"), map.get("args"))
            {
                return Value::Variant {
                    name: name.clone(),
                    args: args.iter().map(json_to_value).collect(),
                };
            }
            let mut out = IndexMap::new();
            for (k, v) in map { out.insert(k.clone(), json_to_value(v)); }
            Value::Record(out)
        }
    }
}

fn value_to_json(v: &Value) -> serde_json::Value {
    use serde_json::Value as J;
    match v {
        Value::Int(n) => J::from(*n),
        Value::Float(f) => J::from(*f),
        Value::Bool(b) => J::Bool(*b),
        Value::Str(s) => J::String(s.clone()),
        Value::Bytes(b) => J::String(b.iter().map(|b| format!("{:02x}", b)).collect()),
        Value::Unit => J::Null,
        Value::List(items) => J::Array(items.iter().map(value_to_json).collect()),
        Value::Tuple(items) => J::Array(items.iter().map(value_to_json).collect()),
        Value::Record(fields) => {
            let mut m = serde_json::Map::new();
            for (k, v) in fields { m.insert(k.clone(), value_to_json(v)); }
            J::Object(m)
        }
        Value::Variant { name, args } => {
            let mut m = serde_json::Map::new();
            m.insert("$variant".into(), J::String(name.clone()));
            m.insert("args".into(), J::Array(args.iter().map(value_to_json).collect()));
            J::Object(m)
        }
        Value::Closure { fn_id, .. } => J::String(format!("<closure fn_{fn_id}>")),
    }
}

