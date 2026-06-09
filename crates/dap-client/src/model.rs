//! Variable tree, session state, and the read-only fetches that build them.
//!
//! `variablesReference` handles are valid only while the session is visiting
//! a breakpoint aka stopped. So the tree is dropped and re-rooted on every stop.
//! An epoch counter tags in-flight work. Replies carrying a stale epoch are discarded.

use anyhow::{Result, bail};
use serde_json::json;

use crate::dap::DapClient;
use crate::dap::types::{
    EvaluateBody, Scope, ScopesBody, StackFrame, StackTraceBody, ThreadsBody, Variable,
    VariablesBody,
};

/// Live-ness of the debug session, as inferred purely from broadcast events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// Connected, handshake done, nothing observed yet.
    Connecting,
    /// A `continued` was seen. Displayed variables are not current.
    Running,
    /// A `stopped` was seen. The tree reflects the current frame.
    Stopped,
    /// `terminated`/`exited`/disconnect.
    Ended,
}

/// One node in the variable tree.
///
/// `children == None` means "not yet fetched". `var_ref == 0` means a leaf that
/// can never be expanded.
pub struct VarNode {
    pub name: String,
    pub value: String,
    pub ty: String,
    /// DAP `evaluateName`: an expression that recomputes this variable in the
    /// current frame. Used to pin the variable as a watch.
    pub eval_name: String,
    pub var_ref: i64,
    pub children: Option<Vec<VarNode>>,
    pub expanded: bool,
}

impl VarNode {
    pub fn expandable(&self) -> bool {
        self.var_ref > 0
    }
}

/// debugpy's synthetic "special variables"/"function variables" grouping nodes.
///
/// Structural and adapter-agnostic: empty `type` AND empty `value` AND no
/// `evaluateName`, but a non-zero `variablesReference`. Real children always
/// carry a `type`/`evaluateName`.
pub fn is_noise(v: &Variable) -> bool {
    v.ty.is_empty()
        && v.value.is_empty()
        && v.evaluate_name.as_deref().unwrap_or("").is_empty()
        && v.variables_reference != 0
}

/// Build a tree node from a DAP variable.
pub fn node_from_var(v: &Variable) -> VarNode {
    VarNode {
        name: v.name.clone(),
        value: v.value.clone(),
        ty: v.ty.clone(),
        eval_name: v.evaluate_name.clone().unwrap_or_default(),
        var_ref: v.variables_reference,
        children: None,
        expanded: false,
    }
}

/// Build a tree node from an `evaluate` result. The watched expression doubles
/// as the node's name and its `eval_name`.
pub fn node_from_evaluate(expression: &str, body: EvaluateBody) -> VarNode {
    VarNode {
        name: expression.to_string(),
        value: body.result,
        ty: body.ty.unwrap_or_default(),
        eval_name: expression.to_string(),
        var_ref: body.variables_reference,
        children: None,
        expanded: false,
    }
}

/// A `Locals`-style scope: open by default. Detect via the spec-defined
/// `presentationHint` first which is portable across adapters, falling back
/// to a loose name match. The name fallback uses partial match rather than
/// exact equality because adapters disagree on the exact spelling — e.g.
/// CodeLLDB reports `Local` (singular), others `Local Variables`, and some
/// may not set the hint at all.
pub fn is_locals_scope(name: &str, hint: Option<&str>) -> bool {
    hint == Some("locals") || name.to_ascii_lowercase().contains("local")
}

/// Whether any scope in a frame matches the locals heuristic.
pub fn any_locals_scope(scopes: &[Scope]) -> bool {
    scopes
        .iter()
        .any(|s| is_locals_scope(&s.name, s.presentation_hint.as_deref()))
}

/// Whether a scope should be opened/shown by default. `Locals`-style scopes
/// always are. When a frame has none — an adapter we haven't seen, with exotic
/// naming and no hint — the first scope is the fallback, since adapters
/// conventionally list the most relevant scope first. This keeps a useful tree
/// open instead of a fully collapsed one. `any_locals` is whether the frame has
/// any locals scope at all, computed once per frame by the caller.
pub fn scope_opens_by_default(scope: &Scope, index: usize, any_locals: bool) -> bool {
    is_locals_scope(&scope.name, scope.presentation_hint.as_deref()) || (!any_locals && index == 0)
}

/// Fetch and filter a node's children. Tolerates a stale-reference error response.
pub async fn fetch_children(client: &DapClient, var_ref: i64) -> Vec<VarNode> {
    match client
        .request("variables", Some(json!({ "variablesReference": var_ref })))
        .await
    {
        Ok(resp) if resp.success => resp
            .parse_body::<VariablesBody>()
            .unwrap_or_default()
            .variables
            .iter()
            .filter(|v| !is_noise(v))
            .map(node_from_var)
            .collect(),
        // Failed/stale reference: no children, no crash.
        _ => Vec::new(),
    }
}

/// A resolved stop: the stopped thread, its full call stack, and the seeded scope
/// roots of the top frame ready to display.
pub struct StopResolution {
    pub thread_id: Option<i64>,
    pub stack: Vec<StackFrame>,
    /// The top frame's id, or nothing when the stop resolved no frames.
    pub frame_id: Option<i64>,
    pub roots: Vec<VarNode>,
}

