//! dap-repl is an interactive console. It evaluates expressions you type
//! against the frame a dap-mux session is stopped at.
//!
//! What you type runs inside the live program, so it can change the shared
//! session.

mod repl;

use std::io::Write;

use anyhow::Result;
use clap::Parser;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc::UnboundedReceiver;

use dap_client::dap::types::{EventMessage, StoppedBody};
use dap_client::dap::{self, ConnEvent, DapClient};
use dap_client::model::SessionState;

use repl::Session;

const DEFAULT_ADDR: &str = "127.0.0.1:5679";

/// Interactive DAP expression evaluator for a dap-mux session.
#[derive(Parser)]
#[command(name = "dap-repl", version, about, long_about = None)]
struct Args {
    /// Mux address as host:port, or a bare port that assumes 127.0.0.1.
    #[arg(value_name = "host:port | port")]
    target: Option<String>,
}

impl Args {
    /// Turn the optional target into a concrete address. A bare port assumes the
    /// loopback host. No argument falls back to the default mux address.
    fn addr(&self) -> String {
        match self.target.as_deref() {
            None => DEFAULT_ADDR.to_string(),
            Some(t) if t.contains(':') => t.to_string(),
            Some(port) => format!("127.0.0.1:{port}"),
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let addr = args.addr();

    // A failed connection usually means nothing is listening on the mux.
    let (client, events) = match dap::connect(&addr).await {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("{e:#}");
            std::process::exit(2);
        }
    };

    let code = run(client, events, &addr).await?;
    std::process::exit(code);
}

fn prompt() {
    print!("(dap) ");
    let _ = std::io::stdout().flush();
}

async fn run(
    client: DapClient,
    mut events: UnboundedReceiver<ConnEvent>,
    addr: &str,
) -> Result<i32> {
    println!("connected to mux at {addr}");
    // Do the minimal late-join handshake. The mux then replays the current stopped state.
    dap::initialize(&client).await?;
    println!("initialized — evaluate an expression at a breakpoint (Ctrl-D to quit)");

    let mut session = Session::new();
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut exit_code = 0;
    prompt();

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                println!();
                break;
            }
            line = lines.next_line() => match line {
                Ok(Some(expr)) => {
                    eval_line(&client, &session, expr.trim()).await;
                    prompt();
                }
                // Stdin reached its end, for example from Ctrl-D.
                Ok(None) => {
                    println!();
                    break;
                }
                Err(e) => {
                    eprintln!("stdin error: {e}");
                    exit_code = 1;
                    break;
                }
            },
            ev = events.recv() => match ev {
                None => break,
                Some(ConnEvent::Disconnected(Some(err))) => {
                    eprintln!("\n!! {err}");
                    exit_code = 1;
                    break;
                }
                Some(ConnEvent::Disconnected(None)) => {
                    println!("\n■ session ended.");
                    break;
                }
                // Re-emit the prompt only when an event actually printed something.
                // Ignored events like output or module notices stay silent.
                Some(ConnEvent::Dap(msg)) => {
                    if handle_event(&client, &mut session, msg).await {
                        prompt();
                    }
                }
            },
        }
    }

    client.disconnect().await;
    Ok(exit_code)
}

/// Evaluate one entered line and print the result, or explain why it can't be.
/// Empty input is ignored.
async fn eval_line(client: &DapClient, session: &Session, expr: &str) {
    if expr.is_empty() {
        return;
    }
    let Some(frame_id) = session.frame_id() else {
        let why = match session.state {
            SessionState::Running => "session is running",
            SessionState::Ended => "session has ended",
            _ => "not stopped at a frame",
        };
        println!("-- nothing to evaluate against ({why})");
        return;
    };
    match repl::evaluate(client, expr, frame_id).await {
        Ok(ev) => match ev.ty {
            Some(ty) if !ty.is_empty() => println!("=> {} : {ty}", ev.result),
            _ => println!("=> {}", ev.result),
        },
        Err(e) => println!("!! {e}"),
    }
}

/// Update session state from a DAP event. Return whether it printed anything.
/// Each printed line starts with a newline so it breaks away from a dangling prompt.
async fn handle_event(client: &DapClient, session: &mut Session, msg: EventMessage) -> bool {
    match msg.event.as_str() {
        "stopped" => {
            let body: StoppedBody = msg
                .body
                .and_then(|b| serde_json::from_value(b).ok())
                .unwrap_or_default();
            match session.on_stopped(client, body.thread_id).await {
                Ok(Some(frame)) => {
                    println!(
                        "\n⏸ stop ({}) → {} @ line {}",
                        body.reason, frame.name, frame.line
                    )
                }
                Ok(None) => println!("\n⏸ stopped ({}) — no frame resolved", body.reason),
                Err(e) => println!("\n!! {e}"),
            }
            true
        }
        "continued" => {
            session.on_continued();
            println!("\n▶ running…");
            true
        }
        "terminated" | "exited" => {
            session.on_ended();
            println!("\n■ session ended.");
            true
        }
        _ => false,
    }
}
