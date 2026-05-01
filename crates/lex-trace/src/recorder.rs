//! Trace recorder — implements `lex_bytecode::vm::Tracer` and builds a
//! `TraceTree` as the VM executes.

use indexmap::IndexMap;
use lex_bytecode::vm::Tracer;
use lex_bytecode::Value;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunId(pub String);

impl RunId {
    pub fn new(seed: &str) -> Self {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(seed.as_bytes());
        h.update(format!("{:?}", std::time::SystemTime::now()).as_bytes());
        let r = h.finalize();
        let mut hex = String::with_capacity(64);
        for b in r { hex.push_str(&format!("{:02x}", b)); }
        RunId(hex)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TraceNodeKind { Call, Effect }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TraceNode {
    pub node_id: String,
    pub kind: TraceNodeKind,
    /// For `Call`: the function name. For `Effect`: `kind.op` (e.g. `io.print`).
    pub target: String,
    pub input: serde_json::Value,
    /// `Some` on success; `None` if the node ended in error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub started_at: u64,
    pub ended_at: u64,
    #[serde(default)]
    pub children: Vec<TraceNode>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TraceTree {
    pub run_id: String,
    pub root_target: String,
    pub root_input: serde_json::Value,
    pub root_output: Option<serde_json::Value>,
    pub root_error: Option<String>,
    pub started_at: u64,
    pub ended_at: u64,
    pub nodes: Vec<TraceNode>,
}

impl TraceTree {
    /// Find a node by `NodeId`, depth-first.
    pub fn find(&self, node_id: &str) -> Option<&TraceNode> {
        for n in &self.nodes {
            if let Some(found) = find_in(n, node_id) { return Some(found); }
        }
        None
    }
}

fn find_in<'a>(n: &'a TraceNode, target: &str) -> Option<&'a TraceNode> {
    if n.node_id == target { return Some(n); }
    for c in &n.children {
        if let Some(f) = find_in(c, target) { return Some(f); }
    }
    None
}

/// Tracer that builds a `TraceTree`. The tree is shared via `Arc<Mutex>`
/// so callers can read it after the VM finishes.
pub struct Recorder {
    state: Arc<Mutex<RecorderState>>,
}

pub(crate) struct RecorderState {
    /// Open frames: each entry has its inputs filled in but `output`/
    /// `error`/`ended_at` not yet known. Children of an open frame are
    /// staged into a sibling buffer; on `exit`, they get attached to the
    /// node that's closing.
    open: Vec<OpenFrame>,
    /// Top-level finished nodes (the call we're tracing might span the
    /// whole VM run, so this is normally a single node tree).
    completed: Vec<TraceNode>,
    /// Effect overrides for replay; keyed by NodeId.
    pub(crate) overrides: IndexMap<String, serde_json::Value>,
}

struct OpenFrame {
    node: TraceNode,
    /// Children that have completed under this frame.
    children: Vec<TraceNode>,
}

impl Recorder {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(RecorderState {
                open: Vec::new(),
                completed: Vec::new(),
                overrides: IndexMap::new(),
            })),
        }
    }

    /// Returned handle stays valid after the tracer is moved into the VM.
    pub fn handle(&self) -> Handle {
        Handle { state: Arc::clone(&self.state) }
    }

    /// Pre-load effect overrides for replay.
    pub fn with_overrides(self, overrides: IndexMap<String, serde_json::Value>) -> Self {
        self.state.lock().unwrap().overrides = overrides;
        self
    }
}

impl Default for Recorder { fn default() -> Self { Self::new() } }

#[derive(Clone)]
pub struct Handle {
    state: Arc<Mutex<RecorderState>>,
}

impl Handle {
    /// Drain the recorder into a finished `TraceTree`. Call after the VM
    /// run returns. `root_target` and `root_input` describe the top-level
    /// call (e.g. the `lex run` entry).
    pub fn finalize(
        &self,
        root_target: impl Into<String>,
        root_input: serde_json::Value,
        root_output: Option<serde_json::Value>,
        root_error: Option<String>,
        started_at: u64,
        ended_at: u64,
    ) -> TraceTree {
        let st = self.state.lock().unwrap();
        TraceTree {
            run_id: RunId::new(&format!("{}-{}", started_at, ended_at)).0,
            root_target: root_target.into(),
            root_input,
            root_output,
            root_error,
            started_at,
            ended_at,
            nodes: st.completed.clone(),
        }
    }
}

