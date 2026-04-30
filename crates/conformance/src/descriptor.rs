//! JSON test descriptor.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Descriptor {
    pub name: String,
    /// One of "lex" (default) or "core". Other languages are reserved.
    #[serde(default = "default_lang")]
    pub language: String,
    /// Source code, or `source_file` (relative to the descriptor) — exactly one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_file: Option<PathBuf>,
    /// Function to invoke.
    #[serde(rename = "fn", default, skip_serializing_if = "Option::is_none")]
    pub func: Option<String>,
    /// Arguments as JSON values; mapped to `Value` at runtime.
    #[serde(default)]
    pub input: Vec<serde_json::Value>,
    /// Expected output JSON. Compared structurally.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_output: Option<serde_json::Value>,
    /// Capability policy.
    #[serde(default)]
    pub policy: PolicyJson,
    /// What we expect to see. Default: `ok`.
    #[serde(default = "default_status")]
    pub expected_status: ExpectedStatus,
}

fn default_lang() -> String { "lex".into() }
fn default_status() -> ExpectedStatus { ExpectedStatus::Ok }

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PolicyJson {
    #[serde(default)] pub allow_effects: Vec<String>,
    #[serde(default)] pub allow_fs_read: Vec<String>,
    #[serde(default)] pub allow_fs_write: Vec<String>,
    #[serde(default)] pub budget: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ExpectedStatus {
    /// `"ok"` or `{ "kind": "ok" }`.
    Tagged(String),
    /// `{ "error_kind": "<name>" }` — assert a specific structured error fired.
    ErrorKind { error_kind: String },
}

impl ExpectedStatus {
    #[allow(non_upper_case_globals)]
    pub const Ok: ExpectedStatus = ExpectedStatus::Tagged(String::new());

    pub fn is_ok(&self) -> bool {
        matches!(self, ExpectedStatus::Tagged(s) if s == "ok" || s.is_empty())
    }
    pub fn error_kind(&self) -> Option<&str> {
        match self {
            ExpectedStatus::ErrorKind { error_kind } => Some(error_kind),
            _ => None,
        }
    }
}

impl Default for ExpectedStatus {
    fn default() -> Self { ExpectedStatus::Tagged("ok".into()) }
}
