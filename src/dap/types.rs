//! Minimal serde types for the DAP messages the watcher depends on.
//!
//! We model only the fields we read. Everything else stays in
//! `serde_json::Value` so we remain resilient to adapter-specific extras.

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

/// A decoded inbound message. DAP frames are tagged by their `type` field as
/// `request` (reverse requests — we ignore them), `response`, or `event`.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Inbound {
    Response(Response),
    Event(EventMessage),
    /// Reverse request (e.g. runInTerminal). Captured to consume the frame, but
    /// never acted on — the mux routes these to the client driving the session.
    #[allow(dead_code)]
    Request(Value),
}

/// Response envelope. `body` is left as `Value` and parsed on demand into one
/// of the typed bodies below.
#[derive(Debug, Deserialize)]
pub struct Response {
    pub request_seq: i64,
    #[serde(default)]
    pub success: bool,
    /// Echoed command and optional error message — modeled for completeness and
    /// useful when diagnosing a failed request.
    #[serde(default)]
    #[allow(dead_code)]
    pub command: String,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub body: Option<Value>,
}

impl Response {
    /// Parse the response body into a typed body, defaulting when absent.
    /// Deserializes from a borrow of the body so the JSON tree is never cloned.
    pub fn parse_body<T: DeserializeOwned + Default>(&self) -> anyhow::Result<T> {
        match &self.body {
            Some(b) => Ok(T::deserialize(b)?),
            None => Ok(T::default()),
        }
    }
}

/// Event envelope. The body shape depends on `event`; we parse it lazily.
#[derive(Debug, Deserialize)]
pub struct EventMessage {
    pub event: String,
    #[serde(default)]
    pub body: Option<Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoppedBody {
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub thread_id: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ThreadsBody {
    #[serde(default)]
    pub threads: Vec<Thread>,
}

#[derive(Debug, Deserialize)]
pub struct Thread {
    pub id: i64,
    #[serde(default)]
    #[allow(dead_code)]
    pub name: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StackTraceBody {
    #[serde(default)]
    pub stack_frames: Vec<StackFrame>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StackFrame {
    pub id: i64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub line: i64,
    #[serde(default)]
    pub source: Option<Source>,
}

#[derive(Debug, Deserialize)]
pub struct Source {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScopesBody {
    #[serde(default)]
    pub scopes: Vec<Scope>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Scope {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub variables_reference: i64,
    #[serde(default)]
    pub presentation_hint: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub expensive: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VariablesBody {
    #[serde(default)]
    pub variables: Vec<Variable>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Variable {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub value: String,
    #[serde(rename = "type", default)]
    pub ty: String,
    #[serde(default)]
    pub evaluate_name: Option<String>,
    #[serde(default)]
    pub variables_reference: i64,
}

/// Body of an `evaluate` response. Shaped like a `Variable`: a rendered
/// `result`, an optional `type`, and a `variablesReference` that is non-zero
/// when the result is a container that can be expanded via `variables`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvaluateBody {
    #[serde(default)]
    pub result: String,
    #[serde(rename = "type", default)]
    pub ty: Option<String>,
    #[serde(default)]
    pub variables_reference: i64,
}