fn now_unix() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn values_to_json(args: &[Value]) -> serde_json::Value {
    serde_json::Value::Array(args.iter().map(value_to_json).collect())
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
        Value::F64Array { rows, cols, data } => {
            let mut m = serde_json::Map::new();
            m.insert("$f64_array".into(), J::Bool(true));
            m.insert("rows".into(), J::from(*rows));
            m.insert("cols".into(), J::from(*cols));
            m.insert("data".into(), J::Array(data.iter().map(|f| J::from(*f)).collect()));
            J::Object(m)
        }
        Value::Map(m) => {
            let mut o = serde_json::Map::new();
            o.insert("$map".into(), J::Bool(true));
            o.insert("entries".into(), J::Array(m.iter().map(|(k, v)| {
                J::Array(vec![value_to_json(&k.as_value()), value_to_json(v)])
            }).collect()));
            J::Object(o)
        }
        Value::Set(s) => {
            let mut o = serde_json::Map::new();
            o.insert("$set".into(), J::Bool(true));
            o.insert("items".into(), J::Array(
                s.iter().map(|k| value_to_json(&k.as_value())).collect()));
            J::Object(o)
        }
    }
}

pub(crate) fn json_to_value(v: &serde_json::Value) -> Value {
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
            // Detect the $variant shape we emit on the way out.
            if let (Some(serde_json::Value::String(name)), Some(serde_json::Value::Array(args))) =
                (map.get("$variant"), map.get("args"))
            {
                return Value::Variant {
                    name: name.clone(),
                    args: args.iter().map(json_to_value).collect(),
                };
            }
            let mut out = indexmap::IndexMap::new();
            for (k, v) in map { out.insert(k.clone(), json_to_value(v)); }
            Value::Record(out)
        }
    }
}

impl Tracer for Recorder {
    fn enter_call(&mut self, node_id: &str, name: &str, args: &[Value]) {
        let mut st = self.state.lock().unwrap();
        st.open.push(OpenFrame {
            node: TraceNode {
                node_id: node_id.to_string(),
                kind: TraceNodeKind::Call,
                target: name.to_string(),
                input: values_to_json(args),
                output: None,
                error: None,
                started_at: now_unix(),
                ended_at: 0,
                children: Vec::new(),
            },
            children: Vec::new(),
        });
    }

    fn enter_effect(&mut self, node_id: &str, kind: &str, op: &str, args: &[Value]) {
        let mut st = self.state.lock().unwrap();
        st.open.push(OpenFrame {
            node: TraceNode {
                node_id: node_id.to_string(),
                kind: TraceNodeKind::Effect,
                target: format!("{kind}.{op}"),
                input: values_to_json(args),
                output: None,
                error: None,
                started_at: now_unix(),
                ended_at: 0,
                children: Vec::new(),
            },
            children: Vec::new(),
        });
    }

    fn exit_ok(&mut self, value: &Value) {
        let mut st = self.state.lock().unwrap();
        if let Some(mut frame) = st.open.pop() {
            frame.node.ended_at = now_unix();
            frame.node.output = Some(value_to_json(value));
            frame.node.children = frame.children;
            attach_completed(&mut st, frame.node);
        }
    }

    fn exit_err(&mut self, message: &str) {
        let mut st = self.state.lock().unwrap();
        if let Some(mut frame) = st.open.pop() {
            frame.node.ended_at = now_unix();
            frame.node.error = Some(message.to_string());
            frame.node.children = frame.children;
            attach_completed(&mut st, frame.node);
        }
    }

    fn exit_call_tail(&mut self) {
        let mut st = self.state.lock().unwrap();
        if let Some(mut frame) = st.open.pop() {
            frame.node.ended_at = now_unix();
            // Tail call: tag with synthetic output so it shows as completed.
            frame.node.output = Some(serde_json::Value::Null);
            frame.node.children = frame.children;
            attach_completed(&mut st, frame.node);
        }
    }

    fn override_effect(&mut self, node_id: &str) -> Option<Value> {
        let st = self.state.lock().unwrap();
        st.overrides.get(node_id).map(json_to_value)
    }
}

fn attach_completed(st: &mut RecorderState, node: TraceNode) {
    if let Some(parent) = st.open.last_mut() {
        parent.children.push(node);
    } else {
        st.completed.push(node);
    }
}
