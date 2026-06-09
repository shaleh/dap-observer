//! Walking the variable tree into JSON for `dump ... as json`.
//!
//! A node carries name, value, type, and children. The tree can be deep or
//! cyclic, so a depth budget bounds recursion. Breadth is not bounded. The
//! engine pulls all of a node's children in one request and does not honor
//! the paging hints, so a dump pointed at a very large collection fetches
//! the whole collection.

use std::future::Future;
use std::pin::Pin;

use serde_json::{Map, Value, json};

use dap_client::{
    dap::{
        DapClient,
        types::{Source, StackFrame},
    },
    model::{VarNode, fetch_children},
};

/// Default number of child levels a `dump` expands when no `depth` is given.
pub const DEFAULT_DEPTH: usize = 3;

/// Walk a node into JSON, fetching descendants up to `depth` levels below it.
/// Every node carries a `children` array so a consumer can iterate it
/// uniformly. A container held back at the depth limit is also marked
/// `truncated`, so the cut is distinguishable from a true leaf with no children.
pub fn node_to_json<'a>(
    client: &'a DapClient,
    node: &'a VarNode,
    depth: usize,
) -> Pin<Box<dyn Future<Output = Value> + 'a>> {
    let mut object = Map::new();

    object.insert("name".to_string(), json!(node.name));
    object.insert("value".to_string(), json!(node.value));
    object.insert("type".to_string(), json!(node.ty));

    let capped = node.var_ref > 0 && depth == 0;
    if capped {
        object.insert("truncated".to_string(), json!(true));
    }

    Box::pin(async move {
        let children = if node.var_ref > 0 && depth > 0 {
            children_to_json(client, node, depth - 1).await
        } else {
            Vec::new()
        };
        object.insert("children".to_string(), Value::Array(children));
        Value::Object(object)
    })
}

/// Expand a container node's children into JSON. Reuses children the engine
/// already fetched for this stop and fetches the rest.
pub async fn children_to_json(client: &DapClient, node: &VarNode, depth: usize) -> Vec<Value> {
    let mut out = Vec::new();

    match &node.children {
        Some(existing) => {
            for child in existing {
                out.push(node_to_json(client, child, depth).await);
            }
        }
        None => {
            let fetched = fetch_children(client, node.var_ref).await;
            for child in &fetched {
                out.push(node_to_json(client, child, depth).await);
            }
        }
    }
    out
}

/// A call stack as a JSON array of frames.
pub fn stack_to_json(stack: &[StackFrame]) -> Value {
    Value::Array(stack.iter().map(frame_to_json).collect())
}

/// A single frame as a JSON object with its name, line, and source.
pub fn frame_to_json(frame: &StackFrame) -> Value {
    let source = frame.source.as_ref().and_then(Source::label);
    json!({
        "name": frame.name,
        "line": frame.line,
        "source": source,
    })
}
