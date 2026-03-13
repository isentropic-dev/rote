// WebSocket transport for CDP messages.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use serde_json::Value;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_tungstenite::tungstenite;

use super::{CdpError, protocol};

use protocol::{Command, Event, Message, RawMessage};

// Trait imports: needed for `.next()` on streams and `.send()` on sinks.
use futures_util::{SinkExt, StreamExt};

/// Pending response senders, keyed by command ID.
type PendingMap = HashMap<u64, oneshot::Sender<Result<Value, protocol::CdpError>>>;

/// Channel capacity for the event broadcast channel.
const EVENT_CHANNEL_CAPACITY: usize = 256;

/// Channel capacity for the internal command channel.
const COMMAND_CHANNEL_CAPACITY: usize = 64;

/// A CDP transport over WebSocket.
///
/// Handles sending commands with incrementing IDs,
/// routing responses back to callers,
/// and broadcasting events to subscribers.
pub struct Transport {
    /// Sends commands to the writer task.
    command_tx: mpsc::Sender<(Command, oneshot::Sender<Result<Value, protocol::CdpError>>)>,
    /// Broadcasts events to subscribers.
    event_tx: broadcast::Sender<Event>,
    /// Next command ID.
    next_id: Arc<AtomicU64>,
    /// Handle to the reader task for shutdown.
    reader_handle: tokio::task::JoinHandle<()>,
    /// Handle to the writer task for shutdown.
    writer_handle: tokio::task::JoinHandle<()>,
}

impl Transport {
    /// Connect to a CDP WebSocket endpoint.
    pub async fn connect(ws_url: &str) -> Result<Self, CdpError> {
        let (ws_stream, _) = tokio_tungstenite::connect_async(ws_url)
            .await
            .map_err(|e| CdpError::Connection(format!("WebSocket connection failed: {e}",)))?;

        let (ws_write, ws_read) = ws_stream.split();
        let (event_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let (command_tx, command_rx) = mpsc::channel(COMMAND_CHANNEL_CAPACITY);
        let next_id = Arc::new(AtomicU64::new(1));

        // Pending response map: command ID → oneshot sender.
        let pending: Arc<tokio::sync::Mutex<PendingMap>> =
            Arc::new(tokio::sync::Mutex::new(HashMap::new()));

        let reader_handle = {
            let event_tx = event_tx.clone();
            let pending = Arc::clone(&pending);
            tokio::spawn(Self::reader_loop(ws_read, event_tx, pending))
        };

        let writer_handle = {
            let pending = Arc::clone(&pending);
            tokio::spawn(Self::writer_loop(ws_write, command_rx, pending))
        };

        Ok(Self {
            command_tx,
            event_tx,
            next_id,
            reader_handle,
            writer_handle,
        })
    }

    /// Send a CDP command and wait for the response.
    pub async fn send(
        &self,
        method: impl Into<String>,
        params: Option<Value>,
    ) -> Result<Value, CdpError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let cmd = Command {
            id,
            method: method.into(),
            params,
        };

        let (response_tx, response_rx) = oneshot::channel();

        self.command_tx
            .send((cmd, response_tx))
            .await
            .map_err(|_| CdpError::Connection("transport writer closed".into()))?;

        response_rx
            .await
            .map_err(|_| CdpError::Connection("response channel dropped".into()))?
            .map_err(|e| CdpError::CommandFailed {
                code: e.code,
                message: e.message,
            })
    }

    /// Subscribe to CDP events.
    ///
    /// Returns a receiver that gets all events.
    /// The caller can filter by method name.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.event_tx.subscribe()
    }

    /// Shut down the transport, aborting background tasks.
    pub fn shutdown(self) {
        self.reader_handle.abort();
        self.writer_handle.abort();
    }

    /// Background task: reads WebSocket messages, routes responses and events.
    async fn reader_loop<S>(
        mut ws_read: S,
        event_tx: broadcast::Sender<Event>,
        pending: Arc<tokio::sync::Mutex<PendingMap>>,
    ) where
        S: StreamExt<Item = Result<tungstenite::Message, tungstenite::Error>> + Unpin,
    {
        while let Some(msg_result) = ws_read.next().await {
            let msg = match msg_result {
                Ok(tungstenite::Message::Text(text)) => text,
                Ok(tungstenite::Message::Close(_)) | Err(_) => break,
                Ok(_) => continue, // Binary, ping, pong — ignore.
            };

            let raw: RawMessage = match serde_json::from_str(&msg) {
                Ok(r) => r,
                Err(_) => continue, // Malformed message — skip.
            };

            match raw.classify() {
                Some(Message::Response(response)) => {
                    let mut pending = pending.lock().await;
                    if let Some(tx) = pending.remove(&response.id) {
                        let _ = tx.send(response.result);
                    }
                }
                Some(Message::Event(event)) => {
                    // Best-effort broadcast — if no subscribers, that's fine.
                    let _ = event_tx.send(event);
                }
                None => {}
            }
        }
    }

    /// Background task: serializes and sends commands over the WebSocket.
    async fn writer_loop<S>(
        mut ws_write: S,
        mut command_rx: mpsc::Receiver<(
            Command,
            oneshot::Sender<Result<Value, protocol::CdpError>>,
        )>,
        pending: Arc<tokio::sync::Mutex<PendingMap>>,
    ) where
        S: SinkExt<tungstenite::Message> + Unpin,
    {
        while let Some((cmd, response_tx)) = command_rx.recv().await {
            let id = cmd.id;
            let Ok(json) = serde_json::to_string(&cmd) else {
                let _ = response_tx.send(Err(protocol::CdpError {
                    code: -1,
                    message: "failed to serialize command".into(),
                    data: None,
                }));
                continue;
            };

            // Register the pending response before sending.
            pending.lock().await.insert(id, response_tx);

            let msg = tungstenite::Message::Text(json.into());
            if ws_write.send(msg).await.is_err() {
                // Remove the pending entry — the channel will report the error
                // when the sender is dropped.
                pending.lock().await.remove(&id);
                break;
            }
        }
    }
}
