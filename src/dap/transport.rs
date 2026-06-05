//! Async DAP transport.
//!
//! A single connection task owns the `TcpStream`, does `Content-Length`
//! framing, and is the only thing that touches the socket. The rest of the app
//! talks to it over channels: requests go in via a command channel (each with a
//! `oneshot` reply), decoded events come out via an event channel.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};

use super::types::{Inbound, Response};

/// A command sent from the app to the connection task.
enum Command {
    Request {
        command: String,
        args: Option<Value>,
        reply: oneshot::Sender<Response>,
    },
    Disconnect {
        /// Fired once the `disconnect` frame has been written and flushed, so a
        /// caller can wait for it before the process exits.
        written: oneshot::Sender<()>,
    },
}

/// Upper bound on the wait for the `disconnect` frame to flush, so a dead
/// connection task can never hold up exit.
const DISCONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// What the connection task pushes toward the app.
pub enum ConnEvent {
    /// A decoded DAP event (`stopped`, `continued`, `terminated`, …).
    Dap(super::types::EventMessage),
    /// The connection ended. `Some(err)` for an error, `None` for a clean EOF.
    Disconnected(Option<String>),
}

/// Handle to the connection task.
#[derive(Clone)]
pub struct DapClient {
    cmd_tx: mpsc::UnboundedSender<Command>,
}

impl DapClient {
    /// Send a request and await its correlated response.
    pub async fn request(&self, command: &str, args: Option<Value>) -> Result<Response> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(Command::Request {
                command: command.to_string(),
                args,
                reply,
            })
            .map_err(|_| anyhow!("DAP connection task is gone"))?;
        rx.await
            .map_err(|_| anyhow!("DAP connection closed before responding to `{command}`"))
    }

    /// Send a non-terminating `disconnect` and wait for it to reach the socket.
    /// The mux synthetic-acks and the shared session keeps running for other
    /// clients. Waiting matters because callers exit the process right after, so
    /// a fire-and-forget send could be dropped before the frame is written.
    pub async fn disconnect(&self) {
        let (written, written_rx) = oneshot::channel();
        if self.cmd_tx.send(Command::Disconnect { written }).is_err() {
            return;
        }
        // A timeout (not just the channel closing) bounds the wait: if the task
        // is wedged mid-write we still exit rather than hang.
        let _ = tokio::time::timeout(DISCONNECT_TIMEOUT, written_rx).await;
    }
}

/// Connect to `address` and spawn the connection task.
///
/// Returns the client handle and the event receiver. A connection failure
/// (no mux listening) surfaces here as an `Err` so `main` can exit non-zero.
pub async fn connect(address: &str) -> Result<(DapClient, mpsc::UnboundedReceiver<ConnEvent>)> {
    let stream = TcpStream::connect(address)
        .await
        .with_context(|| format!("could not connect to {address} — is the mux running?"))?;
    stream.set_nodelay(true).ok();

    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    tokio::spawn(connection_task(stream, cmd_rx, event_tx));
    Ok((DapClient { cmd_tx }, event_rx))
}

async fn connection_task(
    stream: TcpStream,
    mut cmd_rx: mpsc::UnboundedReceiver<Command>,
    event_tx: mpsc::UnboundedSender<ConnEvent>,
) {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut pending: HashMap<i64, oneshot::Sender<Response>> = HashMap::new();
    let mut seq: i64 = 0;
    // Once all client handles drop, `cmd_rx.recv()` returns `None` forever and
    // would be perpetually ready; disabling the branch keeps the `select!` from
    // busy-spinning while it drains in-flight replies/events until EOF.
    let mut cmd_open = true;

    loop {
        tokio::select! {
            cmd = cmd_rx.recv(), if cmd_open => match cmd {
                Some(Command::Request { command, args, reply }) => {
                    seq += 1;
                    pending.insert(seq, reply);
                    if let Err(e) = write_message(&mut write_half, seq, &command, args).await {
                        let _ = event_tx.send(ConnEvent::Disconnected(Some(e.to_string())));
                        break;
                    }
                }
                Some(Command::Disconnect { written }) => {
                    seq += 1;
                    let _ = write_message(&mut write_half, seq, "disconnect", Some(json!({}))).await;
                    let _ = written.send(());
                }
                // All client handles dropped: nothing more to send. Stop polling
                // this branch and keep reading so in-flight replies/events can
                // still arrive until EOF.
                None => cmd_open = false,
            },
            msg = read_message(&mut reader) => match msg {
                Ok(Some(value)) => match serde_json::from_value::<Inbound>(value) {
                    Ok(Inbound::Response(resp)) => {
                        if let Some(tx) = pending.remove(&resp.request_seq) {
                            let _ = tx.send(resp);
                        }
                    }
                    Ok(Inbound::Event(ev)) => {
                        let _ = event_tx.send(ConnEvent::Dap(ev));
                    }
                    // Reverse requests (e.g. runInTerminal) are routed to the
                    // client driving the session, not us, and anything we can't
                    // classify is ignored.
                    Ok(Inbound::Request(_)) | Err(_) => {}
                },
                Ok(None) => {
                    let _ = event_tx.send(ConnEvent::Disconnected(None));
                    break;
                }
                Err(e) => {
                    let _ = event_tx.send(ConnEvent::Disconnected(Some(e.to_string())));
                    break;
                }
            },
        }
    }
}

/// Encode and write one `Content-Length`-framed request.
async fn write_message<W: AsyncWriteExt + Unpin>(
    write_half: &mut W,
    seq: i64,
    command: &str,
    args: Option<Value>,
) -> Result<()> {
    let mut msg = json!({ "seq": seq, "type": "request", "command": command });
    if let Some(args) = args {
        msg["arguments"] = args;
    }
    let data = serde_json::to_vec(&msg)?;
    let header = format!("Content-Length: {}\r\n\r\n", data.len());
    write_half.write_all(header.as_bytes()).await?;
    write_half.write_all(&data).await?;
    write_half.flush().await?;
    Ok(())
}

/// Read one `Content-Length`-framed message. `Ok(None)` signals a clean EOF.
async fn read_message<R: AsyncBufReadExt + Unpin>(reader: &mut R) -> Result<Option<Value>> {
    let mut content_length: Option<usize> = None;
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some(rest) = trimmed.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = Some(rest.trim().parse().context("invalid Content-Length")?);
        }
    }
    let len = content_length.ok_or_else(|| anyhow!("message missing Content-Length header"))?;
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;
    Ok(Some(serde_json::from_slice(&buf)?))
}
