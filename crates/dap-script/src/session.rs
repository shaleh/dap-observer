//! The live session, wrapping the dap-client engine.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::json;
use tokio::sync::mpsc::UnboundedReceiver;

use dap_client::{
    dap::{
        self, ConnEvent, DapClient,
        types::{EventMessage, Response, Source, StackFrame, StoppedBody},
    },
    model::{self, EvalContext, SessionState, StopResolution, VarNode},
};

/// Upper bound on any single wait for the session to stop. A script must never
/// wedge, so `expect stopped` and every navigation verb give up after this and
/// fail rather than block forever. A `continue` between distant breakpoints is
/// the case most likely to brush against it.
const STOP_TIMEOUT: Duration = Duration::from_secs(30);

/// Upper bound on the wait for the protocol to be initialized. Generous because some adapters
/// compile the target on launch, Go's delve most notably, and the `initialized` event
/// only arrives once the build finishes. A launch the adapter rejects outright
/// fails immediately regardless of this bound.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(60);

/// How a wait for the session to settle resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settled {
    Stopped,
    Ended,
}

pub struct Session {
    client: DapClient,
    events: UnboundedReceiver<ConnEvent>,
    state: SessionState,
    /// Thread id from the most recent `stopped` event, used to resolve the stop.
    stop_thread_hint: Option<i64>,
    /// The resolved current stop. `None` while running, ended, or stopped but
    /// not yet resolved.
    stop: Option<StopResolution>,
}

impl Session {
    /// Connect to the adapter and perform the minimal handshake.
    ///
    /// The handshake delegates to the engine, which does not advertise
    /// `supportsRunInTerminalRequest`, so a reverse request never routes here.
    pub async fn connect(address: &str) -> Result<Session> {
        let (client, events) = dap::connect(address).await?;
        dap::initialize(&client).await?;
        let mut session = Session {
            client,
            events,
            state: SessionState::Connecting,
            stop_thread_hint: None,
            stop: None,
        };
        session.drain_pending();
        Ok(session)
    }

    pub fn address(&self) -> &str {
        self.client.address()
    }

    pub fn client(&self) -> &DapClient {
        &self.client
    }

    /// Send the non-terminating `disconnect` and wait for it to flush.
    pub async fn disconnect(&self) {
        self.client.disconnect().await;
    }

    /// Launch a debuggee as the initializing client.
    ///
    /// The DAP launch handshake interleaves a request and an event. The adapter
    /// sends the `initialized` event after `launch` and expects breakpoints and
    /// `configurationDone` before it finishes launching. So the `launch` request
    /// is sent without awaiting its response, the `initialized` event is awaited,
    /// then breakpoints and `configurationDone` follow. A launch the adapter
    /// rejects surfaces as a failed launch response.
    pub async fn launch(
        &mut self,
        config: serde_json::Value,
        breakpoints: Vec<(String, i64)>,
    ) -> Result<()> {
        let client = self.client.clone();
        let mut launch = tokio::spawn(async move { client.request("launch", Some(config)).await });

        let deadline = Instant::now() + LAUNCH_TIMEOUT;
        let mut launched = false;

        loop {
            let time_remaining = deadline
                .checked_duration_since(Instant::now())
                .context("timed out waiting for the launch to initialize")?;
            tokio::select! {
                event = tokio::time::timeout(time_remaining, self.events.recv()) => match event {
                    Ok(Some(ConnEvent::Dap(message))) if message.event == "initialized" => break,
                    Ok(Some(ConnEvent::Dap(message))) => self.apply_dap_event(message),
                    Ok(Some(ConnEvent::Disconnected(e))) => {
                        bail!(
                            "the connection closed during launch{}",
                            e.map(|m| format!(": {m}")).unwrap_or_default()
                        );
                    }
                    Ok(None) => bail!("the connection closed during launch"),
                    Err(_) => bail!("timed out waiting for the launch to initialize"),
                },
                result = &mut launch, if !launched => {
                    launched = true;
                    check_launch(result)?;
                }
            }
        }

        for (file, lines) in group_by_file(breakpoints) {
            let response = self
                .client
                .request(
                    "setBreakpoints",
                    Some(json!({
                        "source": { "path": file },
                        "breakpoints": lines.iter().map(|l| json!({ "line": l })).collect::<Vec<_>>(),
                    })),
                )
                .await?;
            if !response.success {
                bail!(
                    "setBreakpoints failed for {file}: {}",
                    response.message.unwrap_or_else(|| "no detail".into())
                );
            }
            warn_unbound(&file, &response);
        }
        self.client
            .request("configurationDone", Some(json!({})))
            .await?;

        if !launched {
            match tokio::time::timeout(LAUNCH_TIMEOUT, launch).await {
                Ok(joined) => check_launch(joined)?,
                Err(_) => bail!("timed out waiting for the launch response"),
            }
        }
        // The program is now running toward the first breakpoint.
        self.state = SessionState::Running;
        Ok(())
    }

