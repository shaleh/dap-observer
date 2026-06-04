//! Headless observer: used for testing.
//!
//! On each `stopped` DAP event it walks `threads -> stackTrace -> scopes ->
//! variables` for the top frame and prints a filtered tree to stdout.

use std::future::Future;
use std::pin::Pin;

use anyhow::Result;
use serde_json::json;

use crate::dap::types::{ScopesBody, StackTraceBody, StoppedBody, VariablesBody};
use crate::dap::{ConnEvent, DapClient, initialize_args};
use crate::model::is_noise;

const MAX_DEPTH: usize = 2; // how deep to expand containers in the printout
const MAX_CHILDREN: usize = 40; // cap children shown per node
const SHOW_GLOBALS: bool = false; // Globals is huge/noisy. Locals only by default

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
    client
        .request("initialize", Some(initialize_args()))
        .await?;
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

    client.disconnect();
    Ok(exit_code)
}

async fn handle_stop(client: &DapClient, body: StoppedBody, stop_number: u64) {
    let thread_id = body.thread_id.unwrap_or(0);
    let _ = client.request("threads", Some(json!({}))).await;

    let Ok(stack_trace) = client
        .request(
            "stackTrace",
            Some(json!({ "threadId": thread_id, "levels": 1 })),
        )
        .await
    else {
        return;
    };
    let frames = stack_trace
        .parse_body::<StackTraceBody>()
        .unwrap_or_default()
        .stack_frames;
    let Some(top) = frames.into_iter().next() else {
        return;
    };

    let bar = "━".repeat(64);
    println!("\n{bar}");
    println!(
        "⏸  stop #{stop_number}  ({})  →  {}  @ line {}",
        body.reason, top.name, top.line
    );
    println!("{bar}");

    let scopes = match client
        .request("scopes", Some(json!({ "frameId": top.id })))
        .await
    {
        Ok(resp) => resp.parse_body::<ScopesBody>().unwrap_or_default().scopes,
        Err(_) => return,
    };
    for scope in scopes {
        if scope.name == "Globals" && !SHOW_GLOBALS {
            println!(
                "  [{}] (hidden — set SHOW_GLOBALS=True to include)",
                scope.name
            );
            continue;
        }
        println!("  [{}]", scope.name);
        print_tree(client, scope.variables_reference, 1, "    ".to_string()).await;
    }
}

/// Recursively print a container's filtered children.
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
        let Ok(resp) = client
            .request("variables", Some(json!({ "variablesReference": var_ref })))
            .await
        else {
            return;
        };
        let variables = resp
            .parse_body::<VariablesBody>()
            .unwrap_or_default()
            .variables;
        let mut shown = 0;
        for var in variables {
            if is_noise(&var) {
                continue;
            }
            if shown >= MAX_CHILDREN {
                println!("{prefix}… more");
                break;
            }
            shown += 1;
            let ty = if var.ty.is_empty() {
                String::new()
            } else {
                format!(" : {}", var.ty)
            };
            println!("{prefix}{}{ty} = {}", var.name, var.value);
            if var.variables_reference != 0 {
                print_tree(
                    client,
                    var.variables_reference,
                    depth + 1,
                    format!("{prefix}    "),
                )
                .await;
            }
        }
    })
}
