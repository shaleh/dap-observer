//! DAP transport and message types.

pub mod transport;
pub mod types;

use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{Value, json};

pub use transport::{ConnEvent, DapClient, connect};

/// Upper bound on the late-join `initialize` round-trip. A mux that accepts the
/// connection but never answers must not wedge us here: this handshake runs
/// before the event loop, so without a bound there would be no live Ctrl-C or
/// key handling to abort it.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Perform the late-join `initialize` handshake, bounded by `HANDSHAKE_TIMEOUT`.
pub async fn initialize(client: &DapClient) -> Result<()> {
    tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        client.request("initialize", Some(initialize_args())),
    )
    .await
    .context("initialize handshake timed out — is the mux responding?")?
    .context("initialize handshake failed")?;
    Ok(())
}

/// Arguments for the minimal late-join `initialize`.
///
/// Crucially we do NOT advertise `supportsRunInTerminalRequest`, so the mux
/// routes any reverse `runInTerminal` request to the client driving the
/// session and never to us.
pub fn initialize_args() -> Value {
    json!({
        "adapterID": "dap-observer",
        "clientID": "dap-observer",
        "clientName": "dap-observer",
        "pathFormat": "path",
        "linesStartAt1": true,
        "columnsStartAt1": true,
        "supportsVariableType": true,
        "supportsRunInTerminalRequest": false
    })
}
