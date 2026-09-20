//! Session bootstrap via lifecycle hook.
//!
//! Replaces the old arrangement where a `git_status` *tool* existed mainly so
//! that calling it would start the poller. That was a workaround for a real
//! gap — `PluginHook::call` used to receive no host client — but it had a bad
//! consequence: the model was offered a tool whose true purpose was internal
//! bootstrapping, so "show me git state" competed with the sidebar that was
//! already showing exactly that.
//!
//! With `PluginHook::call_with_ctx` the gap is gone. `get_steering` fires every
//! turn carrying `session_id` and `cwd` (API.md §5.7), which is precisely what
//! the poller needs, so bootstrap is driven by a real lifecycle signal and the
//! plugin ships no agent-facing tool at all.
//!
//! The hook itself contributes no steering messages: it is an observer that
//! returns the empty reply.

use kn9t_plugin_sdk::ctx::HookCtx;
use kn9t_plugin_sdk::traits::PluginHook;
use serde_json::{json, Value};

use crate::poller;

pub struct Bootstrap;

impl PluginHook for Bootstrap {
    fn hooks(&self) -> Vec<&'static str> {
        vec!["get_steering"]
    }

    /// Never called: `call_with_ctx` is overridden and does not delegate here.
    /// Present only because the trait requires it.
    fn call(&self, _hook: &str, _payload: &Value) -> Value {
        json!({ "messages": [] })
    }

    fn call_with_ctx(&self, _hook: &str, _payload: &Value, ctx: &HookCtx) -> Value {
        // No cwd means the host could not resolve one for this session; there is
        // nothing to poll, and guessing `std::env::current_dir()` would attach
        // the panel to whatever directory the *plugin process* started in.
        if let Some(cwd) = ctx.cwd.clone() {
            poller::ensure_started(ctx.host.clone(), cwd, ctx.session_id.clone());
        }
        json!({ "messages": [] })
    }
}
