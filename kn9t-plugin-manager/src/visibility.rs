//! Self-managed visibility.
//!
//! The five lifecycle tools ship `hidden: true`, so they cost nothing in the
//! cache prefix while the plugin set is behaving. This sink is what brings them
//! out, and the decision lives here rather than in the server on purpose: the
//! server publishes lifecycle facts and holds no opinion about which plugin
//! deserves to be surfaced. It knows nothing about this plugin's name.
//!
//! What arrives:
//!
//! * `plugin_state` — a plugin stopped, started, was reloaded, or crashed
//!   (the server's own observation of a poisoned host).
//! * `plugin_declared` — a plugin appeared or changed its tool set.
//!
//! Both are moments where "which plugins are loaded, and can I fix one?" stops
//! being idle curiosity. A crash is the strongest signal: the agent may have
//! just lost tools mid-turn and `plugin_reload` is the way back.
//!
//! Re-hiding is left to the user/agent for V1, as the ticket allows: an
//! automatic close after N quiet turns needs a turn counter the plugin does not
//! have, and a manager that vanishes mid-recovery is worse than one that
//! lingers.

use kn9t_plugin_sdk::ctx::HostApiClient;
use kn9t_plugin_sdk::traits::PluginEventSink;
use serde_json::{json, Value};
use std::sync::Mutex;

pub struct Visibility {
    /// Set once a dispatch hands us a client. `PluginEventSink::on_event` gets no
    /// context, so the only way to obtain one is to clone it out of a tool call —
    /// hence the shared slot rather than a field passed at construction.
    host: Mutex<Option<HostApiClient>>,
    /// Avoids re-sending `tool_visibility` on every event once revealed. Cheap,
    /// but the host logs each call, and a burst of `plugin_declared` at startup
    /// would otherwise write one line per plugin.
    revealed: Mutex<bool>,
}

impl Visibility {
    pub fn new() -> Self {
        Visibility {
            host: Mutex::new(None),
            revealed: Mutex::new(false),
        }
    }

    /// Remember a client so the sink can call the host later. Called from any
    /// tool dispatch (see `tools.rs`).
    pub fn adopt(&self, host: &HostApiClient) {
        let mut slot = self.host.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            *slot = Some(host.clone());
        }
    }

    /// Event kinds this plugin wants. Named here so both the direct impl and the
    /// `Arc` newtype answer identically.
    fn event_filter(&self) -> Vec<&'static str> {
        vec!["plugin_state", "plugin_declared"]
    }

    /// Decide whether an incoming lifecycle fact warrants surfacing the tools.
    fn handle(&self, kind: &str, event: &Value) {
        let plugin = event
            .get("plugin")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        match kind {
            "plugin_state" => {
                let state = event.get("state").and_then(|v| v.as_str()).unwrap_or("");
                match state {
                    // A crash may have taken tools away from a turn in progress; being
                    // able to reload is exactly the point.
                    "crashed" => self.reveal(&format!("'{plugin}' crashed")),
                    "stopped" => self.reveal(&format!("'{plugin}' stopped")),
                    // A successful start/reload is the resolution, not a problem. Stay put:
                    // whoever asked for it already had the tools they needed.
                    _ => {}
                }
            }
            "plugin_declared" => self.reveal(&format!("'{plugin}' declared tools")),
            _ => {}
        }
    }

    fn reveal(&self, reason: &str) {        {
            let mut revealed = self.revealed.lock().unwrap_or_else(|e| e.into_inner());
            if *revealed {
                return;
            }
            *revealed = true;
        }
        let client = {
            let slot = self.host.lock().unwrap_or_else(|e| e.into_inner());
            slot.clone()
        };
        // No client yet means no tool of ours has ever run, so nothing has needed
        // us: the next dispatch will adopt one, and the next event will reveal.
        let Some(client) = client else {
            *self.revealed.lock().unwrap_or_else(|e| e.into_inner()) = false;
            return;
        };
        // `tool_visibility` is scoped to the caller by the host, so this can only
        // ever affect our own five tools — there is no plugin name to pass.
        match client.call("tool_visibility", json!({ "hidden": false })) {
            Ok(_) => eprintln!("[plugin-manager] tools revealed ({reason})"),
            Err(e) => {
                eprintln!("[plugin-manager] reveal failed: {e}");
                *self.revealed.lock().unwrap_or_else(|e| e.into_inner()) = false;
            }
        }
    }
}

impl PluginEventSink for Visibility {
    fn event_filter(&self) -> Vec<&'static str> {
        Visibility::event_filter(self)
    }

    fn on_event(&self, kind: &str, event: &Value) {
        self.handle(kind, event);
    }
}

/// The same `Visibility` handed to the plugin as an event sink.
///
/// A newtype rather than `impl PluginEventSink for Arc<Visibility>`, which the orphan
/// rule forbids: both `Arc` and the trait are foreign to this crate.
pub struct Sink(pub std::sync::Arc<Visibility>);

impl PluginEventSink for Sink {
    fn event_filter(&self) -> Vec<&'static str> {
        Visibility::event_filter(&self.0)
    }

    fn on_event(&self, kind: &str, event: &Value) {
        self.0.handle(kind, event);
    }
}