    /// Synchronize on a stop. Proceeds immediately when already parked, otherwise
    /// waits for the next stop within the timeout. Fails when the session ended.
    pub async fn expect_stopped(&mut self) -> Result<()> {
        self.drain_pending();
        match self.state {
            SessionState::Stopped => self.ensure_resolved().await,
            SessionState::Ended => bail!("the session has ended"),
            SessionState::Connecting | SessionState::Running => {
                match self.wait_for_settle(STOP_TIMEOUT).await? {
                    Settled::Stopped => Ok(()),
                    Settled::Ended => bail!("the session ended before it stopped"),
                }
            }
        }
    }

    /// Issue a request and wait for the resulting stop or end.
    /// This re-roots our view of the frame the same way a stop from any other client would.
    pub async fn drive(&mut self, command: &str) -> Result<Settled> {
        self.drain_pending();
        if self.state == SessionState::Ended {
            bail!("cannot {command}: the session has ended");
        }
        // Resolve first so the thread id reflects the current stop, not whatever
        // stop happened to be resolved last. This also drives the thread of a
        // session we joined already parked, before any explicit query.
        self.ensure_resolved().await?;
        let thread_id = self
            .stop
            .as_ref()
            .and_then(|stop| stop.thread_id)
            .context("cannot drive execution: no stopped thread")?;
        model::drive(&self.client, command, thread_id).await?;
        // The drive resumes the program. Forget the old frame and wait for the
        // next stop rather than reading the stale one we were parked at.
        self.state = SessionState::Running;
        self.stop = None;
        self.wait_for_settle(STOP_TIMEOUT).await
    }

    /// Evaluate an expression in the current frame using `watch` context.
    /// Evaluation is not sandboxed, so the expression can still call functions and have side
    /// effects. That is what the eval serialization escape hatch relies on.
    pub async fn eval(&self, expression: &str) -> Result<VarNode> {
        let frame_id = self.frame_id().context("nothing to evaluate against")?;
        model::evaluate(&self.client, expression, frame_id, EvalContext::Watch).await
    }

    pub fn frame_line(&self) -> Result<i64> {
        Ok(self.top_frame()?.line)
    }

    pub fn frame_name(&self) -> Result<String> {
        Ok(self.top_frame()?.name.clone())
    }

    pub fn frame_source(&self) -> Result<String> {
        Ok(self
            .top_frame()?
            .source
            .as_ref()
            .and_then(Source::label)
            .unwrap_or_default())
    }

    pub fn top_frame(&self) -> Result<&StackFrame> {
        self.stack()?.first().context("no current frame")
    }

    pub fn stack(&self) -> Result<&[StackFrame]> {
        Ok(self
            .stop
            .as_ref()
            .context("the session is not stopped")?
            .stack
            .as_slice())
    }

    /// The scope to dump for `locals`. The engine already applied its locals
    /// heuristic when resolving the stop and marked that scope expanded, so this
    /// reuses that decision rather than guessing again from the name alone. Its
    /// children are also already fetched. The first scope is the fallback.
    pub fn locals_scope(&self) -> Result<&VarNode> {
        let roots = &self
            .stop
            .as_ref()
            .context("the session is not stopped")?
            .roots;
        roots
            .iter()
            .find(|root| root.expanded)
            .or_else(|| roots.first())
            .context("no scopes at the current frame")
    }

    fn frame_id(&self) -> Option<i64> {
        self.stop.as_ref().and_then(|s| s.frame_id)
    }

