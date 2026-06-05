//! The REPL engine. It tracks session and frame state and evaluates
//! expressions. Every function returns data instead of printing, so the
//! front-end owns all input and output.

use anyhow::{Result, bail};
use serde_json::json;

use dap_client::dap::DapClient;
use dap_client::dap::types::{EvaluateBody, StackFrame};
use dap_client::model::{SessionState, resolve_top_frame};

/// Session state plus the frame an expression evaluates against.
pub struct Session {
    pub state: SessionState,
    frame_id: Option<i64>,
}

impl Session {
    pub fn new() -> Self {
        Self {
            state: SessionState::Connecting,
            frame_id: None,
        }
    }

    /// The frame an expression evaluates against, or nothing when there is none.
    /// The adapter only keeps frame references valid while the program is
    /// stopped, so this resets when it resumes.
    pub fn frame_id(&self) -> Option<i64> {
        self.frame_id
    }

    /// Adopt the stopped thread's top frame. It returns nothing when the stop resolves no frame.
    pub async fn on_stopped(
        &mut self,
        client: &DapClient,
        thread_hint: Option<i64>,
    ) -> Result<Option<StackFrame>> {
        self.state = SessionState::Stopped;
        let frame = resolve_top_frame(client, thread_hint).await?;
        self.frame_id = frame.as_ref().map(|f| f.id);
        Ok(frame)
    }

    pub fn on_continued(&mut self) {
        self.state = SessionState::Running;
        self.frame_id = None;
    }

    pub fn on_ended(&mut self) {
        self.state = SessionState::Ended;
        self.frame_id = None;
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

/// The outcome of an evaluation. It holds the value the adapter rendered and a
/// type when the adapter provides one.
pub struct Evaluated {
    pub result: String,
    pub ty: Option<String>,
}

/// Evaluate a user-typed expression in the given frame. It uses the [repl
/// evaluate context](https://microsoft.github.io/debug-adapter-protocol/specification#Requests_Evaluate)
/// of the Debug Adapter Protocol.
///
/// That context is not sandboxed. A typed expression can assign to variables or
/// call functions, so evaluating one can change the running program.
pub async fn evaluate(client: &DapClient, expression: &str, frame_id: i64) -> Result<Evaluated> {
    let resp = client
        .request(
            "evaluate",
            Some(json!({
                "expression": expression,
                "frameId": frame_id,
                "context": "repl"
            })),
        )
        .await?;
    if !resp.success {
        bail!(
            "{}",
            resp.message.unwrap_or_else(|| "evaluate failed".into())
        );
    }
    let body = resp.parse_body::<EvaluateBody>()?;
    Ok(Evaluated {
        result: body.result,
        ty: body.ty,
    })
}
