//! Variable tree, session state, and the read-only fetches that build them.
//!
//! `variablesReference` handles are valid only while the session is visiting
//! a breakpoint aka stopped. So the tree is dropped and re-rooted on every stop.
//! An epoch counter tags in-flight work. Replies carrying a stale epoch are discarded.

use anyhow::Result;
use serde_json::json;

use crate::dap::DapClient;
use crate::dap::types::{
    EvaluateBody, ScopesBody, StackTraceBody, StoppedBody, ThreadsBody, Variable, VariablesBody,
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

/// Header describing the resolved top frame.
pub struct FrameHeader {
    pub name: String,
    pub line: i64,
    pub source: Option<String>,
    pub reason: String,
    pub stop_number: u64,
}

/// The result of resolving a stop: a header, the resolved frame id (needed to
/// `evaluate` watches against this frame), and seeded scope roots.
pub struct FrameContext {
    pub header: FrameHeader,
    pub frame_id: i64,
    pub roots: Vec<VarNode>,
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

/// Build a tree node from a DAP variable (not yet expanded, children unfetched).
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
pub fn node_from_evaluate(expr: &str, body: EvaluateBody) -> VarNode {
    VarNode {
        name: expr.to_string(),
        value: body.result,
        ty: body.ty.unwrap_or_default(),
        eval_name: expr.to_string(),
        var_ref: body.variables_reference,
        children: None,
        expanded: false,
    }
}

/// Evaluate a single watch expression against `frame_id`.
///
/// Uses `context: "watch"` and only ever an adapter-provided `evaluateName`, so
/// it recomputes an existing variable and stays side-effect-free — the
/// observer-only contract holds. An `Err` means the expression did not resolve
/// in the current frame (e.g. stepped out of scope); the caller keeps the watch
/// pinned and renders it as unavailable.
pub async fn evaluate_watch(client: &DapClient, expr: &str, frame_id: i64) -> Result<VarNode> {
    let resp = client
        .request(
            "evaluate",
            Some(json!({
                "expression": expr,
                "frameId": frame_id,
                "context": "watch"
            })),
        )
        .await?;
    if !resp.success {
        anyhow::bail!(
            "{}",
            resp.message.unwrap_or_else(|| "evaluate failed".into())
        );
    }
    Ok(node_from_evaluate(expr, resp.parse_body::<EvaluateBody>()?))
}

/// A `Locals`-style scope: open by default. Detect via the spec-defined
/// `presentationHint` first which is portable across adapters, falling back
/// to a loose name match. The name fallback uses partial match rather than
/// exact equality because adapters disagree on the exact spelling — e.g.
/// CodeLLDB reports `Local` (singular), others `Local Variables`, and some
/// may not set the hint at all.
fn is_locals_scope(name: &str, hint: Option<&str>) -> bool {
    hint == Some("locals") || name.to_ascii_lowercase().contains("local")
}

/// Fetch and filter a node's children. Tolerates a stale-reference error
/// response.
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

/// On a stop, resolve the stopped thread's top frame, fetch its scopes, and
/// seed scope nodes.
///
/// Returns `Ok(None)` for the no-frames case, which the UI shows as idle.
pub async fn build_frame(
    client: &DapClient,
    body: StoppedBody,
    stop_number: u64,
) -> Result<Option<FrameContext>> {
    // threads -> stackTrace -> scopes -> variables. The stop event carries the
    // thread id; fall back to the first reported thread if it is absent.
    let thread_id = match body.thread_id {
        Some(id) => {
            let _ = client.request("threads", Some(json!({}))).await;
            id
        }
        None => {
            let resp = client.request("threads", Some(json!({}))).await?;
            match resp.parse_body::<ThreadsBody>()?.threads.first() {
                Some(t) => t.id,
                None => return Ok(None),
            }
        }
    };

    let st = client
        .request(
            "stackTrace",
            Some(json!({ "threadId": thread_id, "levels": 1 })),
        )
        .await?;
    if !st.success {
        // A stale/failed stackTrace (e.g. the session resumed underneath us)
        // is treated as idle rather than fatal.
        anyhow::bail!("stackTrace failed: {}", st.message.unwrap_or_default());
    }
    let frames = st
        .parse_body::<StackTraceBody>()
        .unwrap_or_default()
        .stack_frames;
    let Some(top) = frames.into_iter().next() else {
        return Ok(None);
    };

    let frame_id = top.id;
    let header = FrameHeader {
        name: top.name,
        line: top.line,
        source: top.source.and_then(|s| s.name.or(s.path)),
        reason: body.reason,
        stop_number,
    };

    // Tolerate a scopes failure (IOW a stale frame): show an empty tree, not a crash.
    let scopes = match client
        .request("scopes", Some(json!({ "frameId": frame_id })))
        .await
    {
        Ok(resp) if resp.success => resp.parse_body::<ScopesBody>().unwrap_or_default().scopes,
        _ => Vec::new(),
    };

    // Safety net: if no scope matches the locals heuristic (an adapter we
    // haven't seen, with exotic naming and no hint), fall back to expanding the
    // first scope. Adapters conventionally list the most relevant scope first,
    // so this keeps a useful tree open instead of a fully collapsed one.
    let any_locals = scopes
        .iter()
        .any(|s| is_locals_scope(&s.name, s.presentation_hint.as_deref()));

    let mut roots = Vec::with_capacity(scopes.len());
    for (i, scope) in scopes.into_iter().enumerate() {
        let expand_default = is_locals_scope(&scope.name, scope.presentation_hint.as_deref())
            || (!any_locals && i == 0);
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
        // lazy (fetched on demand when the user expands them).
        if expand_default && node.var_ref > 0 {
            node.children = Some(fetch_children(client, node.var_ref).await);
        }
        roots.push(node);
    }

    Ok(Some(FrameContext {
        header,
        frame_id,
        roots,
    }))
}