    /// Resolve the current stop into a thread, call stack, and scope roots, once
    /// per stop. References are valid only while stopped, so a fresh stop drops
    /// the old resolution and this rebuilds it on demand.
    pub async fn ensure_resolved(&mut self) -> Result<()> {
        if self.state != SessionState::Stopped {
            bail!("the session is not stopped");
        }
        if self.stop.is_none() {
            self.stop = Some(model::resolve_stop(&self.client, self.stop_thread_hint).await?);
        }
        Ok(())
    }

    /// Consume every event already buffered, without blocking, to bring state up
    /// to date before deciding whether a wait is needed.
    fn drain_pending(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            self.apply_event(event);
        }
    }

    /// Block until the session stops or ends, bounded by the timeout. The caller
    /// has already arranged that the current state is not a stale stop.
    async fn wait_for_settle(&mut self, timeout: Duration) -> Result<Settled> {
        let deadline = Instant::now() + timeout;

        loop {
            match self.state {
                SessionState::Stopped => {
                    self.ensure_resolved().await?;
                    return Ok(Settled::Stopped);
                }
                SessionState::Ended => return Ok(Settled::Ended),
                _ => {}
            }
            let remaining = deadline.checked_duration_since(Instant::now());
            let Some(remaining) = remaining else {
                bail!("timed out waiting for the session to stop");
            };
            match tokio::time::timeout(remaining, self.events.recv()).await {
                Ok(Some(event)) => self.apply_event(event),
                Ok(None) => {
                    self.state = SessionState::Ended;
                    bail!("the connection closed while waiting for a stop");
                }
                Err(_) => bail!("timed out waiting for the session to stop"),
            }
        }
    }

    fn apply_event(&mut self, event: ConnEvent) {
        match event {
            ConnEvent::Dap(message) => self.apply_dap_event(message),
            ConnEvent::Disconnected(_) => self.state = SessionState::Ended,
        }
    }

    fn apply_dap_event(&mut self, message: EventMessage) {
        match message.event.as_str() {
            "stopped" => {
                let body: StoppedBody = message
                    .body
                    .and_then(|b| serde_json::from_value(b).ok())
                    .unwrap_or_default();
                self.state = SessionState::Stopped;
                self.stop_thread_hint = body.thread_id;
                // A fresh stop invalidates the previous frame's references.
                self.stop = None;
            }
            "continued" => {
                self.state = SessionState::Running;
                self.stop = None;
            }
            "terminated" | "exited" => {
                self.state = SessionState::Ended;
                self.stop = None;
            }
            _ => {}
        }
    }
}

/// Check the outcome of the spawned `launch` request, mapping a task failure or a
/// rejected launch to a clear error.
fn check_launch(
    result: std::result::Result<Result<Response>, tokio::task::JoinError>,
) -> Result<()> {
    let response = result.context("launch task did not complete")??;
    if !response.success {
        bail!(
            "launch failed: {}",
            response.message.unwrap_or_else(|| "no detail".into())
        );
    }
    Ok(())
}

/// Group breakpoints by source file. The DAP `setBreakpoints` request replaces
/// every breakpoint for a source, so all of a file's lines have to go in one
/// request rather than one request per line.
fn group_by_file(breakpoints: Vec<(String, i64)>) -> HashMap<String, Vec<i64>> {
    let mut grouped: HashMap<String, Vec<i64>> = HashMap::new();
    for (file, line) in breakpoints {
        grouped.entry(file).or_default().push(line);
    }
    grouped
}

/// Warn about breakpoints the adapter accepted but could not bind. An unbound
/// breakpoint never stops the program, which otherwise shows up only as the
/// session ending before any expected stop.
fn warn_unbound(file: &str, response: &Response) {
    let unbound: Vec<i64> = response
        .body
        .as_ref()
        .and_then(|body| body.get("breakpoints"))
        .and_then(|list| list.as_array())
        .map(|breakpoints| {
            breakpoints
                .iter()
                .filter(|breakpoint| {
                    breakpoint
                        .get("verified")
                        .and_then(serde_json::Value::as_bool)
                        == Some(false)
                })
                .filter_map(|breakpoint| breakpoint.get("line").and_then(serde_json::Value::as_i64))
                .collect()
        })
        .unwrap_or_default();
    if !unbound.is_empty() {
        eprintln!(
            "warning: {file}: breakpoint lines {unbound:?} could not be bound, so the program may run to completion without stopping there"
        );
    }
}
