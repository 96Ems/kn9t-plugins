//! The five plugin lifecycle tools. Each one forwards to a `host_api` op and
//! renders the host's reply; none of them decides anything.

use kn9t_plugin_sdk::ctx::ToolCallCtx;
use kn9t_plugin_sdk::traits::PluginTool;
use kn9t_plugin_sdk::wire::{DefaultPolicy, ToolPolicy, ToolSpec};
use kn9t_plugin_sdk::ToolOutput;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::visibility::Visibility;

/// Every tool here is `hidden` and none is `parallel_safe`: stopping or
/// respawning a subprocess while another lifecycle call is in flight on the same
/// plugin is precisely the race the host's locking is there to avoid.
fn spec(name: &str, description: &str, schema: Value, policy: DefaultPolicy) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: description.into(),
        schema,
        parallel_safe: false,
        hidden: true,
        effects: vec![],
        policy: ToolPolicy {
            pattern_field: Some("plugin".into()),
            default_policy: policy,
            builtin_allow: vec![],
            builtin_deny: vec![],
        },
    }
}

/// The `{"plugin": "<name>"}` schema shared by stop/start/reload.
fn plugin_name_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "plugin": {
                "type": "string",
                "description": "Declared name of the plugin, as reported by plugin_list."
            }
        },
        "required": ["plugin"]
    })
}

/// Relay one op and turn the host's `Err` into a tool error rather than a panic:
/// a refused or unknown plugin is an ordinary result the model should read and
/// react to, not a crash of the plugin process.
///
/// Also the moment the event sink acquires a host client: `on_event` is handed no
/// context, so a live dispatch is the only place one exists to be cloned.
fn relay(
    vis: &Visibility,
    ctx: &ToolCallCtx,
    op: &str,
    payload: Value,
) -> Result<Value, ToolOutput> {
    vis.adopt(&ctx.host);
    ctx.host
        .call(op, payload)
        .map_err(|e| ToolOutput::error(&format!("{op}: {e}")))
}

/// Boilerplate shared by all five tools: hold the shared `Visibility` and expose a
/// `new`. Written out once here rather than five times below.
macro_rules! lifecycle_tool {
    ($name:ident) => {
        pub struct $name {
            vis: Arc<Visibility>,
        }

        impl $name {
            pub fn new(vis: Arc<Visibility>) -> Self {
                Self { vis }
            }
        }
    };
}

lifecycle_tool!(PluginList);
lifecycle_tool!(PluginStop);
lifecycle_tool!(PluginStart);
lifecycle_tool!(PluginReload);
lifecycle_tool!(PluginLoad);

impl PluginTool for PluginList {
    fn spec(&self) -> ToolSpec {
        spec(
            "plugin_list",
            "List every loaded plugin with its state (running or stopped) and the tools it \
             contributes. Start here: the other plugin_* tools take the declared names this \
             returns.",
            json!({ "type": "object", "properties": {} }),
            // Read-only: nothing to approve.
            DefaultPolicy::Allow,
        )
    }

    fn execute(&self, _args: &Value, ctx: &ToolCallCtx) -> ToolOutput {
        let reply = match relay(&self.vis, ctx, "plugin_list", json!({})) {
            Ok(v) => v,
            Err(e) => return e,
        };
        let plugins = reply.get("plugins").and_then(|v| v.as_array()).cloned();
        let Some(plugins) = plugins else {
            return ToolOutput::error("plugin_list: host reply had no \"plugins\" array");
        };
        if plugins.is_empty() {
            return ToolOutput::text("No plugins loaded.");
        }
        let mut out = String::new();
        for p in &plugins {
            let name = p.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            let state = p.get("state").and_then(|v| v.as_str()).unwrap_or("?");
            let tools: Vec<&str> = p
                .get("tools")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|t| t.as_str()).collect())
                .unwrap_or_default();
            out.push_str(&format!("{name}  [{state}]  {} tool(s)", tools.len()));
            if !tools.is_empty() {
                out.push_str(&format!(": {}", tools.join(", ")));
            }
            out.push('\n');
        }
        ToolOutput::text(out.trim_end())
    }
}

impl PluginTool for PluginStop {
    fn spec(&self) -> ToolSpec {
        spec(
            "plugin_stop",
            "Stop a plugin's subprocess and leave it off. Its tools stay listed but calls to \
             them are refused until plugin_start. In-flight calls on that plugin are cancelled.",
            plugin_name_schema(),
            // Stopping a plugin can remove capabilities the model is mid-way
            // through using, so it goes through the normal approval path.
            DefaultPolicy::Ask,
        )
    }

    fn execute(&self, args: &Value, ctx: &ToolCallCtx) -> ToolOutput {
        let Some(name) = args.get("plugin").and_then(|v| v.as_str()) else {
            return ToolOutput::error("missing \"plugin\"");
        };
        match relay(&self.vis, ctx, "plugin_stop", json!({ "plugin": name })) {
            Ok(_) => ToolOutput::text(format!(
                "Stopped '{name}'. Its tools are still listed but will refuse to run until you \
                 call plugin_start."
            )),
            Err(e) => e,
        }
    }
}

