//! kn9t-git-integration — git integration for the TUI: status, log and a
//! reviewable working-tree diff.
//!
//! # Why there is no `git_status` tool
//!
//! This plugin used to ship one, ostensibly because "show me the current git
//! state" is a legitimate ask. In practice it existed mainly as a bootstrap
//! trigger: `PluginHook::call` received no host client, so the only way to
//! obtain one was inside a live tool dispatch. That workaround cost more than
//! it bought — the model was offered a tool duplicating a sidebar that already
//! displayed the same information, so it could burn a turn fetching what the
//! user could already see.
//!
//! `PluginHook::call_with_ctx` removed the gap. `get_steering` fires every turn
//! carrying `session_id` and `cwd`, which is exactly what the poller needs, so
//! bootstrap now follows a real lifecycle signal (see `bootstrap.rs`) and this
//! plugin exposes no agent-facing tools at all.
//!
//! # Division of labour
//!
//! Rust runs `git` and parses it: this is a native process with real shell
//! access, unlike the TUI's sandboxed Lua (no `io`, no `os.execute`), so it is
//! the only place that work can happen.
//!
//! Lua renders *and* handles input. `ui.lua` is sent once via
//! `ui_register_lua`, then each poll pushes fresh JSON via `ui_set_state`. The
//! view binds its own keys and clicks through `kn9t.on_key`/`kn9t.on_click`, so
//! the interactive diff panel ships with this plugin instead of every user
//! having to paste it into their own `tui.lua`.

mod bootstrap;
mod diff;
mod event_sink;
mod git;
mod poller;
mod tool;
mod ui;

fn main() {
    kn9t_plugin_sdk::Plugin::new("kn9t-git-integration")
        .hook(bootstrap::Bootstrap)
        .tool(tool::RequestCommitDiff)
        .event_sink(event_sink::GitEventSink::new())
        .run();
}
