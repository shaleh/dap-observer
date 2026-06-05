//! Headless observer: used for testing.
//!
//! On each `stopped` DAP event it walks `threads -> stackTrace -> scopes ->
//! variables` for the top frame and prints a filtered tree to stdout.

use std::future::Future;
use std::pin::Pin;

use anyhow::Result;
use serde_json::json;

use crate::dap::types::{ScopesBody, StoppedBody};
use crate::dap::{ConnEvent, DapClient, initialize};
use crate::model::{any_locals_scope, fetch_children, resolve_top_frame, scope_opens_by_default};

const MAX_DEPTH: usize = 2; // how deep to expand containers in the printout
const MAX_CHILDREN: usize = 40; // cap children shown per node
const SHOW_ALL_SCOPES: bool = false; // non-locals scopes are huge/noisy. Locals only by default

/// Run the headless observer loop until the session ends, the connection drops,
/// or the user hits Ctrl-C. Returns the process exit code.
pub async fn run(
    client: DapClient,
    mut events: tokio::sync::mpsc::UnboundedReceiver<ConnEvent>,
    target: &str,
) -> Result<i32> {
    println!("observer connected to mux at {target}");

    // Minimal late-join handshake. The mux replies with cached capabilities and
    // replays the current stopped state.
    initialize(&client).await?;
    println!("initialized — waiting for the program to stop (breakpoint or step)…");
    println!("(Ctrl-C to stop observing)");

    let mut stop_number: u64 = 0;
    let mut exit_code = 0;
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                println!("\nobserver stopping (Ctrl-C).");
                break;
            }
            ev = events.recv() => match ev {
                None => break,
                Some(ConnEvent::Disconnected(Some(err))) => {
                    eprintln!("\n!! {err}");
                    exit_code = 1;
                    break;
                }
                Some(ConnEvent::Disconnected(_)) => {
                    println!("\n■  session ended.");
                    break;
                }
                Some(ConnEvent::Dap(msg)) => match msg.event.as_str() {
                    "stopped" => {
                        stop_number += 1;
                        let body: StoppedBody = msg
                            .body
                            .and_then(|b| serde_json::from_value(b).ok())
                            .unwrap_or_default();
                        handle_stop(&client, body, stop_number).await;
                    }
                    "continued" => println!("\n▶  running…"),
                    "terminated" | "exited" => {
                        println!("\n■  session ended.");
                        break;
                    }
                    _ => {}
                },
            },
        }
    }

    client.disconnect().await;
    Ok(exit_code)
}

async fn handle_stop(client: &DapClient, body: StoppedBody, stop_number: u64) {
    // Resolve the stopped thread (preferring the stop event's thread id, else
    // the first reported thread) and its top frame. A transport error or a
    // frameless stop is treated as idle.
    let Ok(Some(top)) = resolve_top_frame(client, body.thread_id).await else {
        return;
    };

    let bar = "━".repeat(64);
    println!("\n{bar}");
    println!(
        "⏸  stop #{stop_number}  ({})  →  {}  @ line {}",
        body.reason, top.name, top.line
    );
    println!("{bar}");

    // A stale frame (an `Ok` response with `success == false`) and a transport
    // error both leave us nothing to print; only a successful reply carries
    // scopes.
    let scopes = match client
        .request("scopes", Some(json!({ "frameId": top.id })))
        .await
    {
        Ok(resp) if resp.success => resp.parse_body::<ScopesBody>().unwrap_or_default().scopes,
        _ => return,
    };
    let any_locals = any_locals_scope(&scopes);
    for (index, scope) in scopes.into_iter().enumerate() {
        // Show locals-style scopes by default; others (Globals, etc.) are huge
        // and noisy. Flip SHOW_ALL_SCOPES to include everything.
        if !SHOW_ALL_SCOPES && !scope_opens_by_default(&scope, index, any_locals) {
            println!("  [{}] (hidden — non-locals scope)", scope.name);
            continue;
        }
        println!("  [{}]", scope.name);
        print_tree(client, scope.variables_reference, 1, "    ".to_string()).await;
    }
}

/// Recursively print a container's children, with adapter noise filtered out.
fn print_tree<'a>(
    client: &'a DapClient,
    var_ref: i64,
    depth: usize,
    prefix: String,
) -> Pin<Box<dyn Future<Output = ()> + 'a>> {
    Box::pin(async move {
        if var_ref == 0 || depth > MAX_DEPTH {
            return;
        }
        for (shown, node) in fetch_children(client, var_ref)
            .await
            .into_iter()
            .enumerate()
        {
            if shown >= MAX_CHILDREN {
                println!("{prefix}… more");
                break;
            }
            let ty = if node.ty.is_empty() {
                String::new()
            } else {
                format!(" : {}", node.ty)
            };
            println!("{prefix}{}{ty} = {}", node.name, node.value);
            if node.var_ref != 0 {
                print_tree(client, node.var_ref, depth + 1, format!("{prefix}    ")).await;
            }
        }
    })
}
