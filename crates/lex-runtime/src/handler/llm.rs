//! `agent` effect: MCP client calls, local / cloud LLM completion and the shared token-stream registry.

use super::*;

impl DefaultHandler {
    /// Implementation of `agent.call_mcp(server, tool, args_json)`.
    /// Goes through the LRU client cache (#197): the named server
    /// is spawned on first use and reused on subsequent calls.
    /// On failure the offending client is dropped so the next
    /// call respawns rather than silently failing forever.
    pub(super) fn dispatch_call_mcp(&mut self, args: Vec<Value>) -> Value {
        let server = match args.first() {
            Some(Value::Str(s)) => s.clone(),
            _ => return err(Value::Str(
                "agent.call_mcp(server, tool, args_json): server must be Str".into())),
        };
        let tool = match args.get(1) {
            Some(Value::Str(s)) => s.clone(),
            _ => return err(Value::Str(
                "agent.call_mcp(server, tool, args_json): tool must be Str".into())),
        };
        let args_json = match args.get(2) {
            Some(Value::Str(s)) => s.clone(),
            _ => return err(Value::Str(
                "agent.call_mcp(server, tool, args_json): args_json must be Str".into())),
        };
        let parsed: serde_json::Value = match serde_json::from_str(&args_json) {
            Ok(v) => v,
            Err(e) => return err(Value::Str(format!(
                "agent.call_mcp: args_json is not valid JSON: {e}").into())),
        };
        match self.mcp_clients.call(&server, &tool, parsed) {
            Ok(result) => ok(Value::Str(
                serde_json::to_string(&result).unwrap_or_else(|_| "null".into()).into())),
            Err(e) => err(Value::Str(e.into())),
        }
    }

    /// Implementation of `agent.cloud_stream(prompt) -> Result[Stream[Str], Str]`
    /// (#305 slice 3). The fixture path (`LEX_LLM_STREAM_FIXTURE`)
    /// splits the env-var value on `|` and yields each segment as
    /// one chunk; it's the load-bearing test hook. Live HTTP
    /// chunked-response support is deferred to a follow-up slice.
    pub(super) fn dispatch_cloud_stream(&mut self, args: Vec<Value>) -> Value {
        let _prompt = match args.first() {
            Some(Value::Str(s)) => s.clone(),
            _ => return err(Value::Str(
                "agent.cloud_stream(prompt): prompt must be Str".into())),
        };
        let chunks: Vec<String> = match std::env::var("LEX_LLM_STREAM_FIXTURE") {
            Ok(v) => v.split('|').map(|s| s.to_string()).collect(),
            Err(_) => return err(Value::Str(
                "agent.cloud_stream: live streaming not yet implemented; \
                 set LEX_LLM_STREAM_FIXTURE='chunk1|chunk2|…' for tests".into())),
        };
        let handle = self.register_stream(chunks.into_iter());
        ok(stream_handle_value(handle))
    }

    /// Implementation of `stream.next(s) -> Option[T]` (#305 slice 3).
    /// Returns `Some(chunk)` for each producer yield and `None` once
    /// the producer is exhausted. Unknown handle ids return `None`
    /// rather than erroring so streams can be safely consumed past
    /// the end (matches the semantics of `Iterator::next`).
    pub(super) fn dispatch_stream_next(&mut self, args: Vec<Value>) -> Value {
        let handle = match args.first().and_then(stream_handle_id) {
            Some(h) => h,
            None => return Value::Variant { name: "None".into(), args: vec![] },
        };
        let mut streams = match self.streams.lock() {
            Ok(g) => g,
            Err(_) => return Value::Variant { name: "None".into(), args: vec![] },
        };
        match streams.get_mut(&handle).and_then(|it| it.next()) {
            Some(chunk) => some(Value::Str(chunk.into())),
            None => {
                streams.remove(&handle);
                Value::Variant { name: "None".into(), args: vec![] }
            }
        }
    }