impl PluginTool for PluginStart {
    fn spec(&self) -> ToolSpec {
        spec(
            "plugin_start",
            "Respawn a stopped plugin from the command it was originally loaded with. Only works \
             on a plugin the server already knows; use plugin_load for a new one.",
            plugin_name_schema(),
            DefaultPolicy::Ask,
        )
    }

    fn execute(&self, args: &Value, ctx: &ToolCallCtx) -> ToolOutput {
        let Some(name) = args.get("plugin").and_then(|v| v.as_str()) else {
            return ToolOutput::error("missing \"plugin\"");
        };
        match relay(&self.vis, ctx, "plugin_start", json!({ "plugin": name })) {
            Ok(v) => {
                let started = v.get("started").and_then(|s| s.as_str()).unwrap_or(name);
                let tools = v.get("tools").and_then(|t| t.as_u64()).unwrap_or(0);
                ToolOutput::text(format!(
                    "Started '{started}'. {tools} tool(s) registered in total."
                ))
            }
            Err(e) => e,
        }
    }
}

impl PluginTool for PluginReload {
    fn spec(&self) -> ToolSpec {
        spec(
            "plugin_reload",
            "Restart a plugin in place: cancel its in-flight calls, shut it down, respawn it, and \
             re-register its tools. Use after changing a plugin binary, or to recover one that \
             stopped responding.",
            plugin_name_schema(),
            DefaultPolicy::Ask,
        )
    }

    fn execute(&self, args: &Value, ctx: &ToolCallCtx) -> ToolOutput {
        let Some(name) = args.get("plugin").and_then(|v| v.as_str()) else {
            return ToolOutput::error("missing \"plugin\"");
        };
        match relay(&self.vis, ctx, "plugin_reload", json!({ "plugin": name })) {
            Ok(v) => {
                let reloaded = v.get("reloaded").and_then(|s| s.as_str()).unwrap_or(name);
                let tools = v.get("tools").and_then(|t| t.as_u64()).unwrap_or(0);
                ToolOutput::text(format!(
                    "Reloaded '{reloaded}'. {tools} tool(s) registered in total."
                ))
            }
            Err(e) => e,
        }
    }
}

impl PluginTool for PluginLoad {
    fn spec(&self) -> ToolSpec {
        spec(
            "plugin_load",
            "Load a plugin the server has never seen: either an explicit command, or every new \
             [[plugin]] entry in config.toml with from_config: true.",
            json!({
                "type": "object",
                "properties": {
                    "cmd": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Executable plus arguments, e.g. [\"/path/to/my-plugin\"]."
                    },
                    "env": {
                        "type": "object",
                        "additionalProperties": { "type": "string" },
                        "description": "Environment variables to inject into the subprocess."
                    },
                    "from_config": {
                        "type": "boolean",
                        "description": "Re-read config.toml and load any [[plugin]] entry not \
                                        already running. Ignores cmd/env when true."
                    }
                }
            }),
            // Spawning an arbitrary binary is the most consequential op here.
            DefaultPolicy::Ask,
        )
    }

    fn execute(&self, args: &Value, ctx: &ToolCallCtx) -> ToolOutput {
        let from_config = args
            .get("from_config")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let mut payload = json!({});
        if from_config {
            payload["from_config"] = json!(true);
        } else {
            match args.get("cmd").and_then(|v| v.as_array()) {
                Some(cmd) if !cmd.is_empty() => payload["cmd"] = json!(cmd),
                _ => {
                    return ToolOutput::error(
                        "plugin_load needs either \"cmd\" (non-empty array) or \
                         \"from_config\": true",
                    )
                }
            }
            if let Some(env) = args.get("env") {
                payload["env"] = env.clone();
            }
        }

        let reply = match relay(&self.vis, ctx, "plugin_load", payload) {
            Ok(v) => v,
            Err(e) => return e,
        };

        // `from_config` reports a list; an explicit cmd reports one name.
        if let Some(list) = reply.get("loaded").and_then(|v| v.as_array()) {
            if list.is_empty() {
                return ToolOutput::text("No new plugins found in config.toml.");
            }
            let names: Vec<String> = list
                .iter()
                .map(|p| {
                    let n = p.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                    let t = p.get("tools").and_then(|v| v.as_u64()).unwrap_or(0);
                    format!("{n} ({t} tool(s))")
                })
                .collect();
            return ToolOutput::text(format!("Loaded from config: {}", names.join(", ")));
        }
        let name = reply
            .get("loaded")
            .and_then(|v| v.as_str())
            .unwrap_or("plugin");
        let tools = reply.get("tools").and_then(|v| v.as_u64()).unwrap_or(0);
        ToolOutput::text(format!("Loaded '{name}' with {tools} tool(s)."))
    }
}