/// Resolve a stop into the stopped thread, its call stack, and the top frame's
/// scopes.
///
/// The thread comes from the event when it carries one, otherwise from the first
/// thread the adapter reports. A frameless stop resolves to an idle state with no
/// frame and no roots.
pub async fn resolve_stop(client: &DapClient, thread_hint: Option<i64>) -> Result<StopResolution> {
    let thread_id = match thread_hint {
        Some(id) => Some(id),
        None => {
            let resp = client.request("threads", Some(json!({}))).await?;
            resp.parse_body::<ThreadsBody>()?
                .threads
                .first()
                .map(|t| t.id)
        }
    };
    let stack = match thread_id {
        Some(id) => fetch_stack(client, id).await?,
        None => Vec::new(),
    };
    let (frame_id, roots) = match stack.first() {
        Some(top) => (Some(top.id), build_scope_roots(client, top.id).await?),
        None => (None, Vec::new()),
    };
    Ok(StopResolution {
        thread_id,
        stack,
        frame_id,
        roots,
    })
}

/// Fetch a thread's full call stack. A stale request after the program resumed
/// underneath us yields an empty stack rather than an error.
async fn fetch_stack(client: &DapClient, thread_id: i64) -> Result<Vec<StackFrame>> {
    let resp = client
        .request("stackTrace", Some(json!({ "threadId": thread_id })))
        .await?;
    if !resp.success {
        return Ok(Vec::new());
    }
    Ok(resp.parse_body::<StackTraceBody>()?.stack_frames)
}

/// Fetch a frame's scopes and seed scope nodes, expanding the default scope.
///
/// A stale frame, an `Ok` response with `success == false`, is tolerated as an
/// empty scope list. A transport error means the connection is gone, so it
/// surfaces instead of masquerading as a frame with no variables.
pub async fn build_scope_roots(client: &DapClient, frame_id: i64) -> Result<Vec<VarNode>> {
    let scopes = match client
        .request("scopes", Some(json!({ "frameId": frame_id })))
        .await
    {
        Ok(resp) if resp.success => resp.parse_body::<ScopesBody>().unwrap_or_default().scopes,
        Ok(_) => Vec::new(),
        Err(e) => bail!("scopes failed: {e:#}"),
    };

    let any_locals = any_locals_scope(&scopes);

    let mut roots = Vec::with_capacity(scopes.len());
    for (index, scope) in scopes.into_iter().enumerate() {
        let expand_default = scope_opens_by_default(&scope, index, any_locals);
        let mut node = VarNode {
            name: scope.name,
            value: String::new(),
            ty: String::new(),
            eval_name: String::new(),
            var_ref: scope.variables_reference,
            children: None,
            expanded: expand_default,
        };
        // Only the default-expanded scope is fetched now. Nested containers stay
        // lazy, fetched on demand when the user expands them.
        if expand_default && node.var_ref > 0 {
            node.children = Some(fetch_children(client, node.var_ref).await);
        }
        roots.push(node);
    }
    Ok(roots)
}

/// The [evaluate context](https://microsoft.github.io/debug-adapter-protocol/specification#Requests_Evaluate)
/// of the Debug Adapter Protocol, which tells the adapter how to interpret an
/// expression and how to render its result. `Watch` asks for the value as the
/// variables view would show it, which adapters return cleanly. `Repl` runs the
/// expression as a debug-console entry, which some adapters, lldb among them,
/// echo back in a verbose typed command form rather than a bare value.
#[derive(Clone, Copy)]
pub enum EvalContext {
    Watch,
    Repl,
}

impl EvalContext {
    fn as_str(self) -> &'static str {
        match self {
            EvalContext::Watch => "watch",
            EvalContext::Repl => "repl",
        }
    }
}

/// Evaluate an expression against a frame and return the result as a node, so a
/// structured value can be expanded and pinned like any frame variable.
///
/// Neither context is sandboxed in practice. An expression can call functions or
/// assign, so evaluating one can change the running program. That is what lets a
/// rich value be serialized through the debuggee's own runtime.
pub async fn evaluate(
    client: &DapClient,
    expression: &str,
    frame_id: i64,
    context: EvalContext,
) -> Result<VarNode> {
    let resp = client
        .request(
            "evaluate",
            Some(json!({
                "expression": expression,
                "frameId": frame_id,
                "context": context.as_str()
            })),
        )
        .await?;
    if !resp.success {
        bail!(
            "{}",
            resp.message.unwrap_or_else(|| "evaluate failed".into())
        );
    }
    Ok(node_from_evaluate(
        expression,
        resp.parse_body::<EvaluateBody>()?,
    ))
}

/// Drive execution of the stopped thread. The command is a DAP run-control
/// request such as continue, next, stepIn, stepOut, or pause. The resulting stop
/// or resume arrives later as a broadcast event.
pub async fn drive(client: &DapClient, command: &str, thread_id: i64) -> Result<()> {
    let resp = client
        .request(command, Some(json!({ "threadId": thread_id })))
        .await?;
    if !resp.success {
        bail!(
            "{}",
            resp.message.unwrap_or_else(|| format!("{command} failed"))
        );
    }
    Ok(())
}
