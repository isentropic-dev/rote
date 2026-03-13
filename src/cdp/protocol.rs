// CDP message types and serialization.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// An outgoing CDP command.
#[derive(Debug, Serialize)]
pub struct Command {
    pub id: u64,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// A raw incoming CDP message, before we know if it's a response or event.
#[derive(Debug, Deserialize)]
pub struct RawMessage {
    /// Present on command responses.
    pub id: Option<u64>,
    /// Present on events (and also responses carry the method? no — only events).
    pub method: Option<String>,
    /// Present on successful responses.
    pub result: Option<Value>,
    /// Present on error responses.
    pub error: Option<WireError>,
    /// Present on events.
    pub params: Option<Value>,
}

/// A CDP error object from a failed command (wire-level).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WireError {
    pub code: i64,
    pub message: String,
    pub data: Option<String>,
}

/// A parsed incoming message: either a response to a command or an event.
#[derive(Debug)]
pub enum Message {
    Response(Response),
    Event(Event),
}

/// Response to a command we sent.
#[derive(Debug)]
pub struct Response {
    pub id: u64,
    pub result: Result<Value, WireError>,
}

/// A CDP event pushed by the browser.
#[derive(Debug, Clone)]
pub struct Event {
    pub method: String,
    pub params: Value,
}

impl RawMessage {
    /// Classify this raw message as either a response or an event.
    pub fn classify(self) -> Option<Message> {
        if let Some(id) = self.id {
            // It's a response.
            let result = if let Some(err) = self.error {
                Err(err)
            } else {
                Ok(self.result.unwrap_or(Value::Null))
            };
            Some(Message::Response(Response { id, result }))
        } else if let Some(method) = self.method {
            Some(Message::Event(Event {
                method,
                params: self.params.unwrap_or(Value::Null),
            }))
        } else {
            None
        }
    }
}

/// Response from `http://host:port/json/version`.
#[derive(Debug, Deserialize)]
pub struct BrowserVersion {
    #[serde(rename = "Browser")]
    pub browser: String,
    #[serde(rename = "Protocol-Version")]
    pub protocol_version: String,
    #[serde(rename = "webSocketDebuggerUrl")]
    pub ws_debugger_url: Option<String>,
}

impl BrowserVersion {
    /// Get the WebSocket debugger URL.
    pub fn debugger_url(&self) -> Option<&str> {
        self.ws_debugger_url.as_deref()
    }
}

/// Response entry from `http://host:port/json/list` (tab list).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TabInfo {
    pub id: String,
    #[serde(rename = "type")]
    pub target_type: String,
    pub title: String,
    pub url: String,
    pub web_socket_debugger_url: Option<String>,
}

/// The CDP domains we enable on connection.
pub const ENABLED_DOMAINS: &[&str] = &["Runtime", "Page", "DOM"];

// Test-only helpers for building CDP commands.

#[cfg(test)]
fn enable_command(id: u64, domain: &str) -> Command {
    Command {
        id,
        method: format!("{domain}.enable"),
        params: None,
    }
}

#[cfg(test)]
fn runtime_evaluate(id: u64, expression: &str) -> Command {
    Command {
        id,
        method: "Runtime.evaluate".into(),
        params: Some(serde_json::json!({
            "expression": expression,
            "returnByValue": true,
        })),
    }
}

#[cfg(test)]
fn domain_for_method(method: &str) -> Option<&str> {
    method.split('.').next()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_serializes_without_params() {
        let cmd = Command {
            id: 1,
            method: "Page.enable".into(),
            params: None,
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let parsed: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["id"], 1);
        assert_eq!(parsed["method"], "Page.enable");
        assert!(parsed.get("params").is_none());
    }

    #[test]
    fn command_serializes_with_params() {
        let cmd = runtime_evaluate(42, "document.title");
        let json = serde_json::to_string(&cmd).unwrap();
        let parsed: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["id"], 42);
        assert_eq!(parsed["method"], "Runtime.evaluate");
        assert_eq!(parsed["params"]["expression"], "document.title");
        assert_eq!(parsed["params"]["returnByValue"], true);
    }

    #[test]
    fn raw_response_classifies_as_response() {
        let raw: RawMessage =
            serde_json::from_str(r#"{"id": 1, "result": {"value": "hello"}}"#).unwrap();
        let msg = raw.classify().unwrap();
        match msg {
            Message::Response(r) => {
                assert_eq!(r.id, 1);
                assert!(r.result.is_ok());
            }
            Message::Event(_) => panic!("expected response"),
        }
    }

    #[test]
    fn raw_error_response_classifies() {
        let raw: RawMessage =
            serde_json::from_str(r#"{"id": 2, "error": {"code": -32000, "message": "not found"}}"#)
                .unwrap();
        let msg = raw.classify().unwrap();
        match msg {
            Message::Response(r) => {
                assert_eq!(r.id, 2);
                let err = r.result.unwrap_err();
                assert_eq!(err.code, -32000);
                assert_eq!(err.message, "not found");
            }
            Message::Event(_) => panic!("expected response"),
        }
    }

    #[test]
    fn raw_event_classifies_as_event() {
        let raw: RawMessage = serde_json::from_str(
            r#"{"method": "Page.loadEventFired", "params": {"timestamp": 123.4}}"#,
        )
        .unwrap();
        let msg = raw.classify().unwrap();
        match msg {
            Message::Event(e) => {
                assert_eq!(e.method, "Page.loadEventFired");
                assert_eq!(e.params["timestamp"], 123.4);
            }
            Message::Response(_) => panic!("expected event"),
        }
    }

    #[test]
    fn domain_extraction() {
        assert_eq!(domain_for_method("Page.loadEventFired"), Some("Page"));
        assert_eq!(domain_for_method("Runtime.evaluate"), Some("Runtime"));
        assert_eq!(domain_for_method("nodot"), Some("nodot"));
    }

    #[test]
    fn enable_command_format() {
        let cmd = enable_command(5, "Runtime");
        assert_eq!(cmd.method, "Runtime.enable");
        assert_eq!(cmd.id, 5);
        assert!(cmd.params.is_none());
    }

    #[test]
    fn tab_info_deserializes() {
        let json = r#"{
            "id": "ABC123",
            "type": "page",
            "title": "Test Page",
            "url": "https://example.com",
            "webSocketDebuggerUrl": "ws://localhost:9222/devtools/page/ABC123"
        }"#;
        let tab: TabInfo = serde_json::from_str(json).unwrap();
        assert_eq!(tab.id, "ABC123");
        assert_eq!(tab.target_type, "page");
        assert_eq!(
            tab.web_socket_debugger_url.as_deref(),
            Some("ws://localhost:9222/devtools/page/ABC123"),
        );
    }

    #[test]
    fn browser_version_debugger_url() {
        // camelCase variant.
        let json = r#"{
            "Browser": "Chrome/120",
            "Protocol-Version": "1.3",
            "webSocketDebuggerUrl": "ws://localhost:9222/devtools/browser/abc"
        }"#;
        let ver: BrowserVersion = serde_json::from_str(json).unwrap();
        assert_eq!(
            ver.debugger_url(),
            Some("ws://localhost:9222/devtools/browser/abc"),
        );
    }
}
