//! Internal tools for Lua ↔ Rust communication.
//! These are hidden tools not exposed to the agent, only used by the plugin UI.

use kn9t_plugin_sdk::traits::PluginTool;
use kn9t_plugin_sdk::ctx::ToolCallCtx;
use kn9t_plugin_sdk::wire::ToolSpec;
use kn9t_plugin_sdk::{ToolOutput, ContentBlock};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

use crate::diff;

// Global state: the last requested commit sha
pub static REQUESTED_COMMIT_SHA: std::sync::OnceLock<Arc<Mutex<String>>> = std::sync::OnceLock::new();

pub fn get_requested_sha() -> String {
    let guard = REQUESTED_COMMIT_SHA
        .get_or_init(|| Arc::new(Mutex::new(String::new())))
        .lock()
        .unwrap();
    guard.clone()
}

pub fn set_requested_sha(sha: String) {
    let guard = REQUESTED_COMMIT_SHA
        .get_or_init(|| Arc::new(Mutex::new(String::new())))
        .lock()
        .unwrap();
    drop(guard);
    *REQUESTED_COMMIT_SHA
        .get_or_init(|| Arc::new(Mutex::new(String::new())))
        .lock()
        .unwrap() = sha;
}

pub struct RequestCommitDiff;

impl PluginTool for RequestCommitDiff {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "_request_commit_diff".into(),
            description: "Internal: request diff for a commit (Lua ↔ Rust communication)".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "sha": {
                        "type": "string",
                        "description": "Commit SHA to load diff for"
                    }
                },
                "required": ["sha"]
            }),
            parallel_safe: true,
            hidden: true,
            effects: vec![],
            policy: kn9t_plugin_sdk::wire::ToolPolicy::default(),
        }
    }

    fn execute(&self, args: &Value, ctx: &ToolCallCtx) -> ToolOutput {
        let sha = match args.get("sha").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return ToolOutput::error("missing sha"),
        };

        set_requested_sha(sha);
        ToolOutput::text("ok")
    }
}