    /// Implementation of `stream.collect(s) -> List[T]` (#305 slice 3).
    /// Drains the producer eagerly. Unknown handles drain to an
    /// empty list so the contract is `collect ∘ collect = []`
    /// (idempotent on a closed stream).
    pub(super) fn dispatch_stream_collect(&mut self, args: Vec<Value>) -> Value {
        let handle = match args.first().and_then(stream_handle_id) {
            Some(h) => h,
            None => return Value::List(std::collections::VecDeque::new().into()),
        };
        let mut iter = {
            let mut streams = match self.streams.lock() {
                Ok(g) => g,
                Err(_) => return Value::List(std::collections::VecDeque::new().into()),
            };
            match streams.remove(&handle) {
                Some(it) => it,
                None => return Value::List(std::collections::VecDeque::new().into()),
            }
        };
        let mut out: std::collections::VecDeque<Value> = std::collections::VecDeque::new();
        for chunk in iter.by_ref() {
            out.push_back(Value::Str(chunk.into()));
        }
        Value::List(out.into())
    }

    /// Register a producer iterator and return its handle id. The
    /// handle is monotonic-counter-based so two streams created in
    /// quick succession get distinct ids.
    pub(super) fn register_stream<I>(&self, iter: I) -> String
    where
        I: Iterator<Item = String> + Send + 'static,
    {
        let id = self
            .next_stream_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let handle = format!("stream_{id}");
        if let Ok(mut streams) = self.streams.lock() {
            streams.insert(handle.clone(), Box::new(iter));
        }
        handle
    }
}

/// Build the runtime representation of a `Stream[T]` value:
/// `Variant("__StreamHandle", [Str(handle_id)])`. The opaque tag is
/// prefixed with `__` so it can't collide with a user-declared
/// variant.
pub(super) fn stream_handle_value(handle: String) -> Value {
    Value::Variant {
        name: "__StreamHandle".into(),
        args: vec![Value::Str(handle.into())],
    }
}

/// Inverse of [`stream_handle_value`] — extract the handle id from
/// a Stream value, or `None` if the input doesn't have the
/// expected shape.
pub(super) fn stream_handle_id(v: &Value) -> Option<String> {
    match v {
        Value::Variant { name, args } if name == "__StreamHandle" => match args.first() {
            Some(Value::Str(h)) => Some(h.to_string()),
            _ => None,
        },
        _ => None,
    }
}

/// Implementation of `agent.local_complete(prompt)` (#196).
/// Hits Ollama (or any compatible HTTP service via `OLLAMA_HOST`)
/// and returns the completion text. Override at the
/// `EffectHandler` layer if you need a different transport.
pub(super) fn dispatch_llm_local(args: Vec<Value>) -> Value {
    let prompt = match args.first() {
        Some(Value::Str(s)) => s.clone(),
        _ => return err(Value::Str(
            "agent.local_complete(prompt): prompt must be Str".into())),
    };
    match crate::llm::local_complete(&prompt) {
        Ok(text) => ok(Value::Str(text.into())),
        Err(e) => err(Value::Str(e.into())),
    }
}

/// Implementation of `agent.cloud_complete(prompt)` (#196).
/// Hits OpenAI's chat-completions API (or any compatible
/// service via `OPENAI_BASE_URL`) and returns the assistant
/// message. Requires `OPENAI_API_KEY`. Override at the
/// `EffectHandler` layer for custom auth, batching, or other
/// providers.
pub(super) fn dispatch_llm_cloud(args: Vec<Value>) -> Value {
    let prompt = match args.first() {
        Some(Value::Str(s)) => s.clone(),
        _ => return err(Value::Str(
            "agent.cloud_complete(prompt): prompt must be Str".into())),
    };
    match crate::llm::cloud_complete(&prompt) {
        Ok(text) => ok(Value::Str(text.into())),
        Err(e) => err(Value::Str(e.into())),
    }
}
