// Chrome DevTools Protocol client.
//
// Finds, launches, and communicates with Chrome/Edge over CDP.
//
// # Integration testing
//
// Most of this module requires a real browser and is inherently
// integration-level.
// The following would need integration tests with Chrome installed:
//
// - `Browser::launch` end-to-end (discovery → launch → connect → domains).
// - `Transport::send` / `subscribe` with real CDP commands.
// - Tab listing and WebSocket URL resolution.
// - Domain enablement responses.
// - Event delivery (e.g. `Page.loadEventFired`).
//
// Unit tests cover serialization, deserialization, and discovery logic.

mod discover;
mod launch;
pub(crate) mod protocol;
mod transport;

pub use discover::{BrowserBinary, find_browser};
pub use launch::DEFAULT_PORT;
pub use protocol::Event;
pub use transport::Transport;

use serde_json::Value;
use tokio::sync::broadcast;

use protocol::ENABLED_DOMAINS;

/// Errors from the CDP layer.
#[derive(Debug, thiserror::Error)]
pub enum CdpError {
    /// Browser binary not found on the system.
    #[error("no supported browser found (Chrome or Edge)")]
    NoBrowser,

    /// Failed to launch the browser process.
    #[error("browser launch failed: {0}")]
    BrowserLaunch(String),

    /// No page tab available to connect to.
    #[error("no page tab found in browser")]
    NoTab,

    /// WebSocket or HTTP connection failure.
    #[error("connection error: {0}")]
    Connection(String),

    /// CDP protocol-level error (malformed response, unexpected format).
    #[error("protocol error: {0}")]
    Protocol(String),

    /// A CDP command returned an error.
    #[error("CDP command failed ({code}): {message}")]
    CommandFailed { code: i64, message: String },
}

/// A high-level handle to a browser controlled via CDP.
///
/// Manages the full lifecycle: discovery → launch → WebSocket connect → domain enablement.
/// On drop, the browser process is killed and the temporary profile directory is cleaned up.
pub struct Browser {
    // Field order matters: Rust drops fields in declaration order.
    // Transport must drop first (aborting WebSocket tasks) before
    // the process is killed.
    transport: Transport,
    #[allow(dead_code)] // Held for its Drop impl, which kills the browser.
    process: launch::BrowserProcess,
}

impl Browser {
    /// Find a browser, launch it, connect via CDP, and enable core domains.
    ///
    /// Uses the default debugging port.
    pub async fn launch() -> Result<Self, CdpError> {
        Self::launch_on_port(DEFAULT_PORT).await
    }

    /// Launch on a specific debugging port.
    pub async fn launch_on_port(port: u16) -> Result<Self, CdpError> {
        let binary = find_browser().ok_or(CdpError::NoBrowser)?;
        Self::launch_binary(&binary, port).await
    }

    /// Launch a specific browser binary on the given port.
    pub async fn launch_binary(binary: &BrowserBinary, port: u16) -> Result<Self, CdpError> {
        let process = launch::BrowserProcess::launch(binary, port).await?;

        // Get the WebSocket URL for the first page tab.
        let tab = process.first_page_tab().await?;
        let ws_url = tab
            .web_socket_debugger_url
            .ok_or_else(|| CdpError::Protocol("tab has no webSocketDebuggerUrl".into()))?;

        let transport = Transport::connect(&ws_url).await?;

        // Enable core domains.
        let mut browser = Self { transport, process };
        browser.enable_domains().await?;

        Ok(browser)
    }

    /// Send a CDP command and wait for the response.
    pub async fn send(
        &self,
        method: impl Into<String>,
        params: Option<Value>,
    ) -> Result<Value, CdpError> {
        self.transport.send(method, params).await
    }

    /// Evaluate a JavaScript expression in the page context.
    pub async fn evaluate(&self, expression: &str) -> Result<Value, CdpError> {
        self.send(
            "Runtime.evaluate",
            Some(serde_json::json!({
                "expression": expression,
                "returnByValue": true,
            })),
        )
        .await
    }

    /// Subscribe to CDP events.
    ///
    /// Returns a broadcast receiver.
    /// Filter by `event.method` to get specific event types.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.transport.subscribe()
    }

    /// Enable the core CDP domains needed for rote.
    async fn enable_domains(&mut self) -> Result<(), CdpError> {
        for domain in ENABLED_DOMAINS {
            self.send(format!("{domain}.enable"), None).await?;
        }
        Ok(())
    }
}
