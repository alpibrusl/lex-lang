//! M5: effect runtime + sandbox. See spec §7.4 and §8.5.
//!
//! What's here:
//! - `policy::Policy` and `policy::check_program` — the static capability
//!   gate that walks declared effects and rejects programs whose effects
//!   are out of bounds before any code runs.
//! - `handler::DefaultHandler` — the host-side effect handler that the VM
//!   dispatches `EFFECT_CALL` through.
//!
//! What's not here yet (deferred):
//! - WASM-level isolation (`wasmtime` integration). The `--unsafe-no-sandbox`
//!   flag in the spec is operationally implicit for now: native execution
//!   only. We ship the policy/dispatch layer, which is the user-visible
//!   half of §7.4 and what the §7.6 acceptance tests exercise.

pub mod builtins;
pub mod cli;
pub mod policy;
pub mod handler;
pub mod ws;
pub mod mcp_client;
pub mod llm;

pub use builtins::{is_pure_module, try_pure_builtin};
pub use handler::{CapturedSink, DefaultHandler, IoSink, StdoutSink};
pub use policy::{check_program, Policy, PolicyReport, PolicyViolation};
