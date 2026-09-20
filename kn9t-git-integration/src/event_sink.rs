//! Event sink for UI interactions from the TUI.
//!
//! The Lua UI calls `kn9t.notify({ event = "...", ... })` which routes through
//! the server's `POST /plugin/{name}/ui_event` endpoint to this sink.

use kn9t_plugin_sdk::traits::PluginEventSink;
use serde_json::Value;

use crate::poller;

pub struct GitEventSink;

impl GitEventSink {
    pub fn new() -> Self {
        Self
    }
}

impl PluginEventSink for GitEventSink {
    fn event_filter(&self) -> Vec<&'static str> {
        vec!["plugin_notification", "ui_interaction"]
    }

    fn on_event(&self, _kind: &str, event: &Value) {
        let ui_event = event.get("event").and_then(|e| e.as_str()).unwrap_or("");
        let session_id = event.get("session_id").and_then(|s| s.as_str()).unwrap_or("");
        let data = event.get("data").cloned().unwrap_or(Value::Null);

        let Some(cwd) = poller::get_session_cwd(session_id) else {
            return;
        };

        match ui_event {
            "request_commit_diff" => {
                if let Some(sha) = data.get("sha").and_then(|s| s.as_str()) {
                    poller::request_commit_diff(&cwd, sha.to_string());
                }
            }
            "clear_commit_diff" => {
                poller::clear_commit_diff(&cwd);
            }
            _ => {}
        }
    }
}
