//! Plugin lifecycle as tools the agent can call itself.
//!
//! # Why this is a plugin and not five server tools
//!
//! The lifecycle operations already existed as HTTP routes, which made them
//! reachable by a human or the TUI and by nobody else. An agent that notices a
//! plugin is wedged had no way to act on it.
//!
//! Exposing them as tools could have meant special-casing them inside the
//! server, but DESIGN §13.9 is explicit that the core ships zero tools — even
//! `bash` and `read` live in the separate `kn9t-tools` plugin. So this is a
//! plugin like any other, and each tool is a thin relay over `host_api` to the
//! *same* `ServerState` methods the routes call. One implementation, two
//! callers (human over HTTP, agent over a tool call); no lifecycle logic lives
//! here to drift out of step.
//!
//! # Why every tool is `hidden`
//!
//! Five permanently-visible meta-tools would sit in the level-1 cache prefix of
//! every request for the sake of a situation that arises rarely. They are
//! declared `hidden: true` and this plugin reveals them itself, on a lifecycle
//! event it subscribes to (see `visibility.rs`). The server publishes the fact
//! and knows nothing about this plugin; the visibility policy is ours. Hidden
//! tools remain fully executable, so revealing one is a visibility change, not a
//! registration.

mod tools;
mod visibility;

use std::sync::Arc;

fn main() {
    // The sink is shared with the tools: `PluginEventSink::on_event` receives no
    // context, so the only way it can reach the host is a client cloned out of a
    // live tool dispatch (`Visibility::adopt`).
    let visibility = Arc::new(visibility::Visibility::new());

    kn9t_plugin_sdk::Plugin::new("kn9t-plugin-manager")
        .tool(tools::PluginList::new(visibility.clone()))
        .tool(tools::PluginStop::new(visibility.clone()))
        .tool(tools::PluginStart::new(visibility.clone()))
        .tool(tools::PluginReload::new(visibility.clone()))
        .tool(tools::PluginLoad::new(visibility.clone()))
        .event_sink(visibility::Sink(visibility))
        .run();
}
